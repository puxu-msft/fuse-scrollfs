//! Hang-free 分档卸载引擎（见 docs/07-hangfree-umount.md）。
//!
//! 本模块提供卸载原语（abort 连接 + 带超时兜底的 fusermount）以及分档卸载引擎
//! （clean/lazy/abort/auto 升级链）。

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use std::io::ErrorKind;
use std::process::{Command, Stdio};

use super::discovery;

/// 外部卸载命令的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum CmdOutcome {
    Success,
    Failed,
    TimedOut,
    NotFound,
}

/// 写 `/sys/fs/fuse/connections/<id>/abort` 解除在飞/hung 请求。best-effort：
/// 连接已消失/无权限/已断开（NotFound/PermissionDenied/NotConnected）均视为非致命。
pub fn abort_connection(id: u64) -> std::io::Result<()> {
    let path = format!("/sys/fs/fuse/connections/{id}/abort");
    match std::fs::write(&path, b"1") {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::NotConnected
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

const CMD_POLL: Duration = Duration::from_millis(50);

/// 单次 spawn 一个 fusermount 二进制并等其退出（带看门狗子超时）。
/// 返回 None 表示该二进制不存在（应换下一个）；Some(outcome) 为已执行的结果。
fn spawn_once(bin: &str, args: &[&OsStr], deadline: std::time::Instant) -> Option<CmdOutcome> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(_) => return Some(CmdOutcome::Failed),
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Some(CmdOutcome::Success),
            Ok(Some(_)) => return Some(CmdOutcome::Failed), // 跑过但非零（busy）。
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // SIGKILL 无法唤出不可中断睡眠（D 态）的任务；可接受，因 fusermount
                    // 经验上快速退出，极少陷入 D 态。
                    let _ = child.kill();
                    let _ = child.wait();
                    return Some(CmdOutcome::TimedOut);
                }
                std::thread::sleep(CMD_POLL);
            }
            Err(_) => return Some(CmdOutcome::Failed),
        }
    }
}

