//! 可逆生命周期：apply（切换）/ restore（还原）/ remount（重挂）/ purge（清 backing）。
//!
//! 配方移植自 crash-tested 的 `zipfs-cutover.sh` / `zipfs-rollback.sh`，并强化为
//! **sidecar 提交标记**：只有 backing 内 `.zipfs.meta` 存在且 `committed=1` 才算灌入完成、可挂载
//! （评审 C1/C2）。每个不可逆步骤前后补 fsync（父目录 dirent、backing 树、sidecar），
//! 使任意时刻崩溃要么可续、要么 fail-closed，绝不把半灌数据当权威挂出。

use std::fs;
use std::io;
use std::path::Path;

use crate::enable::daemon::{MountSpec, Mounter};
use crate::enable::discovery::{self, now_unix, Meta};
use crate::enable::model::{ApplyOptions, Backend, Paths};

/// apply 成功汇总（CLI/TUI 回显）。
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub files: u64,
    pub bytes_src: u64,
    pub bytes_archive: u64,
}

impl ApplyOutcome {
    pub fn ratio(&self) -> f64 {
        if self.bytes_archive == 0 {
            0.0
        } else {
            self.bytes_src as f64 / self.bytes_archive as f64
        }
    }
}

/// 切换：mv 备份 → ingest --verify → 写提交标记 → 挂载。任一步失败回滚到 Plain，零丢失。
pub fn apply(
    paths: &Paths,
    name: &str,
    opts: ApplyOptions,
    force: bool,
    mounter: &dyn Mounter,
) -> io::Result<ApplyOutcome> {
    let mp = paths.mountpoint(name);
    let orig = paths.orig(name);
    let backend = opts.backend;
    let backing = paths.backing(name, backend);
    let meta_path = paths.meta_path(name);

    // ── 前置校验 ──
    if orig.exists() {
        return Err(err(format!(
            "{} 已存在备份（疑似已切换），先 restore",
            orig.display()
        )));
    }
    if !mp.is_dir() {
        return Err(err(format!("项目目录不存在：{}", mp.display())));
    }
    if mounter.is_mounted(&mp) {
        return Err(err(format!("{} 已是挂载点", mp.display())));
    }
    if backing_occupied(&backing) {
        return Err(err(format!(
            "backing {} 已存在（疑似上次 restore 后未清理），先 `enable purge {name}` 再 apply（防误删 no-unconscious）",
            backing.display()
        )));
    }
    // 残留提交标记（任一后端的上次 apply 未 purge）→ 拒绝，避免跨后端遗留 backing 成孤儿（评审 H3）。
    if paths.meta_path(name).exists() {
        return Err(err(format!(
            "{name} 残留提交标记（上次切换未清理），先 `enable purge {name}` 再 apply",
        )));
    }
    // ── 活跃防护（紧贴 rename 前再查一次，收窄 TOCTOU）──
    if !force {
        let act = discovery::detect_activity(&mp);
        if let Some(reason) = act.reason() {
            return Err(err(format!(
                "项目活跃（{reason}）；拒绝 apply。确认空闲后用 --force / 在 TUI 键入 APPLY",
            )));
        }
    }

    // ── mv 源 → 备份（可逆关键），fsync 父目录持久化 dirent ──
    fs::rename(&mp, &orig)?;
    fsync_parent(&mp);
    fs::create_dir_all(paths.back_root())?;
    fs::create_dir(&mp)?;

    // ── 按 backend 流式灌入 + 逐字节校验；任何失败回滚到 Plain ──
    let ingest_res = match backend {
        Backend::Shadow => {
            if let Err(e) = fs::create_dir_all(&backing) {
                let rb = rollback_to_plain(&mp, &orig, &backing, &meta_path);
                return Err(rollback_msg(
                    rb,
                    &orig,
                    name,
                    format!("建 backing 失败：{e}"),
                ));
            }
            crate::ingest::ingest_tree(&orig, &backing, opts.chunk_size, opts.level, true)
        }
        Backend::Container => crate::ingest::ingest_tree_to_container(
            &orig,
            &backing,
            opts.chunk_size,
            opts.level,
            true,
        ),
    };
    let stats = match ingest_res {
        Ok(s) if s.errors.is_empty() && s.verified == s.files && s.skipped == 0 => s,
        Ok(s) if s.skipped > 0 => {
            let rb = rollback_to_plain(&mp, &orig, &backing, &meta_path);
            let what = match backend {
                Backend::Shadow => "特殊文件（FIFO/socket/设备）",
                Backend::Container => "符号链接/特殊文件（container 布局无法表示，可改用 shadow）",
            };
            return Err(rollback_msg(
                rb,
                &orig,
                name,
                format!("目录含 {} 个{what}，拒绝切换（避免静默丢失）", s.skipped),
            ));
        }
        Ok(s) => {
            let rb = rollback_to_plain(&mp, &orig, &backing, &meta_path);
            return Err(rollback_msg(
                rb,
                &orig,
                name,
                format!(
                    "灌入校验未全通过（files={} verified={} errors={}）",
                    s.files,
                    s.verified,
                    s.errors.len()
                ),
            ));
        }
        Err(e) => {
            let rb = rollback_to_plain(&mp, &orig, &backing, &meta_path);
            return Err(rollback_msg(rb, &orig, name, format!("灌入失败：{e}")));
        }
    };

    // ── fsync backing 使其持久（shadow 递归 fsync 目录树；container 的 redb 已 sync_all，
    //    再 fsync 其父目录持久化文件 dirent）──
    match backend {
        Backend::Shadow => fsync_tree(&backing),
        Backend::Container => fsync_parent(&backing),
    }

    // ── 写提交标记（write_meta 自身 fsync sidecar + 父目录）→ 此刻起 committed，可挂载 ──
    let meta = Meta::from_apply(&opts, stats.bytes_src, stats.bytes_archive, now_unix());
    discovery::write_meta(&meta_path, &meta)?;

    // ── 挂载守护；失败时 backing 已 committed → 保留切换态为 STOPPED，绝不删已提交 backing ──
    // Bug B：旧码无条件 rollback_to_plain → remove_backing，把一个已 ingest 完整 + 逐字节
    // 校验通过 + committed 的 backing 当半灌垃圾删掉，放大损坏、强制全量重灌。事故现场正是
    // 反复 apply 撞上孤儿守护占用的挂载点而 mount 失败、每次又删掉刚灌好的 backing。
    // 改：mount 失败但已提交 → 不回滚、不还原 orig，数据安全留在 orig 备份 + 已提交 backing，
    // 状态为 STOPPED（orig 在、未挂载、committed），用户 `enable remount` 直接复用，无需重灌。
    let spec = mount_spec(paths, name, &opts);
    if let Err(e) = mounter.spawn(&spec) {
        let _ = mounter.unmount(&mp); // 清理可能残留的半挂载 endpoint
        return Err(err(format!(
            "挂载失败：{e}；backing 已提交、数据完好（未删除），状态置为 STOPPED。\
             运行 `enable remount {name}` 重挂，或 `enable restore {name}` 回到 Plain"
        )));
    }

    Ok(ApplyOutcome {
        files: stats.files,
        bytes_src: stats.bytes_src,
        bytes_archive: stats.bytes_archive,
    })
}

