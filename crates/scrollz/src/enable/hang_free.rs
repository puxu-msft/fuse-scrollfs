//! Hang-free 原语：把可能在 wedge FUSE 上永久阻塞（D 睡眠）的调用包进带超时的工作线程。
//! wedged 挂载下 stat/canonicalize/opendir 会不可中断阻塞；本模块提供统一的超时逃逸，
//! 供 discovery 探测与 force_umount 卸载引擎共用。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 探测类操作（stat/canonicalize）的默认超时上界。超时即视为「不可达/卡死」。
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// hung 挂载熔断缓存的 TTL：命中未过期的 key 直接判卡死、跳过起线程。取 `PROBE_TIMEOUT` 量级
/// （非卸载的 3s STEP_TIMEOUT——两者无关；过长会放大「同路径 remount 健康 daemon」的误判窗口）。
pub(crate) const HUNG_TTL: Duration = Duration::from_secs(1);

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

/// 进程级 hung 挂载缓存：`挂载路径 → 上次判定卡死的时刻`。
fn hung_cache() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    static HUNG: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    HUNG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 带熔断缓存的 `with_timeout`：若 `key` 近期（`HUNG_TTL` 内）已判卡死，直接返回 `None` 跳过起线程，
/// 避免对同一 hung 挂载**反复起线程反复泄漏**。成功则清除 key（恢复），超时则记入 key。
///
/// 注意：**不消除**首次泄漏——首次超时仍泄漏一个线程；本包装把泄漏频率界定为「≤1 次/TTL/挂载」。
/// 仅用于键为**被探测路径本身**的探测（如 `endpoint_ok` 对挂载点 stat）；**不要**用于结果会回退
/// 到其它路径的操作（如 `canonicalized_target` 的父目录 canonicalize——None 会毒化 `is_mounted`）。
pub(crate) fn with_timeout_memo<T, F>(key: &Path, dur: Duration, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    memo_with_ttl(key, dur, HUNG_TTL, f)
}

/// `with_timeout_memo` 的 TTL 可注入版（供测试用短 TTL，避免等满 `HUNG_TTL`）。
fn memo_with_ttl<T, F>(key: &Path, dur: Duration, ttl: Duration, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    // 锁仅护 HashMap 的 get/insert/remove（皆不 panic）；被探测闭包 `f` 在 with_timeout 的**独立
    // 线程**里跑、不持本锁，故其 panic 不会毒化本锁——`unwrap` 实不可达（评审 M3）。
    // 命中且未过 TTL → 直接判卡死，跳过起线程。
    if let Some(&t) = hung_cache().lock().unwrap().get(key) {
        if t.elapsed() < ttl {
            return None;
        }
    }
    match with_timeout(dur, f) {
        Some(v) => {
            hung_cache().lock().unwrap().remove(key); // 恢复：清除卡死标记
            Some(v)
        }
        None => {
            let mut cache = hung_cache().lock().unwrap();
            // 顺带清理过期条目，把 map 规模界定为「一个 TTL 窗口内探测过的挂载」，避免长驻 TUI
            // 反复 scan 时未再探测的旧 key 无界堆积（评审 M1）。
            cache.retain(|_, t| t.elapsed() < ttl);
            cache.insert(key.to_path_buf(), Instant::now());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

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

    // ── 阶段 B：hung 挂载熔断缓存 ──────────────────────────────────────────

    #[test]
    fn memo_skips_closure_on_cached_hung_within_ttl() {
        // 第一次探测超时 → 记入缓存；TTL 内第二次同 key 不再执行闭包（杜绝重复起线程/泄漏）。
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("mnt-a"); // 进程级 static 缓存，用唯一路径避免测试间串扰
        let calls = Arc::new(AtomicUsize::new(0));

        let c1 = calls.clone();
        let r1 = with_timeout_memo(&key, Duration::from_millis(20), move || {
            c1.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200)); // 超过 dur → 超时
            1u32
        });
        assert_eq!(r1, None, "首次超时应返回 None");
        std::thread::sleep(Duration::from_millis(60)); // 确保首次闭包已 +1 且入缓存
        let after_first = calls.load(Ordering::SeqCst);
        assert_eq!(after_first, 1, "首次闭包应已执行一次");

        let c2 = calls.clone();
        let r2 = with_timeout_memo(&key, Duration::from_millis(20), move || {
            c2.fetch_add(1, Ordering::SeqCst);
            2u32
        });
        assert_eq!(r2, None, "TTL 内命中缓存应直接返回 None");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            after_first,
            "TTL 内不应再执行闭包"
        );
    }

    #[test]
    fn memo_reexecutes_after_ttl_expiry() {
        // TTL 过期后同 key 应重新探测（用短 TTL 内部版，避免等满 1s 常量 HUNG_TTL）。
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("mnt-b");
        let calls = Arc::new(AtomicUsize::new(0));
        let ttl = Duration::from_millis(50);

        let c1 = calls.clone();
        let r1 = memo_with_ttl(&key, Duration::from_millis(20), ttl, move || {
            c1.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            1u32
        });
        assert_eq!(r1, None);
        std::thread::sleep(Duration::from_millis(80)); // 闭包已 +1 且 TTL 已过
        let n1 = calls.load(Ordering::SeqCst);

        let c2 = calls.clone();
        let r2 = memo_with_ttl(&key, Duration::from_millis(500), ttl, move || {
            c2.fetch_add(1, Ordering::SeqCst);
            7u32
        });
        assert_eq!(r2, Some(7), "TTL 过期后应重跑并拿到结果");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            n1 + 1,
            "TTL 过期后应重新执行闭包"
        );
    }

    #[test]
    fn memo_does_not_cache_on_success() {
        // 闭包成功（未超时）时不入缓存/清除 key：连续两次都应执行闭包。
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("mnt-c");
        let calls = Arc::new(AtomicUsize::new(0));

        for expect in [1u32, 2u32] {
            let c = calls.clone();
            let r = with_timeout_memo(&key, Duration::from_millis(500), move || {
                let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                n as u32
            });
            assert_eq!(r, Some(expect), "成功路径每次都应执行闭包");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "成功不缓存，两次都应跑");
    }
}