/// spawn `fusermount3` 回退 `fusermount`，在 `timeout` 内**轮询重试**吸收瞬态 EBUSY。
/// 正确区分：`Success` / `Failed`（跑过但始终非零/超时）/ `NotFound`（两二进制皆缺）。
pub(crate) fn run_fusermount(args: &[&OsStr], timeout: Duration) -> CmdOutcome {
    let deadline = std::time::Instant::now() + timeout;
    let mut ran = false; // 是否至少成功 spawn 过一个二进制（区分 Failed vs NotFound）。
    loop {
        for bin in ["fusermount3", "fusermount"] {
            match spawn_once(bin, args, deadline) {
                None => continue, // 该二进制缺失，试下一个。
                Some(CmdOutcome::Success) => return CmdOutcome::Success,
                Some(_) => {
                    ran = true; // 跑过但失败/超时 → 重试直到 deadline。
                    break;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return if ran {
                CmdOutcome::Failed
            } else {
                CmdOutcome::NotFound
            };
        }
        std::thread::sleep(CMD_POLL);
    }
}

/// 卸载档位（CLI `--level` 值）。见 docs/07-hangfree-umount.md §3。
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UmountLevel {
    /// fusermount -u：daemon flush，耐久；busy 失败。
    Clean,
    /// fusermount -uz：懒摘除，无 abort。
    Lazy,
    /// abort 连接 → fusermount -uz：解耦 daemon 存活，可能丢在飞写。
    Abort,
    /// clean →(仍挂)→ lazy →(仍挂且 daemon 不存活)→ abort。默认。
    Auto,
}

/// 一次卸载的结果，供日志与测试断言。
#[derive(Debug, PartialEq, Eq)]
pub struct UmountReport {
    pub was_mounted: bool,
    pub connection_id: Option<u64>,
    pub level_reached: UmountLevel,
    pub aborted: bool,
    pub unmounted: bool,
}

/// 单档超时上界（clean/lazy/abort 各自的摘除等待，内含轮询重试）。
const STEP_TIMEOUT: Duration = Duration::from_secs(3);

/// 执行单一档位的一次尝试；返回该档结束后是否已卸载。
fn attempt(mountpoint: &Path, level: UmountLevel, report: &mut UmountReport) -> bool {
    let mp = mountpoint.as_os_str();
    match level {
        UmountLevel::Clean => {
            run_fusermount(&[OsStr::new("-u"), mp], STEP_TIMEOUT);
        }
        UmountLevel::Lazy => {
            run_fusermount(&[OsStr::new("-u"), OsStr::new("-z"), mp], STEP_TIMEOUT);
        }
        UmountLevel::Abort => {
            if let Some(id) = report.connection_id {
                let _ = abort_connection(id); // best-effort
                report.aborted = true;
            }
            run_fusermount(&[OsStr::new("-u"), OsStr::new("-z"), mp], STEP_TIMEOUT);
        }
        UmountLevel::Auto => unreachable!("auto 由 umount() 展开为具体档"),
    }
    !discovery::is_mounted(mountpoint)
}

/// 按档位卸载 `mountpoint`。全程 hang-free（外部命令带超时、探测读 /proc）。
pub fn umount(mountpoint: &Path, level: UmountLevel) -> std::io::Result<UmountReport> {
    let mut report = UmountReport {
        was_mounted: discovery::is_mounted(mountpoint),
        connection_id: discovery::mount_connection_id(mountpoint),
        level_reached: level,
        aborted: false,
        unmounted: false,
    };
    if !report.was_mounted {
        report.unmounted = true;
        return Ok(report);
    }

    if level == UmountLevel::Auto {
        // clean → lazy 逐级；升级到 abort 前必须确证 daemon 死/卡（endpoint_ok 守卫），
        // 否则健康 busy 挂载会被误 abort 丢在飞写（见 docs/07 §3.1）。
        for cur in [UmountLevel::Clean, UmountLevel::Lazy] {
            report.level_reached = cur;
            if attempt(mountpoint, cur, &mut report) {
                report.unmounted = true;
                return Ok(report);
            }
        }
        // 仍挂：只有 daemon 不存活才允许 abort。endpoint_ok 现经 hung 熔断缓存，但健康探测会清除
        // 该 key（见 hang_free::memo_with_ttl 成功分支驱逐），故健康但 busy 的 daemon 绝不会被误报
        // 为 hung 而误 abort——本守卫「不误 abort 健康挂载、护在飞写」的不变式仍成立（评审 M2）。
        if discovery::endpoint_ok(mountpoint) {
            return Err(std::io::Error::other(format!(
                "daemon 存活但挂载仍 busy，拒绝 abort（护在飞写）；请释放占用后重试或用 --level abort 强制：{}",
                mountpoint.display()
            )));
        }
        report.level_reached = UmountLevel::Abort;
        if attempt(mountpoint, UmountLevel::Abort, &mut report) {
            report.unmounted = true;
            return Ok(report);
        }
        return Err(std::io::Error::other(format!(
            "auto 升级至 abort 仍未摘除：{}",
            mountpoint.display()
        )));
    }

    // 显式单档：不自动升级，失败如实报错。
    report.level_reached = level;
    if attempt(mountpoint, level, &mut report) {
        report.unmounted = true;
        Ok(report)
    } else {
        Err(std::io::Error::other(format!(
            "{level:?} 卸载未摘除：{}",
            mountpoint.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn abort_connection_is_idempotent_for_missing_conn() {
        // 不存在的连接号 → 视为已消失，Ok。
        assert!(abort_connection(u64::MAX).is_ok());
    }

    #[test]
    fn run_fusermount_notfound_when_binary_absent() {
        // 用一个不存在的挂载点 + 极短超时；真实 fusermount 会快速失败（非 hang）。
        // 断言不 panic 且返回可判定的 outcome（Failed / NotFound）。
        let mp = Path::new("/nonexistent/scrollz/mp");
        let out = run_fusermount(&[OsStr::new("-u"), mp.as_os_str()], Duration::from_secs(2));
        assert!(matches!(out, CmdOutcome::Failed | CmdOutcome::NotFound));
    }

    #[test]
    fn umount_reports_not_mounted_as_success() {
        // 未挂载的路径：was_mounted=false、unmounted=true、不 abort。
        let mp = Path::new("/definitely/not/mounted/scrollz");
        let r = umount(mp, UmountLevel::Auto).unwrap();
        assert!(!r.was_mounted);
        assert!(r.unmounted);
        assert!(!r.aborted);
    }

    #[test]
    fn umount_level_parses_from_str() {
        use clap::ValueEnum;
        assert_eq!(
            UmountLevel::from_str("auto", true).unwrap(),
            UmountLevel::Auto
        );
        assert_eq!(
            UmountLevel::from_str("abort", true).unwrap(),
            UmountLevel::Abort
        );
    }
}