/// 还原：卸载 → 删空挂载点 → 还原源备份。幂等、零丢失；backing 保留（另 purge）。
pub fn restore(paths: &Paths, name: &str, mounter: &dyn Mounter) -> io::Result<()> {
    let mp = paths.mountpoint(name);
    let orig = paths.orig(name);
    if !orig.exists() {
        return Err(err(format!("无备份 {}，无法还原", orig.display())));
    }
    mounter.unmount(&mp)?;
    // 删空挂载点目录（apply 时建的空 dir；仍非空 = 仍挂载）。崩溃后已删则跳过（幂等续做）。
    match fs::remove_dir(&mp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(err(format!("{} 非空（仍挂载？）：{e}", mp.display()))),
    }
    fs::rename(&orig, &mp)?;
    fsync_parent(&mp);
    let _ = fs::remove_file(paths.pid_file(name));
    // 使遗留 meta 失效（committed=false）：保留 backend/选项供 list 标 purgeable 与 purge 识别，
    // 但 remount/compact/seal 据 committed 过滤 → 拒绝对已还原项目误操作（评审 H4）。
    if let Ok(Some(mut m)) = discovery::read_meta(&paths.meta_path(name)) {
        if m.committed {
            m.committed = false;
            let _ = discovery::write_meta(&paths.meta_path(name), &m);
        }
    }
    Ok(())
}

