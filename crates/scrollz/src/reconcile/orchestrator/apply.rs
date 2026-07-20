//! 逐条目落盘（stash 前镜像 → 合并 → 原子写 orig → reingest → 删除门）。
use std::io;

use super::*;

/// 逐条目落盘（**严格顺序**，评审 I-3/C-a）：
/// 1. **先 stash orig 前镜像**（改 orig 前留底，可回滚）。
/// 2. 计算合并明文（Union：`session_merge` 并集；New：incoming bytes）。
/// 3. `atomic_write(orig/<rel>)`（全有或全无）。
/// 4. `reingest_one_file`（原子替换 backing archive）。
/// 5. `delete_permitted` 通过**才**删 underlay 条目；不过则中止该条目、underlay 保留、notes 记原因。
///
/// **优先路由**（先于 plan 匹配）：subagents 目录条目 → `reconcile_subagents_dir`（强制并集）；
/// memory 透传（backing 顶层段是 symlink）→ `passthrough_restore_memory`（例外规则）。
/// Identical 无需改 orig/backing，直接过 `delete_permitted`（ByteEqual）删 underlay。
/// KeepSeparate → `quarantine_reuse` 隔离（base 不动，ByteEqual 删除门）。仅 KeepBoth 仍 deferred。
///
/// `ts` 是**贯穿整个 reconcile run 的单一时间戳**（Task7 Minor2）：orig 前镜像 stash、quarantine、
/// passthrough stash 全用同一 `ts`，令一次 reconcile 的所有落盘落同一代次目录（而非每条目各自
/// `now_unix_secs`）。由 `reconcile` 从 `UnderlaySnapshot::ts` 传入。
pub fn apply_entry(
    paths: &Paths,
    name: &str,
    snap_entry: &EntrySnapshot,
    plan: &EntryPlan,
    mp: &Path,
    ts: &str,
) -> io::Result<EntryReport> {
    validate_name(name)?;
    let rel = snap_entry.rel.clone();
    let orig_file = paths.orig(name).join(&rel);
    let mut notes: Vec<String> = Vec::new();

    // 优先路由：subagents 子会话一律无损并集，绕过 plan（防 SuspectReuse 误隔离子会话）。
    if is_subagents_entry(&rel) {
        return reconcile_subagents_dir(paths, name, snap_entry, mp, ts);
    }

    // 优先路由：memory 透传。backing 顶层段是 symlink → 该条目属外链 memory 的物化回落写。
    // （plan_entries 现也产 Passthrough plan；据 backing symlink 判定，两条路由等价、互为兜底。）
    if matches!(plan, EntryPlan::Passthrough) || is_passthrough_entry(paths, name, &rel) {
        if let Some((top, target)) = passthrough_top_symlink(paths, name, &rel)? {
            let underlay_dir = mp.join(&top);
            let stash_dir = paths.quarantine(name, ts).join(&top);
            let notes = passthrough_restore_memory(&underlay_dir, &target, &stash_dir)?;
            // 据结果如实报 action（评审 M4）：路径安全闸未过时 underlay 未动，不能谎报 restored。
            let action = passthrough_action(&notes);
            // 实际 relocate（memory-restored）→ ReportMemory（undo 只报告待人工 git 回退）；透传 noop 或
            // 路径安全闸拦截（underlay 未动）→ Noop。
            let reversal = if action == "memory-restored" {
                ReversalClass::ReportMemory
            } else {
                ReversalClass::Noop
            };
            return Ok(EntryReport {
                name: rel,
                decision: "passthrough".into(),
                action: action.into(),
                notes,
                reversal,
            });
        }
    }

    match plan {
        EntryPlan::Union => {
            let base_bytes = std::fs::read(&orig_file)?;
            let base_str = String::from_utf8_lossy(&base_bytes);
            let inc_str = String::from_utf8_lossy(&snap_entry.bytes);
            let merged = session_merge(base_str.as_ref(), inc_str.as_ref());
            let merged_bytes = lines_to_bytes(&merged.merged_lines);
            // 评审 R-C1（双向超集铁律 base 半边）：incoming ⊆ merged 由 finish_delete 删除门把关；
            // base ⊆ merged 在此 fail-fast 校验——merged 若丢了 base 任一记录（疑合并核缺陷），
            // **绝不覆盖金源 orig、绝不删 underlay**，保两份待人工，杜绝静默失真。
            if !crate::reconcile::merge::base_covered_by_merged(
                &base_str,
                &String::from_utf8_lossy(&merged_bytes),
            ) {
                notes.push(
                    "合并结果未覆盖 base 全部记录（疑合并核缺陷）→ 中止：不改 orig、不删 underlay，保两份".into(),
                );
                return Ok(EntryReport {
                    name: rel,
                    decision: "union".into(),
                    action: "aborted-base-not-covered".into(),
                    notes,
                    reversal: ReversalClass::Noop,
                });
            }
            let has_preimage = stash_orig_preimage(paths, name, &rel, ts, &mut notes)?;
            atomic_write(&orig_file, &merged_bytes)?;
            reingest_one_file(paths, name, &rel)?;
            finish_delete(
                snap_entry,
                &orig_file,
                SupersetMode::LinesSuperset,
                mp,
                "union",
                reversal_for_preimage(has_preimage),
                notes,
            )
        }
        EntryPlan::New => {
            let has_preimage = stash_orig_preimage(paths, name, &rel, ts, &mut notes)?;
            if let Some(parent) = orig_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            atomic_write(&orig_file, &snap_entry.bytes)?;
            reingest_one_file(paths, name, &rel)?;
            finish_delete(
                snap_entry,
                &orig_file,
                SupersetMode::LinesSuperset,
                mp,
                "new",
                reversal_for_preimage(has_preimage),
                notes,
            )
        }
        EntryPlan::Identical => {
            // Minor1：Identical 前提是 incoming 已在 backing。但「orig 有、backing 缺」时直接删
            // underlay 会致挂载视图缺该文件（backing 才是被 serve 的一侧）。缺则先从 orig 重灌补齐
            // backing，再走删除门；orig 不动（已与 incoming 逐字节相同）。
            let backing_file = paths.backing(name, Backend::Shadow).join(&rel);
            if !backing_file.exists() {
                notes.push(format!(
                    "backing/{rel} 缺失（orig 有 backing 缺）→ 降级 reingest 补齐后再删"
                ));
                reingest_one_file(paths, name, &rel)?;
            }
            // orig 未改、backing 已有 incoming：undo 无需反做（underlay 快照全局还原即可）。
            finish_delete(
                snap_entry,
                &orig_file,
                SupersetMode::ByteEqual,
                mp,
                "identical",
                ReversalClass::Noop,
                notes,
            )
        }
        EntryPlan::KeepSeparate => {
            // 疑 reuse：隔离 underlay 那份到 quarantine（移出树、保 UUID），base 不动，ByteEqual 删除门。
            let q = quarantine_reuse(paths, name, ts, snap_entry, mp)?;
            notes.push(format!("quarantine={}", q.display()));
            finish_delete(
                snap_entry,
                &q,
                SupersetMode::ByteEqual,
                mp,
                "keep-separate",
                ReversalClass::RemoveQuarantine,
                notes,
            )
        }
        other => {
            notes.push(format!(
                "{other:?} 未在本任务落盘（KeepBoth 待人工/后续），underlay 保留待处理"
            ));
            Ok(EntryReport {
                name: rel,
                decision: format!("{other:?}"),
                action: "deferred".into(),
                notes,
                reversal: ReversalClass::Noop,
            })
        }
    }
}

