//! 统一指标注册表：全 crate 单一 `Arc<Metrics>`，各子系统经类型化方法自增
//! （热路径无锁 `Relaxed`），单一 Prometheus text 序列化出口。零依赖。
//!
//! 设计动机：取代「新增一个指标要改 3 处硬编码」的耦合。加一个指标需要三步——加一个原子
//! 字段、加一个 `record_*`/`observe_*` 方法、在 [`Metrics::write_prometheus`] 里加一行 `emit`。
//! 各埋点只调类型化方法，序列化格式集中在一处，互不牵连。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// FUSE 操作延迟直方图桶上界（微秒）。覆盖亚毫秒到百毫秒（FUSE 操作典型区间）。
const LATENCY_BUCKETS_US: [u64; 11] = [
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000,
];

/// 手写 Prometheus histogram（零依赖）。桶计数非累积存储，序列化时累加成累积桶
/// `_bucket{le="X"}`（观测值 ≤ X 的累计数）+ `_sum`（秒）+ `_count`。热路径无锁 `Relaxed`。
#[derive(Debug, Default)]
pub struct Histogram {
    /// 每桶计数（非累积；序列化时累加成累积桶）。长度 = LATENCY_BUCKETS_US.len()。
    buckets: [AtomicU64; 11],
    /// 落在最后一个有限桶之上（即 > 100ms，对应 +Inf 桶相对最后有限桶的增量）。
    overflow: AtomicU64,
    /// 观测值总和（微秒，整数累加避免浮点原子）。序列化为秒。
    sum_us: AtomicU64,
    /// 观测总数。
    count: AtomicU64,
}

