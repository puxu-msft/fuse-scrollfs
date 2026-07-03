//! Hang-free 原语：把可能在 wedge FUSE 上永久阻塞（D 睡眠）的调用包进带超时的工作线程。
//! wedged 挂载下 stat/canonicalize/opendir 会不可中断阻塞；本模块提供统一的超时逃逸，
//! 供 discovery 探测与 force_umount 卸载引擎共用。

use std::sync::mpsc;
use std::time::Duration;

/// 探测类操作（stat/canonicalize）的默认超时上界。超时即视为「不可达/卡死」。
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// 在独立线程运行 `f`，最多等 `dur`。超时返回 `None`。
///
/// 取舍：超时时工作线程可能仍卡在 D 睡眠里无法回收（线程泄漏），这是刻意的——
/// 宁可泄漏一个短命进程里的线程，也绝不让主线程被 hung FUSE 永久拖住。
pub(crate) fn with_timeout<T, F>(dur: Duration, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(dur).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_timeout_returns_none_when_closure_exceeds_deadline() {
        let got = with_timeout(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(5));
            42u32
        });
        assert_eq!(got, None, "超时应返回 None");
    }

    #[test]
    fn with_timeout_returns_some_when_closure_finishes_in_time() {
        assert_eq!(with_timeout(Duration::from_secs(5), || 7u32), Some(7));
    }
}
