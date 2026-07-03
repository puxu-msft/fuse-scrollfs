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
use crate::enable::force_umount::UmountLevel;
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
    crate::core::validate_chunk_size(opts.chunk_size)?; // 评审：挡过大 chunk_size 的 OOM
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
    // 评审 A1：rename 已发生，此后任何建目录失败都必须回滚 rename，否则项目目录"消失"到
    // orig（enable list 跳过 *.zipfs-orig 后缀 → 列表里蒸发），用户恐慌驱动二次误操作。
    if let Err(e) = fs::create_dir_all(paths.back_root()).and_then(|_| fs::create_dir(&mp)) {
        let rb = rollback_to_plain(&mp, &orig, &backing, &meta_path);
        return Err(rollback_msg(
            rb,
            &orig,
            name,
            format!("建挂载点/backing 目录失败：{e}"),
        ));
    }

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
    // 评审 M3：write_meta 失败（ENOSPC 等）时 backing 已完整 + fsync，仅差提交标记 → 项目落
    // Broken（orig+backing 都在，数据安全）。给出明确指引而非裸 io error，避免用户误判数据丢失。
    if let Err(e) = discovery::write_meta(&meta_path, &meta) {
        return Err(err(format!(
            "写提交标记失败：{e}；backing 已完整灌入并 fsync、源安全保留在 {}（项目状态 Broken）。\
             腾出空间后 `enable reingest {name}` 重建，或 `enable restore {name}` 回到 Plain",
            orig.display()
        )));
    }

    // ── 挂载守护；失败时 backing 已 committed → 保留切换态为 STOPPED，绝不删已提交 backing ──
    // Bug B：旧码无条件 rollback_to_plain → remove_backing，把一个已 ingest 完整 + 逐字节
    // 校验通过 + committed 的 backing 当半灌垃圾删掉，放大损坏、强制全量重灌。事故现场正是
    // 反复 apply 撞上孤儿守护占用的挂载点而 mount 失败、每次又删掉刚灌好的 backing。
    // 改：mount 失败但已提交 → 不回滚、不还原 orig，数据安全留在 orig 备份 + 已提交 backing，
    // 状态为 STOPPED（orig 在、未挂载、committed），用户 `enable remount` 直接复用，无需重灌。
    let spec = mount_spec(paths, name, &opts);
    if let Err(e) = mounter.spawn(&spec) {
        let _ = mounter.unmount(name, &mp, UmountLevel::Auto); // 清理可能残留的半挂载 endpoint
        return Err(err(format!(
            "挂载失败：{e}；backing 已提交、数据完好（未删除），状态置为 STOPPED。\
             运行 `enable remount {name}` 重挂，或 `enable restore {name}` 回到 Plain"
        )));
    }

    // 挂载成功 → 注册自启（systemd：enable zipfs@<esc>，重启后自动重挂）。best-effort：失败
    // 不回滚已成功的挂载（RealMounter 下是 no-op），但要 warn——否则 systemd 下注册失败会让
    // 项目重启后不自动重挂，用户无从知晓。
    if let Err(e) = mounter.enable_autostart(name) {
        log::warn!(
            "{name} 已挂载，但 systemd 自启注册失败：{e}（重启后需手动 `enable remount {name}`）"
        );
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
    // Auto：还原只 mv orig 回项目路径、不改写 backing，故 wedge 可 lazy/abort 摘除、无损坏风险。
    mounter.unmount(name, &mp, UmountLevel::Auto)?;
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
    // 注销自启（systemd：disable zipfs@<esc>）。best-effort（RealMounter 下 no-op），warn 可观测。
    if let Err(e) = mounter.disable_autostart(name) {
        log::warn!(
            "{name} 已还原，但 systemd 自启注销失败：{e}（可手动 `systemctl --user disable`）"
        );
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
        // 清 stale endpoint。卸载失败仅 warn（dead 挂载 fusermount 常返错，不一定是真失败）。
        if let Err(e) = mounter.unmount(name, &mp, UmountLevel::Auto) {
            log::warn!("remount：清 {name} stale endpoint 失败：{e}");
        }
        // 评审 M1：复核实际挂载态——若挂载点仍被占（stale 未清除），spawn 必撞已占用挂载点，
        // 提前 fail 而非盲目 spawn 留半清理态。
        if mounter.is_mounted(&mp) {
            return Err(err(format!(
                "{name} 挂载点仍被占（stale endpoint 未清除），手动 `fusermount3 -u {}` 后重试",
                mp.display()
            )));
        }
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

/// 路径加后缀得 sibling（`<path><suffix>`）。用 OsString 拼接避免要求路径是合法 UTF-8。
fn sibling_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// 用源备份（orig）重新灌入某项目的 backing（**修复指令**）。仅 shadow。
///
/// 典型用途：backing 由**旧版本**二进制灌入、带早期缺陷（如 Bug D 不保留 mtime），用修好的
/// 当前二进制从 orig 金副本重建。container ingest 一直经 `file_attr` 保留属性，无此问题，故拒绝。
///
/// 流程：`ingest_tree` orig→临时 backing（lock-free，旧守护继续服务）+ 逐字节 verify → 卸载并等
/// 守护退出（释放 flock）→ 旧 backing 移到 `<backing>.reingest-bak` **保留** → 新 backing 就位 →
/// 重挂 → 工具自写更新后的 committed meta（含新字节数，免手编 sidecar）。
///
/// **数据安全**：旧 backing 始终留底为 `.reingest-bak`（不删，供核对后手动清理）；orig 不动；
/// 活跃项目无 `--force` 拒绝（避免写入中重建丢新数据，与 apply 一致）。
pub fn reingest(
    paths: &Paths,
    name: &str,
    force: bool,
    mounter: &dyn Mounter,
) -> io::Result<String> {
    let meta = committed_meta(paths, name)?;
    if meta.backend != Backend::Shadow {
        return Err(err(
            "reingest 仅支持 shadow（container 一直保留文件属性，无需重灌）".into(),
        ));
    }
    let orig = paths.orig(name);
    if !orig.exists() {
        return Err(err(format!(
            "{name} 无源备份 {}，无法 reingest（已 purge?）",
            orig.display()
        )));
    }
    if !force {
        if let Some(reason) = discovery::detect_activity(&paths.mountpoint(name)).reason() {
            return Err(err(format!(
                "项目活跃（{reason}）；拒绝 reingest。确认空闲后加 --force"
            )));
        }
    }
    let backing = paths.backing(name, Backend::Shadow);
    let tmp = sibling_suffix(&backing, ".reingest-tmp");
    let bak = sibling_suffix(&backing, ".reingest-bak");
    if bak.exists() {
        return Err(err(format!(
            "{} 已存在（上次 reingest 未清理）；核对后删除再试",
            bak.display()
        )));
    }

    // 1) orig → 临时 backing（lock-free，旧守护仍可服务）+ 逐字节校验。
    let _ = fs::remove_dir_all(&tmp);
    let opts = meta.options();
    let stats = crate::ingest::ingest_tree(&orig, &tmp, opts.chunk_size, opts.level, true)?;
    if !stats.errors.is_empty() || stats.skipped > 0 || stats.verified != stats.files {
        let _ = fs::remove_dir_all(&tmp);
        return Err(err(format!(
            "reingest 灌入校验未通过（files={} verified={} skipped={} errors={}），已弃临时 backing",
            stats.files,
            stats.verified,
            stats.skipped,
            stats.errors.len()
        )));
    }

    // 2) 卸载并**确认守护退出**（释放 flock）。守护未退出就换 backing 会与活守护抢同一锁文件、
    //    新 spawn 必失败，且正撞上 Bug A 的孤儿守护场景——故 fail-fast、不动 backing、重挂回去
    //    （与 maintain 对 container 的守卫同理；shadow flock 有同样的退出依赖）。
    let mp = paths.mountpoint(name);
    if mounter.is_mounted(&mp) {
        // Clean：reingest 换 backing，必须守护干净退出释放 flock；决不 lazy/abort 一个仍在写的活守护。
        mounter.unmount(name, &mp, UmountLevel::Clean)?;
    }
    if !wait_daemon_exit(paths, name) {
        let _ = fs::remove_dir_all(&tmp);
        let _ = mounter.spawn(&mount_spec(paths, name, &opts)); // 尽力恢复原挂载
        return Err(err(format!(
            "reingest 取消：守护未在超时内退出、flock 未释放（未动 backing，已尝试重挂 {name}）"
        )));
    }

    // 3) 旧 backing 留底 → 新 backing 就位（失败尽力回滚 + 重挂；回滚也失败则大声指明 bak 位置）。
    fs::rename(&backing, &bak)?;
    if let Err(e) = fs::rename(&tmp, &backing) {
        let restored = fs::rename(&bak, &backing).is_ok();
        let _ = mounter.spawn(&mount_spec(paths, name, &opts));
        return Err(err(if restored {
            format!("reingest 换 backing 失败：{e}（已回滚旧 backing + 重挂）")
        } else {
            format!(
                "reingest 换 backing 失败：{e}；且回滚失败——数据完好留存于 {}，请手动 `mv {} {}` 后 `enable remount {name}`",
                bak.display(),
                bak.display(),
                backing.display()
            )
        }));
    }
    fsync_parent(&backing);

    // 4) 重挂（结束态 = 已挂载）。
    let spawn_err = mounter.spawn(&mount_spec(paths, name, &opts)).err();

    // 5) 工具自写更新后的 committed meta（含新字节数）——免手编 sidecar。backing 已就位且
    //    committed 不变，故 meta 写失败只是 list 显示的字节数过时（非数据/挂载安全），warn 不 fail。
    let new_meta = Meta::from_apply(&opts, stats.bytes_src, stats.bytes_archive, now_unix());
    let meta_warn = match discovery::write_meta(&paths.meta_path(name), &new_meta) {
        Ok(()) => String::new(),
        Err(e) => format!("；但 meta 更新失败：{e}，list 显示的字节数可能过时"),
    };

    match spawn_err {
        None => Ok(format!(
            "reingest: {} 文件从 orig 重灌（{:.2}x），mtime/属性已保留；旧 backing 留底于 {}（核对后可删）{meta_warn}",
            stats.files,
            stats.ratio(),
            bak.display()
        )),
        Some(re) => Err(err(format!(
            "reingest 已重建 backing 但重挂失败：{re}，请 `enable remount {name}`；旧 backing 留底于 {}{meta_warn}",
            bak.display()
        ))),
    }
}

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
        // Clean：compact/seal 离线改写 backing，须守护干净退出（释放 redb 锁 / shadow backing）；
        // lazy/abort 会留活守护并发写 backing → 损坏。fail-closed 由下方 wait_daemon_exit 兜底。
        mounter.unmount(name, &mp, UmountLevel::Clean)?;
        let exited = wait_daemon_exit(paths, name);
        // 两种后端都须等旧守护退出才动手：container 释放 redb 排他锁，shadow 释放 backing
        // flock（compact/seal 现取同一锁，评审 A3）。未确认退出则不动手，重挂回去（H2）。
        if !exited {
            let _ = mounter.spawn(&mount_spec(paths, name, &meta.options()));
            return Err(err(format!(
                "{op_name} 取消：守护未在超时内退出、backing 锁未释放（已尝试重挂 {name}）"
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
///
/// 评审 D4/H2：单次读 pid-file 失败不立即判退出——守护被 systemd `Restart=on-failure` 重启时，
/// pid-file 有"删除→新守护重写"的窗口，此刻读失败误判退出会在锁实际仍被占时动手换 backing。
/// 故要求 pid-file **连续缺失** N 次才判退出；中途出现存活 pid 则继续等其退出。最终防线仍是
/// compact/seal 自身的 backing flock（评审 A3）——锁被占时维护操作 fail-closed，不依赖本探测。
fn wait_daemon_exit(paths: &Paths, name: &str) -> bool {
    let pid_file = paths.pid_file(name);
    const POLLS: u32 = 30;
    const MISS_THRESHOLD: u32 = 3; // 连续缺失阈值（~300ms），riding out 重启重写窗口
    let mut consecutive_missing = 0u32;
    for _ in 0..POLLS {
        match fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            None => {
                // 无 pid 文件 / 不可解析。可能已退出，也可能守护正被重启、pid 文件刚删将重写。
                consecutive_missing += 1;
                if consecutive_missing >= MISS_THRESHOLD {
                    return true; // 持续缺失 → 确认无守护
                }
            }
            Some(pid) if pid <= 0 => return true, // 0 = FakeMounter 占位，无真实进程
            Some(pid) => {
                consecutive_missing = 0; // 出现 pid → 重置缺失计数
                                         // kill(pid, 0)：仅探测存活，不发信号。返回 -1/ESRCH = 已退出。
                                         // SAFETY: 标准存活探测，sig=0 不影响目标。
                let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
                if !alive {
                    return true;
                }
            }
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
        // managed/systemd 挂载用块缓存默认值（ApplyOptions 暂不暴露此旋钮；默认 128MiB 压力感知
        // 正是 ~/.claude 托管挂载的目标行为）。
        block_cache_bytes: crate::core::blockcache::DEFAULT_CACHE_BYTES,
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
    // 评审 M2：删 backing 失败要 warn 且纳入回滚完整性判定——否则 rollback_msg 谎称"已回滚"
    // 而磁盘上躺着孤儿 backing（下次 apply 撞 backing_occupied 才暴露）。
    if let Err(e) = remove_backing(backing) {
        log::warn!(
            "rollback：删 backing {} 失败：{e}（残留，需 `enable purge`）",
            backing.display()
        );
    }
    let _ = fs::remove_file(meta_path);
    // Plain 不变式：源回到挂载点、无遗留备份、且无残留 backing/meta。任一不满足即回滚未完成。
    mp.exists() && !orig.exists() && !backing.exists() && !meta_path.exists()
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

    #[test]
    fn rollback_to_plain_reports_incomplete_when_backing_residue() {
        // 评审 M2：删 backing 失败时 rollback 必须返回 false（未完整回滚），不谎称已回 Plain。
        use std::os::unix::fs::PermissionsExt;
        // root 无视目录权限位，注入不成立 → 跳过（CI 可能以 root 跑）。
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mp = tmp.path().join("proj");
        let orig = tmp.path().join("proj.zipfs-orig");
        let backing = tmp.path().join("backdir");
        let meta = tmp.path().join("p.zipfs.meta");
        // 源已 mv 到 orig（apply 中段态），backing 是含一个文件的目录。
        fs::create_dir(&orig).unwrap();
        fs::write(orig.join("a.jsonl"), b"data").unwrap();
        fs::create_dir(&backing).unwrap();
        fs::write(backing.join("inner"), b"x").unwrap();
        // 把 backing 设为只读 → 非 root 无法删除其条目 → remove_dir_all 失败、backing 残留。
        fs::set_permissions(&backing, fs::Permissions::from_mode(0o500)).unwrap();

        let ok = rollback_to_plain(&mp, &orig, &backing, &meta);
        // 还原权限以便 TempDir 清理。
        let _ = fs::set_permissions(&backing, fs::Permissions::from_mode(0o755));

        assert!(mp.join("a.jsonl").exists(), "源应已还原到挂载点");
        assert!(
            !ok,
            "backing 残留时 rollback 须报未完成，不谎称已回滚（评审 M2）"
        );
    }

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
    fn unmount_level_contract_maintenance_clean_revert_auto() {
        // 安全契约：改写 backing 的维护操作（compact/reingest）须请求 Clean（要求守护干净退出，
        // 决不 lazy/abort 一个仍在写 backing 的活守护）；还原类请求 Auto（wedge 也能摘除、无损坏）。
        // 逐操作精确断言各自记录的档位（而非仅"存在某次 Clean"）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"line\n".repeat(2000).as_slice());
        let m = FakeMounter::default();
        apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();

        // compact：卸载→op→重挂。其卸载是本序列首个 unmount，必须 Clean。
        compact(&paths, "demo", &m).unwrap();
        assert_eq!(
            m.unmount_calls.lock().unwrap().first().map(|(_, l)| *l),
            Some(UmountLevel::Clean),
            "compact 必须以 Clean 卸载：{:?}",
            m.unmount_calls.lock().unwrap()
        );

        // reingest（另一改写 backing 路径）：其卸载也必须 Clean。记录本次调用前的长度以隔离。
        let before = m.unmount_calls.lock().unwrap().len();
        reingest(&paths, "demo", false, &m).unwrap();
        let calls = m.unmount_calls.lock().unwrap();
        assert!(
            calls[before..].iter().all(|(_, l)| *l == UmountLevel::Clean),
            "reingest 必须以 Clean 卸载：{:?}",
            &calls[before..]
        );
        drop(calls);

        // restore：不改写 backing → Auto。它是最后一次卸载。
        restore(&paths, "demo", &m).unwrap();
        assert_eq!(
            m.unmount_calls.lock().unwrap().last().map(|(_, l)| *l),
            Some(UmountLevel::Auto),
            "还原（restore）应以 Auto 卸载：{:?}",
            m.unmount_calls.lock().unwrap()
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
    fn apply_rolls_back_rename_when_dir_setup_fails() {
        // 评审 A1：rename(mp→orig) 与 create_dir(mp) 之间失败时，旧码用 `?` 直接传播、
        // 不回滚 rename → 项目目录"消失"到 .zipfs-orig，enable list 扫不到，用户恐慌。
        // 注入：在 back_root 路径放一个普通文件，使 create_dir_all(back_root) 必失败。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let content = b"real session\n".repeat(10);
        make_project(&paths, "demo", "a.jsonl", &content);
        let m = FakeMounter::default();

        // back_root 占位为文件 → create_dir_all 失败（路径已存在且非目录）。
        fs::create_dir_all(&paths.zipfs_home).unwrap();
        fs::write(paths.back_root(), b"blocker").unwrap();

        let res = apply(&paths, "demo", ApplyOptions::default(), true, &m);
        assert!(res.is_err(), "建目录失败应返回 Err");

        // 关键：必须回滚 rename，源数据回到原挂载点、无遗留 .zipfs-orig 备份。
        assert!(
            paths.mountpoint("demo").join("a.jsonl").exists(),
            "源应回到原挂载点（rename 已回滚），而非消失到 orig"
        );
        assert_eq!(
            fs::read(paths.mountpoint("demo").join("a.jsonl")).unwrap(),
            content,
            "回滚后内容逐字节一致"
        );
        assert!(!paths.orig("demo").exists(), "不应残留 .zipfs-orig 备份");
        assert!(!m.is_mounted(&paths.mountpoint("demo")));
    }

    #[test]
    fn reingest_rebuilds_backing_from_orig_preserving_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(
            &paths,
            "demo",
            "a.jsonl",
            b"session data\n".repeat(50).as_slice(),
        );
        let m = FakeMounter::default();
        apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();

        // orig 文件盖一个已知过去 mtime（模拟原始会话时间，2020-01-01）。
        let orig_file = paths.orig("demo").join("a.jsonl");
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800);
        crate::core::set_file_times(&orig_file, past, past).unwrap();

        let report = reingest(&paths, "demo", true, &m).unwrap();
        assert!(report.contains("reingest"), "报告：{report}");

        // 新 backing archive 文件 mtime == orig 的过去时间（reingest 从 orig 重建并保留 mtime）。
        let backing_file = paths.backing("demo", Backend::Shadow).join("a.jsonl");
        let got = std::fs::metadata(&backing_file)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(got, past, "reingest 后 backing mtime 应保留 orig 原始时间");

        // 旧 backing 留底于 .reingest-bak（不删，可恢复）。
        let bak = {
            let mut s = paths.backing("demo", Backend::Shadow).into_os_string();
            s.push(".reingest-bak");
            std::path::PathBuf::from(s)
        };
        assert!(bak.is_dir(), "旧 backing 应留底于 .reingest-bak");

        // 重新挂载 + meta 仍 committed（工具自写）。
        assert!(
            m.is_mounted(&paths.mountpoint("demo")),
            "reingest 后应已重挂"
        );
        assert!(
            discovery::read_meta(&paths.meta_path("demo"))
                .unwrap()
                .is_some_and(|mm| mm.committed),
            "meta 应仍 committed"
        );
    }

    #[test]
    fn reingest_rejects_without_orig() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"x");
        let m = FakeMounter::default();
        apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();
        // 移走 orig（模拟已 purge 源备份）→ reingest 无源，拒绝。
        std::fs::remove_dir_all(paths.orig("demo")).unwrap();
        let res = reingest(&paths, "demo", true, &m);
        assert!(res.is_err(), "无 orig 应拒绝 reingest");
        assert!(res.unwrap_err().to_string().contains("无源备份"));
    }

    #[test]
    fn apply_enables_and_restore_disables_autostart() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_project(&paths, "demo", "a.jsonl", b"data\n".repeat(100).as_slice());
        let m = FakeMounter::default();

        apply(&paths, "demo", ApplyOptions::default(), true, &m).unwrap();
        assert_eq!(
            &*m.autostart_enabled.lock().unwrap(),
            &["demo".to_string()],
            "apply 成功后应 enable_autostart(demo)"
        );

        restore(&paths, "demo", &m).unwrap();
        assert_eq!(
            &*m.autostart_disabled.lock().unwrap(),
            &["demo".to_string()],
            "restore 后应 disable_autostart(demo)"
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
        m.unmount("demo", &paths.mountpoint("demo"), UmountLevel::Auto)
            .unwrap();
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