impl Histogram {
    /// 观测一次延迟（微秒）。找第一个 `us <= 桶上界` 的桶 +1；都不满足则 overflow +1。
    /// 边界语义：`us` 恰等于某桶上界时落入该桶（Prometheus `le` = less-or-equal）。
    #[inline]
    pub fn observe_us(&self, us: u64) {
        let mut placed = false;
        for (i, &b) in LATENCY_BUCKETS_US.iter().enumerate() {
            if us <= b {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
                placed = true;
                break;
            }
        }
        if !placed {
            self.overflow.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// 序列化为 Prometheus histogram text（`name` 不带后缀，本函数补 `_bucket`/`_sum`/`_count`）。
    ///
    /// 累积不变量：`cum` 从 0 起，按桶顺序累加每桶计数后 emit `_bucket{le=...}`；
    /// 最后累加 `overflow` emit `_bucket{le="+Inf"}`。由于每个有限观测都恰好 +1 到某个有限桶、
    /// 每个 overflow 观测都 +1 到 overflow，且 `_count` 同步 +1，故 `+Inf` 桶累计
    /// （= Σ 有限桶 + overflow = 全部观测）**恒等于** `_count`。这是本实现保证 `+Inf == _count` 的根据。
    fn write_prometheus(&self, out: &mut String, name: &str, help: &str) {
        use std::fmt::Write;
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} histogram");
        let mut cum: u64 = 0;
        for (i, &b) in LATENCY_BUCKETS_US.iter().enumerate() {
            cum += self.buckets[i].load(Ordering::Relaxed);
            // le 值用秒：桶上界(us) / 1e6，如 50us→0.00005、100000us→0.1。
            let le_s = b as f64 / 1_000_000.0;
            let _ = writeln!(out, "{name}_bucket{{le=\"{le_s}\"}} {cum}");
        }
        cum += self.overflow.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {cum}");
        let sum_s = self.sum_us.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let _ = writeln!(out, "{name}_sum {sum_s}");
        let _ = writeln!(out, "{name}_count {}", self.count.load(Ordering::Relaxed));
    }
}

/// 全 crate 共享的指标注册表。计数用 `Relaxed` 原子：指标是纯观测量，
/// 不参与任何 happens-before 约束，热路径不该为它付内存序开销。
#[derive(Debug, Default)]
pub struct Metrics {
    /// counter：container 提交（`commit_pending` flush 到 redb）成功次数。
    commit_ok: AtomicU64,
    /// counter：提交失败并合并回 active（避免数据丢失）的次数。
    commit_failed: AtomicU64,
    /// counter：累计落后端（redb）的块数。
    blocks_flushed: AtomicU64,
    /// gauge：flushing 缓冲字节峰值（`fetch_max` 单调抬高）。
    flushing_bytes_peak: AtomicU64,
    /// counter：FUSE read 回调成功次数。
    fuse_read_ops: AtomicU64,
    /// counter：FUSE read 累计返回字节数。
    fuse_read_bytes: AtomicU64,
    /// counter：FUSE write 回调成功次数。
    fuse_write_ops: AtomicU64,
    /// counter：FUSE write 累计写入字节数。
    fuse_write_bytes: AtomicU64,
    /// counter：FUSE fsync+flush 同步屏障操作成功次数。
    fuse_fsync_ops: AtomicU64,
    /// counter：FUSE read/write/fsync/flush 返回错误次数。
    fuse_errors: AtomicU64,
    /// counter：read_range 内部块查 block_cache 命中（免整块解压）次数。
    blockcache_hits: AtomicU64,
    /// counter：read_range 内部块查 block_cache 未命中（走 Store + 解压）次数。
    blockcache_misses: AtomicU64,
    /// counter：尾块封块/重压落后端次数（wsession seal/materialize 每次把尾块压缩落 Store 记一次）。
    seals: AtomicU64,
    /// counter：ShadowStore（布局 S）提交一个脏会话（commit_session 经 ArchiveUpdater 落 archive）次数。
    shadow_commits: AtomicU64,
    /// counter：ShadowStore ArchiveReader 缓存命中（复用已解析 reader，免重解析 footer/index）次数。
    shadow_reader_hits: AtomicU64,
    /// counter：ShadowStore ArchiveReader 缓存未命中（打开并解析新 reader）次数。
    shadow_reader_misses: AtomicU64,
    /// counter：ShadowStore 尾日志增量追加（append_tail）次数。
    shadow_tail_appends: AtomicU64,
    /// histogram：FUSE read handler 端到端延迟（微秒观测，序列化为秒）。
    read_latency: Histogram,
    /// histogram：FUSE write handler 端到端延迟。
    write_latency: Histogram,
    /// histogram：FUSE fsync/flush handler 端到端延迟。
    fsync_latency: Histogram,
}

impl Metrics {
    /// 建一个共享注册表。全 crate 传同一个 `Arc` clone。
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 记一次提交成功，并累加本次落盘的块数。
    #[inline]
    pub fn record_commit_ok(&self, blocks: u64) {
        self.commit_ok.fetch_add(1, Ordering::Relaxed);
        self.blocks_flushed.fetch_add(blocks, Ordering::Relaxed);
    }

