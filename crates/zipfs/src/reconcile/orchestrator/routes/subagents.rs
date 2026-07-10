//! subagents 子会话目录：强制无损并集（绝不按 mtime 取舍）。
use std::io;

use crate::reconcile::orchestrator::*;

/// 判定快照条目是否属于 subagents 子会话目录（`<uuid>/subagents/*.jsonl`）。
///
/// 判据：`rel` 含名为 `subagents` 的普通路径段 **且** 以 `.jsonl` 结尾。此类条目在 `apply_entry`
/// 里被**优先路由**到 `reconcile_subagents_dir` 强制无损并集，绕过 advisor 的 SuspectReuse→隔离——
/// 子代理 transcript 天然按子代理分文件、uuid 各自独立，并集安全且不可按 mtime 取舍。
pub(crate) fn is_subagents_entry(rel: &str) -> bool {
    rel.ends_with(".jsonl")
        && Path::new(rel).components().any(
            |c| matches!(c, std::path::Component::Normal(s) if s.eq_ignore_ascii_case("subagents")),
        )
}

/// `rel` 顶层路径段（第一个 Normal 组件）。用于把 memory 物化条目归到其顶层目录（如 `memory`）。
fn top_component(rel: &str) -> Option<String> {
    Path::new(rel).components().find_map(|c| match c {
        std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
        _ => None,
    })
}

/// 判定条目是否属 memory 透传：其顶层段在 backing 里是 **symlink**（apply 期照 Claude 外链重建的
/// `memory` 软链）。停用期 Claude 把外链内容物化进 underlay 真实目录，此类条目应走透传恢复而非并入历史。
pub(crate) fn is_passthrough_entry(paths: &Paths, name: &str, rel: &str) -> bool {
    passthrough_top_symlink(paths, name, rel)
        .ok()
        .flatten()
        .is_some()
}

/// 若条目顶层段在 backing 是 symlink，返回 `(顶层段, symlink 目标)`；否则 `None`。
/// name 已在 `apply_entry` 入口 `validate_name`，此处只读 backing 元数据。
pub(crate) fn passthrough_top_symlink(
    paths: &Paths,
    name: &str,
    rel: &str,
) -> io::Result<Option<(String, PathBuf)>> {
    let Some(top) = top_component(rel) else {
        return Ok(None);
    };
    let link = paths.backing(name, Backend::Shadow).join(&top);
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(&link)?;
            Ok(Some((top, target)))
        }
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// subagents 子会话 jsonl 合并（**与主 jsonl 同一 `session_merge` 规则**，但强制无损并集）：
///
/// orig 有对应文件 → `session_merge` 并集写回；orig 无 → 全新落盘（New）。随后 `reingest_one_file`
/// 原子重灌 backing，最后经 `finish_delete`（LinesSuperset）删 underlay。**绝不按 mtime 删较旧者**、
/// 同名异内容一律并集保两侧——子代理 transcript disjoint uuid 是常态（各子代理独立），并入无丢失；
/// 隔离/取舍反而会丢子会话历史，故 subagents 一律并集。改 orig 前照旧 stash 前镜像（可回滚）。
pub fn reconcile_subagents_dir(
    paths: &Paths,
    name: &str,
    snap_entry: &EntrySnapshot,
    mp: &Path,
    ts: &str,
) -> io::Result<EntryReport> {
    validate_name(name)?;
    let rel = snap_entry.rel.clone();
    let orig_file = paths.orig(name).join(&rel);
    let mut notes: Vec<String> = vec!["subagents：强制无损并集（绝不按 mtime 取舍）".into()];

    let has_preimage = stash_orig_preimage(paths, name, &rel, ts, &mut notes)?;
    if let Some(parent) = orig_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let merged_bytes = if orig_file.exists() {
        let base_bytes = std::fs::read(&orig_file)?;
        let base_str = String::from_utf8_lossy(&base_bytes);
        let inc_str = String::from_utf8_lossy(&snap_entry.bytes);
        let merged = session_merge(base_str.as_ref(), inc_str.as_ref());
        let merged_bytes = lines_to_bytes(&merged.merged_lines);
        // 评审 R-C1：base 侧超集铁律（同 apply_entry Union）。不覆盖则中止、保两份。
        if !crate::reconcile::merge::base_covered_by_merged(
            &base_str,
            &String::from_utf8_lossy(&merged_bytes),
        ) {
            notes.push(
                "subagents 合并未覆盖 base 全部记录（疑合并核缺陷）→ 中止：不改 orig、不删 underlay".into(),
            );
            return Ok(EntryReport {
                name: rel,
                decision: "subagents".into(),
                action: "aborted-base-not-covered".into(),
                notes,
                reversal: ReversalClass::Noop,
            });
        }
        merged_bytes
    } else {
        snap_entry.bytes.clone()
    };
    atomic_write(&orig_file, &merged_bytes)?;
    reingest_one_file(paths, name, &rel)?;
    finish_delete(
        snap_entry,
        &orig_file,
        SupersetMode::LinesSuperset,
        mp,
        "subagents-union",
        reversal_for_preimage(has_preimage),
        notes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::orchestrator::testsupport::*;

    #[test]
    fn subagents_dir_unions_disjoint_uuids_without_mtime_delete() {
        // subagents 同名两侧 disjoint uuid（主 jsonl 规则会判 SuspectReuse→隔离），但 subagents
        // 强制无损并集：两侧 uuid 都保留、无一方被删。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "sess-uuid/subagents/agent-1.jsonl";
        let base = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"sa1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
        );
        let incoming = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"sb1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
        );
        let orig_file = write_orig(&paths, "demo", rel, base.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, incoming.as_bytes());

        let report = reconcile_subagents_dir(&paths, "demo", &snap_e, &mp, "0").unwrap();

        // orig 现含两侧 uuid（并集，无一方被丢）。
        let merged = std::fs::read_to_string(&orig_file).unwrap();
        assert!(merged.contains("sa1"), "base uuid 保留：{merged}");
        assert!(merged.contains("sb1"), "incoming uuid 并入：{merged}");
        // backing 重灌为并集。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), merged.as_bytes());
        // underlay 经 LinesSuperset 校验后删。
        assert!(!mp.join(rel).exists(), "并集且校验后应删 underlay");
        assert!(report.action.contains("underlay-removed"));
        assert!(report.decision.contains("subagents"));
    }

}