/// 把当前 `orig/<rel>` 拷进 `reconcile_stash(name,ts)/orig/<rel>` 并 fsync（评审 I-3，改 orig 前留底）。
/// **返回是否真拷了前镜像**（= orig 预存）：orig 不存在（New 条目）→ 无前镜像可 stash，记 note 返回
/// `false`；实际拷贝 → 返回 `true`。该布尔是 union/subagents 伞下精确区分 merge（`RestoreOrig`）与
/// new（`RemoveOrig`）的判别子（防 undo 孤儿）。stash 路径记入 `notes`（回滚定位）。
///
/// `ts` 是**贯穿整个 reconcile run 的单一时间戳**（= `UnderlaySnapshot::ts`，Task7 Minor2）：一次
/// reconcile 内所有条目的前镜像与快照落同一 `reconcile_stash(name,ts)` 代次，便于审计/回滚定位，
/// 不再每条目各自 `now_unix_secs`（会散落到多个代次目录）。
pub(crate) fn stash_orig_preimage(
    paths: &Paths,
    name: &str,
    rel: &str,
    ts: &str,
    notes: &mut Vec<String>,
) -> io::Result<bool> {
    let orig_file = paths.orig(name).join(rel);
    if !orig_file.exists() {
        notes.push(format!("orig/{rel} 不存在，无前镜像可 stash（New 条目）"));
        return Ok(false);
    }
    let stash_root = paths.reconcile_stash(name, ts);
    let dst = stash_root.join("orig").join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&orig_file, &dst)?;
    fsync_path(&dst)?;
    if let Some(parent) = dst.parent() {
        fsync_dir_chain(parent, &stash_root)?;
    }
    notes.push(format!("stash-preimage={}", dst.display()));
    Ok(true)
}

