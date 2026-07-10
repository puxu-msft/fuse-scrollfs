//! 逐条目分类规划（据快照，非 live）+ advisor 建议构造。
use std::io;

use super::*;

/// has_preimage 布尔 → 逆转类：改 orig 前有前镜像（union/merge）→ `RestoreOrig`（可原子还原）；无前镜像
/// （New，orig 是新增出来的）→ `RemoveOrig`（undo 删 orig + backing，防孤儿）。判别子就是 has_preimage。
pub(crate) fn reversal_for_preimage(has_preimage: bool) -> ReversalClass {
    if has_preimage {
        ReversalClass::RestoreOrig
    } else {
        ReversalClass::RemoveOrig
    }
}

/// 合成审计条目（非真实 rel）判定：`<prune>`/`<meta>`/`<rebuild>`/`<prune-symlinks>` 等以 `<` 开头的占位
/// 名，仅供人类审计、无对应磁盘 rel，一律不写入 manifest（评审 I-plan1）。
pub(crate) fn is_synthetic_rel(rel: &str) -> bool {
    rel.starts_with('<')
}

/// 逐条目规划（**从快照读 incoming**，非 live underlay，评审 I-7；base 取 orig；**不动盘**）。
///
/// **优先路由**（先于 size-cap/base 分类，与 `apply_entry` 同序，令 dry-run 报告如实反映 apply）：
/// - `is_subagents_entry` → `Union`（子会话强制无损并集，apply 绕过 advisor 隔离）。
/// - `is_passthrough_entry`（backing 顶层段是外链 symlink）→ `Passthrough`（写 canonical target，绝不落 orig）。
///
/// 否则对每个快照条目：base = `orig/<rel>`（存在则读，不存在 = New），incoming = 快照 `bytes`。
/// 超 `MAX_MERGE_FILE_BYTES`（快照未整体读入、`bytes` 为空）→ 降级 `KeepBoth`；否则：
/// orig 缺 → `New`；base 逐字节 == incoming → `Identical`；`.jsonl` → `session_merge` + `recommend`
/// 按 advisor 动作映射 EntryPlan；非 jsonl 且不同 → 保守 `KeepSeparate`（不做行合并，留 Task 8）。
pub fn plan_entries(
    paths: &Paths,
    name: &str,
    snap: &UnderlaySnapshot,
) -> io::Result<Vec<(String, EntryPlan, Recommendation)>> {
    validate_name(name)?;
    let orig_root = paths.orig(name);
    let mut out = Vec::with_capacity(snap.entries.len());
    for e in &snap.entries {
        // 优先路由（与 apply_entry 同序）：subagents/透传绕过 plan 的 size-cap/base 分类，否则报告
        // 会显示 New/KeepSeparate 而 apply 实际走并集/透传（写 canonical target，绝不落 orig）。
        if is_subagents_entry(&e.rel) {
            out.push((e.rel.clone(), EntryPlan::Union, subagents_rec()));
            continue;
        }
        if is_passthrough_entry(paths, name, &e.rel) {
            out.push((e.rel.clone(), EntryPlan::Passthrough, passthrough_rec()));
            continue;
        }
        if e.size > MAX_MERGE_FILE_BYTES {
            out.push((e.rel.clone(), EntryPlan::KeepBoth, oversize_rec()));
            continue;
        }
        let base = match std::fs::read(orig_root.join(&e.rel)) {
            Ok(b) => Some(b),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        let (plan, rec) = match base {
            None => (EntryPlan::New, new_entry_rec()),
            Some(base_bytes) if base_bytes == e.bytes => (EntryPlan::Identical, identical_rec()),
            Some(base_bytes) if e.rel.ends_with(".jsonl") => {
                let base_str = String::from_utf8_lossy(&base_bytes);
                let inc_str = String::from_utf8_lossy(&e.bytes);
                let merged = session_merge(base_str.as_ref(), inc_str.as_ref());
                let rec = recommend(&merged);
                (plan_from_action(&rec.action), rec)
            }
            Some(_) => (EntryPlan::KeepSeparate, non_jsonl_diff_rec()),
        };
        out.push((e.rel.clone(), plan, rec));
    }
    Ok(out)
}

/// advisor 动作 → EntryPlan 映射（jsonl 合并路径）。
fn plan_from_action(a: &Action) -> EntryPlan {
    match a {
        Action::UnionIntoBase => EntryPlan::Union,
        Action::KeepSeparate => EntryPlan::KeepSeparate,
        Action::PassthroughRestore => EntryPlan::Passthrough,
        Action::KeepBoth => EntryPlan::KeepBoth,
    }
}

fn oversize_rec() -> Recommendation {
    Recommendation {
        action: Action::KeepBoth,
        confidence: Confidence::Low,
        rationale: format!(
            "超单文件合并上限 {MAX_MERGE_FILE_BYTES}B，降级 KeepBoth 保两份（不有损合并）"
        ),
    }
}

fn subagents_rec() -> Recommendation {
    Recommendation {
        action: Action::UnionIntoBase,
        confidence: Confidence::High,
        rationale:
            "subagents 子会话无损并集（apply 时并入 orig 对应路径 + reingest，绝不按 mtime 取舍）"
                .into(),
    }
}

fn passthrough_rec() -> Recommendation {
    Recommendation {
        action: Action::PassthroughRestore,
        confidence: Confidence::Medium,
        rationale: "memory 外链透传恢复：新文件复制进 canonical 目标、冲突改名保两份，绝不落 orig"
            .into(),
    }
}

fn new_entry_rec() -> Recommendation {
    Recommendation {
        action: Action::UnionIntoBase,
        confidence: Confidence::High,
        rationale: "orig 无此条目，全新 fall-through 文件直接落 orig（无 base 冲突）".into(),
    }
}

fn identical_rec() -> Recommendation {
    Recommendation {
        action: Action::UnionIntoBase,
        confidence: Confidence::High,
        rationale: "incoming 与 orig 逐字节相同，无需改写，直接删 underlay".into(),
    }
}

fn non_jsonl_diff_rec() -> Recommendation {
    Recommendation {
        action: Action::KeepSeparate,
        confidence: Confidence::Low,
        rationale: "非 .jsonl 且与 orig base 不同，不做行合并；留待人工/Task 8 处理".into(),
    }
}

/// merged_lines → 字节：以 `\n` 连接并补尾 `\n`（jsonl 行语义），使删除许可的行超集比对含尾
/// 空行 token 也自洽（incoming 尾 `\n` split 出的空串在 merged 中同样出现）。空则空字节。
pub(crate) fn lines_to_bytes(lines: &[String]) -> Vec<u8> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut s = lines.join("\n");
    s.push('\n');
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::orchestrator::testsupport::*;

    #[test]
    fn plan_entries_downgrades_oversize_to_keep_both() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        // 超限条目：bytes 留空（快照未整体读入），size 记超限值。
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![EntrySnapshot {
                rel: "huge.jsonl".into(),
                bytes: Vec::new(),
                mtime: SystemTime::UNIX_EPOCH,
                size: MAX_MERGE_FILE_BYTES + 1,
                ino: 1,
            }],
        };
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::KeepBoth, "超限应降级 KeepBoth");
    }

    #[test]
    fn plan_entries_routes_subagents_to_union_matching_apply() {
        // 报告准确性：subagents 条目即便 orig 无对应文件（朴素分类会判 New），plan_entries 必须
        // 与 apply_entry 同序优先路由到 Union（无损并集），否则 dry-run 报告与实际 apply 不符。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "sess-uuid/subagents/agent.jsonl";
        let body = b"{\"type\":\"assistant\",\"uuid\":\"sa1\",\"parentUuid\":null}\n";
        let snap_e = snap_entry_of(&mp, rel, body);
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };

        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(
            plans[0].1,
            EntryPlan::Union,
            "subagents 应判 Union 而非 New"
        );
        assert!(
            plans[0].2.rationale.contains("subagents"),
            "rationale 应说明 subagents 并集：{}",
            plans[0].2.rationale
        );

        // plan↔apply 一致性：apply_entry 对同条目实际路由到 subagents 并集。
        let report = apply_entry(&paths, "demo", &snap.entries[0], &plans[0].1, &mp, "0").unwrap();
        assert!(
            report.decision.contains("subagents"),
            "apply 实际应走 subagents，plan 须匹配：{}",
            report.decision
        );
    }

    #[test]
    fn plan_entries_routes_memory_passthrough_matching_apply() {
        // 报告准确性：backing/memory 是外链 symlink 时，memory/* 条目必须判 Passthrough 而非
        // New/KeepSeparate——apply_entry 会走透传恢复（写 canonical target，绝不落 orig）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // backing/memory = 指向树外 target 的 symlink（apply 期照 Claude 外链重建）。
        let target = tmp.path().join("external-memory");
        std::fs::create_dir_all(&target).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();

        // 非 jsonl memory 文件：朴素分类（orig 无）会判 New。
        let rel = "memory/foo.md";
        let snap_e = snap_entry_of(&mp, rel, b"body\n");
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };

        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(
            plans[0].1,
            EntryPlan::Passthrough,
            "memory 外链条目应判 Passthrough 而非 New/KeepSeparate"
        );
        assert!(
            matches!(plans[0].2.action, Action::PassthroughRestore),
            "透传建议 action 应为 PassthroughRestore：{:?}",
            plans[0].2.action
        );
        assert!(
            plans[0].2.rationale.contains("透传") || plans[0].2.rationale.contains("memory"),
            "rationale 应说明 memory 透传：{}",
            plans[0].2.rationale
        );

        // plan↔apply 一致性：apply_entry 对同条目实际路由到透传恢复。
        let report = apply_entry(&paths, "demo", &snap.entries[0], &plans[0].1, &mp, "0").unwrap();
        assert_eq!(
            report.decision, "passthrough",
            "apply 实际应走透传，plan 须匹配"
        );
    }

}
