//! 分档卸载集成：真起 zipfs shadow 挂载，验证各档摘除（clean/lazy/auto）与 wedge 恢复
//! （SIGKILL daemon 后经 abort/auto 兜底）。见 docs/07-hangfree-umount.md。
//!
//! 无法挂载（无 /dev/fuse 或无 fusermount）优雅 SKIP，不 panic（对齐 tests/mount_rw.rs）。
//! 每个用例结束必清理：reap 子进程 + best-effort fusermount -uz 兜底摘除，避免留陈旧挂载
//! 污染下一个用例。真挂载须 `--test-threads=1`。

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use zipfs::enable::discovery::is_mounted;
use zipfs::enable::force_umount::{umount, UmountLevel};

fn zipfs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zipfs"))
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(bin))
        .find(|p| p.is_file())
}

fn skip_reason() -> Option<String> {
    if !Path::new("/dev/fuse").exists() {
        return Some("/dev/fuse 不存在".to_string());
    }
    if !["fusermount3", "fusermount"]
        .iter()
        .any(|b| which(b).is_some())
    {
        return Some("找不到 fusermount3/fusermount".to_string());
    }
    None
}

fn wait_mounted(mountpoint: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if is_mounted(mountpoint) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    is_mounted(mountpoint)
}

/// best-effort 强摘除（lazy/MNT_DETACH），供清理兜底；失败静默（用例可能已摘干净）。
fn force_detach(mountpoint: &Path) {
    for bin in ["fusermount3", "fusermount"] {
        if which(bin).is_some() {
            let _ = Command::new(bin)
                .arg("-u")
                .arg("-z")
                .arg(mountpoint)
                .status();
        }
    }
}

/// 起一个健康的 shadow 挂载。返回持有 daemon `Child`（其 pid 即前台 FUSE 守护）与挂载点的
/// guard，Drop 时保证 reap 子进程 + 兜底摘除，即便断言 panic 也不留陈旧挂载。
struct MountGuard {
    child: Option<Child>,
    mountpoint: PathBuf,
    _backing: tempfile::TempDir,
    _mountdir: tempfile::TempDir,
}

