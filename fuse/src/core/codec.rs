//! 压缩 codec + 不可压缩启发式（P1：zstd；lz4 留 TODO）。
//!
//! 设计见 docs/01-zipfs-design.md §3。每块独立压缩，记录压缩后长度；若
//! `clen >= raw * 阈值`（如 0.95）则原样存 + 置 flag，省解压成本并避免膨胀
//! （对齐 btrfs 行为）。压缩是 Core 的职责，Store 只搬运不透明字节（§2、§5）。

use std::io;

/// 压缩算法选择。`--algo` 切换，见 §13 已定项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    /// 原样存储（不压缩）—— 等价于全程 verbatim。
    None,
    /// zstd，带等级。
    Zstd,
    /// lz4_flex 速度对照（P1 暂未实现，留 TODO）。
    Lz4,
}

/// 不可压缩启发式阈值：压缩后 >= 原始 * 该比例则放弃压缩，原样存储。
pub const INCOMPRESSIBLE_RATIO: f64 = 0.95;

/// 压缩一个逻辑块。返回 `(存储字节, stored_verbatim)`。
///
/// `stored_verbatim == true` 表示返回的就是原始字节（要么 `algo==None`，要么
/// 触发了不可压缩启发式）。调用方据此在 archive 块 flags 里置位，读时跳过解压。
///
/// 空块直接原样返回（压缩空数据无意义，且能避免下游解压空流的边角）。
pub fn compress(raw: &[u8], algo: Algo, level: i32) -> io::Result<(Vec<u8>, bool)> {
    if raw.is_empty() {
        return Ok((Vec::new(), true));
    }
    match algo {
        Algo::None => Ok((raw.to_vec(), true)),
        Algo::Lz4 => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "lz4 codec 尚未实现（P1 仅 zstd），见 docs/01-zipfs-design.md §3",
        )),
        Algo::Zstd => {
            let compressed = zstd::stream::encode_all(raw, level)
                .map_err(|e| io::Error::other(format!("zstd 压缩失败：{e}")))?;
            // 不可压缩启发式：压缩没省下足够空间就原样存，避免「解压成本 + 轻微膨胀」双输。
            if is_incompressible(raw.len(), compressed.len()) {
                Ok((raw.to_vec(), true))
            } else {
                Ok((compressed, false))
            }
        }
    }
}

/// 解压一个逻辑块。`stored_verbatim` 为真时返回的就是原字节（不解压）。
pub fn decompress(stored: &[u8], algo: Algo, stored_verbatim: bool) -> io::Result<Vec<u8>> {
    if stored_verbatim || algo == Algo::None {
        return Ok(stored.to_vec());
    }
    match algo {
        Algo::Zstd => zstd::stream::decode_all(stored)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("zstd 解压失败：{e}"))),
        Algo::Lz4 => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "lz4 codec 尚未实现（P1 仅 zstd）",
        )),
        // verbatim 已在上面短路；走到这说明 algo==None 但 stored_verbatim==false，
        // 仍按原样返回（None 不压缩）。
        Algo::None => Ok(stored.to_vec()),
    }
}

/// 不可压缩判定：`clen >= raw * INCOMPRESSIBLE_RATIO`。
///
/// 用整数比较避免浮点抖动：`clen * 100 >= raw * 95`（阈值 0.95 化为整数）。
fn is_incompressible(raw_len: usize, clen: usize) -> bool {
    // INCOMPRESSIBLE_RATIO 当前为 0.95；以 *100 / *95 表达，避免浮点边界误判。
    debug_assert!((INCOMPRESSIBLE_RATIO - 0.95).abs() < f64::EPSILON);
    (clen as u128) * 100 >= (raw_len as u128) * 95
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zstd_压缩可压缩数据_round_trip() {
        // 高度可压缩：重复字节。
        let raw = vec![b'a'; 64 * 1024];
        let (stored, verbatim) = compress(&raw, Algo::Zstd, 3).unwrap();
        assert!(!verbatim, "高度可压缩数据不应触发 verbatim");
        assert!(stored.len() < raw.len(), "压缩应显著缩小");
        let back = decompress(&stored, Algo::Zstd, verbatim).unwrap();
        assert_eq!(back, raw, "解压必须 round-trip 一致");
    }

    #[test]
    fn 不可压缩数据触发_verbatim_flag() {
        // 伪随机不可压缩数据（线性同余，确定性，避免依赖 rand）。
        let mut raw = Vec::with_capacity(4096);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..4096 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            raw.push((x >> 24) as u8);
        }
        let (stored, verbatim) = compress(&raw, Algo::Zstd, 3).unwrap();
        assert!(verbatim, "不可压缩数据应触发 verbatim 原样存储");
        assert_eq!(stored, raw, "verbatim 时存储字节即原始字节");
        let back = decompress(&stored, Algo::Zstd, verbatim).unwrap();
        assert_eq!(back, raw, "verbatim 解压仍 round-trip");
    }

    #[test]
    fn algo_none_总是_verbatim() {
        let raw = vec![b'x'; 1000];
        let (stored, verbatim) = compress(&raw, Algo::None, 0).unwrap();
        assert!(verbatim);
        assert_eq!(stored, raw);
        assert_eq!(decompress(&stored, Algo::None, verbatim).unwrap(), raw);
    }

    #[test]
    fn 空块_round_trip() {
        let (stored, verbatim) = compress(&[], Algo::Zstd, 3).unwrap();
        assert!(verbatim);
        assert!(stored.is_empty());
        assert_eq!(
            decompress(&stored, Algo::Zstd, verbatim).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn 不可压缩阈值_整数边界() {
        // clen == raw*0.95 正好触发（>=）。
        assert!(is_incompressible(100, 95));
        // clen 略低于阈值则不触发。
        assert!(!is_incompressible(100, 94));
        // 压缩到一半显然可压缩。
        assert!(!is_incompressible(1000, 500));
    }

    #[test]
    fn lz4_返回_unsupported() {
        let err = compress(b"hello", Algo::Lz4, 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