/// 重挂（守护崩溃/重启后）。已挂跳过；stale endpoint 先卸载；须 backing 已提交。
pub fn remount(paths: &Paths, name: &str, mounter: &dyn Mounter) -> io::Result<()> {
    let mp = paths.mountpoint(name);
    if mounter.is_mounted(&mp) && discovery::endpoint_ok(&mp) {
        return Ok(()); // 幂等。
    }
    if !discovery::endpoint_ok(&mp) {
        let _ = mounter.unmount(&mp); // 清 stale endpoint。
    }
    let meta = discovery::read_meta(&paths.meta_path(name))?
        .filter(|m| m.committed)
        .ok_or_else(|| {
            err(format!(
                "{} backing 未提交（半灌），需 re-ingest 或 restore",
                name
            ))
        })?;
    let spec = mount_spec(paths, name, &meta.options());
    mounter.spawn(&spec)
}

/// 重挂所有 STOPPED 项目（守护死、backing 已提交），跳过 BROKEN（需人工）。返回 (名, 结果)。
pub fn remount_all(
    paths: &Paths,
    mounter: &dyn Mounter,
) -> io::Result<Vec<(String, io::Result<()>)>> {
    use crate::enable::model::ProjectStatus;
    let mut results = Vec::new();
    for info in discovery::scan(paths)? {
        if info.status == ProjectStatus::Stopped {
            let r = remount(paths, &info.name, mounter);
            results.push((info.name, r));
        }
    }
    Ok(results)
}

/// 离线压实某项目 backing：卸载（若挂着）→ compact → 恢复挂载。回收空间。
pub fn compact(paths: &Paths, name: &str, mounter: &dyn Mounter) -> io::Result<String> {
    let meta = committed_meta(paths, name)?;
    let backing = paths.backing(name, meta.backend);
    maintain(paths, name, &meta, mounter, "compact", move || {
        match meta.backend {
            Backend::Shadow => {
                let s = crate::compact::compact_shadow_tree(&backing, meta.level)?;
                Ok(format!(
                    "shadow compact: compacted={} {}B→{}B ({:.2}x)",
                    s.compacted,
                    s.bytes_before,
                    s.bytes_after,
                    s.ratio()
                ))
            }
            Backend::Container => {
                let mut st = crate::store::container::ContainerStore::open(&backing)?;
                let reclaimed = st.compact()?;
                drop(st);
                Ok(format!(
                    "container compact: {}",
                    if reclaimed {
                        "已回收 MVCC 旧页"
                    } else {
                        "无可回收"
                    }
                ))
            }
        }
    })
}

/// 冷文件封存某项目（仅 shadow）：卸载（若挂着）→ seal（大块高等级重编码）→ 恢复挂载。
pub fn seal(
    paths: &Paths,
    name: &str,
    seal_chunk: u32,
    level: i32,
    mounter: &dyn Mounter,
) -> io::Result<String> {
    let meta = committed_meta(paths, name)?;
    if meta.backend != Backend::Shadow {
        return Err(err("seal 仅支持 shadow 布局".into()));
    }
    let backing = paths.backing(name, Backend::Shadow);
    maintain(paths, name, &meta, mounter, "seal", move || {
        let s = crate::seal::seal_shadow_tree(&backing, seal_chunk, level)?;
        Ok(format!(
            "seal: sealed={} skipped={} {}B→{}B ({:.2}x)",
            s.sealed,
            s.skipped,
            s.bytes_before,
            s.bytes_after,
            s.ratio()
        ))
    })
}