/// 落盘尾闸：`delete_permitted` 通过才删 underlay 条目（唯一删除入口），否则保留并记原因。
/// 删除后 fsync 父目录持久化 dirent。返回带 action 的 `EntryReport`。
pub(crate) fn finish_delete(
    snap_entry: &EntrySnapshot,
    receiver: &Path,
    mode: SupersetMode,
    mp: &Path,
    kind: &str,
    reversal: ReversalClass,
    mut notes: Vec<String>,
) -> io::Result<EntryReport> {
    let rel = snap_entry.rel.clone();
    let action = if delete_permitted(receiver, snap_entry, mode, mp)? {
        let live = mp.join(&rel);
        match std::fs::remove_file(&live) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(parent) = live.parent() {
            let _ = fsync_dir(parent);
        }
        format!("{kind}-applied+underlay-removed")
    } else {
        notes.push(
            "delete_permitted 未通过（接收方非超集 或 live underlay 自快照已变）：underlay 保留"
                .into(),
        );
        format!("{kind}-applied+underlay-kept")
    };
    Ok(EntryReport {
        name: rel,
        decision: kind.into(),
        action,
        notes,
        reversal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::orchestrator::testsupport::*;

    #[test]
    fn apply_union_log_only_merges_orig_reingests_and_removes_underlay() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "s.jsonl";
        let orig_file = write_orig(&paths, "demo", rel, BASE_LOG.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, INCOMING_LOG.as_bytes());
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };

        // incoming 只有日志记录（无 uuid）→ LogOnly → Union。
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::Union);

        let report = apply_entry(
            &paths,
            "demo",
            &snap.entries[0],
            &EntryPlan::Union,
            &mp,
            "0",
        )
        .unwrap();

        // orig 现含合并结果：base(u1/old) 全留 + incoming(new/mode) 并入。
        let merged = std::fs::read_to_string(&orig_file).unwrap();
        for needle in ["u1", "old", "new", "\"mode\""] {
            assert!(
                merged.contains(needle),
                "orig 合并结果应含 {needle}：{merged}"
            );
        }
        // underlay 条目已删（delete_permitted 通过）。
        assert!(
            !mp.join(rel).exists(),
            "delete_permitted 通过 → underlay 应删"
        );
        // backing 该文件已原子重灌为合并结果（read-back 逐字节等于 orig）。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), merged.as_bytes());
        assert!(
            report.action.contains("underlay-removed"),
            "report.action={}",
            report.action
        );
    }

    #[test]
    fn apply_union_stashes_orig_preimage_before_mutating_and_is_rollbackable() {
        // 评审 I-3：改 orig 前 stash 里必须已有旧版，中途放弃可从 stash 回滚 orig。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "s.jsonl";
        let orig_file = write_orig(&paths, "demo", rel, BASE_LOG.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, INCOMING_LOG.as_bytes());

        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::Union, &mp, "0").unwrap();

        // orig 已被合并改写（≠ base）。
        assert_ne!(std::fs::read(&orig_file).unwrap(), BASE_LOG.as_bytes());
        // stash 前镜像 = 改 orig 前的 base，可回滚。
        let stashed = report
            .notes
            .iter()
            .find_map(|n| n.strip_prefix("stash-preimage="))
            .expect("应记录 stash-preimage 路径");
        let stashed = PathBuf::from(stashed);
        assert!(stashed.exists(), "stash 前镜像文件应存在");
        assert_eq!(
            std::fs::read(&stashed).unwrap(),
            BASE_LOG.as_bytes(),
            "stash 应是改 orig 前的镜像"
        );
        // 从 stash 回滚 orig → 复原 base。
        std::fs::copy(&stashed, &orig_file).unwrap();
        assert_eq!(std::fs::read_to_string(&orig_file).unwrap(), BASE_LOG);
    }

    #[test]
    fn apply_union_keeps_underlay_when_live_changed_after_snapshot() {
        // 评审 C-a：接收方即便超集，若 live underlay 自快照后被追加 → delete_permitted 不过、underlay 保留。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "s.jsonl";
        write_orig(&paths, "demo", rel, BASE_LOG.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, INCOMING_LOG.as_bytes());

        // 快照后 Claude 追加 live → size/mtime 变 → live_entry_unchanged 为假。
        let live = mp.join(rel);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .unwrap();
        f.write_all(b"{\"type\":\"extra\"}\n").unwrap();
        f.sync_all().unwrap();
        drop(f);

        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::Union, &mp, "0").unwrap();
        assert!(
            mp.join(rel).exists(),
            "live 已变 → underlay 必须保留（防丢尾）"
        );
        assert!(
            report.action.contains("underlay-kept"),
            "report.action={}",
            report.action
        );
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("delete_permitted 未通过")),
            "notes 应记未通过原因：{:?}",
            report.notes
        );
    }

    #[test]
    fn apply_new_entry_writes_orig_backing_and_removes_underlay() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "fresh.jsonl";
        let incoming = b"{\"type\":\"summary\",\"summary\":\"s\"}\n";
        let snap_e = snap_entry_of(&mp, rel, incoming);
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };

        // orig 无此条目 → New。
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::New);

        let report =
            apply_entry(&paths, "demo", &snap.entries[0], &EntryPlan::New, &mp, "0").unwrap();

        let orig_file = paths.orig("demo").join(rel);
        assert_eq!(std::fs::read(&orig_file).unwrap(), incoming);
        assert!(!mp.join(rel).exists(), "New 落盘后 underlay 应删");
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), incoming);
        assert!(report.action.contains("underlay-removed"));
    }

    #[test]
    fn apply_identical_removes_underlay_without_touching_orig_or_backing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "same.jsonl";
        let content = b"{\"type\":\"x\",\"uuid\":\"z\"}\n";
        let orig_file = write_orig(&paths, "demo", rel, content);
        // 预灌 backing（Identical 前提：incoming 已在 backing）。
        reingest_one_file(&paths, "demo", rel).unwrap();
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        let backing_before = read_archive(&backing_file);

        let snap_e = snap_entry_of(&mp, rel, content);
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::Identical);

        let report = apply_entry(
            &paths,
            "demo",
            &snap.entries[0],
            &EntryPlan::Identical,
            &mp,
            "0",
        )
        .unwrap();
        assert!(!mp.join(rel).exists(), "Identical 应直接删 underlay");
        // orig / backing 均未改。
        assert_eq!(std::fs::read(&orig_file).unwrap(), content);
        assert_eq!(read_archive(&backing_file), backing_before);
        assert!(report.action.contains("underlay-removed"));
    }

    #[test]
    fn apply_identical_missing_backing_downgrades_to_reingest() {
        // Minor1（Task7 遗留）：orig 有、backing 缺时，Identical 直接删 underlay 会致挂载视图缺
        // 该文件。降级：先 reingest 从 orig 补齐 backing，再走删除门。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "same.jsonl";
        let content = b"{\"type\":\"x\",\"uuid\":\"z\"}\n";
        let orig_file = write_orig(&paths, "demo", rel, content);
        // 关键前提：orig 有、backing 无（不预灌）。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert!(!backing_file.exists(), "前提：backing 缺失");

        let snap_e = snap_entry_of(&mp, rel, content);
        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::Identical, &mp, "0").unwrap();

        // 降级 reingest：backing 被补齐为 orig 内容。
        assert_eq!(
            read_archive(&backing_file),
            content,
            "backing 应补齐为 orig 内容"
        );
        assert!(!mp.join(rel).exists(), "补齐 backing 后应删 underlay");
        assert!(report.action.contains("underlay-removed"));
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("backing") && n.contains("reingest")),
            "notes 应记录降级 reingest：{:?}",
            report.notes
        );
        assert_eq!(std::fs::read(&orig_file).unwrap(), content, "orig 不变");
    }

    #[test]
    fn apply_entry_routes_subagents_to_union_not_quarantine() {
        // 即便 plan 判 KeepSeparate（disjoint uuid），subagents 路径必须优先路由到并集而非隔离。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let rel = "s/subagents/a.jsonl";
        let base = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"x1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
        );
        let incoming = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"y1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
        );
        write_orig(&paths, "demo", rel, base.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, incoming.as_bytes());

        // 传 KeepSeparate plan，但路由据 subagents 路径改走并集。
        let report =
            apply_entry(&paths, "demo", &snap_e, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert!(
            report.decision.contains("subagents"),
            "应优先走 subagents 并集：{}",
            report.decision
        );
        let orig_file = paths.orig("demo").join(rel);
        let merged = std::fs::read_to_string(&orig_file).unwrap();
        assert!(
            merged.contains("x1") && merged.contains("y1"),
            "两侧 uuid 并集：{merged}"
        );
        // 未落隔离区（quarantine 未记录）。
        assert!(
            !report.notes.iter().any(|n| n.starts_with("quarantine=")),
            "subagents 不应走隔离"
        );
    }

    #[test]
    fn subagents_new_entry_falls_to_new_when_orig_missing() {
        // orig 无对应 subagents 文件 → New 落盘（不崩），reingest + 删 underlay。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let rel = "u/subagents/fresh.jsonl";
        let incoming = b"{\"type\":\"summary\",\"summary\":\"s\"}\n";
        let snap_e = snap_entry_of(&mp, rel, incoming);
        let report = reconcile_subagents_dir(&paths, "demo", &snap_e, &mp, "0").unwrap();
        let orig_file = paths.orig("demo").join(rel);
        assert_eq!(std::fs::read(&orig_file).unwrap(), incoming);
        assert!(!mp.join(rel).exists());
        assert!(report.action.contains("underlay-removed"));
    }

    #[test]
    fn keep_separate_quarantines_reuse_preserving_uuid_and_leaves_base() {
        // SuspectReuse → KeepSeparate：隔离副本保原 <uuid>.jsonl 名、移出 projects 树；base 不动；
        // underlay 经 ByteEqual 超集校验后删。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // Claude 会话文件名即 <uuid>.jsonl。
        let rel = "3f2a-b1c2-uuid.jsonl";
        let base = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
        );
        let incoming = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"b1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
        );
        let orig_file = write_orig(&paths, "demo", rel, base.as_bytes());
        reingest_one_file(&paths, "demo", rel).unwrap();
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        let backing_before = read_archive(&backing_file);

        let snap_e = snap_entry_of(&mp, rel, incoming.as_bytes());
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };
        // disjoint uuid、无桥、时间窗不交 → SuspectReuse → KeepSeparate。
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::KeepSeparate);

        let report = apply_entry(
            &paths,
            "demo",
            &snap.entries[0],
            &EntryPlan::KeepSeparate,
            &mp,
            "0",
        )
        .unwrap();

        // 隔离副本：quarantine 下出现原 <uuid>.jsonl，内容 == underlay incoming。
        let q = report
            .notes
            .iter()
            .find_map(|n| n.strip_prefix("quarantine="))
            .map(PathBuf::from)
            .expect("应记 quarantine 路径");
        assert_eq!(
            q.file_name().unwrap().to_str().unwrap(),
            rel,
            "保原 UUID 文件名"
        );
        assert_eq!(std::fs::read(&q).unwrap(), incoming.as_bytes());
        // 隔离区在 projects 树外（scrollz_home 下），不在 projects_root。
        assert!(
            q.starts_with(&paths.scrollz_home),
            "quarantine 应在 scrollz_home 下"
        );
        assert!(
            !q.starts_with(&paths.projects_root),
            "quarantine 应移出 projects 树"
        );
        // base（orig/backing）绝不改动。
        assert_eq!(
            std::fs::read(&orig_file).unwrap(),
            base.as_bytes(),
            "orig base 不变"
        );
        assert_eq!(
            read_archive(&backing_file),
            backing_before,
            "backing base 不变"
        );
        // underlay 经 ByteEqual 校验后删。
        assert!(!mp.join(rel).exists(), "隔离且校验后应删 underlay");
        assert!(report.action.contains("underlay-removed"));
    }

    #[test]
    fn apply_keep_both_still_deferred_keeps_underlay() {
        // KeepBoth 仍 deferred：underlay 保留、报告标 deferred（与已实现的 KeepSeparate 区分）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let rel = "kb.jsonl";
        let snap_e = snap_entry_of(&mp, rel, b"{\"a\":1}\n");
        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::KeepBoth, &mp, "0").unwrap();
        assert_eq!(report.action, "deferred");
        assert!(mp.join(rel).exists(), "deferred 计划不得删 underlay");
    }

    #[test]
    fn apply_entry_routes_memory_passthrough_via_backing_symlink() {
        // backing/memory 是 symlink → apply_entry 应据此路由到透传恢复。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // 外部 memory 目标（无 `..`，存在可写）。
        let target = tmp.path().join("external-memory");
        std::fs::create_dir_all(&target).unwrap();
        // backing/memory = 指向 target 的 symlink（apply 期照 Claude 外链重建）。
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();

        // underlay：memory 被物化，Claude 写了新文件。
        let rel = "memory/NOTES.md";
        let snap_e = snap_entry_of(&mp, rel, b"note-body\n");

        // 传 KeepSeparate（模拟 plan_entries 对非 jsonl 的保守判定），路由应改走透传。
        let report =
            apply_entry(&paths, "demo", &snap_e, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert_eq!(report.decision, "passthrough", "应路由到透传");
        // 文件送进 target。
        assert_eq!(
            std::fs::read(target.join("NOTES.md")).unwrap(),
            b"note-body\n"
        );
        // underlay memory 侧无残留（**不复原 symlink**；挂载由 backing/memory 服务）。
        assert!(
            mp.join("memory").symlink_metadata().is_err(),
            "underlay memory 应无残留（无目录、无复原 symlink）"
        );
    }

    #[test]
    fn apply_entry_second_memory_entry_is_idempotent_noop() {
        // 同一 reconcile 内多条 memory/* 条目：首条复原 symlink 后，次条应幂等跳过（不再 relocate）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let target = tmp.path().join("external-memory");
        std::fs::create_dir_all(&target).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();

        // underlay 物化两个文件。
        let e1 = snap_entry_of(&mp, "memory/A.md", b"aaa\n");
        let e2 = snap_entry_of(&mp, "memory/B.md", b"bbb\n");

        let r1 = apply_entry(&paths, "demo", &e1, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert_eq!(r1.action, "memory-restored");
        // 首条已把整目录 relocate 并复原 symlink；A、B 都进了 target。
        assert_eq!(std::fs::read(target.join("A.md")).unwrap(), b"aaa\n");
        assert_eq!(std::fs::read(target.join("B.md")).unwrap(), b"bbb\n");

        let r2 = apply_entry(&paths, "demo", &e2, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert_eq!(r2.action, "memory-noop", "次条应如实报 noop");
        assert!(
            r2.notes.iter().any(|n| n.contains("不存在")),
            "次条应因 underlay memory 已整目录 relocate 移除而 noop：{:?}",
            r2.notes
        );
        // memory 侧无残留（首条已 relocate 整目录、未复原 symlink，次条不再触碰）。
        assert!(
            mp.join("memory").symlink_metadata().is_err(),
            "underlay memory 应无残留"
        );
    }

    #[test]
    fn apply_entry_memory_deferred_action_when_target_dangling() {
        // 评审 M4：路径安全闸拦截（悬空目标）时 underlay 未动，action 必须如实为 memory-deferred。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        // backing/memory 指向不存在的目标（悬空）。
        std::os::unix::fs::symlink(tmp.path().join("gone-mem"), backing.join("memory")).unwrap();
        let e = snap_entry_of(&mp, "memory/N.md", b"n\n");

        let report = apply_entry(&paths, "demo", &e, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert_eq!(report.decision, "passthrough");
        assert_eq!(report.action, "memory-deferred", "悬空 → 不能谎报 restored");
        // underlay 文件保留（未动）。
        assert!(mp.join("memory/N.md").exists(), "悬空目标 → underlay 保留");
    }

}
