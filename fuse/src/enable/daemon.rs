//! 挂载守护编排：`Mounter` trait + 真实实现（re-exec 自身为 detached 守护）。
//!
//! 把挂载抽象成 trait，使 `lifecycle` 的 apply/restore/remount 可注入 `FakeMounter` 单测，
//! 而无需 `/dev/fuse`。真实守护沿用被验证过的 mount server：以现有 mount flags re-exec 自身
//! （notifier/sd-notify/metrics 全保留），`setsid` 脱离 TUI 进程组、随父退出而 reparent 存活。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::enable::discovery;
use crate::enable::force_umount::{self, UmountLevel};

/// 挂载一个守护所需参数。
#[derive(Debug, Clone)]
pub struct MountSpec {
    /// project 原始名（如 `-home-xp-src-neighbors`）。`SystemdMounter` 据此拼实例名；
    /// `RealMounter` 忽略（其 argv 不含 name，靠 backing/mountpoint 即可）。
    pub name: String,
    pub backend: crate::enable::model::Backend,
    pub backing: PathBuf,
    pub mountpoint: PathBuf,
    pub chunk_size: u32,
    pub level: i32,
    pub pid_file: PathBuf,
    pub dict: Option<PathBuf>,
    pub threads: usize,
    pub writeback: bool,
    pub max_write: u32,
    pub no_tail_buffer: bool,
    pub allow_other: bool,
    pub auto_unmount: bool,
    pub metrics_file: Option<PathBuf>,
    /// 解压块缓存字节上限（perf #1），见 `MountArgs::block_cache_bytes`。默认 `DEFAULT_CACHE_BYTES`。
    pub block_cache_bytes: usize,
}

/// 挂载操作抽象（可注入测试）。
pub trait Mounter {
    /// 启动 detached 守护并等待挂载就绪；超时/早死 → Err（不留半挂状态）。
    fn spawn(&self, spec: &MountSpec) -> std::io::Result<()>;
    /// 卸载挂载点，全程 hang-free（经 `force_umount` 引擎）。`level` 定卸载档：
    /// 改写 backing 的维护操作（compact/seal/reingest）须传 `Clean`（要求守护干净退出，
    /// 决不 lazy/abort 一个仍在写 backing 的活守护 → 防损坏）；清理/还原类传 `Auto`
    /// （wedge 也能摘除，且不改写 backing 故 lazy/abort 无损）。`name` 供 systemd 实现
    /// `systemctl stop` 用，`RealMounter`/`FakeMounter` 忽略（直接卸 mountpoint）。
    fn unmount(&self, name: &str, mountpoint: &Path, level: UmountLevel) -> std::io::Result<()>;
    /// 是否为活的 zipfs 挂载点。
    fn is_mounted(&self, mountpoint: &Path) -> bool;

    /// 注册项目自启（apply 成功后调用）。default no-op：`RealMounter`/`FakeMounter` 无 systemd
    /// 自启概念。`SystemdMounter` 覆盖为 `systemctl --user enable zipfs@<esc>`，使 apply 的项目
    /// 重启后自动重挂。best-effort：失败不应回滚已成功的挂载（调用方忽略错误）。
    fn enable_autostart(&self, _name: &str) -> std::io::Result<()> {
        Ok(())
    }

    /// 注销项目自启（restore/purge 时调用）。default no-op；`SystemdMounter` 覆盖为
    /// `systemctl --user disable zipfs@<esc>`。
    fn disable_autostart(&self, _name: &str) -> std::io::Result<()> {
        Ok(())
    }
}

/// 就绪/卸载轮询步进与上限。
const POLL_STEP: Duration = Duration::from_millis(100);
const POLL_MAX: u32 = 50; // 5s

/// 真实挂载器：re-exec 当前二进制为 detached 守护，fusermount3 卸载。
pub struct RealMounter;