/// 维护编排（compact/seal 共用，评审 H1/H2）：卸载→等守护退出→op→**无论成败都恢复挂载**。
/// container 必须确认守护退出（redb 排他锁）才动手，否则 fail-fast 并重挂回去。
fn maintain(
    paths: &Paths,
    name: &str,
    meta: &Meta,
    mounter: &dyn Mounter,
    op_name: &str,
    op: impl FnOnce() -> io::Result<String>,
) -> io::Result<String> {
    let mp = paths.mountpoint(name);
    let was_mounted = mounter.is_mounted(&mp);
    if was_mounted {
        mounter.unmount(&mp)?;
        let exited = wait_daemon_exit(paths, name);
        // container 的 redb 排他锁须等旧守护退出才释放；未确认退出则不动手，重挂回去（H2）。
        if meta.backend == Backend::Container && !exited {
            let _ = mounter.spawn(&mount_spec(paths, name, &meta.options()));
            return Err(err(format!(
                "{op_name} 取消：守护未在超时内退出、redb 锁未释放（已尝试重挂 {name}）"
            )));
        }
    }
    let result = op();
    // 无论 op 成败，原本挂着就尝试重挂，绝不把项目悬在「已卸载」（H1）。
    let remount_err = if was_mounted {
        mounter
            .spawn(&mount_spec(paths, name, &meta.options()))
            .err()
    } else {
        None
    };
    match (result, remount_err) {
        (Ok(report), None) => Ok(report),
        (Ok(report), Some(re)) => Err(err(format!(
            "{report}；但重挂失败：{re}，项目当前离线，请 `enable remount {name}`"
        ))),
        (Err(e), None) => Err(err(format!("{op_name} 失败：{e}（已重挂回原状）"))),
        (Err(e), Some(re)) => Err(err(format!(
            "{op_name} 失败：{e}；且重挂失败：{re}，项目当前离线，请 `enable remount {name}`"
        ))),
    }
}

/// 取某项目已提交的 meta，否则报错（compact/seal 前置）。
fn committed_meta(paths: &Paths, name: &str) -> io::Result<Meta> {
    discovery::read_meta(&paths.meta_path(name))?
        .filter(|m| m.committed)
        .ok_or_else(|| err(format!("{name} 未切换/未提交，无法维护")))
}

