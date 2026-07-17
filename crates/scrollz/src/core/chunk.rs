//! 分块数学 + 读改写（RMW）占位（P3 填充）。
//!
//! 设计见 docs/01-scrollz-design.md §3、§4.1。逻辑文件 = 定长逻辑块序列；随机写命中块
//! 走 read-modify-write：解压整块 → 打补丁 → 重压 → 写回。首尾非块对齐块按 RMW 处理，
//! 整块覆盖可跳过读。越界 / 空洞按零填充，而非把缺块当错误。

/// 把字节区间 `[offset, offset+len)` 映射到覆盖的逻辑块下标区间 `[first, last]`。
///
/// 纯函数、无副作用，P0 即可实现并测试（分块数学是后续 RMW 的地基）。
pub fn block_range(offset: u64, len: u64, chunk_size: u64) -> (u64, u64) {
    debug_assert!(chunk_size > 0, "chunk_size 必须为正");
    if len == 0 {
        let idx = offset / chunk_size;
        return (idx, idx);
    }
    let first = offset / chunk_size;
    let last = (offset + len - 1) / chunk_size;
    (first, last)
}

/// 块内偏移：给定全局偏移落在某块内的相对位置。
pub fn offset_in_block(offset: u64, chunk_size: u64) -> u64 {
    offset % chunk_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_range_within_one_block_maps_to_same_block() {
        // chunk=64KiB，区间完全落在块 0 内。
        assert_eq!(block_range(0, 100, 65536), (0, 0));
        assert_eq!(block_range(100, 200, 65536), (0, 0));
    }

    #[test]
    fn cross_block_range_covers_correct_block_span() {
        // 从块 0 末尾跨到块 1。
        assert_eq!(block_range(65500, 100, 65536), (0, 1));
        // 恰好对齐到块 2 的起点，长度 1 字节。
        assert_eq!(block_range(131072, 1, 65536), (2, 2));
    }

    #[test]
    fn zero_length_range_degenerates_to_containing_block() {
        assert_eq!(block_range(70000, 0, 65536), (1, 1));
    }

    #[test]
    fn intra_block_offset_modulo_correct() {
        assert_eq!(offset_in_block(65536, 65536), 0);
        assert_eq!(offset_in_block(65600, 65536), 64);
    }
}
