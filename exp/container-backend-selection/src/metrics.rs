//! 测量工具：吞吐与延迟分位（p50/p99）。

use hdrhistogram::Histogram;
use std::time::Duration;

/// 一次场景运行的结果汇总。
#[derive(Clone, Debug)]
pub struct Stats {
    /// 操作数（如插入的块数、RMW 次数）。
    pub ops: u64,
    /// 移动的有效字节数（用于带宽换算）。
    pub bytes: u64,
    /// 总墙钟耗时。
    pub elapsed: Duration,
    /// 每操作延迟分位（纳秒），可选（批量插入不一定逐操作记录）。
    pub p50_ns: Option<u64>,
    pub p99_ns: Option<u64>,
    pub p999_ns: Option<u64>,
    pub max_ns: Option<u64>,
}

impl Stats {
    /// 操作吞吐（ops/s）。
    pub fn ops_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.ops as f64 / secs
        }
    }

    /// 数据吞吐（MiB/s），按有效 blob 字节计。
    pub fn mib_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            (self.bytes as f64 / (1024.0 * 1024.0)) / secs
        }
    }
}

/// 逐操作延迟记录器。记录纳秒延迟，输出分位。
pub struct LatencyRecorder {
    hist: Histogram<u64>,
}

impl LatencyRecorder {
    pub fn new() -> Self {
        // 1ns..600s 范围，3 位有效数字精度，足够覆盖单 blob RMW 延迟。
        LatencyRecorder {
            hist: Histogram::new_with_bounds(1, 600_000_000_000, 3)
                .expect("histogram 边界合法"),
        }
    }

    #[inline]
    pub fn record_ns(&mut self, ns: u64) {
        // 饱和记录：超界值钳到上界，避免丢样本导致分位失真。
        let clamped = ns.clamp(1, 600_000_000_000);
        self.hist
            .record(clamped)
            .expect("钳位后必在合法范围内");
    }

    pub fn p50(&self) -> u64 {
        self.hist.value_at_quantile(0.50)
    }
    pub fn p99(&self) -> u64 {
        self.hist.value_at_quantile(0.99)
    }
    pub fn p999(&self) -> u64 {
        self.hist.value_at_quantile(0.999)
    }
    pub fn max(&self) -> u64 {
        self.hist.max()
    }
}

impl Default for LatencyRecorder {
    fn default() -> Self {
        Self::new()
    }
}

/// 人类可读的字节数。
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.2} {}", UNITS[u])
}

/// 纳秒转毫秒字符串。
pub fn ns_to_ms(ns: u64) -> String {
    format!("{:.3}", ns as f64 / 1_000_000.0)
}