/// 卸载后等守护进程真正退出（释放 redb 排他锁 / 落盘）。轮询 pid-file 的 pid 直至不存活或超时（≤3s）。
/// 返回是否**确认退出**（true=已退出/无 pid；false=超时仍存活）。fusermount3 -u 只摘挂载点，
/// 守护 `session.join()` 返回后才退出，故 container compact 前必须等并据返回值决定是否动手。
fn wait_daemon_exit(paths: &Paths, name: &str) -> bool {
    let pid_file = paths.pid_file(name);
    let Ok(s) = fs::read_to_string(&pid_file) else {
        return true; // 无 pid 文件（已退出/未写）→ 视为已退出。
    };
    let Ok(pid) = s.trim().parse::<i32>() else {
        return true;
    };
    if pid <= 0 {
        return true; // 0 = FakeMounter 占位，无真实进程。
    }
    for _ in 0..30 {
        // kill(pid, 0)：仅探测存活，不发信号。返回 -1/ESRCH = 已退出。
        // SAFETY: 标准存活探测，sig=0 不影响目标。
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        if !alive {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// 删除 backing + meta（仅在已还原、未挂载时）。`yes` 必须为 true（二次确认）。
pub fn purge_backing(paths: &Paths, name: &str, yes: bool) -> io::Result<()> {
    if !yes {
        return Err(err("purge 需 --yes 确认".into()));
    }
    let orig = paths.orig(name);
    let mp = paths.mountpoint(name);
    if orig.exists() {
        return Err(err(format!(
            "{} 仍有备份（未还原），先 restore 再 purge",
            orig.display()
        )));
    }
    if discovery::is_mounted(&mp) {
        return Err(err(format!("{} 仍挂载，先 restore", mp.display())));
    }
    // 由 meta 反推 backend 以删对 backing（shadow 目录 / container redb 文件）；无 meta 时两路都试删。
    let backend = discovery::read_meta(&paths.meta_path(name))
        .ok()
        .flatten()
        .map(|m| m.backend);
    match backend {
        Some(b) => remove_backing(&paths.backing(name, b))?,
        None => {
            remove_backing(&paths.backing(name, Backend::Shadow))?;
            remove_backing(&paths.backing(name, Backend::Container))?;
        }
    }
    let _ = fs::remove_file(paths.meta_path(name));
    Ok(())
}

// ── 内部助手 ──────────────────────────────────────────────────────────────

/// 组装挂载守护参数（apply / remount 共用，DRY）。
/// 由 paths + 选项构造一个 MountSpec（apply / remount / mount-managed 共享）。
pub(crate) fn mount_spec(paths: &Paths, name: &str, opts: &ApplyOptions) -> MountSpec {
    MountSpec {
        name: name.to_string(),
        backend: opts.backend,
        backing: paths.backing(name, opts.backend),
        mountpoint: paths.mountpoint(name),
        chunk_size: opts.chunk_size,
        level: opts.level,
        pid_file: paths.pid_file(name),
        dict: opts.dict.clone(),
        threads: opts.threads,
        writeback: opts.writeback,
        max_write: opts.max_write,
        no_tail_buffer: opts.no_tail_buffer,
        allow_other: opts.allow_other,
        auto_unmount: opts.auto_unmount,
        metrics_file: opts.metrics_file.clone(),
    }
}

/// backing 是否已被占用（shadow 目录非空 / container redb 文件存在）。
fn backing_occupied(backing: &Path) -> bool {
    if backing.is_dir() {
        dir_nonempty(backing)
    } else {
        backing.exists()
    }
}

/// 删除 backing：目录递归删、文件删；不存在视为成功。
fn remove_backing(backing: &Path) -> io::Result<()> {
    if backing.is_dir() {
        fs::remove_dir_all(backing)
    } else if backing.exists() {
        fs::remove_file(backing)
    } else {
        Ok(())
    }
}

/// 回滚到 Plain：删空挂载点 → 还原源备份 → 删 backing + meta。尽力而为。
/// 返回是否**完整**回到 Plain 不变式（源在挂载点、无遗留备份）；false = 源备份仍留在 `orig`，需人工 restore。
fn rollback_to_plain(mp: &Path, orig: &Path, backing: &Path, meta_path: &Path) -> bool {
    let _ = fs::remove_dir(mp); // 空目录
    if orig.exists() {
        let _ = fs::rename(orig, mp);
        fsync_parent(mp);
    }
    let _ = remove_backing(backing);
    let _ = fs::remove_file(meta_path);
    // Plain 不变式：源回到挂载点且无遗留备份。任一不满足即回滚未完成（源安全留在 orig）。
    mp.exists() && !orig.exists()
}

/// 构造「失败 + 回滚」错误：如实反映回滚是否完成，绝不在残留 BROKEN 时谎称已回滚。
fn rollback_msg(rolled_back: bool, orig: &Path, name: &str, reason: String) -> io::Error {
    if rolled_back {
        err(format!("{reason}，已回滚到 Plain"))
    } else {
        err(format!(
            "{reason}；自动回滚未完成，源数据安全保留在 {}，请手动 `enable restore {name}` 完成还原",
            orig.display()
        ))
    }
}

/// 目录存在且非空。
fn dir_nonempty(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(mut rd) => rd.next().is_some(),
        Err(_) => false,
    }
}

/// fsync `path` 的父目录（持久化 rename/create 的 dirent）。尽力而为。
fn fsync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(f) = fs::File::open(parent) {
            let _ = f.sync_all();
        }
    }
}

/// 递归 fsync 目录树的各子目录（archive 文件已各自 sync_all）。尽力而为。
fn fsync_tree(dir: &Path) {
    if let Ok(f) = fs::File::open(dir) {
        let _ = f.sync_all();
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for dent in rd.flatten() {
            if dent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                fsync_tree(&dent.path());
            }
        }
    }
}

