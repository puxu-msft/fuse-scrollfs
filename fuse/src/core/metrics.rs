//! 统一指标注册表：全 crate 单一 `Arc<Metrics>`，各子系统经类型化方法自增
//! （热路径无锁 `Relaxed`），单一 Prometheus text 序列化出口。零依赖。
//!
//! 设计动机：取代「新增一个指标要改 3 处硬编码」的耦合。加一个指标需要三步——加一个原子
//! 字段、加一个 `record_*`/`observe_*` 方法、在 [`Metrics::write_prometheus`] 里加一行 `emit`。
//! 各埋点只调类型化方法，序列化格式集中在一处，互不牵连。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
            "zipfs_commit_ok_total",
            "counter",
            "container 提交成功次数",
            self.commit_ok.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_commit_failed_total",
            "counter",
            "提交失败并合并回 active（避免数据丢失）次数",
            self.commit_failed.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_blocks_flushed_total",
            "counter",
            "累计落后端的块数",
            self.blocks_flushed.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_flushing_bytes_peak",
            "gauge",
            "flushing 缓冲字节峰值",
            self.flushing_bytes_peak.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_fuse_read_ops_total",
            "counter",
            "FUSE read 成功次数",
            self.fuse_read_ops.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_fuse_read_bytes_total",
            "counter",
            "FUSE read 累计返回字节数",
            self.fuse_read_bytes.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_fuse_write_ops_total",
            "counter",
            "FUSE write 成功次数",
            self.fuse_write_ops.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_fuse_write_bytes_total",
            "counter",
            "FUSE write 累计写入字节数",
            self.fuse_write_bytes.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_fuse_fsync_ops_total",
            "counter",
            "fsync+flush 同步操作次数",
            self.fuse_fsync_ops.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_fuse_errors_total",
            "counter",
            "read/write/fsync/flush 返回错误次数",
            self.fuse_errors.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_blockcache_hits_total",
            "counter",
            "block_cache 命中（read_range 内部块免整块解压）次数",
            self.blockcache_hits.load(Ordering::Relaxed),
        );
        emit(
            out,
            "zipfs_blockcache_misses_total",
            "counter",
            "block_cache 未命中（read_range 内部块走 Store + 解压）次数",
            self.blockcache_misses.load(Ordering::Relaxed),
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
            out.contains("zipfs_commit_ok_total 1"),
            "提交成功计数应为 1，实际输出：\n{out}"
        );
        assert!(
            out.contains("zipfs_blocks_flushed_total 3"),
            "累计块数应为 3，实际输出：\n{out}"
        );
        assert!(
            out.contains("zipfs_commit_failed_total 1"),
            "提交失败计数应为 1，实际输出：\n{out}"
        );
        assert!(
            out.contains("zipfs_flushing_bytes_peak 4096"),
            "峰值应为 4096（不被更小的 1024 下调），实际输出：\n{out}"
        );
        // Prometheus text 格式：counter/gauge 类型行齐备。
        assert!(out.contains("# TYPE zipfs_commit_ok_total counter"));
        assert!(out.contains("# TYPE zipfs_flushing_bytes_peak gauge"));
        assert!(out.contains("# HELP zipfs_blocks_flushed_total"));
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
            out.contains("zipfs_fuse_read_ops_total 1"),
            "read ops 应为 1：\n{out}"
        );
        assert!(
            out.contains("zipfs_fuse_read_bytes_total 100"),
            "read bytes 应为 100：\n{out}"
        );
        assert!(
            out.contains("zipfs_fuse_write_ops_total 1"),
            "write ops 应为 1：\n{out}"
        );
        assert!(
            out.contains("zipfs_fuse_write_bytes_total 50"),
            "write bytes 应为 50：\n{out}"
        );
        assert!(
            out.contains("zipfs_fuse_fsync_ops_total 1"),
            "fsync ops 应为 1：\n{out}"
        );
        assert!(
            out.contains("zipfs_fuse_errors_total 1"),
            "errors 应为 1：\n{out}"
        );
        // 类型行齐备（均为 counter）。
        assert!(out.contains("# TYPE zipfs_fuse_read_ops_total counter"));
        assert!(out.contains("# TYPE zipfs_fuse_read_bytes_total counter"));
        assert!(out.contains("# TYPE zipfs_fuse_write_ops_total counter"));
        assert!(out.contains("# TYPE zipfs_fuse_write_bytes_total counter"));
        assert!(out.contains("# TYPE zipfs_fuse_fsync_ops_total counter"));
        assert!(out.contains("# TYPE zipfs_fuse_errors_total counter"));
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
            out.contains("zipfs_fuse_read_ops_total 2"),
            "两次读：\n{out}"
        );
        assert!(
            out.contains("zipfs_fuse_read_bytes_total 40"),
            "read bytes 10+30=40：\n{out}"
        );
        assert!(
            out.contains("zipfs_fuse_write_ops_total 2"),
            "两次写：\n{out}"
        );
        assert!(
            out.contains("zipfs_fuse_write_bytes_total 12"),
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
            out.contains("zipfs_blockcache_hits_total 2"),
            "命中计数应为 2：\n{out}"
        );
        assert!(
            out.contains("zipfs_blockcache_misses_total 1"),
            "未命中计数应为 1：\n{out}"
        );
        // 命中率由 Prometheus 侧 hits/(hits+misses) 算，进程内不算，只暴露两个 counter。
        assert!(out.contains("# TYPE zipfs_blockcache_hits_total counter"));
        assert!(out.contains("# TYPE zipfs_blockcache_misses_total counter"));
        assert!(out.contains("# HELP zipfs_blockcache_hits_total"));
        assert!(out.contains("# HELP zipfs_blockcache_misses_total"));
    }

    #[test]
    fn multiple_commits_accumulate() {
        let m = Metrics::new();
        m.record_commit_ok(2);
        m.record_commit_ok(5);
        let mut out = String::new();
        m.write_prometheus(&mut out);
        assert!(out.contains("zipfs_commit_ok_total 2"), "两次成功：\n{out}");
        assert!(
            out.contains("zipfs_blocks_flushed_total 7"),
            "块数累加 2+5=7：\n{out}"
        );
    }
}