    /// 记一次提交失败（内容已合并回 active，等待下次 fsync 重试，数据不丢）。
    #[inline]
    pub fn record_commit_failed(&self) {
        self.commit_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// 观测一次 flushing 缓冲字节数，单调抬高峰值 gauge。
    #[inline]
    pub fn observe_flushing_bytes(&self, bytes: u64) {
        self.flushing_bytes_peak.fetch_max(bytes, Ordering::Relaxed);
    }

    /// 记一次 FUSE read 成功，并累加本次返回字节数。
    #[inline]
    pub fn record_read(&self, bytes: u64) {
        self.fuse_read_ops.fetch_add(1, Ordering::Relaxed);
        self.fuse_read_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 记一次 FUSE write 成功，并累加本次写入字节数。
    #[inline]
    pub fn record_write(&self, bytes: u64) {
        self.fuse_write_ops.fetch_add(1, Ordering::Relaxed);
        self.fuse_write_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 记一次 FUSE 同步屏障操作（fsync 与 flush 都调它，语义等价）。
    #[inline]
    pub fn record_fsync(&self) {
        self.fuse_fsync_ops.fetch_add(1, Ordering::Relaxed);
    }

    /// 记一次 FUSE read/write/fsync/flush 返回错误。
    #[inline]
    pub fn record_fuse_error(&self) {
        self.fuse_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// 记一次 block_cache 命中（read_range 内部块免整块解压）。
    #[inline]
    pub fn record_cache_hit(&self) {
        self.blockcache_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// 记一次 block_cache 未命中（read_range 内部块走 Store + 解压）。
    #[inline]
    pub fn record_cache_miss(&self) {
        self.blockcache_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// 记一次尾块封块（把一个尾块压缩并落 Store）。wsession 的 seal/materialize 无尾日志路径调它。
    #[inline]
    pub fn record_seal(&self) {
        self.seals.fetch_add(1, Ordering::Relaxed);
    }

    /// 读累计封块次数（供 [`crate::core::wsession`] 的 `seal_count()` 委托，保其 API 与语义不变）。
    pub fn seals(&self) -> u64 {
        self.seals.load(Ordering::Relaxed)
    }

    /// 记一次 ShadowStore 脏会话提交（commit_session 经 ArchiveUpdater 落 archive）。
    #[inline]
    pub fn record_shadow_commit(&self) {
        self.shadow_commits.fetch_add(1, Ordering::Relaxed);
    }

    /// 记一次 ShadowStore ArchiveReader 缓存命中（复用已解析 reader）。
    #[inline]
    pub fn record_reader_hit(&self) {
        self.shadow_reader_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// 记一次 ShadowStore ArchiveReader 缓存未命中（打开并解析新 reader）。
    #[inline]
    pub fn record_reader_miss(&self) {
        self.shadow_reader_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// 记一次 ShadowStore 尾日志增量追加（append_tail）。
    #[inline]
    pub fn record_tail_append(&self) {
        self.shadow_tail_appends.fetch_add(1, Ordering::Relaxed);
    }

    /// 观测一次 FUSE read handler 延迟（微秒）。转发到 read 延迟直方图。
    #[inline]
    pub fn observe_read_latency_us(&self, us: u64) {
        self.read_latency.observe_us(us);
    }

    /// 观测一次 FUSE write handler 延迟（微秒）。转发到 write 延迟直方图。
    #[inline]
    pub fn observe_write_latency_us(&self, us: u64) {
        self.write_latency.observe_us(us);
    }

    /// 观测一次 FUSE fsync/flush handler 延迟（微秒）。转发到 fsync 延迟直方图。
    #[inline]
    pub fn observe_fsync_latency_us(&self, us: u64) {
        self.fsync_latency.observe_us(us);
    }

    /// 序列化为 Prometheus text 追加进 `out`。
    /// **新增指标只在此加一行**（配合上面加字段 + 方法）。
    pub fn write_prometheus(&self, out: &mut String) {
        use std::fmt::Write;
        fn emit(out: &mut String, name: &str, typ: &str, help: &str, val: u64) {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} {typ}\n{name} {val}"
            );
        }
        emit(
            out,
            "scrollz_commit_ok_total",
            "counter",
            "container 提交成功次数",
            self.commit_ok.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_commit_failed_total",
            "counter",
            "提交失败并合并回 active（避免数据丢失）次数",
            self.commit_failed.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_blocks_flushed_total",
            "counter",
            "累计落后端的块数",
            self.blocks_flushed.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_flushing_bytes_peak",
            "gauge",
            "flushing 缓冲字节峰值",
            self.flushing_bytes_peak.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_fuse_read_ops_total",
            "counter",
            "FUSE read 成功次数",
            self.fuse_read_ops.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_fuse_read_bytes_total",
            "counter",
            "FUSE read 累计返回字节数",
            self.fuse_read_bytes.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_fuse_write_ops_total",
            "counter",
            "FUSE write 成功次数",
            self.fuse_write_ops.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_fuse_write_bytes_total",
            "counter",
            "FUSE write 累计写入字节数",
            self.fuse_write_bytes.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_fuse_fsync_ops_total",
            "counter",
            "fsync+flush 同步操作次数",
            self.fuse_fsync_ops.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_fuse_errors_total",
            "counter",
            "read/write/fsync/flush 返回错误次数",
            self.fuse_errors.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_blockcache_hits_total",
            "counter",
            "block_cache 命中（read_range 内部块免整块解压）次数",
            self.blockcache_hits.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_blockcache_misses_total",
            "counter",
            "block_cache 未命中（read_range 内部块走 Store + 解压）次数",
            self.blockcache_misses.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_seals_total",
            "counter",
            "尾块封块/重压落后端次数",
            self.seals.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_shadow_commits_total",
            "counter",
            "ShadowStore（布局 S）脏会话提交次数",
            self.shadow_commits.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_shadow_reader_hits_total",
            "counter",
            "ShadowStore ArchiveReader 缓存命中（免重解析 footer/index）次数",
            self.shadow_reader_hits.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_shadow_reader_misses_total",
            "counter",
            "ShadowStore ArchiveReader 缓存未命中（打开并解析新 reader）次数",
            self.shadow_reader_misses.load(Ordering::Relaxed),
        );
        emit(
            out,
            "scrollz_shadow_tail_appends_total",
            "counter",
            "ShadowStore 尾日志增量追加（append_tail）次数",
            self.shadow_tail_appends.load(Ordering::Relaxed),
        );
        // 延迟直方图（Prometheus 惯例：单位秒、名带 _seconds、不带 _total）。
        self.read_latency.write_prometheus(
            out,
            "scrollz_read_latency_seconds",
            "FUSE read handler 端到端延迟（秒）",
        );
        self.write_latency.write_prometheus(
            out,
            "scrollz_write_latency_seconds",
            "FUSE write handler 端到端延迟（秒）",
        );
        self.fsync_latency.write_prometheus(
            out,
            "scrollz_fsync_latency_seconds",
            "FUSE fsync/flush handler 端到端延迟（秒）",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_prometheus_reflects_recorded_counts_and_peak() {
        let m = Metrics::new();
        m.record_commit_ok(3);
        m.record_commit_failed();
        m.observe_flushing_bytes(4096);
        // 峰值单调：更小的值不下调。
        m.observe_flushing_bytes(1024);

        let mut out = String::new();
        m.write_prometheus(&mut out);

        assert!(
            out.contains("scrollz_commit_ok_total 1"),
            "提交成功计数应为 1，实际输出：\n{out}"
        );
        assert!(
            out.contains("scrollz_blocks_flushed_total 3"),
            "累计块数应为 3，实际输出：\n{out}"
        );
        assert!(
            out.contains("scrollz_commit_failed_total 1"),
            "提交失败计数应为 1，实际输出：\n{out}"
        );
        assert!(
            out.contains("scrollz_flushing_bytes_peak 4096"),
            "峰值应为 4096（不被更小的 1024 下调），实际输出：\n{out}"
        );
        // Prometheus text 格式：counter/gauge 类型行齐备。
        assert!(out.contains("# TYPE scrollz_commit_ok_total counter"));
        assert!(out.contains("# TYPE scrollz_flushing_bytes_peak gauge"));
        assert!(out.contains("# HELP scrollz_blocks_flushed_total"));
    }

    #[test]
    fn write_prometheus_reflects_fuse_per_op_counters() {
        let m = Metrics::new();
        m.record_read(100);
        m.record_write(50);
        m.record_fsync();
        m.record_fuse_error();

        let mut out = String::new();
        m.write_prometheus(&mut out);

        assert!(
            out.contains("scrollz_fuse_read_ops_total 1"),
            "read ops 应为 1：\n{out}"
        );
        assert!(
            out.contains("scrollz_fuse_read_bytes_total 100"),
            "read bytes 应为 100：\n{out}"
        );
        assert!(
            out.contains("scrollz_fuse_write_ops_total 1"),
            "write ops 应为 1：\n{out}"
        );
        assert!(
            out.contains("scrollz_fuse_write_bytes_total 50"),
            "write bytes 应为 50：\n{out}"
        );
        assert!(
            out.contains("scrollz_fuse_fsync_ops_total 1"),
            "fsync ops 应为 1：\n{out}"
        );
        assert!(
            out.contains("scrollz_fuse_errors_total 1"),
            "errors 应为 1：\n{out}"
        );
        // 类型行齐备（均为 counter）。
        assert!(out.contains("# TYPE scrollz_fuse_read_ops_total counter"));
        assert!(out.contains("# TYPE scrollz_fuse_read_bytes_total counter"));
        assert!(out.contains("# TYPE scrollz_fuse_write_ops_total counter"));
        assert!(out.contains("# TYPE scrollz_fuse_write_bytes_total counter"));
        assert!(out.contains("# TYPE scrollz_fuse_fsync_ops_total counter"));
        assert!(out.contains("# TYPE scrollz_fuse_errors_total counter"));
    }

    #[test]
    fn fuse_read_write_counters_accumulate() {
        let m = Metrics::new();
        m.record_read(10);
        m.record_read(30);
        m.record_write(5);
        m.record_write(7);
        let mut out = String::new();
        m.write_prometheus(&mut out);
        assert!(
            out.contains("scrollz_fuse_read_ops_total 2"),
            "两次读：\n{out}"
        );
        assert!(
            out.contains("scrollz_fuse_read_bytes_total 40"),
            "read bytes 10+30=40：\n{out}"
        );
        assert!(
            out.contains("scrollz_fuse_write_ops_total 2"),
            "两次写：\n{out}"
        );
        assert!(
            out.contains("scrollz_fuse_write_bytes_total 12"),
            "write bytes 5+7=12：\n{out}"
        );
    }

    #[test]
    fn write_prometheus_reflects_blockcache_hit_rate_counters() {
        let m = Metrics::new();
        m.record_cache_hit();
        m.record_cache_hit();
        m.record_cache_miss();

        let mut out = String::new();
        m.write_prometheus(&mut out);

        assert!(
            out.contains("scrollz_blockcache_hits_total 2"),
            "命中计数应为 2：\n{out}"
        );
        assert!(
            out.contains("scrollz_blockcache_misses_total 1"),
            "未命中计数应为 1：\n{out}"
        );
        // 命中率由 Prometheus 侧 hits/(hits+misses) 算，进程内不算，只暴露两个 counter。
        assert!(out.contains("# TYPE scrollz_blockcache_hits_total counter"));
        assert!(out.contains("# TYPE scrollz_blockcache_misses_total counter"));
        assert!(out.contains("# HELP scrollz_blockcache_hits_total"));
        assert!(out.contains("# HELP scrollz_blockcache_misses_total"));
    }

    #[test]
    fn write_prometheus_reflects_seal_counter() {
        let m = Metrics::new();
        m.record_seal();
        m.record_seal();
        m.record_seal();

        assert_eq!(m.seals(), 3, "record_seal ×3 后 seals() 应为 3");

        let mut out = String::new();
        m.write_prometheus(&mut out);

        assert!(
            out.contains("scrollz_seals_total 3"),
            "封块计数应为 3，实际输出：\n{out}"
        );
        assert!(out.contains("# TYPE scrollz_seals_total counter"));
        assert!(out.contains("# HELP scrollz_seals_total"));
    }

    #[test]
    fn multiple_commits_accumulate() {
        let m = Metrics::new();
        m.record_commit_ok(2);
        m.record_commit_ok(5);
        let mut out = String::new();
        m.write_prometheus(&mut out);
        assert!(out.contains("scrollz_commit_ok_total 2"), "两次成功：\n{out}");
        assert!(
            out.contains("scrollz_blocks_flushed_total 7"),
            "块数累加 2+5=7：\n{out}"
        );
    }

    /// 从 Prometheus text 抽一个指标行的数值（`name{labels} VALUE` 或 `name VALUE`）。
    /// 返回该行 VALUE 的字符串（未解析），找不到返回 None。
    fn metric_line_value<'a>(out: &'a str, prefix: &str) -> Option<&'a str> {
        out.lines()
            .find(|l| l.starts_with(prefix) && !l.starts_with('#'))
            .and_then(|l| l.rsplit(' ').next())
    }

    #[test]
    fn histogram_cumulative_buckets_are_monotonic_and_inf_equals_count() {
        let m = Metrics::new();
        // 覆盖低桶(≤50us)、中桶(≤500us)、高桶(≤5000us)、overflow(>100ms)。
        m.observe_read_latency_us(30); // 落 le=50us 桶
        m.observe_read_latency_us(300); // 落 le=500us 桶
        m.observe_read_latency_us(3000); // 落 le=5000us 桶
        m.observe_read_latency_us(300_000); // > 100ms → overflow/+Inf

        let mut out = String::new();
        m.write_prometheus(&mut out);

        // 逐个累积桶：le 越大，累计计数不减（单调不减）。
        let name = "scrollz_read_latency_seconds";
        let bucket_les = [
            "0.00005", "0.0001", "0.00025", "0.0005", "0.001", "0.0025", "0.005", "0.01", "0.025",
            "0.05", "0.1", "+Inf",
        ];
        let mut prev: u64 = 0;
        let mut cum_values = Vec::new();
        for le in bucket_les {
            let prefix = format!("{name}_bucket{{le=\"{le}\"}} ");
            let v: u64 = metric_line_value(&out, &prefix)
                .unwrap_or_else(|| panic!("缺 bucket le={le}：\n{out}"))
                .parse()
                .unwrap();
            assert!(
                v >= prev,
                "累积桶必须单调不减：le={le} 的 {v} < 前一桶 {prev}\n{out}"
            );
            prev = v;
            cum_values.push(v);
        }

        // _count == 观测数(4)。
        let count: u64 = metric_line_value(&out, &format!("{name}_count "))
            .expect("缺 _count")
            .parse()
            .unwrap();
        assert_eq!(count, 4, "观测 4 次：\n{out}");

        // +Inf 桶累计 == _count（Prometheus histogram 硬不变量）。
        let inf = *cum_values.last().unwrap();
        assert_eq!(inf, count, "+Inf 桶累计必须等于 _count：\n{out}");

        // 具体累积语义：le=50us 桶累计 1（仅 30us 那次），le=500us 累计 2（+300us），
        // le=5000us 累计 3（+3000us），到 le=0.1 仍是 3（300ms 不在有限桶内），+Inf 才是 4。
        assert_eq!(cum_values[0], 1, "le=0.00005 累计应为 1：\n{out}");
        assert_eq!(cum_values[3], 2, "le=0.0005 累计应为 2：\n{out}");
        assert_eq!(cum_values[6], 3, "le=0.005 累计应为 3：\n{out}");
        assert_eq!(cum_values[10], 3, "le=0.1(最后有限桶) 累计应为 3：\n{out}");
        assert_eq!(cum_values[11], 4, "le=+Inf 累计应为 4：\n{out}");

        // _sum == Σ 秒（30+300+3000+300000 = 303330 us = 0.30333 s），浮点近似断言。
        let sum: f64 = metric_line_value(&out, &format!("{name}_sum "))
            .expect("缺 _sum")
            .parse()
            .unwrap();
        assert!(
            (sum - 0.303_33).abs() < 1e-6,
            "_sum 应约 0.30333 s，实际 {sum}：\n{out}"
        );

        // 类型行为 histogram。
        assert!(out.contains(&format!("# TYPE {name} histogram")));
        assert!(out.contains(&format!("# HELP {name} ")));
    }

    #[test]
    fn histogram_write_and_fsync_latencies_are_independent() {
        let m = Metrics::new();
        m.observe_write_latency_us(100);
        m.observe_write_latency_us(100);
        m.observe_fsync_latency_us(2000);

        let mut out = String::new();
        m.write_prometheus(&mut out);

        // write 直方图观测 2 次。
        assert_eq!(
            metric_line_value(&out, "scrollz_write_latency_seconds_count "),
            Some("2"),
            "write count=2：\n{out}"
        );
        // fsync 直方图观测 1 次，独立于 write。
        assert_eq!(
            metric_line_value(&out, "scrollz_fsync_latency_seconds_count "),
            Some("1"),
            "fsync count=1：\n{out}"
        );
        // read 直方图未观测：count=0，+Inf=0。
        assert_eq!(
            metric_line_value(&out, "scrollz_read_latency_seconds_count "),
            Some("0"),
            "read count=0：\n{out}"
        );
        assert_eq!(
            metric_line_value(&out, "scrollz_read_latency_seconds_bucket{le=\"+Inf\"} "),
            Some("0"),
            "read +Inf=0：\n{out}"
        );
    }

    #[test]
    fn histogram_boundary_value_falls_in_bucket_at_or_below() {
        // 边界：us 恰等于桶上界应落入该桶（observe 用 us <= b）。
        let m = Metrics::new();
        m.observe_read_latency_us(50); // 恰 le=50us 上界 → 落该桶
        m.observe_read_latency_us(100_000); // 恰 le=100ms 上界 → 落最后有限桶，不 overflow

        let mut out = String::new();
        m.write_prometheus(&mut out);

        let name = "scrollz_read_latency_seconds";
        // le=0.00005 累计 1（50us 落此桶）。
        assert_eq!(
            metric_line_value(&out, &format!("{name}_bucket{{le=\"0.00005\"}} ")),
            Some("1"),
        );
        // le=0.1(=100ms) 累计 2：50us + 100ms 都在有限桶内。
        assert_eq!(
            metric_line_value(&out, &format!("{name}_bucket{{le=\"0.1\"}} ")),
            Some("2"),
        );
        // +Inf 也是 2，无 overflow。
        assert_eq!(
            metric_line_value(&out, &format!("{name}_bucket{{le=\"+Inf\"}} ")),
            Some("2"),
        );
    }

    #[test]
    fn write_prometheus_reflects_shadow_backend_counters() {
        let m = Metrics::new();
        m.record_shadow_commit();
        m.record_shadow_commit();
        m.record_reader_hit();
        m.record_reader_hit();
        m.record_reader_hit();
        m.record_reader_miss();
        m.record_tail_append();

        let mut out = String::new();
        m.write_prometheus(&mut out);

        assert!(
            out.contains("scrollz_shadow_commits_total 2"),
            "shadow 提交计数应为 2：\n{out}"
        );
        assert!(
            out.contains("scrollz_shadow_reader_hits_total 3"),
            "reader 命中应为 3：\n{out}"
        );
        assert!(
            out.contains("scrollz_shadow_reader_misses_total 1"),
            "reader 未命中应为 1：\n{out}"
        );
        assert!(
            out.contains("scrollz_shadow_tail_appends_total 1"),
            "尾日志追加应为 1：\n{out}"
        );
        // 类型/HELP 行齐备（均为 counter）。
        assert!(out.contains("# TYPE scrollz_shadow_commits_total counter"));
        assert!(out.contains("# TYPE scrollz_shadow_reader_hits_total counter"));
        assert!(out.contains("# TYPE scrollz_shadow_reader_misses_total counter"));
        assert!(out.contains("# TYPE scrollz_shadow_tail_appends_total counter"));
        assert!(out.contains("# HELP scrollz_shadow_commits_total"));
    }
}
