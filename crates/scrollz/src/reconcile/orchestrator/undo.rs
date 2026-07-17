//! reconcile-undo：回退最近一代 reconcile（underlay + orig + backing）。
use std::io;

use super::*;

/// 回退**最近一代** reconcile（§10）：把项目还原到该 run **之前**的状态（underlay + orig + backing），
/// 随后可换选项重跑 `reconcile`。**无 mounter 参数**（评审 M4：undo 不重挂，未挂载判定走
/// `discovery::is_mounted`）。全程与 reconcile 对称：reconcile 锁 + `reconciling` marker + 陈旧门 +
/// 逐条守卫，满足零丢失铁律。
///
/// **前置门禁（缺一即拒，§10.2）：**
/// 1. `validate_name`；项目**未挂载**（`is_mounted` 为真即拒）；**shadow** 后端（读 meta 判，非 shadow 拒）。
/// 2. 取 **reconcile 锁**（`reconcile_lock`，持锁到结束，与 reconcile / 其他 undo 互斥）。
/// 3. 选**目标代次 = 全体 ts 最大**的一代：无任何代次→Err；**最新代次无 manifest**（崩溃未完成的 run）
///    →Err 且**绝不清 marker**（marker 归崩溃 run）。该代次已 `.undone`→no-op（幂等，返回空 reversed）。
/// 4. **陈旧门（§10.2 C1）**：`detect_activity` 空闲否则拒；且对 `stash/<ts>/underlay/**` 每个快照文件，
///    比对 live `mp/<rel>`：**live 缺失或与快照逐字节相等**才算未变（mtime/size/ino 未随进程存活，用
///    byte-equal，比 mtime 更强）。任一 live 已变（存在且字节不同）→ **拒绝整个 undo**、报告该 rel、零改动。
///
/// **逆转（§10.3）：** `set_reconciling(true)`（半改写窗口保护）→ 逐条目按 manifest `ReversalClass` 反做
/// → 统一还原 underlay（逐条守卫）→ `set_reconciling(false)` **先于** 落 `.undone`（闭合崩溃 wedge 窗口，
/// Task4 Important）→ 剪空目录。逆转/还原任一步
/// 出错即传播 `Err` 而**不清 marker**（marker 留存 → 生命周期维护让路、可修复后重跑，重跑幂等）。
/// 置 marker 后复检挂载态、命中即清 marker 并中止的可测小函数（Task1 Important）。抽出以便单测——真实
/// 挂载态复检（`discovery::is_mounted` 读 `/proc/self/mountinfo`）在集成环境验证。`mounted` 为真表示复检
/// 发现项目已在 undo 准备期间被挂载：先 `set_reconciling(false)` 清 marker（此刻尚未任何改写、清 marker
/// 安全），再返回 `Err` 中止（绝不留滞留 marker）；为假则放行（`Ok`）。
pub(crate) fn abort_if_mounted_clearing_marker(paths: &Paths, name: &str, mounted: bool) -> io::Result<()> {
    if mounted {
        set_reconciling(paths, name, false)?;
        return Err(io::Error::other(format!(
            "{name} 在 undo 准备期间被挂载，已中止；请卸载后重试"
        )));
    }
    Ok(())
}

