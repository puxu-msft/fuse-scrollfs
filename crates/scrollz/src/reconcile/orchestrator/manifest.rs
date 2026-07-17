//! per-generation manifest（undo 依赖，§10.1）：落盘 / 读回 / rel 安全校验。
use std::io;

use super::*;

// ── per-generation manifest（undo 依赖，§10.1） ───────────────────────────────

/// 落盘一次 reconcile run 的 per-generation manifest 到 `reconcile_manifest(name,ts)`：首行 `ts`，其后
/// 每行真实 `rel\tclass`（逆转类）。**过滤合成审计条目**（`<prune>`/`<meta>` 等非真实 rel）与 `Noop`
/// 条目（identical/skip 等无需反做，underlay 快照全局还原即可覆盖），只写 undo 真正需要逐条反做的条目。
///
/// 原子写 + fsync（`atomic_write`）：manifest 存在即代表该代次可 undo；不完整写入绝不半落盘。best-effort
/// 由调用方兜底（写失败该 run 不可 undo，但不阻断收尾）。
pub(crate) fn write_manifest(paths: &Paths, name: &str, ts: &str, entries: &[EntryReport]) -> io::Result<()> {
    validate_name(name)?;
    let mut body = String::new();
    body.push_str(ts);
    body.push('\n');
    for e in entries {
        if is_synthetic_rel(&e.name) || e.reversal == ReversalClass::Noop {
            continue;
        }
        body.push_str(&e.name);
        body.push('\t');
        body.push_str(e.reversal.as_str());
        body.push('\n');
    }
    let dst = paths.reconcile_manifest(name, ts);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&dst, body.as_bytes())
}

/// 读回 `reconcile_manifest(name,ts)`：manifest 不存在 → `Ok(None)`（该代次无 undo 依据）；存在则解析
/// 首行 `ts` 之后的每行 `rel\tclass` 为 `(rel, ReversalClass)`。空行跳过；无法解析的行（缺 tab / 未知
/// class）→ `Err`（fail-closed：宁可拒绝 undo 也不静默漏条）。`reconcile_undo` 消费。
/// 校验 manifest 读回的相对路径 `rel`（Task2 Minor，纵深防御）：`rel` 是**多段相对路径**（如
/// `<uuid>/subagents/x.jsonl`），须每个组件均为 `Normal`——即非 `..`、非绝对根、非 `.`、非空。rel 实源自
/// 真实目录 walk（无 `..`）、stash 本地同信任域，风险低，但反做入口直接 `orig/backing/mp.join(rel)`，故作
/// 纵深防御拒绝穿越/绝对/空。命中（返回 `false`）由 `read_manifest` 跳过该条 + 记 warn，不中止整个 undo。
pub(crate) fn is_safe_rel(rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let mut saw_normal = false;
    for comp in Path::new(rel).components() {
        match comp {
            std::path::Component::Normal(_) => saw_normal = true,
            // RootDir（绝对）/ ParentDir（..）/ CurDir（.）/ Prefix 均拒。
            _ => return false,
        }
    }
    saw_normal
}

pub(crate) fn read_manifest(
    paths: &Paths,
    name: &str,
    ts: &str,
) -> io::Result<Option<Vec<(String, ReversalClass)>>> {
    validate_name(name)?;
    let path = paths.reconcile_manifest(name, ts);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    // 首行是 ts 头，跳过。
    for line in content.lines().skip(1) {
        if line.is_empty() {
            continue;
        }
        let (rel, class) = line.split_once('\t').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest 行缺 tab 分隔：{line:?}"),
            )
        })?;
        let reversal = ReversalClass::parse(class).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest 未知逆转类：{class:?}"),
            )
        })?;
        // Task2 Minor：纵深防御——拒绝含 `..`/绝对/空组件的 rel（跳过该条 + warn，不中止整个 undo）。
        if !is_safe_rel(rel) {
            log::warn!(
                "{name} 代次 {ts} manifest rel {rel:?} 含非法组件（穿越/绝对/空），跳过该条 undo"
            );
            continue;
        }
        out.push((rel.to_owned(), reversal));
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::orchestrator::testsupport::*;

    #[test]
    fn read_manifest_skips_traversal_and_absolute_rel() {
        // Task2 Minor：manifest 含 `../evil` / 绝对 / 空 rel → 跳过该条（不 join 到树外），合法条保留。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let ts = "7";
        let manifest = paths.reconcile_manifest("demo", ts);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        // 首行 ts 头，随后混入穿越/绝对/空 rel 与一条合法 rel。
        std::fs::write(
            &manifest,
            "7\n\
             ../evil.jsonl\tRestoreOrig\n\
             /etc/passwd\tRemoveOrig\n\
             a/../../b.jsonl\tRestoreOrig\n\
             good/s.jsonl\tRemoveOrig\n",
        )
        .unwrap();

        let out = read_manifest(&paths, "demo", ts).unwrap().unwrap();
        // 仅合法条保留；三条非法 rel 全被跳过。
        assert_eq!(
            out,
            vec![("good/s.jsonl".to_string(), ReversalClass::RemoveOrig)],
            "穿越/绝对/含 .. 的 rel 必须被跳过，仅保留合法条"
        );
    }

    #[test]
    fn is_safe_rel_accepts_multi_segment_rejects_traversal() {
        // 多段相对路径合法；`..`/绝对/`.`/空 均拒。
        assert!(is_safe_rel("uuid/subagents/x.jsonl"));
        assert!(is_safe_rel("s.jsonl"));
        assert!(!is_safe_rel(""));
        assert!(!is_safe_rel("../evil"));
        assert!(!is_safe_rel("a/../b"));
        assert!(!is_safe_rel("/etc/passwd"));
        assert!(!is_safe_rel("."));
    }

}