fn err(msg: String) -> io::Error {
    io::Error::other(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enable::daemon::fake::FakeMounter;
    use crate::enable::model::ProjectStatus;

    /// 构造隔离 Paths（直接给路径，绕过 env）。
    fn paths_in(root: &Path) -> Paths {
        Paths {
            projects_root: root.join("projects"),
            zipfs_home: root.join("zip"),
        }
    }

    /// 建一个项目目录并写一个文件，返回该文件原始内容。
    fn make_project(paths: &Paths, name: &str, file: &str, content: &[u8]) {
        let dir = paths.mountpoint(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(file), content).unwrap();
    }

    #[test]
    fn apply_then_status_active_and_restore_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let content = b"{\"k\":1}\nline two\n".repeat(2000);
        make_project(&paths, "demo", "a.jsonl", &content);
        let m = FakeMounter::default();

        // apply（force：测配方，绕活跃判定）。
        let out = apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();
        assert_eq!(out.files, 1);
        assert!(out.ratio() > 1.0, "可压缩内容比值>1：{}", out.ratio());

        // 备份在、backing 有提交标记、FakeMounter 标记挂载。
        assert!(paths.orig("demo").exists(), "源备份应存在");
        let meta = discovery::read_meta(&paths.meta_path("demo"))
            .unwrap()
            .unwrap();
        assert!(meta.committed, "应写入 committed=1");
        assert!(m.is_mounted(&paths.mountpoint("demo")));

        // probe 用真实 mountinfo，FakeMounter 不可见 → 这里以注入挂载器为准（Active 态由
        // model::classify 真值表覆盖）。

        // restore → 还原源、解除挂载、内容逐字节一致。
        restore(&paths, "demo", &m).unwrap();
        assert!(!paths.orig("demo").exists(), "还原后备份应消失");
        assert!(!m.is_mounted(&paths.mountpoint("demo")));
        let got = fs::read(paths.mountpoint("demo").join("a.jsonl")).unwrap();
        assert_eq!(got, content, "还原内容须逐字节等于原文");
    }

    #[test]
    fn restore_invalidates_meta_then_blocks_compact_and_reapply() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"x\n".repeat(400).as_slice());
        let m = FakeMounter::default();
        apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();
        restore(&paths, "demo", &m).unwrap();

        // restore 后 meta committed=false（评审 H4）：compact 拒绝、remount 拒绝。
        let meta = discovery::read_meta(&paths.meta_path("demo"))
            .unwrap()
            .unwrap();
        assert!(!meta.committed, "restore 应使 meta committed=false");
        assert!(
            compact(&paths, "demo", &m).is_err(),
            "已还原项目不应可 compact"
        );
        assert!(
            remount(&paths, "demo", &m).is_err(),
            "已还原项目不应可 remount"
        );

        // 残留 backing+meta → 再 apply 被拒，提示先 purge（评审 H3，防孤儿 backing）。
        let res = apply(&paths, "demo", ApplyOptions::default(), true, &m);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("purge"));
    }

    #[test]
    fn compact_remounts_after_op() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(
            &paths,
            "demo",
            "a.jsonl",
            b"compactable line\n".repeat(2000).as_slice(),
        );
        let m = FakeMounter::default();
        apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();
        // compact：卸载→op→重挂；结束仍挂载。
        let report = compact(&paths, "demo", &m).unwrap();
        assert!(report.contains("compact"), "报告应含 compact：{report}");
        assert!(
            m.is_mounted(&paths.mountpoint("demo")),
            "compact 后应已重挂"
        );
    }

    #[test]
    fn apply_rolls_back_to_plain_on_ingest_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"hello");
        let m = FakeMounter::default();

        // chunk_size=0 让 ingest_tree 立即失败 → 触发回滚。
        let bad = ApplyOptions {
            chunk_size: 0,
            ..ApplyOptions::default()
        };
        let res = apply(&paths, "demo", bad, true, &m);
        assert!(res.is_err(), "应失败");

        // 回滚到 Plain：源在原位、无备份、无 backing、未挂载。
        assert!(
            paths.mountpoint("demo").join("a.jsonl").exists(),
            "源应回到原位"
        );
        assert!(!paths.orig("demo").exists(), "不应残留备份");
        assert!(
            !paths.backing("demo", Backend::Shadow).exists(),
            "应删除半灌 backing"
        );
        assert!(!m.is_mounted(&paths.mountpoint("demo")));
        assert_eq!(
            discovery::probe(&paths, "demo").status,
            ProjectStatus::Plain
        );
    }

    #[test]
    fn apply_keeps_committed_backing_on_mount_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"hello");
        // 灌入+校验+提交成功，但挂载守护起不来：backing 已 committed，绝不删（Bug B）。
        // 旧码无条件 rollback_to_plain 删掉有效 backing、放大损坏、强制全量重灌；
        // 新码保留切换态为 STOPPED（数据在 orig 备份 + 已提交 backing），可直接 remount。
        let m = FakeMounter {
            fail_spawn: true,
            ..FakeMounter::default()
        };

        let res = apply(&paths, "demo", ApplyOptions::default(), true, &m);
        let e = res.unwrap_err().to_string();
        assert!(e.contains("挂载失败"), "应点明挂载失败：{e}");
        assert!(e.contains("remount"), "应提示可 remount 恢复：{e}");

        // 已提交 backing 必须保留（不被误删）。
        assert!(
            paths.backing("demo", Backend::Shadow).exists(),
            "已提交 backing 不应被删除（Bug B 核心）"
        );
        // 源备份保留、未挂载 → STOPPED（可 remount，无需重灌）。
        assert!(paths.orig("demo").exists(), "源备份应保留");
        assert!(!m.is_mounted(&paths.mountpoint("demo")));
        assert_eq!(
            discovery::probe(&paths, "demo").status,
            ProjectStatus::Stopped,
            "mount 失败但已提交 → STOPPED"
        );

        // 用正常 mounter 可直接 remount（证明数据完好、无需重灌）。
        let ok = FakeMounter::default();
        remount(&paths, "demo", &ok).unwrap();
        assert!(ok.is_mounted(&paths.mountpoint("demo")), "应能重挂恢复");
    }

    #[test]
    fn apply_blocked_when_active_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        // 新建 .jsonl → mtime=now → detect_activity 判活跃。
        make_project(&paths, "demo", "live.jsonl", b"{}\n");
        let m = FakeMounter::default();

        let res = apply(&paths, "demo", ApplyOptions::default(), false, &m);
        assert!(res.is_err(), "活跃项目无 --force 应拒绝");
        // 不变更：仍是 Plain。
        assert!(!paths.orig("demo").exists());
        assert_eq!(
            discovery::probe(&paths, "demo").status,
            ProjectStatus::Plain
        );
    }

    #[test]
    fn remount_after_daemon_death() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"data\n".repeat(500).as_slice());
        let m = FakeMounter::default();
        apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();

        // 模拟守护死：从 FakeMounter 移除挂载，状态变 STOPPED（备份在 + 已提交）。
        m.unmount(&paths.mountpoint("demo")).unwrap();
        assert_eq!(
            discovery::probe(&paths, "demo").status,
            ProjectStatus::Stopped
        );

        // remount → 重新挂上。
        remount(&paths, "demo", &m).unwrap();
        assert!(m.is_mounted(&paths.mountpoint("demo")));
    }

    #[test]
    fn apply_recreates_symlink_in_backing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"hi\n");
        // Claude 的 `memory` 外链：apply 应照原样重建到 backing（运行时经 readlink 透明服务）。
        std::os::unix::fs::symlink(
            "/some/external/memory",
            paths.mountpoint("demo").join("memory"),
        )
        .unwrap();
        let m = FakeMounter::default();

        let out = apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();
        assert_eq!(out.files, 1, "仅 1 个常规文件");
        // backing 里软链按原样重建、target 不变。
        let link = paths.backing("demo", Backend::Shadow).join("memory");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::path::Path::new("/some/external/memory")
        );
    }

    #[test]
    fn apply_refuses_and_rolls_back_on_fifo() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"hi\n");
        // 真正特殊文件（FIFO）：shadow 无法表示 → 应拒绝并回滚（评审 C1）。
        let fifo = paths.mountpoint("demo").join("pipe");
        let cpath = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: 标准 mkfifo(3)，路径来自受控 tempdir。
        let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo 应成功");
        let m = FakeMounter::default();

        let res = apply(&paths, "demo", ApplyOptions::default(), true, &m);
        assert!(res.is_err(), "含 FIFO 应拒绝");
        assert!(res.unwrap_err().to_string().contains("特殊文件"));
        assert!(paths.mountpoint("demo").join("a.jsonl").exists());
        assert!(!paths.orig("demo").exists());
        assert!(!paths.backing("demo", Backend::Shadow).exists());
    }

    #[test]
    fn purge_requires_restore_first() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"x\n".repeat(200).as_slice());
        let m = FakeMounter::default();
        apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();

        // 未 restore（备份仍在）→ purge 拒绝。
        assert!(purge_backing(&paths, "demo", true).is_err());
        // restore 后 purge 成功，backing 消失。
        restore(&paths, "demo", &m).unwrap();
        purge_backing(&paths, "demo", true).unwrap();
        assert!(!paths.backing("demo", Backend::Shadow).exists());
    }
}