pub fn reconcile_undo(paths: &Paths, name: &str) -> io::Result<UndoReport> {
    validate_name(name)?;

    // 前置门禁 1a：项目必须未挂载（undo 半改写 orig/backing，不能作用在挂载态视图上）。
    let mp = paths.mountpoint(name);
    if discovery::is_mounted(&mp) {
        return Err(io::Error::other(format!(
            "{name} 已挂载，拒绝 reconcile-undo；请先卸载后重试"
        )));
    }

    // 前置门禁 1b：shadow 后端（container 无 fall-through / per-file 语义，undo 不适用）。无 meta = 未 apply。
    let meta = discovery::read_meta(&paths.meta_path(name))?.ok_or_else(|| {
        io::Error::other(format!(
            "{name} 无提交标记 meta，无法 reconcile-undo（未 apply？）"
        ))
    })?;
    if meta.backend != Backend::Shadow {
        return Err(io::Error::other(format!(
            "reconcile-undo 仅支持 shadow 后端；{name:?} 为 {}，不适用",
            meta.backend.flag()
        )));
    }

    // 前置门禁 2：取 reconcile 锁（与 reconcile / 其他 undo 互斥），持锁到函数末。
    let lock_path = paths.reconcile_lock(name);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = acquire_exclusive_retry(&lock_path)?;

    // 前置门禁 3：选目标代次 = 全体 ts 最大的一代（§10.2/评审 I2）。
    let Some(ts) = latest_generation(paths, name)? else {
        return Err(io::Error::other(format!(
            "{name} 无可回退的 reconcile 记录（无任何代次）"
        )));
    };
    let stash_root = paths.reconcile_stash(name, &ts);
    // 最新代次无 manifest = 崩溃未完成的 run → 拒绝，**绝不清属于崩溃 run 的 marker**（此处也从未动 marker）。
    let manifest = read_manifest(paths, name, &ts)?.ok_or_else(|| {
        io::Error::other(format!(
            "{name} 最新代次 {ts} 无 manifest（该 reconcile run 未完成、不可 undo）；请查 stash 手动恢复"
        ))
    })?;
    // 幂等：该代次已 `.undone` → no-op（返回回填 ts 的空报告，防二次误触）。
    let undone_marker = stash_root.join(".undone");
    if undone_marker.exists() {
        // 防御性清 marker（闭合崩溃 wedge 窗口，Task4）：若上一次 undo 在「.undone 已落、marker 未清」
        // 两次 fsync 之间崩溃，reconciling marker 会滞留、经 bail_if_reconciling 永久挡住 remount/维护
        // （数据已还原、无丢失，但项目卡死）。此处在短路 return 前顺手清——幂等：正常二次 undo 时 marker
        // 已不在、无副作用；若因旧崩溃滞留则顺手闭合窗口。no-op 报告不变。
        set_reconciling(paths, name, false)?;
        return Ok(UndoReport {
            ts,
            ..Default::default()
        });
    }

    // 前置门禁 4：陈旧门（§10.2 C1）——detect_activity 空闲 + 每条快照 underlay 文件 live 缺失或逐字节相等。
    if let Some(reason) = detect_activity(&mp).reason() {
        return Err(io::Error::other(format!(
            "{name} 挂载点疑似活跃（{reason}），拒绝 reconcile-undo；确认空闲后重试"
        )));
    }
    let stash_underlay = stash_root.join("underlay");
    let changed = live_underlay_changed_since_snapshot(&stash_underlay, &mp)?;
    if !changed.is_empty() {
        return Err(io::Error::other(format!(
            "{name} reconcile 后 live underlay 已有新写，拒绝整个 undo（零改动）：{changed:?}；\
             请先 `enable reconcile` 收编新写、或手动处理"
        )));
    }

    // ── 逆转（§10.3）：置 marker（半改写窗口）→ 逐条目反做 → 还原 underlay → .undone → 清 marker。
    set_reconciling(paths, name, true)?;

    // Task1 Important：置 marker **后、任何改写前**复检挂载态，闭合「未挂载判定（门禁 1a）→ 置 marker」
    // 间的自启挂载竞态窗口。此窗口内项目 = 未挂载 + underlay 已被上代 reconcile 抽干（空）+ marker 未置：
    // reconcile 靠「underlay 非空」挡自启，但 undo 的 underlay 是空的、该保护失效，仅 marker 能挡——而 marker
    // 到此刻才置。故此空档 systemd 自启（underlay 空 + 无 marker → 放行）可把项目挂上，undo 随后在活 FUSE
    // 挂载之上改写 backing/写回 mp → 不一致却「成功」返回。加此复检后：任何挂载要么早于置 marker（被本复检
    // 抓到）、要么晚于置 marker（被 marker 挡下自启入口）→ 窗口闭合。命中即先清 marker（此刻尚未改写、清
    // marker 安全）再返回 Err（与既有崩溃窗口修复同精神：中止路径绝不留滞留 marker）。
    abort_if_mounted_clearing_marker(paths, name, discovery::is_mounted(&mp))?;

    let mut report = UndoReport {
        ts: ts.clone(),
        ..Default::default()
    };
    for (rel, class) in &manifest {
        match class {
            ReversalClass::RestoreOrig => {
                undo_restore_orig(paths, name, &stash_root, rel)?;
                report.reversed.push((rel.clone(), "RestoreOrig".into()));
            }
            ReversalClass::RemoveOrig => {
                undo_remove_orig(paths, name, rel)?;
                report.reversed.push((rel.clone(), "RemoveOrig".into()));
            }
            ReversalClass::RemoveQuarantine => {
                undo_remove_quarantine(paths, name, &ts, &stash_root, rel)?;
                report
                    .reversed
                    .push((rel.clone(), "RemoveQuarantine".into()));
            }
            // ReportMemory：仅报告本代次往外部目标写过的文件，绝不触碰外部 memory 目标（§10.4）。
            ReversalClass::ReportMemory => report.memory_manual.push(rel.clone()),
            // manifest 已过滤 Noop（write_manifest），防御性忽略。
            ReversalClass::Noop => {}
        }
    }

    // 统一还原 underlay：stash/<ts>/underlay/** 逐文件拷回 mp/<rel>，逐条守卫记 skipped_live_changed。
    restore_underlay_from_snapshot(&stash_underlay, &mp, &mut report.skipped_live_changed)?;

    // 先清 marker 再落 .undone（Task4 Important：闭合崩溃 wedge 窗口）→ 剪除还原可能留的空目录。
    // 次序理由：先清 marker 后若两次 fsync 间崩溃，.undone 缺失 → 重跑重做幂等 undo 并再清 marker，
    // 收敛；且此空档 underlay 已还原为非空 → ensure_underlay_empty 仍挡自启挂载，不误挂。反之（旧序）
    // 先落 .undone 后崩溃 → .undone 在而 marker 滞留 → 短路直接 return 永不清 marker、永久 wedge
    //（上方 `.undone` 短路已补防御清 marker，双保险）。
    set_reconciling(paths, name, false)?;
    write_undone_marker(&undone_marker)?;
    prune_empty_underlay_dirs(&mp)?;

    Ok(report)
}

