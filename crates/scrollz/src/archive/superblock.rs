//! SuperBlock：崩溃安全提交协议的原子提交点（docs/04 §2.1/§4，TDD §8.1）
//!
//! 两个定长 superblock 槽固定在文件头部（header 之后），交替写、带单调 seq + CRC。
//! open 取「sb_magic+sb_crc 通过且 seq 最大」者（完整「槽可用」级联校验——再加 index_crc +
//! 尾日志可重放——在 ArchiveReader::open 层做，见 docs/04 §4 M4）。本节只做 superblock 自身的
//! 编解码与 seq 择优（纯函数，可隔离测试）。

use super::format::{crc32, get_u32, get_u64, put_u32, put_u64};
use super::{HeadCache, SB_LEN};

/// superblock 魔数（"ZSB2" 小端），区分未初始化/损坏槽。
pub const SB_MAGIC: u32 = u32::from_le_bytes(*b"ZSB2");
/// sb_crc 在槽内的偏移：覆盖 `[0, SB_CRC_OFFSET)` 的全部字段 + 零填充。
const SB_CRC_OFFSET: usize = SB_LEN as usize - 4;

/// 解析后的 superblock 视图。`head_cache` 三字段全 0 → None（吸收自 docs/02 的 head 缓存）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuperBlock {
    /// 单调提交序号；open 取最大且校验通过者。绝不重置（含压实后），u64 永不耗尽。
    pub seq: u64,
    pub chunk_size: u32,
    /// 逻辑文件大小（= Σ封块 rawlen + 尾日志重放字节，单一视图下互斥覆盖）。
    pub uncompressed_size: u64,
    pub chunk_count: u64,
    pub index_offset: u64,
    pub index_len: u64,
    pub index_crc: u32,
    /// 尾日志区位置与长度（0 = 无未封尾）。
    pub tail_journal_offset: u64,
    pub tail_journal_len: u64,
    /// head 缓存（发现读快路径）；None = 无。
    pub head_cache: Option<HeadCache>,
}

