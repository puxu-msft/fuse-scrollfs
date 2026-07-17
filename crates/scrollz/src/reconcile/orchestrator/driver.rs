//! 顶层 reconcile 主循环（把各 handler 串成端到端 driver）+ meta 字节收尾。
use std::io;

use super::*;

/// 顶层 reconcile 主循环（把 Task 1-8 的 handler 串成端到端 driver）：
///
/// 1. 读 meta 取 backend（无 meta / 非 shadow → 拒）；`check_preconditions` 取串行锁 + underlay 快照。
/// 2. `dry_run` → 只 `plan_entries` 出建议单、构建报告返回，**零改动**（不 set_reconciling、不 apply）。
/// 3. 否则 `set_reconciling(true)` → 对每条目 `plan_entries` 给 (rel,plan,rec) → `confirm` 裁决：
///    `Accept`→`apply_entry`；`KeepBoth`→按 KeepBoth handler（不删 base、underlay 保留）；`Skip`→跳过。
/// 4. 逐条处置后：underlay 已清空且非 rebuild → meta 字节数收尾（重扫 backing/orig，committed 不变）；
///    随后 `set_reconciling(false)` 关闭半改写窗口。
/// 5. `rebuild` → 委托 `lifecycle::reingest`（从 orig 全量重建 backing、重挂、自写 meta）。
///
/// **run ts 单一**（Task7 Minor2）：`snapshot.ts` 贯穿整个 run，所有 stash（快照 + 各条目 orig 前
/// 镜像 + quarantine）落同一 `reconcile_stash(name,ts)` 代次。
///
/// **零丢失**：dry_run 绝不动盘；`apply_entry` 的唯一删除门（durable 超集 + live 未变）逐条把关；
/// 崩溃续跑幂等（reconciling 标记在→重跑安全，合并是并集不动点、已删条目不在新快照里故不复现）。
///
/// **rebuild 崩溃恢复**：若在清标记后、`reingest` 中途崩溃，此时 underlay 已清空 → 再跑 reconcile
/// 会被前置门禁拒（无 fall-through）；但 orig 是已 fsync 的权威源、`reingest` 会回滚 backing 并留
/// `.reingest-bak`，故无数据丢失，恢复走 `enable remount` / 手动 `enable reingest` 而非重跑 reconcile。
pub fn reconcile(
    paths: &Paths,
    name: &str,
    opts: ReconcileOptions,
    mounter: &dyn Mounter,
) -> io::Result<ReconcileReport> {
    validate_name(name)?;

    // backend 从 meta 读（无 meta 拒——未 apply）。非 shadow 由 check_preconditions 拒。
    let meta = discovery::read_meta(&paths.meta_path(name))?.ok_or_else(|| {
        io::Error::other(format!(
            "{name} 无提交标记 meta，无法 reconcile（未 apply？）"
        ))
    })?;
    let backend = meta.backend;

    // 1. 门禁 + 快照（取串行锁；backend 非 shadow 在此拒）。锁随 `pre` 存活到函数末。
    let pre = check_preconditions(paths, name, backend, opts.force)?;
    let ts = pre.snapshot.ts.clone();
    let stash_dir = paths.reconcile_stash(name, &ts);
    let mp = paths.mountpoint(name);

    // 2. dry_run：只 plan、零改动。
    if opts.dry_run {
        let plans = plan_entries(paths, name, &pre.snapshot)?;
        let entries = plans
            .into_iter()
            .map(|(rel, plan, rec)| EntryReport {
                name: rel,
                decision: format!("{plan:?}"),
                action: "dry-run".into(),
                notes: vec![rec.rationale],
                reversal: ReversalClass::Noop,
            })
            .collect();
        return Ok(ReconcileReport { entries, stash_dir });
    }

    // 3. 进行中标记（半改写 orig 窗口开）→ 逐条裁决落盘。
    set_reconciling(paths, name, true)?;
    let plans = plan_entries(paths, name, &pre.snapshot)?;
    let mut entries = Vec::with_capacity(plans.len());
    for (rel, plan, rec) in plans {
        // plan 的 rel 恒来自快照；找回对应 EntrySnapshot 供 apply（快照是合并/删除唯一基准）。
        let Some(snap_entry) = pre.snapshot.entries.iter().find(|e| e.rel == rel) else {
            // 理论不可达（plan 源自快照）；防御地记一条审计条目而非静默跳过，绝不动盘。
            entries.push(EntryReport {
                name: rel,
                decision: "skip".into(),
                action: "unmatched-snapshot".into(),
                notes: vec!["plan 条目在快照中无对应项（不可达），防御跳过、underlay 不动".into()],
                reversal: ReversalClass::Noop,
            });
            continue;
        };
        let report = match (opts.confirm)(&rel, &rec) {
            Confirm::Accept => apply_entry(paths, name, snap_entry, &plan, &mp, &ts)?,
            // KeepBoth：按现有 KeepBoth handler（不删 base、underlay 保留；快照 stash 已存副本）。
            Confirm::KeepBoth => {
                apply_entry(paths, name, snap_entry, &EntryPlan::KeepBoth, &mp, &ts)?
            }
            Confirm::Skip => EntryReport {
                name: rel,
                decision: "skip".into(),
                action: "skipped+underlay-kept".into(),
                notes: vec!["用户跳过此条：underlay 原样保留、orig 不动".into()],
                reversal: ReversalClass::Noop,
            },
        };
        entries.push(report);
    }

    // 逐条 apply 后剪除已抽干的空 underlay 子目录（评审 final BREACH 1）：finish_delete 只删文件不
    // rmdir，`<uuid>/subagents/*.jsonl` 抽干后空 `<uuid>/` 仍是顶层非白名单条目，令下面的 drained 复扫
    // 永假、`ensure_underlay_empty` 永久拒挂。自底向上、只删「仅含白名单/全空」目录，仍存 Skip/KeepBoth/
    // 未删条目的目录保留。best-effort：剪枝报错绝不阻断收尾（非数据安全，数据已抽干），与下面 meta
    // finalize 同为「失败仅记 warn、不 wedge」——否则清标记被跳过、reconciling 标记永久卡住把维护拦死。
    if let Err(e) = prune_empty_underlay_dirs(&mp) {
        entries.push(EntryReport {
            name: format!("<prune {name}>"),
            decision: "prune-empty-dirs".into(),
            action: "warn".into(),
            notes: vec![format!(
                "剪除空 underlay 子目录失败（仅影响重挂门禁，非数据安全）：{e}"
            )],
            reversal: ReversalClass::Noop,
        });
    }

    // 并列清除与 backing 同名同目标的顶层冗余 underlay 软链（§6 memory-symlink 短路）：memory 软链
    // 在、写已透传 canonical → walk_snapshot 跳过 symlink 不处理，却令 fall-through 永真拒挂。删这类
    // 冗余软链（backing 有同款、挂载时透传）解锁重挂；目标不一致/异常项保留并报告，绝不误删。best-effort：
    // 与空目录剪枝同为「失败仅记 warn、不 wedge」（非数据安全，软链无内容）。
    match prune_redundant_symlinks(paths, name, &mp) {
        Ok(sym_notes) if !sym_notes.is_empty() => {
            entries.push(EntryReport {
                name: format!("<prune-symlinks {name}>"),
                decision: "prune-redundant-symlinks".into(),
                action: "kept-anomaly".into(),
                notes: sym_notes,
                reversal: ReversalClass::Noop,
            });
        }
        Ok(_) => {}
        Err(e) => {
            entries.push(EntryReport {
                name: format!("<prune-symlinks {name}>"),
                decision: "prune-redundant-symlinks".into(),
                action: "warn".into(),
                notes: vec![format!(
                    "清除冗余 underlay 软链失败（仅影响重挂门禁，非数据安全）：{e}"
                )],
                reversal: ReversalClass::Noop,
            });
        }
    }

    // 4. underlay 清空且非 rebuild → meta 字节数收尾（rebuild 由 reingest 自写 meta，不重复）。
    //    收尾是**纯 list 显示**（非数据安全），失败绝不能阻断下面的 set_reconciling(false)——否则
    //    underlay 已清空、下轮 reconcile 会在前置门禁因「无 fall-through」被拒（永远走不到清标记），
    //    reconciling 标记就永久卡住、把所有生命周期维护经 bail_if_reconciling 拦死。与 reingest 的
    //    「meta 写失败 warn-not-fail」一致：best-effort，失败只记 warn 条目。
    //    同理 underlay 复扫用 unwrap_or(true)（探测出错→保守视为未清空、跳过收尾），绝不因复扫报错
    //    而阻断清标记。
    let drained = !underlay_has_fallthrough(&mp).unwrap_or(true);
    if !opts.rebuild && drained {
        if let Err(e) = finalize_meta_bytes(paths, name, &meta) {
            entries.push(EntryReport {
                name: format!("<meta {name}>"),
                decision: "meta-finalize".into(),
                action: "warn".into(),
                notes: vec![format!(
                    "meta 字节数收尾失败（仅影响 list 显示，非数据安全）：{e}"
                )],
                reversal: ReversalClass::Noop,
            });
        }
    }
    // per-generation manifest（undo 依赖，§10.1）：在条目循环后、清标记前落盘（评审 M3），记每条真实
    // 条目的逆转类供 Task 4 `reconcile_undo` 消费。best-effort：写失败仅 warn（该 run 不可 undo，但绝不
    // 阻断清标记——否则 reconciling 标记永久卡住把维护拦死，与 meta finalize 同策）。合成条目由
    // write_manifest 内部过滤，不入 manifest。
    if let Err(e) = write_manifest(paths, name, &ts, &entries) {
        log::warn!("{name} reconcile manifest 落盘失败（该 run 不可 undo，非数据安全）：{e}");
    }
    // 关闭半改写窗口：逐条 apply 已各自原子完成，orig 处于一致态。崩溃续跑靠「标记在→重跑幂等」，
    // 故仅在正常收尾时清标记（中途崩溃则标记留存，让生命周期维护让路、下次 reconcile 续做）。
    set_reconciling(paths, name, false)?;

    // 5. rebuild：清标记后委托 reingest 从 orig 全量重建 backing + 重挂（committed 全程不变满足其前提）。
    // 评审 W2（已知窄窗口，未修）：此处必须先清标记——lifecycle::reingest 的重挂经 mounter.spawn →
    // ensure_mountable，marker 在则拒挂，故 reingest 的自我重挂要求标记已清。代价：重建+重挂窗口内
    // 标记已清 + underlay 已空，外部 systemd 自启理论上可挂到半重建 backing / 双挂载。彻底修复需
    // 「可信重挂」路径（区分 reconcile 自身重挂 vs 外部自启），属较大改动，留后续；无数据丢失（orig
    // 权威、单文件 reingest 原子）。
    if opts.rebuild {
        let msg = crate::enable::lifecycle::reingest(paths, name, opts.force, mounter)?;
        entries.push(EntryReport {
            name: format!("<rebuild {name}>"),
            decision: "rebuild".into(),
            action: "reingest-delegated".into(),
            notes: vec![msg],
            reversal: ReversalClass::Noop,
        });
    }

    Ok(ReconcileReport { entries, stash_dir })
}