/// 枚举 `reconcile_stash(name)` 下所有 `<ts>` 代次目录，返回**数值 ts 最大**者。无代次/目录不存在 → `None`。
/// ts 是 unix 秒字符串，按 `u64` 解析比较（解析失败退化为字典序，容错）。
pub(crate) fn latest_generation(paths: &Paths, name: &str) -> io::Result<Option<String>> {
    // reconcile_stash(name, ts) 的父目录即 `<name>` 代次根，取一个占位 ts 后 parent。
    let probe = paths.reconcile_stash(name, "0");
    let Some(gen_root) = probe.parent() else {
        return Ok(None);
    };
    let rd = match std::fs::read_dir(gen_root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut best: Option<String> = None;
    for dent in rd {
        let dent = dent?;
        if !dent.file_type()?.is_dir() {
            continue;
        }
        let ts = dent.file_name().to_string_lossy().into_owned();
        best = Some(match best {
            Some(cur) if !ts_greater(&ts, &cur) => cur,
            _ => ts,
        });
    }
    Ok(best)
}

/// ts 数值比较（unix 秒），解析失败退化为字典序。返回 `a > b`。
fn ts_greater(a: &str, b: &str) -> bool {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x > y,
        _ => a > b,
    }
}

/// 陈旧门比对（§10.2 C1）：遍历 `stash_underlay`（快照）下每个文件，与 `mp/<rel>` 逐字节比对。
/// live **存在且内容不同** → 收进返回 Vec（reconcile 后有新写）；live 缺失或逐字节相等 → 视为未变。
///
/// 用 byte-equal（而非 mtime/size/ino）：快照落盘只存内容，身份三元组随进程退出已丢失，byte-equal
/// 是可从磁盘复算的、比 mtime 更强的「未变」判据。
pub(crate) fn live_underlay_changed_since_snapshot(
    stash_underlay: &Path,
    mp: &Path,
) -> io::Result<Vec<String>> {
    let mut changed = Vec::new();
    walk_compare_snapshot(stash_underlay, stash_underlay, mp, &mut changed)?;
    Ok(changed)
}