/// 序列化一个 superblock 槽为定长 `SB_LEN` 字节（含尾部 `sb_crc`）。
pub fn serialize_superblock(sb: &SuperBlock) -> [u8; SB_LEN as usize] {
    let mut buf = [0u8; SB_LEN as usize];
    let mut p = 0usize;
    put_u32(&mut buf, &mut p, SB_MAGIC);
    put_u64(&mut buf, &mut p, sb.seq);
    put_u32(&mut buf, &mut p, sb.chunk_size);
    put_u64(&mut buf, &mut p, sb.uncompressed_size);
    put_u64(&mut buf, &mut p, sb.chunk_count);
    put_u64(&mut buf, &mut p, sb.index_offset);
    put_u64(&mut buf, &mut p, sb.index_len);
    put_u32(&mut buf, &mut p, sb.index_crc);
    put_u64(&mut buf, &mut p, sb.tail_journal_offset);
    put_u64(&mut buf, &mut p, sb.tail_journal_len);
    let hc = sb.head_cache.unwrap_or(HeadCache {
        offset: 0,
        clen: 0,
        rawlen: 0,
        verbatim: false,
    });
    put_u64(&mut buf, &mut p, hc.offset);
    put_u64(&mut buf, &mut p, hc.clen);
    put_u64(&mut buf, &mut p, hc.rawlen);
    put_u32(&mut buf, &mut p, u32::from(hc.verbatim));
    debug_assert_eq!(p, 96, "字段布局与文档 §2.1 不符");
    // [96..SB_CRC_OFFSET) 已是零填充，纳入 crc。
    let crc = crc32(&buf[..SB_CRC_OFFSET]);
    buf[SB_CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
    buf
}

/// 解析一个 superblock 槽。`sb_magic` 不符或 `sb_crc` 不符（半截写/损坏）→ None。
pub fn parse_superblock(buf: &[u8]) -> Option<SuperBlock> {
    if buf.len() < SB_LEN as usize {
        return None;
    }
    let buf = &buf[..SB_LEN as usize];
    if u32::from_le_bytes(buf[0..4].try_into().unwrap()) != SB_MAGIC {
        return None;
    }
    let stored_crc = u32::from_le_bytes(buf[SB_CRC_OFFSET..].try_into().unwrap());
    if crc32(&buf[..SB_CRC_OFFSET]) != stored_crc {
        return None;
    }
    let mut p = 4usize; // 跳过 magic
    let seq = get_u64(buf, &mut p);
    let chunk_size = get_u32(buf, &mut p);
    let uncompressed_size = get_u64(buf, &mut p);
    let chunk_count = get_u64(buf, &mut p);
    let index_offset = get_u64(buf, &mut p);
    let index_len = get_u64(buf, &mut p);
    let index_crc = get_u32(buf, &mut p);
    let tail_journal_offset = get_u64(buf, &mut p);
    let tail_journal_len = get_u64(buf, &mut p);
    let hc_offset = get_u64(buf, &mut p);
    let hc_clen = get_u64(buf, &mut p);
    let hc_rawlen = get_u64(buf, &mut p);
    let hc_flags = get_u32(buf, &mut p);
    let head_cache = if hc_offset == 0 && hc_clen == 0 && hc_rawlen == 0 {
        None
    } else {
        Some(HeadCache {
            offset: hc_offset,
            clen: hc_clen,
            rawlen: hc_rawlen,
            verbatim: hc_flags & 1 != 0,
        })
    };
    Some(SuperBlock {
        seq,
        chunk_size,
        uncompressed_size,
        chunk_count,
        index_offset,
        index_len,
        index_crc,
        tail_journal_offset,
        tail_journal_len,
        head_cache,
    })
}

/// 双槽选活跃：在已通过 `sb_magic`+`sb_crc` 校验的槽中取 `seq` 最大者；相等取 A
/// （确定性 tie-break——正常不应相等，seq 单调不重置，docs/04 §6/C3）。
///
/// 注意：完整「槽可用」是级联校验（本函数的 superblock 自身有效性 + index_crc + 尾日志可重放，
/// docs/04 §4 M4），后两者需读文件，在 `ArchiveReader::open` 层做；本函数只负责 seq 择优。
pub fn pick_active(a: Option<SuperBlock>, b: Option<SuperBlock>) -> Option<SuperBlock> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if y.seq > x.seq { y } else { x }),
        (Some(x), None) => Some(x),
        (None, other) => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SuperBlock 编解码 + 双槽选活（docs/04 §8.1，TDD）----

    /// 构造一个样本 superblock（指定 seq，便于 pick_active 测试）。
    fn sample_sb(seq: u64, head: Option<HeadCache>) -> SuperBlock {
        SuperBlock {
            seq,
            chunk_size: 1 << 20,
            uncompressed_size: 123_456,
            chunk_count: 3,
            index_offset: 4096,
            index_len: 60,
            index_crc: 0xDEAD_BEEF,
            tail_journal_offset: 8192,
            tail_journal_len: 256,
            head_cache: head,
        }
    }

    #[test]
    fn superblock_round_trip_no_head_cache() {
        let sb = sample_sb(42, None);
        let bytes = serialize_superblock(&sb);
        assert_eq!(bytes.len(), SB_LEN as usize);
        assert_eq!(
            parse_superblock(&bytes),
            Some(sb),
            "无 head 缓存 round-trip 应一致"
        );
    }

    #[test]
    fn superblock_round_trip_with_head_cache() {
        let hc = HeadCache {
            offset: 500,
            clen: 20,
            rawlen: 65536,
            verbatim: true,
        };
        let sb = sample_sb(7, Some(hc));
        let bytes = serialize_superblock(&sb);
        assert_eq!(
            parse_superblock(&bytes),
            Some(sb),
            "带 head 缓存 round-trip 应一致"
        );
    }

    #[test]
    fn superblock_crc_detects_any_single_byte_flip() {
        let sb = sample_sb(1, None);
        let good = serialize_superblock(&sb);
        // 翻转字段区/填充区任一字节，crc 都应检出（除 sb_crc 自身的边角，逐个验证字段+填充区）。
        for i in 0..SB_CRC_OFFSET {
            let mut bad = good;
            bad[i] ^= 0xFF;
            assert_eq!(
                parse_superblock(&bad),
                None,
                "第 {i} 字节翻转应被 sb_crc 检出为损坏"
            );
        }
    }

    #[test]
    fn superblock_bad_magic_returns_none() {
        let mut bytes = serialize_superblock(&sample_sb(1, None));
        bytes[0] ^= 0xFF; // 破坏 magic
        assert_eq!(parse_superblock(&bytes), None);
    }

    #[test]
    fn superblock_short_buffer_returns_none() {
        assert_eq!(parse_superblock(&[0u8; 10]), None);
        assert_eq!(parse_superblock(&[]), None);
    }

    #[test]
    fn pick_active_picks_max_seq_regardless_of_order() {
        let a = sample_sb(5, None);
        let b = sample_sb(3, None);
        assert_eq!(pick_active(Some(a), Some(b)).unwrap().seq, 5);
        assert_eq!(pick_active(Some(b), Some(a)).unwrap().seq, 5);
    }

    #[test]
    fn pick_active_one_slot_corrupt_picks_other_both_corrupt_none() {
        let a = sample_sb(9, None);
        assert_eq!(pick_active(Some(a), None), Some(a));
        assert_eq!(pick_active(None, Some(a)), Some(a));
        assert_eq!(pick_active(None, None), None);
    }

    #[test]
    fn pick_active_equal_seq_picks_a_deterministic() {
        // seq 相等（正常不应发生）：tie-break 取 A，确定性。用不同 chunk_count 区分。
        let mut a = sample_sb(5, None);
        a.chunk_count = 1;
        let mut b = sample_sb(5, None);
        b.chunk_count = 2;
        assert_eq!(
            pick_active(Some(a), Some(b)),
            Some(a),
            "seq 相等应确定性取 A"
        );
    }
}