impl Mounter for RealMounter {
    fn spawn(&self, spec: &MountSpec) -> std::io::Result<()> {
        // 删除任何 stale pid 文件，避免读到陈旧 pid（评审 H3）。
        let _ = std::fs::remove_file(&spec.pid_file);

        let exe = std::env::current_exe()?;
        let mut cmd = Command::new(exe);
        cmd.args(mount_argv(spec))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // pre_exec 只做 setsid（async-signal-safe），脱离父进程组/控制终端 → 父退出不连带杀守护。
        // setsid 失败（极罕见，如已是组长）则让 spawn 整体失败，绝不产出「未脱离、随父死」的半守护。
        // SAFETY: 仅调用 setsid()，不分配、不取锁，满足 fork 后 exec 前的 async-signal-safe 约束。
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let pid = child.id();

        // 轮询就绪：挂载条目在 + endpoint 可用 + 守护仍活（try_wait 顺带 reap，杜绝僵尸）。
        for _ in 0..POLL_MAX {
            if let Some(status) = child.try_wait()? {
                return Err(std::io::Error::other(format!(
                    "挂载守护提前退出（{status}）：{}",
                    spec.mountpoint.display()
                )));
            }
            if discovery::is_mounted(&spec.mountpoint) && discovery::endpoint_ok(&spec.mountpoint) {
                return Ok(()); // 守护 detached，child 句柄随 spawn() 返回即丢弃，父退出后 reparent。
            }
            std::thread::sleep(POLL_STEP);
        }
        // 超时：SIGTERM 让守护干净卸载（pid 即我们刚 spawn 的进程，无需再校验来源），再 reap。
        // SAFETY: 向自己刚 spawn 的子进程发 SIGTERM。
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        let _ = child.wait();
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("挂载超时 5s：{}", spec.mountpoint.display()),
        ))
    }

    fn unmount(&self, _name: &str, mountpoint: &Path, level: UmountLevel) -> std::io::Result<()> {
        // hang-free 分档引擎：外部 fusermount 带超时重试、wedge 可 abort（见 force_umount）。
        force_umount::umount(mountpoint, level).map(|_| ())
    }

    fn is_mounted(&self, mountpoint: &Path) -> bool {
        discovery::is_mounted(mountpoint)
    }
}

/// 构造 re-exec 守护的 argv（与 `run_mount` 的 flag 名一致）。
fn mount_argv(spec: &MountSpec) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mut v = vec![
        OsString::from("--backend"),
        OsString::from(spec.backend.flag()),
        OsString::from("--backing"),
        spec.backing.clone().into_os_string(),
        OsString::from("--mountpoint"),
        spec.mountpoint.clone().into_os_string(),
        OsString::from("--chunk-size"),
        OsString::from(spec.chunk_size.to_string()),
        OsString::from("--level"),
        OsString::from(spec.level.to_string()),
        OsString::from("--pid-file"),
        spec.pid_file.clone().into_os_string(),
    ];
    if let Some(dict) = &spec.dict {
        v.push(OsString::from("--dict"));
        v.push(dict.clone().into_os_string());
    }
    if spec.threads > 0 {
        v.push(OsString::from("--threads"));
        v.push(OsString::from(spec.threads.to_string()));
    }
    if spec.writeback {
        v.push(OsString::from("--writeback"));
    }
    if spec.max_write > 0 {
        v.push(OsString::from("--max-write"));
        v.push(OsString::from(spec.max_write.to_string()));
    }
    if spec.no_tail_buffer {
        v.push(OsString::from("--no-tail-buffer"));
    }
    if spec.allow_other {
        v.push(OsString::from("--allow-other"));
    }
    if spec.auto_unmount {
        v.push(OsString::from("--auto-unmount"));
    }
    if let Some(mf) = &spec.metrics_file {
        v.push(OsString::from("--metrics-file"));
        v.push(mf.clone().into_os_string());
    }
    // 仅在非默认时传递（默认由 CLI default_value_t 兜底，保持 argv 最小）。
    if spec.block_cache_bytes != crate::core::blockcache::DEFAULT_CACHE_BYTES {
        v.push(OsString::from("--block-cache-bytes"));
        v.push(OsString::from(spec.block_cache_bytes.to_string()));
    }
    v
}

#[cfg(test)]
pub(crate) mod fake {
    //! 测试用假挂载器：以 backing 内 marker 文件模拟「已挂载」，无 FUSE。
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// 记录被「挂载」的挂载点路径。spawn 把 mountpoint 加入集合，unmount 移除。
    #[derive(Default)]
    pub(crate) struct FakeMounter {
        pub mounted: Mutex<HashSet<PathBuf>>,
        /// 若为 true，spawn 直接失败（模拟挂载失败路径）。
        pub fail_spawn: bool,
        /// 记录 enable_autostart 被调用的项目名（验证 apply 成功后注册自启）。
        pub autostart_enabled: Mutex<Vec<String>>,
        /// 记录 disable_autostart 被调用的项目名（验证 restore/purge 注销自启）。
        pub autostart_disabled: Mutex<Vec<String>>,
        /// 记录 unmount 的调用参数（name, level）：验证维护操作传 Clean、清理/还原传 Auto。
        pub unmount_calls: Mutex<Vec<(String, UmountLevel)>>,
    }

