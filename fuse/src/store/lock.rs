//! 跨进程文件锁原语（advisory flock）。
//!
//! Bug A 根因：shadow backing 打开时无任何并发互斥，两个守护（尤其孤儿守护 + 新守护）
//! 能同时持有同一目录树，孤儿用启动时的空内存视图周期性覆盖、清空刚 ingest 的数据。
//! 用一把 `flock(LOCK_EX|LOCK_NB)` 排他锁把住 backing 的打开路径即可堵死该机制。
//!
//! flock 选型理由：
//! - **advisory + per-OFD**：每次独立 `open` 得到独立 OFD，同进程不同 open 也会互斥，
//!   故单进程内即可测出冲突（无需起第二个进程）。
//! - **内核自动释放**：进程退出（含被 SIGKILL）时内核释放锁，正好解决「僵尸守护被 kill
//!   后锁不残留」——不会留下死锁文件挡住后续合法 open。
//! - 仅约束走 open 路径的 zipfs 守护互相（足够);非 zipfs 进程不受 advisory 锁约束（可接受）。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// 在 `path` 上取一把非阻塞排他 flock，成功则返回持锁的 `File`（drop 即释放）。
///
/// 锁文件按需 create（不存在则建空文件）。已被他人持有时返回
/// `io::ErrorKind::WouldBlock`，调用方据此报「backing 已被另一守护持有」。
pub(crate) fn acquire_exclusive(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    // SAFETY: `file` 在本次调用内有效，其 fd 在 flock 期间不被关闭；flock 对任意有效 fd
    // 安全，仅设置该 OFD 上的 advisory 锁，不读写用户内存。
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        // EWOULDBLOCK（=EAGAIN）→ 已被持有，归一成 WouldBlock 便于上层识别。
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("锁已被持有：{}", path.display()),
            ));
        }
        return Err(err);
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_on_same_path_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("x.lock");
        let h1 = acquire_exclusive(&lock).unwrap();
        let h2 = acquire_exclusive(&lock);
        assert!(
            matches!(
                h2.as_ref().map_err(|e| e.kind()),
                Err(io::ErrorKind::WouldBlock)
            ),
            "第二次取同一锁应得 WouldBlock，实际：{h2:?}"
        );
        drop(h1);
        let h3 = acquire_exclusive(&lock);
        assert!(h3.is_ok(), "释放后应可重新取锁");
    }
}