/// `live_underlay_changed_since_snapshot` 的递归实现。`root` 是快照 underlay 根（算 rel 用）。
fn walk_compare_snapshot(
    root: &Path,
    dir: &Path,
    mp: &Path,
    changed: &mut Vec<String>,
) -> io::Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for dent in rd {
        let dent = dent?;
        let path = dent.path();
        let ft = dent.file_type()?;
        if ft.is_dir() {
            walk_compare_snapshot(root, &path, mp, changed)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("stash underlay 条目逃出根"))?;
        let live = mp.join(rel);
        match std::fs::read(&live) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {} // live 缺失 → 未变
            Err(e) => return Err(e),
            Ok(live_bytes) => {
                if live_bytes != std::fs::read(&path)? {
                    changed.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    Ok(())
}

/// RestoreOrig 反做：**先 fail-closed 校验** `stash/<ts>/orig/<rel>` 前镜像存在（缺→Err 中止，绝不
/// 静默半还原，评审 I-plan2）→ 读前镜像 → `atomic_write(orig/<rel>)` 原子还原 → `reingest_one_file`
/// 原子重建 `backing/<rel>`（与 reconcile 同原语）。
fn undo_restore_orig(paths: &Paths, name: &str, stash_root: &Path, rel: &str) -> io::Result<()> {
    let preimage = stash_root.join("orig").join(rel);
    let bytes = match std::fs::read(&preimage) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::other(format!(
                "RestoreOrig 反做中止：前镜像 {} 缺失（绝不静默半还原）；reconciling 标记保留，修复后可重跑",
                preimage.display()
            )));
        }
        Err(e) => return Err(e),
    };
    let orig_file = paths.orig(name).join(rel);
    if let Some(parent) = orig_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&orig_file, &bytes)?;
    reingest_one_file(paths, name, rel)
}

/// RemoveOrig 反做：删 `orig/<rel>` + `backing/<rel>`（NotFound 容忍，幂等重跑安全）。防 undo 后残留
/// new 增出的孤儿。
fn undo_remove_orig(paths: &Paths, name: &str, rel: &str) -> io::Result<()> {
    remove_file_if_exists(&paths.orig(name).join(rel))?;
    // 评审 R-lock：删 backing/<rel> 也取 backing 锁，与 compact/seal/守护互斥（同 reingest_one_file）。
    let backing = paths.backing(name, Backend::Shadow);
    let _backing_lock = crate::store::lock::acquire_backing_retry(&backing)?;
    remove_file_if_exists(&backing.join(rel))
}

/// RemoveQuarantine 反做：`quarantine(name,ts)/<rel>` 副本先逐字节校验 == `stash/<ts>/underlay/<rel>`
/// **快照**（校验基准是快照、非 live，评审 I1）后删除。quarantine 副本缺失 → 幂等跳过（已删）；校验
/// 不符 → Err（绝不误删与快照不符的隔离副本，可能被人工改过）。orig/backing **绝不触碰**（keep-separate
/// 当初就没改 base）。
fn undo_remove_quarantine(
    paths: &Paths,
    name: &str,
    ts: &str,
    stash_root: &Path,
    rel: &str,
) -> io::Result<()> {
    let quarantine_file = paths.quarantine(name, ts).join(rel);
    let q_bytes = match std::fs::read(&quarantine_file) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()), // 已删，幂等
        Err(e) => return Err(e),
        Ok(b) => b,
    };
    let snapshot_file = stash_root.join("underlay").join(rel);
    if q_bytes != std::fs::read(&snapshot_file)? {
        return Err(io::Error::other(format!(
            "RemoveQuarantine 反做中止：隔离副本 {} 与快照 {} 不符（绝不误删）",
            quarantine_file.display(),
            snapshot_file.display()
        )));
    }
    remove_file_if_exists(&quarantine_file)
}

/// 删单文件（NotFound 容忍，幂等）并 best-effort fsync 父目录持久化 dirent。
fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        let _ = fsync_dir(parent);
    }
    Ok(())
}

/// 统一还原 underlay（§10.3 步3）：把 `stash_underlay`（快照）下每个文件拷回 `mp/<rel>`（重建目录结构）。
///
/// **逐条守卫（承 §10.2 C1）**：仅当 live **缺失**或与快照**逐字节一致**才覆盖还原；live 已存在且不同
/// （reconcile 后新写）→ **保留 live、记入 `skipped`、绝不覆盖**（陈旧门 + 此守卫双保险，绝不用旧快照
/// 盖新数据）。原子写还原（`atomic_write`）。
pub(crate) fn restore_underlay_from_snapshot(
    stash_underlay: &Path,
    mp: &Path,
    skipped: &mut Vec<String>,
) -> io::Result<()> {
    walk_restore_snapshot(stash_underlay, stash_underlay, mp, skipped)
}