    impl Mounter for FakeMounter {
        fn spawn(&self, spec: &MountSpec) -> std::io::Result<()> {
            if self.fail_spawn {
                return Err(std::io::Error::other("fake spawn 失败"));
            }
            // 写一个 pid 文件以贴近真实（lifecycle 退出时会清理）。
            let _ = std::fs::write(&spec.pid_file, "0\n");
            self.mounted.lock().unwrap().insert(spec.mountpoint.clone());
            Ok(())
        }
        fn unmount(&self, name: &str, mountpoint: &Path, level: UmountLevel) -> std::io::Result<()> {
            self.unmount_calls
                .lock()
                .unwrap()
                .push((name.to_string(), level));
            self.mounted.lock().unwrap().remove(mountpoint);
            Ok(())
        }
        fn is_mounted(&self, mountpoint: &Path) -> bool {
            self.mounted.lock().unwrap().contains(mountpoint)
        }
        fn enable_autostart(&self, name: &str) -> std::io::Result<()> {
            self.autostart_enabled
                .lock()
                .unwrap()
                .push(name.to_string());
            Ok(())
        }
        fn disable_autostart(&self, name: &str) -> std::io::Result<()> {
            self.autostart_disabled
                .lock()
                .unwrap()
                .push(name.to_string());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_argv_uses_shadow_and_all_flags() {
        let spec = MountSpec {
            name: "demo".to_string(),
            backend: crate::enable::model::Backend::Shadow,
            backing: PathBuf::from("/b"),
            mountpoint: PathBuf::from("/m"),
            chunk_size: 1048576,
            level: 3,
            pid_file: PathBuf::from("/m.pid"),
            dict: None,
            threads: 0,
            writeback: false,
            max_write: 0,
            no_tail_buffer: false,
            allow_other: false,
            auto_unmount: false,
            metrics_file: None,
            block_cache_bytes: crate::core::blockcache::DEFAULT_CACHE_BYTES,
        };
        let argv: Vec<String> = mount_argv(&spec)
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "--backend",
                "shadow",
                "--backing",
                "/b",
                "--mountpoint",
                "/m",
                "--chunk-size",
                "1048576",
                "--level",
                "3",
                "--pid-file",
                "/m.pid",
            ]
        );
    }

    #[test]
    fn mount_argv_container_and_optional_flags() {
        let spec = MountSpec {
            name: "demo".to_string(),
            backend: crate::enable::model::Backend::Container,
            backing: PathBuf::from("/b.redb"),
            mountpoint: PathBuf::from("/m"),
            chunk_size: 65536,
            level: 19,
            pid_file: PathBuf::from("/m.pid"),
            dict: Some(PathBuf::from("/d.dict")),
            threads: 8,
            writeback: true,
            max_write: 4194304,
            no_tail_buffer: true,
            allow_other: true,
            auto_unmount: true,
            metrics_file: Some(PathBuf::from("/z.prom")),
            block_cache_bytes: crate::core::blockcache::DEFAULT_CACHE_BYTES,
        };
        let argv: Vec<String> = mount_argv(&spec)
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(argv.windows(2).any(|w| w == ["--backend", "container"]));
        assert!(argv.windows(2).any(|w| w == ["--dict", "/d.dict"]));
        assert!(argv.windows(2).any(|w| w == ["--threads", "8"]));
        assert!(argv.contains(&"--writeback".to_string()));
        assert!(argv.windows(2).any(|w| w == ["--max-write", "4194304"]));
        assert!(argv.contains(&"--no-tail-buffer".to_string()));
        assert!(argv.contains(&"--allow-other".to_string()));
        assert!(argv.contains(&"--auto-unmount".to_string()));
        assert!(argv.windows(2).any(|w| w == ["--metrics-file", "/z.prom"]));
    }
}
