//! 底层格式原语：CRC32、定长整数小端读写、定位读、损坏错误构造。
//!
//! 这些是 archive 各子模块（superblock/journal/reader/writer/updater）共用的最底层工具，
//! 不依赖任何上层类型，集中在此避免布局漂移与重复实现。

use std::io;

use crate::blockio::BlockIo;

// ===========================================================================
// CRC32（IEEE / CRC-32/ISO-HDLC）—— 校验 index / superblock / 尾日志记录完整性
// ===========================================================================

/// 计算 IEEE CRC32（多项式 0xEDB88320）。用 `crc32fast`（SIMD 加速）替代原手搓逐位实现：
/// 崩溃安全提交协议（docs/04）给每块 / superblock / 尾日志记录都加 CRC，校验从「小 index」
/// 升为热点，逐位法不再够用。`crc32fast::hash` 与原逐位实现同为 CRC-32/ISO-HDLC，值一致，
/// 既有 archive 与测试保持兼容。
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

// ===========================================================================
// 小工具：定长整数读写（小端），集中显式错误处理
// ===========================================================================

#[inline]
pub(crate) fn put_u32(buf: &mut [u8], p: &mut usize, v: u32) {
    buf[*p..*p + 4].copy_from_slice(&v.to_le_bytes());
    *p += 4;
}
#[inline]
pub(crate) fn put_u64(buf: &mut [u8], p: &mut usize, v: u64) {
    buf[*p..*p + 8].copy_from_slice(&v.to_le_bytes());
    *p += 8;
}
#[inline]
pub(crate) fn get_u32(buf: &[u8], p: &mut usize) -> u32 {
    let v = u32::from_le_bytes(buf[*p..*p + 4].try_into().unwrap());
    *p += 4;
    v
}
#[inline]
pub(crate) fn get_u64(buf: &[u8], p: &mut usize) -> u64 {
    let v = u64::from_le_bytes(buf[*p..*p + 8].try_into().unwrap());
    *p += 8;
    v
}

/// 定位读（pread）：用绝对偏移读，**不移动文件游标**。这让同一只读 `File`（如缓存的
/// `ArchiveReader`）可被多线程并发 `read_block` 而不发生 seek 竞争（fuser 多线程派发，
/// reader 缓存按 `Arc` 共享，见 store::shadow 的 per-fh reader 缓存）。
///
/// 经 `BlockIo` 接缝（生产为 `impl BlockIo for File`，注入为 `FaultIo`），使「打开/恢复阶段
/// 读失败」可被故障注入覆盖（docs/05 §3）。
pub(crate) fn read_exact_at(io: &impl BlockIo, buf: &mut [u8], offset: u64) -> io::Result<()> {
    io.read_at(offset, buf)
}

/// 构造一个 InvalidData 错误，带统一前缀，便于排查。
pub(crate) fn corrupt(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("archive 损坏：{msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vectors() {
        // "123456789" 的 IEEE CRC32 标准向量 = 0xCBF43926。
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