impl MountGuard {
    fn pid(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or(0)
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        // 兜底摘除仍在的挂载（用例已成功摘除则为 no-op）。
        if is_mounted(&self.mountpoint) {
            force_detach(&self.mountpoint);
        }
        // reap 守护子进程（无论其是否已被 SIGKILL / 因会话结束而自退）。
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 起一个健康 shadow 挂载并等就绪；起停范式照抄 mount_rw.rs。调用方须先 skip_reason() 门控。
fn mount_shadow() -> MountGuard {
    let backing = tempfile::tempdir().expect("backing tempdir");
    let mountdir = tempfile::tempdir().expect("mount tempdir");
    let mountpoint = mountdir.path().to_path_buf();

    let child = Command::new(zipfs_bin())
        .arg("--backend")
        .arg("shadow")
        .arg("--backing")
        .arg(backing.path())
        .arg("--mountpoint")
        .arg(&mountpoint)
        .arg("--chunk-size")
        .arg("65536") // MIN_CHUNK_SIZE=64KiB（core::mod 强制下限）；卸载测试与块大小无关，取最小合法值即可。
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("spawn zipfs mount");

    let guard = MountGuard {
        child: Some(child),
        mountpoint,
        _backing: backing,
        _mountdir: mountdir,
    };

    if !wait_mounted(&guard.mountpoint, Duration::from_secs(5)) {
        // Drop 会 reap 子进程 + 兜底摘除；这里直接 panic 让用例失败（skip 已在门控处判过）。
        panic!(
            "5s 内未观察到 shadow 挂载就绪（mp={}）",
            guard.mountpoint.display()
        );
    }
    guard
}

/// SIGKILL 指定 pid（daemon 前台进程），制造 wedge：守护死、陈旧挂载残留。
fn sigkill(pid: u32) {
    // SAFETY: 仅向本 harness 亲自 spawn 的子进程发信号；pid 由 Child::id() 提供。
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[test]
fn clean_level_unmounts_healthy_mount() {
    if let Some(r) = skip_reason() {
        eprintln!("[SKIP] clean_level_unmounts_healthy_mount：{r}");
        return;
    }
    let m = mount_shadow();
    assert!(is_mounted(&m.mountpoint), "起挂后应为已挂载");

    let report = umount(&m.mountpoint, UmountLevel::Clean).expect("clean 卸载");
    assert!(report.unmounted, "clean 应摘除健康挂载");
    assert!(!report.aborted, "clean 档不应写连接 abort");
    assert!(!is_mounted(&m.mountpoint), "卸载后应不再挂载");
}

#[test]
fn lazy_level_unmounts_healthy_mount() {
    if let Some(r) = skip_reason() {
        eprintln!("[SKIP] lazy_level_unmounts_healthy_mount：{r}");
        return;
    }
    let m = mount_shadow();
    assert!(is_mounted(&m.mountpoint), "起挂后应为已挂载");

    let report = umount(&m.mountpoint, UmountLevel::Lazy).expect("lazy 卸载");
    assert!(report.unmounted, "lazy 应摘除健康挂载");
    assert!(!is_mounted(&m.mountpoint), "卸载后应不再挂载");
}

#[test]
fn auto_stops_at_clean_for_healthy_mount() {
    if let Some(r) = skip_reason() {
        eprintln!("[SKIP] auto_stops_at_clean_for_healthy_mount：{r}");
        return;
    }
    let m = mount_shadow();
    assert!(is_mounted(&m.mountpoint), "起挂后应为已挂载");

    let report = umount(&m.mountpoint, UmountLevel::Auto).expect("auto 卸载");
    assert_eq!(
        report.level_reached,
        UmountLevel::Clean,
        "健康挂载 auto 应停在 clean（不应升级）"
    );
    assert!(!report.aborted, "健康挂载不应升级到 abort");
    assert!(!is_mounted(&m.mountpoint), "卸载后应不再挂载");
}

#[test]
fn explicit_abort_recovers_wedged_mount() {
    if let Some(r) = skip_reason() {
        eprintln!("[SKIP] explicit_abort_recovers_wedged_mount：{r}");
        return;
    }
    let m = mount_shadow();
    sigkill(m.pid()); // 守护死 → 留陈旧挂载。
    assert!(is_mounted(&m.mountpoint), "SIGKILL 守护后陈旧挂载应仍在");

    let report = umount(&m.mountpoint, UmountLevel::Abort).expect("abort 卸载");
    assert!(report.unmounted, "abort 应摘除 wedge 挂载");
    assert!(report.aborted, "显式 abort 档应写过连接 abort");
    assert!(!is_mounted(&m.mountpoint), "卸载后应不再挂载");
}

#[test]
fn auto_recovers_wedged_mount() {
    if let Some(r) = skip_reason() {
        eprintln!("[SKIP] auto_recovers_wedged_mount：{r}");
        return;
    }
    let m = mount_shadow();
    sigkill(m.pid());
    assert!(is_mounted(&m.mountpoint), "SIGKILL 守护后陈旧挂载应仍在");

    // daemon 已死（endpoint_ok=false），auto 守卫放行升级。哪一档摘除取决于内核/libfuse：
    // 部分内核（如 WSL2）下守护死后连 clean（fusermount -u）都能摘除陈旧挂载，故停在 Clean；
    // 另一些环境要 lazy（fusermount -uz，真实事故里生效的情形）甚至 abort 才摘。
    // 关键不变量是「wedge 最终被恢复」，故断言最终摘除，不锁定停在哪一档。
    let report = umount(&m.mountpoint, UmountLevel::Auto).expect("auto 卸载");
    assert!(report.unmounted, "auto 应恢复 wedge 挂载");
    assert!(!is_mounted(&m.mountpoint), "卸载后应不再挂载");
    assert!(
        matches!(
            report.level_reached,
            UmountLevel::Clean | UmountLevel::Lazy | UmountLevel::Abort
        ),
        "wedge 恢复应停在 clean/lazy/abort 其一，实得 {:?}",
        report.level_reached
    );
}