/// `restore_underlay_from_snapshot` 的递归实现。`root` 是快照 underlay 根（算 rel 用）。
fn walk_restore_snapshot(
    root: &Path,
    dir: &Path,
    mp: &Path,
    skipped: &mut Vec<String>,
) -> io::Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for dent in rd {
        let dent = dent?;
        let path = dent.path();
        let ft = dent.file_type()?;
        if ft.is_dir() {
            walk_restore_snapshot(root, &path, mp, skipped)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("stash underlay 条目逃出根"))?;
        let live = mp.join(rel);
        let snap_bytes = std::fs::read(&path)?;
        match std::fs::read(&live) {
            // live 缺失 → 还原快照。
            Err(e) if e.kind() == io::ErrorKind::NotFound => restore_one(&live, &snap_bytes)?,
            Err(e) => return Err(e),
            // 逐字节一致 → 已是快照内容，幂等 no-op（不重写）。
            Ok(live_bytes) if live_bytes == snap_bytes => {}
            // live 已存在且不同（reconcile 后新写）→ 保留 live、记 skipped、绝不覆盖。
            Ok(_) => skipped.push(rel.to_string_lossy().into_owned()),
        }
    }
    Ok(())
}

/// 原子还原单个 underlay 文件（重建父目录链）。
fn restore_one(live: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = live.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(live, bytes)
}

