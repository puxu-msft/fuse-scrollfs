//! Hang-free 分档卸载引擎（见 docs/07-hangfree-umount.md）。
//!
//! 本模块仅提供卸载原语（abort 连接 + 带超时兜底的 fusermount）；由后续任务
//! 的分档卸载引擎接线消费，故此处允许暂时未使用的 crate 内部条目。
#![allow(dead_code)]

use std::ffi::OsStr;
use std::time::Duration;

use std::io::ErrorKind;
use std::process::{Command, Stdio};

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
        let mp = Path::new("/nonexistent/zipfs/mp");
        let out = run_fusermount(&[OsStr::new("-u"), mp.as_os_str()], Duration::from_secs(2));
        assert!(matches!(out, CmdOutcome::Failed | CmdOutcome::NotFound));
    }
}