/// meta 字节数收尾（**非数据安全，仅 list 显示**）：重扫 backing 求 `bytes_archive`、扫 orig 求
/// `bytes_src`，据原 meta 选项重写 committed meta（committed 保持 true，仅字节数/applied_at 更新）。
pub(crate) fn finalize_meta_bytes(paths: &Paths, name: &str, meta: &discovery::Meta) -> io::Result<()> {
    let bytes_src = dir_file_bytes(&paths.orig(name))?;
    let bytes_archive = dir_file_bytes(&paths.backing(name, Backend::Shadow))?;
    let new_meta = discovery::Meta::from_apply(
        &meta.options(),
        bytes_src,
        bytes_archive,
        discovery::now_unix(),
    );
    discovery::write_meta(&paths.meta_path(name), &new_meta)
}

/// 递归求目录下所有常规文件字节数之和（meta 字节收尾用）。目录不存在 → 0；symlink/特殊文件不计。
pub(crate) fn dir_file_bytes(dir: &Path) -> io::Result<u64> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut total = 0u64;
    for dent in rd {
        let dent = dent?;
        let ft = dent.file_type()?;
        if ft.is_dir() {
            total = total.saturating_add(dir_file_bytes(&dent.path())?);
        } else if ft.is_file() {
            total = total.saturating_add(dent.metadata()?.len());
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::orchestrator::testsupport::*;
    use crate::enable::daemon::fake::FakeMounter;

    #[test]
    fn reconcile_dry_run_reports_without_mutating() {
        // dry_run：只出建议单，orig/underlay/backing/marker 全不变。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        let backing_before = read_archive(&backing_file);
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let underlay_before = std::fs::read(mp.join(rel)).unwrap();
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: true,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        assert!(!report.entries.is_empty(), "dry_run 应出建议单");
        assert!(
            report.entries.iter().all(|e| e.action == "dry-run"),
            "dry_run 条目动作应标 dry-run：{:?}",
            report.entries
        );
        // 零改动。
        assert_eq!(
            std::fs::read(&orig_file).unwrap(),
            BASE_LOG.as_bytes(),
            "orig 不变"
        );
        assert_eq!(
            std::fs::read(mp.join(rel)).unwrap(),
            underlay_before,
            "underlay 不变"
        );
        assert_eq!(read_archive(&backing_file), backing_before, "backing 不变");
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "dry_run 不落 reconciling 标记"
        );
    }

    #[test]
    fn reconcile_full_flow_accept_drains_underlay_and_updates_meta() {
        // 全流程 Accept：门禁→set_reconciling(true)→逐条 apply→underlay 清空→meta 更新→清标记。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // underlay 清空（结束态可挂：ensure_underlay_empty 放行）。
        assert!(
            !underlay_has_fallthrough(&mp).unwrap(),
            "Accept 全流程后 underlay 应清空"
        );
        assert!(!mp.join(rel).exists());
        crate::reconcile::guard::ensure_underlay_empty(&mp).unwrap();
        // orig 合并 incoming。
        let merged = std::fs::read_to_string(&orig_file).unwrap();
        for needle in ["u1", "old", "new", "\"mode\""] {
            assert!(
                merged.contains(needle),
                "orig 应含合并结果 {needle}：{merged}"
            );
        }
        // backing 重灌为合并结果。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), merged.as_bytes());
        // reconciling 标记已清。
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾应清 reconciling 标记"
        );
        // meta 字节数收尾，committed 全程不变。
        let meta = discovery::read_meta(&paths.meta_path("demo"))
            .unwrap()
            .unwrap();
        assert!(meta.committed, "committed 全程不变");
        assert_eq!(
            meta.bytes_src,
            dir_file_bytes(&paths.orig("demo")).unwrap(),
            "bytes_src 应重扫 orig"
        );
        assert_eq!(
            meta.bytes_archive,
            dir_file_bytes(&paths.backing("demo", Backend::Shadow)).unwrap(),
            "bytes_archive 应重扫 backing"
        );
        assert!(meta.bytes_src > 0 && meta.bytes_archive > 0);
        assert!(
            report
                .entries
                .iter()
                .any(|e| e.action.contains("underlay-removed")),
            "应有条目报 underlay-removed：{:?}",
            report.entries
        );
    }

    #[test]
    fn reconcile_skip_keeps_that_underlay_entry_and_clears_marker() {
        // 中途 Skip 某条：该 underlay 保留、orig 不落该条；其余 Accept 落盘。收尾清 reconciling 标记。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        // 两条全新 fall-through（orig 无 → New）。
        write_underlay(
            &mp,
            "a.jsonl",
            b"{\"type\":\"summary\",\"summary\":\"a\"}\n",
        );
        write_underlay(
            &mp,
            "b.jsonl",
            b"{\"type\":\"summary\",\"summary\":\"b\"}\n",
        );
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: Box::new(|rel, _| {
                if rel == "a.jsonl" {
                    Confirm::Skip
                } else {
                    Confirm::Accept
                }
            }),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // Skip 的 a：underlay 保留、orig 未落。
        assert!(mp.join("a.jsonl").exists(), "Skip 的条目 underlay 应保留");
        assert!(
            !paths.orig("demo").join("a.jsonl").exists(),
            "Skip 的条目不应落 orig"
        );
        // Accept 的 b：underlay 删除、orig 落盘。
        assert!(
            !mp.join("b.jsonl").exists(),
            "Accept 的条目 underlay 应删除"
        );
        assert!(
            paths.orig("demo").join("b.jsonl").exists(),
            "Accept 的条目应落 orig"
        );
        // reconciling 标记已清（半改写窗口正常关闭）。
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾应清 reconciling 标记"
        );
        assert!(
            report.entries.iter().any(|e| e.decision == "skip"),
            "报告应含 skip 条目：{:?}",
            report.entries
        );
    }

    #[test]
    fn reconcile_crash_resume_is_idempotent() {
        // 崩溃续跑幂等：同一 incoming 重现在 underlay（上次崩溃在删 underlay 前）→ 重跑收敛，
        // orig 不放大（并集不动点）、underlay 再次清空、不重复删。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();

        let opts1 = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        reconcile(&paths, "demo", opts1, &m).unwrap();
        let merged1 = std::fs::read(&orig_file).unwrap();
        assert!(!mp.join(rel).exists(), "首轮后 underlay 清空");

        // 模拟崩溃续跑：同一 incoming 再次出现。
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let opts2 = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        reconcile(&paths, "demo", opts2, &m).unwrap();
        let merged2 = std::fs::read(&orig_file).unwrap();

        assert_eq!(merged1, merged2, "重跑不放大 orig（并集不动点）");
        assert!(!mp.join(rel).exists(), "重跑收敛：underlay 再次清空");
    }

    #[test]
    fn reconcile_rebuild_delegates_to_reingest_and_remounts() {
        // rebuild：逐条 apply 后清标记，委托 reingest 从 orig 全量重建 backing + 重挂；旧 backing 留底。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: true,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        assert!(
            !mp.join(rel).exists(),
            "rebuild 前逐条 apply 应先清 underlay"
        );
        // reingest 重挂 → FakeMounter 记挂载。
        assert!(m.is_mounted(&mp), "rebuild 委托 reingest 后应重挂");
        // reingest 特征：旧 backing 留底 .reingest-bak。
        let bak = {
            let mut s = paths.backing("demo", Backend::Shadow).into_os_string();
            s.push(".reingest-bak");
            PathBuf::from(s)
        };
        assert!(bak.is_dir(), "reingest 应留旧 backing 底本");
        // 报告含 rebuild 委托项，标记已清。
        assert!(
            report.entries.iter().any(|e| e.decision == "rebuild"),
            "报告应含 rebuild 委托项：{:?}",
            report.entries
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "rebuild 前须清 reconciling 标记"
        );
        // backing 读回合并结果（reingest 从 orig 重建）。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(
            read_archive(&backing_file),
            std::fs::read(paths.orig("demo").join(rel)).unwrap()
        );
    }

    #[test]
    fn reconcile_single_run_ts_all_stash_in_one_generation() {
        // Task7 Minor2：一次 reconcile 所有 stash（快照 underlay + 各条目 orig 前镜像）落同一 ts 代次。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        // 两条 Union（orig 有 base、underlay 有 incoming）。
        for rel in ["a.jsonl", "b.jsonl"] {
            write_orig(&paths, "demo", rel, BASE_LOG.as_bytes());
            reingest_one_file(&paths, "demo", rel).unwrap();
            write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        }
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // 只有一个 ts 代次目录。
        let gen_root = paths.scrollz_home.join("reconcile-stash").join("demo");
        let ts_dirs: Vec<PathBuf> = std::fs::read_dir(&gen_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(
            ts_dirs.len(),
            1,
            "一次 reconcile 所有 stash 应落同一 ts 代次：{ts_dirs:?}"
        );
        // report.stash_dir 即该唯一代次，且快照 + 两条前镜像全在其下。
        assert_eq!(report.stash_dir, ts_dirs[0]);
        assert!(report.stash_dir.join("underlay/a.jsonl").exists(), "a 快照");
        assert!(report.stash_dir.join("underlay/b.jsonl").exists(), "b 快照");
        assert!(
            report.stash_dir.join("orig/a.jsonl").exists(),
            "a orig 前镜像"
        );
        assert!(
            report.stash_dir.join("orig/b.jsonl").exists(),
            "b orig 前镜像"
        );
    }

    #[test]
    fn reconcile_rejects_without_committed_meta() {
        // 无 meta（未 apply）→ 拒绝。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        write_underlay(&mp, "s.jsonl", b"{}\n");
        let m = FakeMounter::default();
        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let e = reconcile(&paths, "demo", opts, &m).unwrap_err();
        assert!(e.to_string().contains("meta"), "无 meta 应拒绝：{e}");
    }

    #[test]
    fn reconcile_rejects_container_backend() {
        // meta 记 container 后端 → check_preconditions 拒（无 fall-through 语义）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        write_underlay(&mp, "s.jsonl", b"{}\n");
        std::fs::create_dir_all(paths.back_root()).unwrap();
        let meta = discovery::Meta::from_apply(
            &ApplyOptions {
                backend: Backend::Container,
                ..ApplyOptions::default()
            },
            0,
            0,
            0,
        );
        discovery::write_meta(&paths.meta_path("demo"), &meta).unwrap();
        let m = FakeMounter::default();
        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let e = reconcile(&paths, "demo", opts, &m).unwrap_err();
        assert!(e.to_string().contains("shadow"), "container 应拒绝：{e}");
    }

    #[test]
    fn reconcile_meta_finalize_failure_still_clears_marker_no_wedge() {
        // 评审 HIGH：meta 字节收尾（纯 list 显示）失败绝不能阻断 set_reconciling(false)——否则
        // underlay 已清空、下轮 reconcile 被前置门禁拒、标记永久卡住把维护全拦死。best-effort 验证。
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root 无视权限位，注入不成立 → 跳过。
        }
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        // 注入 finalize 失败：orig 下放一个不可读子目录 → dir_file_bytes 递归 read_dir 失败。
        // back_root 保持可写（set_reconciling(false) 仍能删标记，隔离出「收尾失败 ≠ 清标记失败」）。
        let blocked = paths.orig("demo").join("blocked-sub");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(blocked.join("x"), b"y").unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();
        // 恢复权限便于 tempdir 清理。
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        // underlay 已清空，但标记必须已清（不 wedge）。
        assert!(!mp.join(rel).exists(), "underlay 应已清空");
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾 meta 失败也须清 reconciling 标记（不 wedge）"
        );
        // 报告如实记 meta-finalize warn。
        assert!(
            report.entries.iter().any(|e| e.action == "warn"),
            "应记 meta 收尾 warn 条目：{:?}",
            report.entries
        );
    }

    #[test]
    fn reconcile_prunes_drained_subdirs_and_removes_memory_symlink_so_remount_unblocked() {
        // 整分支收尾评审两处集成缝 bug（BREACH 1 + BREACH 2）：reconcile 抽干 underlay 后必须让
        // `underlay_has_fallthrough` 归假（= `ensure_underlay_empty` 放行 → 重挂解锁），且零丢失。
        //   (a) 嵌套 `<uuid>/subagents/x.jsonl` 抽干后空目录 `<uuid>/subagents/`、`<uuid>/` 若不剪除，
        //       顶层 `<uuid>/` 令 fall-through 永真（BREACH 1）。
        //   (b) memory 分裂脑：backing/memory 是指向树外 target 的 symlink，underlay/memory 是含真实
        //       文件的目录。透传若在 underlay 复原 memory symlink，顶层 memory 条目令 fall-through 永真
        //       （BREACH 2）。underlay 侧必须无任何 memory 残留（挂载由 backing/memory 服务）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // (a) 嵌套子代理 fall-through 文件（orig 无 → New；subagents 路由强制并集）。
        let sub_rel = "sess-uuid/subagents/agent.jsonl";
        let sub_body = b"{\"type\":\"assistant\",\"uuid\":\"sa1\",\"parentUuid\":null}\n";
        write_underlay(&mp, sub_rel, sub_body);

        // (b) memory 分裂脑：树外 target + backing/memory symlink → target + underlay/memory 真实目录含文件。
        let target = tmp.path().join("external-memory"); // 移出 projects 树
        std::fs::create_dir_all(&target).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();
        let mem_body = b"# NOTES\nrelocated-body\n";
        write_underlay(&mp, "memory/NOTE.md", mem_body);

        // 挂载前顶层确有 fall-through（<uuid>/ 与 memory/ 两个顶层条目）。
        assert!(
            underlay_has_fallthrough(&mp).unwrap(),
            "前提：reconcile 前 underlay 顶层含 fall-through"
        );

        let m = FakeMounter::default();
        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // ── 核心断言：underlay 顶层归空 → ensure_underlay_empty 放行 → 重挂解锁（两 breach 均已修）。
        assert!(
            !crate::reconcile::guard::underlay_has_fallthrough(&mp).unwrap(),
            "抽干后 underlay 顶层必须无 fall-through（否则重挂永久 wedge）：{:?}",
            report.entries
        );
        crate::reconcile::guard::ensure_underlay_empty(&mp).unwrap();

        // BREACH 1：抽干的空子目录被剪除（顶层 <uuid>/ 无残留）。
        assert!(
            mp.join("sess-uuid").symlink_metadata().is_err(),
            "抽干的空 <uuid>/ 目录应被剪除"
        );
        // BREACH 2：underlay 侧 memory 无任何残留（既非目录也非复原的 symlink）。
        assert!(
            mp.join("memory").symlink_metadata().is_err(),
            "underlay memory 必须无残留（不复原 symlink）"
        );

        // ── 零丢失（a）：子代理会话内容落 orig 且已重灌 backing。
        let orig_sub = paths.orig("demo").join(sub_rel);
        assert_eq!(
            std::fs::read(&orig_sub).unwrap(),
            sub_body,
            "子代理会话内容应无损落 orig"
        );
        let backing_sub = backing.join(sub_rel);
        assert_eq!(
            read_archive(&backing_sub),
            sub_body,
            "子代理会话内容应重灌进 backing"
        );

        // ── 零丢失（b）：memory 文件被安置到 canonical target（挂载时 backing/memory symlink 服务）。
        assert_eq!(
            std::fs::read(target.join("NOTE.md")).unwrap(),
            mem_body,
            "memory 文件应安置到 canonical target"
        );

        // 无静默丢弃：报告含子代理 underlay-removed 与 memory-restored。
        assert!(
            report
                .entries
                .iter()
                .any(|e| e.action.contains("underlay-removed") && e.decision.contains("subagents")),
            "应有 subagents underlay-removed 条目：{:?}",
            report.entries
        );
        assert!(
            report.entries.iter().any(|e| e.action == "memory-restored"),
            "应有 memory-restored 条目：{:?}",
            report.entries
        );
        // reconciling 标记正常清（不 wedge）。
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾应清 reconciling 标记"
        );
    }

    #[test]
    fn reconcile_writes_manifest_with_per_entry_reversal_class() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // union：orig 预存 .jsonl + LogOnly incoming → Union（有前镜像）→ RestoreOrig。
        let rel_union = "s.jsonl";
        write_orig(&paths, "demo", rel_union, BASE_LOG.as_bytes());
        reingest_one_file(&paths, "demo", rel_union).unwrap();
        write_underlay(&mp, rel_union, INCOMING_LOG.as_bytes());

        // new：orig 缺 → New（无前镜像）→ RemoveOrig。
        let rel_new = "new.jsonl";
        write_underlay(&mp, rel_new, INCOMING_LOG.as_bytes());

        // keep-separate：orig 预存、disjoint uuid + 时间窗不交 → SuspectReuse → KeepSeparate →
        // RemoveQuarantine。
        let rel_keep = "3f2a-b1c2-uuid.jsonl";
        let keep_base = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
        );
        let keep_incoming = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"b1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
        );
        write_orig(&paths, "demo", rel_keep, keep_base.as_bytes());
        reingest_one_file(&paths, "demo", rel_keep).unwrap();
        write_underlay(&mp, rel_keep, keep_incoming.as_bytes());

        let m = FakeMounter::default();
        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // run ts = stash_dir 末段。
        let ts = report
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let manifest = read_manifest(&paths, "demo", &ts)
            .unwrap()
            .expect("reconcile 后 manifest 应存在");
        let map: std::collections::HashMap<String, ReversalClass> = manifest.into_iter().collect();

        assert_eq!(
            map.get(rel_union),
            Some(&ReversalClass::RestoreOrig),
            "union（orig 预存）→ RestoreOrig：{map:?}"
        );
        assert_eq!(
            map.get(rel_new),
            Some(&ReversalClass::RemoveOrig),
            "new（orig 缺）→ RemoveOrig：{map:?}"
        );
        assert_eq!(
            map.get(rel_keep),
            Some(&ReversalClass::RemoveQuarantine),
            "keep-separate → RemoveQuarantine：{map:?}"
        );
        // 合成条目（`<prune>`/`<meta>`/`<rebuild>`/`<prune-symlinks>`）绝不入 manifest。
        assert!(
            map.keys().all(|k| !k.starts_with('<')),
            "合成条目不应出现在 manifest：{map:?}"
        );
    }

    #[test]
    fn reconcile_dry_run_writes_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: true,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();
        let ts = report
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            read_manifest(&paths, "demo", &ts).unwrap().is_none(),
            "dry_run 不应写 manifest"
        );
    }

}