/// 落 `.undone` 标记（空文件 + fsync 文件与父目录）到目标代次 stash：防二次误触（再敲 undo 认出已消费）。
fn write_undone_marker(marker: &Path) -> io::Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    File::create(marker)?.sync_all()?;
    if let Some(parent) = marker.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::orchestrator::testsupport::*;
    use crate::enable::daemon::fake::FakeMounter;

    #[test]
    fn abort_if_mounted_clears_marker_and_errs_when_mounted() {
        // Task1 Important 复检路径：置 marker 后若复检发现已挂载 → 中止 + marker 已清（此刻尚未改写，
        // 清 marker 安全）。真实挂载态复检（is_mounted 读 /proc/self/mountinfo）靠集成环境；此处直接
        // 以 mounted=true 驱动抽出的可测函数。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        // 模拟逆转前刚置 marker 的状态。
        set_reconciling(&paths, "demo", true).unwrap();
        assert!(
            paths.reconciling_marker("demo").exists(),
            "前置：marker 已置"
        );

        let e = abort_if_mounted_clearing_marker(&paths, "demo", true).unwrap_err();
        assert!(e.to_string().contains("被挂载"), "应报被挂载中止");
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "中止路径必须已清 marker，绝不留滞留 marker"
        );
    }

    #[test]
    fn abort_if_mounted_is_noop_when_not_mounted() {
        // mounted=false → 放行（Ok），marker 原样保留供后续逆转使用。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        set_reconciling(&paths, "demo", true).unwrap();

        abort_if_mounted_clearing_marker(&paths, "demo", false).unwrap();
        assert!(
            paths.reconciling_marker("demo").exists(),
            "未挂载：marker 应原样保留"
        );
    }

    #[test]
    fn reconcile_undo_full_flow_restores_orig_removes_new_and_quarantine() {
        // union+new+keep-separate reconcile 后 undo：RestoreOrig 还原前镜像、RemoveOrig 删新增、
        // RemoveQuarantine 删隔离副本、underlay 从快照还原、结束态可再 reconcile。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let ts = reconcile_three_kinds(&paths, &mp);
        let orig_union = paths.orig("demo").join("s.jsonl");
        let orig_new = paths.orig("demo").join("new.jsonl");
        let orig_keep = paths.orig("demo").join("3f2a-b1c2-uuid.jsonl");
        let quarantine_keep = paths.quarantine("demo", &ts).join("3f2a-b1c2-uuid.jsonl");

        // reconcile 后态：underlay 清空、union orig 已合并、new orig 已建、keep 已隔离。
        assert!(!mp.join("s.jsonl").exists());
        assert!(orig_new.exists(), "new 应落 orig");
        assert!(quarantine_keep.exists(), "keep 应隔离");
        assert_ne!(
            std::fs::read(&orig_union).unwrap(),
            BASE_LOG.as_bytes(),
            "union orig 已合并（≠ base）"
        );

        // ── undo ──
        let report = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(report.ts, ts, "选中最近一代");

        // RestoreOrig：union orig 还原前镜像（== base）；backing 重建为 base。
        assert_eq!(
            std::fs::read(&orig_union).unwrap(),
            BASE_LOG.as_bytes(),
            "union orig 还原前镜像"
        );
        assert_eq!(
            read_archive(&paths.backing("demo", Backend::Shadow).join("s.jsonl")),
            BASE_LOG.as_bytes(),
            "union backing 重建为 base"
        );
        // RemoveOrig：new orig + backing 删除。
        assert!(!orig_new.exists(), "new orig 应删");
        assert!(
            !paths
                .backing("demo", Backend::Shadow)
                .join("new.jsonl")
                .exists(),
            "new backing 应删"
        );
        // RemoveQuarantine：隔离副本删除；keep orig base 不动。
        assert!(!quarantine_keep.exists(), "keep 隔离副本应删");
        assert_eq!(
            std::fs::read(&orig_keep).unwrap(),
            KEEP_BASE.as_bytes(),
            "keep orig base 绝不触碰"
        );

        // underlay 从快照还原：三条都回 mp。
        assert_eq!(
            std::fs::read(mp.join("s.jsonl")).unwrap(),
            INCOMING_LOG.as_bytes()
        );
        assert_eq!(
            std::fs::read(mp.join("new.jsonl")).unwrap(),
            INCOMING_LOG.as_bytes()
        );
        assert_eq!(
            std::fs::read(mp.join("3f2a-b1c2-uuid.jsonl")).unwrap(),
            KEEP_INCOMING.as_bytes()
        );

        // reversed 记三条逆转类。
        let rev: std::collections::HashMap<String, String> = report.reversed.into_iter().collect();
        assert_eq!(rev.get("s.jsonl").map(String::as_str), Some("RestoreOrig"));
        assert_eq!(rev.get("new.jsonl").map(String::as_str), Some("RemoveOrig"));
        assert_eq!(
            rev.get("3f2a-b1c2-uuid.jsonl").map(String::as_str),
            Some("RemoveQuarantine")
        );

        // .undone 落 + reconciling 清 + 结束态可再 reconcile（underlay 又有 fall-through）。
        assert!(
            paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "落 .undone 标记"
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾清 reconciling 标记"
        );
        assert!(
            underlay_has_fallthrough(&mp).unwrap(),
            "还原后 underlay 又有 fall-through → 可再 reconcile"
        );
    }

    #[test]
    fn reconcile_undo_stale_gate_rejects_and_zero_change() {
        // 陈旧门：undo 前对某快照条目在 mp 写不同内容 → 拒绝整个 undo、零改动、报告该 rel。
        // 回拨 mtime 绕过 5min 活跃门，单独验证 byte 门（活跃门另有覆盖）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let orig_merged = std::fs::read(&orig_file).unwrap();

        // reconcile 后 Claude 又写不同内容（回拨 mtime → 非活跃，但与快照字节不同）。
        write_underlay(&mp, rel, b"{\"type\":\"NEW-AFTER-RECONCILE\"}\n");
        backdate_mtime(&mp.join(rel), 600);

        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("已有新写") && e.to_string().contains(rel),
            "陈旧门应拒绝并报告 rel：{e}"
        );
        // 零改动：orig 未还原、live 未动、marker 未落、.undone 未落。
        assert_eq!(
            std::fs::read(&orig_file).unwrap(),
            orig_merged,
            "拒绝→orig 未动"
        );
        assert_eq!(
            std::fs::read(mp.join(rel)).unwrap(),
            b"{\"type\":\"NEW-AFTER-RECONCILE\"}\n",
            "拒绝→live 未动"
        );
        assert!(
            !paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "拒绝→不落 .undone"
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "拒绝前从未置 reconciling 标记"
        );
    }

    #[test]
    fn stale_gate_helper_reports_only_changed_rels() {
        // 陈旧门核心比对：live 与快照逐字节不同 → 报告该 rel；相等或 live 缺失 → 不报。
        let tmp = tempfile::tempdir().unwrap();
        let stash_underlay = tmp.path().join("stash").join("underlay");
        std::fs::create_dir_all(&stash_underlay).unwrap();
        std::fs::write(stash_underlay.join("same.jsonl"), b"SNAP\n").unwrap();
        std::fs::write(stash_underlay.join("diff.jsonl"), b"SNAP\n").unwrap();
        std::fs::write(stash_underlay.join("gone.jsonl"), b"SNAP\n").unwrap();
        let mp = tmp.path().join("mp");
        std::fs::create_dir_all(&mp).unwrap();
        std::fs::write(mp.join("same.jsonl"), b"SNAP\n").unwrap(); // 相等
        std::fs::write(mp.join("diff.jsonl"), b"CHANGED\n").unwrap(); // 不同
                                                                      // gone.jsonl live 缺失

        let changed = live_underlay_changed_since_snapshot(&stash_underlay, &mp).unwrap();
        assert_eq!(changed, vec!["diff.jsonl".to_string()], "仅报字节不同者");
    }

    #[test]
    fn restore_underlay_guard_keeps_changed_live() {
        // 逐条守卫：还原步遇 live 缺失 → 还原快照；live 与快照不同 → 不覆盖、保留 live、记 skipped。
        let tmp = tempfile::tempdir().unwrap();
        let stash_underlay = tmp.path().join("stash").join("underlay");
        std::fs::create_dir_all(&stash_underlay).unwrap();
        std::fs::write(stash_underlay.join("a.jsonl"), b"SNAP-A\n").unwrap();
        std::fs::write(stash_underlay.join("b.jsonl"), b"SNAP-B\n").unwrap();
        let mp = tmp.path().join("mp");
        std::fs::create_dir_all(&mp).unwrap();
        // a 缺失 → 还原；b 已存在且不同 → 保留 live。
        std::fs::write(mp.join("b.jsonl"), b"LIVE-B-CHANGED\n").unwrap();

        let mut skipped = Vec::new();
        restore_underlay_from_snapshot(&stash_underlay, &mp, &mut skipped).unwrap();

        assert_eq!(
            std::fs::read(mp.join("a.jsonl")).unwrap(),
            b"SNAP-A\n",
            "缺失 → 还原快照"
        );
        assert_eq!(
            std::fs::read(mp.join("b.jsonl")).unwrap(),
            b"LIVE-B-CHANGED\n",
            "不同 → 保留 live、绝不覆盖"
        );
        assert_eq!(
            skipped,
            vec!["b.jsonl".to_string()],
            "记 skipped_live_changed"
        );
    }

    #[test]
    fn reconcile_undo_marker_stays_on_restore_orig_preimage_missing() {
        // marker 对称：中途注入失败（RestoreOrig 前镜像缺失）→ reconciling 标记仍在、无 .undone、可重跑。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let preimage = paths.reconcile_stash("demo", &ts).join("orig").join(rel);
        assert!(preimage.exists(), "reconcile 应留 union 前镜像");
        let preimage_bytes = std::fs::read(&preimage).unwrap();

        // 注入失败：删前镜像 → RestoreOrig fail-closed 中止。
        std::fs::remove_file(&preimage).unwrap();
        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("前镜像") && e.to_string().contains("缺失"),
            "应因前镜像缺失 fail-closed 中止：{e}"
        );
        // marker 仍在（半改写窗口未收尾），无 .undone。
        assert!(
            paths.reconciling_marker("demo").exists(),
            "中途失败 → reconciling 标记保留，让维护让路"
        );
        assert!(
            !paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "失败 → 不落 .undone"
        );

        // 修复（复原前镜像）后重跑 → 幂等成功收尾。
        std::fs::write(&preimage, &preimage_bytes).unwrap();
        let report = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(report.ts, ts);
        assert_eq!(
            std::fs::read(&orig_file).unwrap(),
            BASE_LOG.as_bytes(),
            "重跑后 orig 还原前镜像"
        );
        assert!(
            paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "重跑成功落 .undone"
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "重跑成功清 reconciling 标记"
        );
    }

    #[test]
    fn reconcile_undo_rejects_crashed_run_without_manifest_and_keeps_marker() {
        // 最新代次无 manifest（崩溃未完成的 run）→ 拒绝，且绝不清崩溃 run 的 reconciling marker。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();

        // 崩溃代次：有 underlay 快照但无 manifest。
        let ts = "1000";
        let stash_underlay = paths.reconcile_stash("demo", ts).join("underlay");
        std::fs::create_dir_all(&stash_underlay).unwrap();
        std::fs::write(stash_underlay.join("s.jsonl"), b"{}\n").unwrap();
        // 崩溃 run 遗留的 reconciling 标记。
        set_reconciling(&paths, "demo", true).unwrap();

        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("manifest") && e.to_string().contains("未完成"),
            "无 manifest 的崩溃 run 应拒绝：{e}"
        );
        assert!(
            paths.reconciling_marker("demo").exists(),
            "绝不清除属于崩溃 run 的 reconciling 标记"
        );
    }

    #[test]
    fn reconcile_undo_rejects_when_no_generation() {
        // 无任何 reconcile 代次 → Err「无可回退」。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();
        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("无可回退") || e.to_string().contains("无任何代次"),
            "无代次应拒绝：{e}"
        );
    }

    #[test]
    fn reconcile_undo_second_time_is_noop() {
        // .undone 二次 undo → no-op（返回回填 ts 的空报告，零改动）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        let r1 = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(r1.ts, ts);
        assert!(!r1.reversed.is_empty(), "首次 undo 有实际逆转");

        // 二次 undo：.undone 已在 → no-op（在活跃/陈旧门之前短路，即便 underlay 已还原）。
        let r2 = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(r2.ts, ts, "no-op 仍回填 ts");
        assert!(
            r2.reversed.is_empty(),
            ".undone 二次 undo → 空 reversed（no-op）"
        );
    }

    #[test]
    fn reconcile_undo_reports_memory_manual_without_touching_target() {
        // ReportMemory：memory 条目进 memory_manual、外部目标未被 undo 触碰、underlay 从快照还原。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // backing/memory = 指向树外 target 的 symlink；underlay memory 被物化含文件。
        let target = tmp.path().join("external-memory");
        std::fs::create_dir_all(&target).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();
        let mem_body = b"# NOTES\nrelocated\n";
        write_underlay(&mp, "memory/NOTE.md", mem_body);

        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        // reconcile 后：memory 文件安置进 target。
        assert_eq!(std::fs::read(target.join("NOTE.md")).unwrap(), mem_body);

        let report = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(report.ts, ts);
        // memory 条目进 memory_manual（供用户 git 回退）。
        assert!(
            report.memory_manual.iter().any(|m| m == "memory/NOTE.md"),
            "memory 条目应进 memory_manual：{:?}",
            report.memory_manual
        );
        // undo 绝不触碰外部 memory 目标（target 仍有 reconcile 写入的文件）。
        assert_eq!(
            std::fs::read(target.join("NOTE.md")).unwrap(),
            mem_body,
            "undo 绝不触碰外部 memory 目标"
        );
        // underlay memory 从快照还原。
        assert_eq!(
            std::fs::read(mp.join("memory/NOTE.md")).unwrap(),
            mem_body,
            "underlay memory 从快照还原"
        );
    }

    #[test]
    fn latest_generation_picks_numeric_max_not_lexical() {
        // ts 按数值比较（"9" < "100"），非字典序。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        for ts in ["9", "100", "20"] {
            std::fs::create_dir_all(paths.reconcile_stash("demo", ts)).unwrap();
        }
        assert_eq!(
            latest_generation(&paths, "demo").unwrap().as_deref(),
            Some("100"),
            "数值最大而非字典序最大"
        );
    }

    #[test]
    fn reconcile_undo_rejects_non_shadow_backend() {
        // 前置门禁 1b：container 后端（无 fall-through / per-file 语义）→ 拒，错误含 "shadow"。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();
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

        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("shadow"),
            "container 后端应拒绝 reconcile-undo：{e}"
        );
    }

    #[test]
    fn reconcile_undo_rejects_without_meta() {
        // 前置门禁 1b：无 meta（未 apply / 无可回退记录）→ 拒。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();

        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("meta"),
            "无 meta 应拒绝 reconcile-undo：{e}"
        );
    }

    #[test]
    fn reconcile_undo_short_circuit_clears_lingering_marker() {
        // 崩溃窗口回归（Task4 Important）：模拟上一次 undo 在「.undone 已落、marker 未清」两次 fsync
        // 之间崩溃 → marker 滞留。再调 reconcile_undo 命中 `.undone` 短路 → 短路防御必须顺手清 marker，
        // 闭合 wedge 窗口。RED-before（旧码短路直接 return、永不清 marker）/ GREEN-after。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        // 首次 undo：正常收尾（清 marker + 落 .undone）。
        reconcile_undo(&paths, "demo").unwrap();
        assert!(
            paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "首次 undo 应落 .undone"
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "首次 undo 应已清 marker"
        );

        // 模拟崩溃窗口：.undone 已在，但 reconciling marker 被（旧序崩溃）重新滞留。
        set_reconciling(&paths, "demo", true).unwrap();
        assert!(
            paths.reconciling_marker("demo").exists(),
            "前提：marker 滞留（模拟崩溃窗口）"
        );

        // 再调 undo → 命中 .undone 短路，短路防御清 marker。
        let report = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(report.ts, ts, "短路仍回填 ts");
        assert!(report.reversed.is_empty(), "短路 no-op → 空 reversed");
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "短路防御必须清滞留 marker（闭合崩溃 wedge 窗口）"
        );
    }

}
