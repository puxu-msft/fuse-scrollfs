//! 场景 harness：四个测量场景的纯逻辑，与具体后端解耦。
//!
//! 场景对照设计 §6（布局 V）与 §6.1（三档形态 / 写批处理陷阱）：
//!  1. 批量插入：N 文件 × M 块，测吞吐。
//!  2. 随机 RMW：随机选 (ino,idx) 读出 → 改写成新变长 blob → 写回，测吞吐 + p50/p99。
//!  3. 事务策略对比：PerBlock vs Batched(K)，量化「每块一事务」陷阱。
//!  4. 空间：随机更新后的膨胀，以及 compact 后大小。

use std::time::Instant;

use crate::backend::{Backend, CommitPolicy};
use crate::blobgen::{gen_blob_into, BlobSizeRange};
use crate::metrics::{LatencyRecorder, Stats};

/// 一次完整跑的输入参数。
#[derive(Clone, Copy, Debug)]
pub struct RunParams {
    pub seed: u64,
    pub num_files: u64,
    pub blocks_per_file: u64,
    pub size: BlobSizeRange,
    /// 批量事务的 K。
    pub batch_k: usize,
    /// RMW 操作次数。
    pub rmw_ops: u64,
}

impl RunParams {
    pub fn total_blocks(&self) -> u64 {
        self.num_files * self.blocks_per_file
    }
}

/// 用一个独立 LCG 流来选 RMW 的目标 (ino, idx)，确定性可复现，与 blob 内容流分开。
struct TargetPicker {
    state: u64,
    num_files: u64,
    blocks_per_file: u64,
}

impl TargetPicker {
    fn new(seed: u64, num_files: u64, blocks_per_file: u64) -> Self {
        TargetPicker {
            state: seed ^ 0xA5A5_5A5A_DEAD_BEEF,
            num_files,
            blocks_per_file,
        }
    }

    /// 返回下一个目标 (ino, idx)。ino 从 1 开始（0 保留）。
    #[inline]
    fn next(&mut self) -> (u64, u64) {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = self.state ^ (self.state >> 31);
        let ino = 1 + (r % self.num_files);
        let idx = (r >> 20) % self.blocks_per_file;
        (ino, idx)
    }
}

/// 生成全部初始块 (ino, idx, blob)。ino ∈ [1, num_files]，idx ∈ [0, blocks_per_file)。
pub fn build_initial_blocks(p: &RunParams) -> Vec<(u64, u64, Vec<u8>)> {
    let total = p.total_blocks() as usize;
    let mut out = Vec::with_capacity(total);
    let mut buf = Vec::new();
    for ino in 1..=p.num_files {
        for idx in 0..p.blocks_per_file {
            gen_blob_into(&mut buf, p.seed, ino, idx, 0, p.size);
            out.push((ino, idx, buf.clone()));
        }
    }
    out
}

/// 场景 1+3：批量插入，按给定策略提交。返回吞吐统计。
pub fn scenario_bulk_insert(
    backend: &mut dyn Backend,
    blocks: &[(u64, u64, Vec<u8>)],
    policy: CommitPolicy,
) -> Stats {
    let start = Instant::now();
    let bytes = backend.bulk_insert(blocks, policy);
    let elapsed = start.elapsed();
    Stats {
        ops: blocks.len() as u64,
        bytes,
        elapsed,
        p50_ns: None,
        p99_ns: None,
        p999_ns: None,
        max_ns: None,
    }
}

/// 场景 2+3：随机 RMW。每次 read → 生成新版本 blob → write back。
/// `policy` 决定写回提交粒度：PerBlock 每次 RMW 独立 commit（逐次测延迟）；
/// Batched(K) 把 K 次 RMW 的写回攒成一个事务（仍逐 RMW 测端到端延迟，但延迟含批边界）。
pub fn scenario_random_rmw(
    backend: &mut dyn Backend,
    p: &RunParams,
    policy: CommitPolicy,
) -> Stats {
    let mut picker = TargetPicker::new(p.seed, p.num_files, p.blocks_per_file);
    let mut rec = LatencyRecorder::new();
    let mut readbuf = Vec::new();
    let mut newbuf = Vec::new();
    let mut bytes = 0u64;

    let start = Instant::now();

    match policy {
        CommitPolicy::PerBlock => {
            for _ in 0..p.rmw_ops {
                let (ino, idx) = picker.next();
                let op_start = Instant::now();
                // 读半程。
                let _ = backend.get_block(ino, idx, &mut readbuf);
                // 生成新变长版本（version 取一个稳定派生值，这里用 ops 计数无所谓——
                // 关键是大小重新落在区间，模拟「压缩后长度变了」）。
                gen_blob_into(&mut newbuf, p.seed, ino, idx, 1 + (ino ^ idx), p.size);
                backend.put_block_committed(ino, idx, &newbuf);
                let ns = op_start.elapsed().as_nanos() as u64;
                rec.record_ns(ns);
                bytes += newbuf.len() as u64;
            }
        }
        CommitPolicy::Batched(k) => {
            // 攒 K 次 RMW 的写回到一个事务。延迟按「这批的端到端」近似分摊到每次。
            let mut pending: Vec<(u64, u64, Vec<u8>)> = Vec::with_capacity(k);
            let mut op_starts: Vec<Instant> = Vec::with_capacity(k);
            let mut done = 0u64;
            while done < p.rmw_ops {
                pending.clear();
                op_starts.clear();
                let this_batch = core::cmp::min(k as u64, p.rmw_ops - done);
                for _ in 0..this_batch {
                    let (ino, idx) = picker.next();
                    let op_start = Instant::now();
                    let _ = backend.get_block(ino, idx, &mut readbuf);
                    gen_blob_into(&mut newbuf, p.seed, ino, idx, 1 + (ino ^ idx), p.size);
                    pending.push((ino, idx, newbuf.clone()));
                    op_starts.push(op_start);
                    bytes += newbuf.len() as u64;
                }
                let commit_start = Instant::now();
                backend.put_batch_committed(&pending);
                let commit_end = Instant::now();
                // 每次 RMW 的端到端延迟 = 从它的 read 开始到本批 commit 结束。
                for st in &op_starts {
                    let ns = commit_end.duration_since(*st).as_nanos() as u64;
                    rec.record_ns(ns);
                }
                let _ = commit_start; // 仅文档化：commit 段在批末。
                done += this_batch;
            }
        }
    }

    let elapsed = start.elapsed();
    Stats {
        ops: p.rmw_ops,
        bytes,
        elapsed,
        p50_ns: Some(rec.p50()),
        p99_ns: Some(rec.p99()),
        p999_ns: Some(rec.p999()),
        max_ns: Some(rec.max()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_params() -> RunParams {
        RunParams {
            seed: 7,
            num_files: 4,
            blocks_per_file: 8,
            size: BlobSizeRange {
                min: 1024,
                max: 4096,
            },
            batch_k: 4,
            rmw_ops: 16,
        }
    }

    #[test]
    fn initial_blocks_count_and_determinism() {
        let p = small_params();
        let a = build_initial_blocks(&p);
        let b = build_initial_blocks(&p);
        assert_eq!(a.len(), p.total_blocks() as usize);
        assert_eq!(a, b, "初始块集应确定性可复现");
    }

    #[test]
    fn picker_targets_in_range() {
        let p = small_params();
        let mut picker = TargetPicker::new(p.seed, p.num_files, p.blocks_per_file);
        for _ in 0..1000 {
            let (ino, idx) = picker.next();
            assert!((1..=p.num_files).contains(&ino));
            assert!(idx < p.blocks_per_file);
        }
    }
}
