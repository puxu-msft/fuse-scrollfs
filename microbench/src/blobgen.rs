//! 确定性伪随机 blob 生成器。
//!
//! 设计要点（见任务约束）：
//! - 不使用真随机（`rand`/系统时间）。一切由固定种子加 `(ino, idx, version)` 派生，
//!   保证可复现：同一组参数每次跑得到完全相同的 blob 序列。
//! - 模拟「压缩后的变长 chunk」：源块定长（64KiB 或 256KiB），压缩后长度在一个区间内变动。
//!   我们直接生成「压缩后」大小的 blob，不真正跑 zstd —— microbench 关心的是
//!   「容器后端存变长 blob 并随机更新」的代价，压缩本身在 Core 层，已被隔离掉。
//! - blob 内容用 LCG 填充，半可压缩但对本测无关紧要（容器存的是不透明字节）。

/// 一个 64 位线性同余发生器（数值取自 Numerical Recipes 的常量）。
/// 纯函数式推进：`next` 返回新状态与输出，绝不就地修改外部数据。
#[derive(Clone, Copy)]
pub struct Lcg {
    state: u64,
}

impl Lcg {
    pub fn new(seed: u64) -> Self {
        // 避免 0 种子退化。
        Lcg {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// 推进一步，返回 (新发生器, 64 位输出)。不可变风格。
    #[inline]
    pub fn step(self) -> (Self, u64) {
        let next = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // 输出做一次 xorshift 扰动，改善低位质量。
        let out = next ^ (next >> 29);
        (Lcg { state: next }, out)
    }
}

/// 把 `(seed, ino, idx, version)` 混成一个稳定的 64 位派生种子。
/// `version` 用于 RMW：同一 (ino,idx) 改写多次，每次内容/大小都不同，
/// 但仍完全由 version 决定，可复现。
#[inline]
fn derive_seed(seed: u64, ino: u64, idx: u64, version: u64) -> u64 {
    let mut h = seed;
    for v in [ino, idx, version, 0xD1B5_4A32_D192_ED03] {
        h ^= v;
        h = h.wrapping_mul(0xff51afd7ed558ccd);
        h ^= h >> 33;
    }
    h
}

/// 变长 blob 的大小区间配置（字节）。模拟某源块大小压缩后的分布。
#[derive(Clone, Copy, Debug)]
pub struct BlobSizeRange {
    pub min: usize,
    pub max: usize,
}

impl BlobSizeRange {
    /// 在 [min, max] 内确定性地取一个大小。
    #[inline]
    pub fn pick(&self, derived: u64) -> usize {
        let span = (self.max - self.min) as u64 + 1;
        self.min + (derived % span) as usize
    }
}

/// 确定性生成一个 blob：长度落在 `range` 内，内容由派生种子的 LCG 填充。
///
/// 复用调用方提供的 `buf` 以避免每块分配（热路径里反复 new Vec 会污染吞吐测量）。
/// 这是性能敏感路径上的就地写入，符合用户风格里「热点路径允许 mutation」的例外。
pub fn gen_blob_into(
    buf: &mut Vec<u8>,
    seed: u64,
    ino: u64,
    idx: u64,
    version: u64,
    range: BlobSizeRange,
) {
    let derived = derive_seed(seed, ino, idx, version);
    let len = range.pick(derived);
    buf.clear();
    buf.reserve(len);

    let mut lcg = Lcg::new(derived);
    // 每步 LCG 出 8 字节，批量写入。
    let mut written = 0usize;
    while written < len {
        let (next, word) = lcg.step();
        lcg = next;
        let bytes = word.to_le_bytes();
        let take = core::cmp::min(8, len - written);
        buf.extend_from_slice(&bytes[..take]);
        written += take;
    }
    debug_assert_eq!(buf.len(), len);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_generation_is_deterministic() {
        let range = BlobSizeRange {
            min: 8 * 1024,
            max: 64 * 1024,
        };
        let mut a = Vec::new();
        let mut b = Vec::new();
        gen_blob_into(&mut a, 42, 7, 99, 0, range);
        gen_blob_into(&mut b, 42, 7, 99, 0, range);
        assert_eq!(a, b, "同参数两次生成必须完全一致");
    }

    #[test]
    fn different_version_changes_content_and_size() {
        let range = BlobSizeRange {
            min: 8 * 1024,
            max: 64 * 1024,
        };
        let mut v0 = Vec::new();
        let mut v1 = Vec::new();
        gen_blob_into(&mut v0, 42, 7, 99, 0, range);
        gen_blob_into(&mut v1, 42, 7, 99, 1, range);
        assert_ne!(v0, v1, "改写后 version 不同，blob 应不同");
    }

    #[test]
    fn size_within_range() {
        let range = BlobSizeRange {
            min: 30 * 1024,
            max: 200 * 1024,
        };
        let mut buf = Vec::new();
        for idx in 0..500u64 {
            gen_blob_into(&mut buf, 1, 1, idx, 0, range);
            assert!(buf.len() >= range.min && buf.len() <= range.max);
        }
    }

    #[test]
    fn sizes_actually_vary() {
        // 确认大小确实变长，不是常数。
        let range = BlobSizeRange {
            min: 8 * 1024,
            max: 64 * 1024,
        };
        let mut buf = Vec::new();
        let mut sizes = std::collections::HashSet::new();
        for idx in 0..200u64 {
            gen_blob_into(&mut buf, 5, 3, idx, 0, range);
            sizes.insert(buf.len());
        }
        assert!(sizes.len() > 50, "变长 blob 大小应有充分多样性");
    }
}
