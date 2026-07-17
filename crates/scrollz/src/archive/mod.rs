//! 布局 S 的每文件分块压缩包格式（footer 在尾部）。
//!
//! 设计见 docs/01-scrollz-design.md §7。逻辑文件 = 定长逻辑块序列，每块独立压缩。
//! **索引（chunk index）置于文件尾部**，紧接固定大小 footer，使追加只需在末尾增量写、
//! 重写小 footer，而**不触碰前部数据**（§1.1 追加写硬约束）。打开时先读尾部 footer
//! 定位 index → O(1) 拿到块表，无需扫全文。
//!
//! ```text
//! [magic|version]                                  ← 文件头（固定）
//! [compressed chunk 0]...[compressed chunk N]      ← 数据区
//! [compressed head-cache]                          ← 可选：首 HEAD_CACHE_BYTES 独立压缩流（仅大文件，发现读快路径）
//! [chunk_index: (offset, clen, flags) × count]     ← 索引（footer 前）
//! [footer: chunk_size|uncompressed_size|chunk_count|index_offset|crc
//!          |head_cache_offset|head_cache_clen|head_cache_rawlen|head_cache_flags] ← 尾部固定大小
//! ```
//!
//! **head 缓存（docs/02-layered-chunking.md）**：会话发现读每文件首 64KB（harness `tan`）。若整文件
//! 命中均匀大块（1MiB），读 64KB 要解压整块（16x 放大，实测 HOT 352us）。head 缓存把首
//! `HEAD_CACHE_BYTES` 单独压一份存档内（~20KB），发现读经 Core 快路径解压它（~62us）。缓存随 index/footer
//! 在每次 commit 的「元数据尾区」重写（非永久 fixture，靠相同 barrier + EOF footer fail-closed 兜底）。
//! `head_cache_* = 0` 表示无缓存（小文件 / 未触发）。项目无历史 archive，故单一格式、不背向后兼容。
//!
//! 本模块只做「格式读写」，不碰压缩（压缩在 core::codec，§2）。`ArchiveReader::read_block` /
//! `read_head_cache` 返回压缩字节 + flags，由上层 Core 解压。
//!
//! ## 子模块划分
//!
//! 本文件（`mod.rs`）只保留跨子模块共享的布局常量与类型（ChunkEntry/HeadCache/Footer），
//! 具体读写逻辑按角色拆分：
//! - [`format`]：CRC32 / 定长整数编解码 / 定位读 / 损坏错误（最底层原语）。
//! - [`superblock`]：双 superblock 提交点的编解码与 seq 择优（纯函数）。
//! - [`journal`]：尾日志记录编解码与重放。
//! - [`reader`]：`ArchiveReader` + 崩溃安全恢复读链 + index 编解码。
//! - [`writer`]：`ArchiveWriter`（离线 fixture 写）。
//! - [`updater`]：`ArchiveUpdater`（append-only 在线更新 + 原子提交）。

mod format;
mod journal;
mod reader;
mod superblock;
mod updater;
mod writer;

pub use format::crc32;
pub use journal::{replay_journal, serialize_journal_record, JOURNAL_REC_HEADER_LEN};
pub use reader::ArchiveReader;
pub use superblock::{parse_superblock, pick_active, serialize_superblock, SuperBlock, SB_MAGIC};
pub use updater::ArchiveUpdater;
pub use writer::ArchiveWriter;

/// 文件头魔数，标识布局 S 的 archive。历史值取 "ZIPFSAR1" 的字节。
// COMPAT-FROZEN: 改字节=存量归档不可读,禁止改。品牌无关。
pub const MAGIC: [u8; 8] = *b"ZIPFSAR\x01";

/// 当前格式版本。v2 = 崩溃安全双 superblock 提交协议（docs/04）。无历史 archive，不背兼容。
pub const VERSION: u32 = 2;

/// 文件头大小：magic(8) + version(4)。
pub(crate) const HEADER_LEN: u64 = 12;

/// 单个 superblock 槽的定长字节数（字段占 96B，[96..124] 零填充纳入 crc，[124..128] 为 sb_crc）。
pub const SB_LEN: u64 = 128;

/// 双 superblock 槽在文件头部的固定偏移（commit 点，docs/04 §2）。`pub`：故障注入测试按
/// 语义 offset 区间调度（docs/05 §4），SB 槽区间是稳定的格式契约。
pub const SB_A_OFFSET: u64 = HEADER_LEN;
pub const SB_B_OFFSET: u64 = HEADER_LEN + SB_LEN;
/// 数据区起点：header + 两个 superblock 槽。块/index/journal/head 缓存一律 append 到此后。
/// `pub`：故障注入测试用它界定「数据区写」注入区间（docs/05 §4）。
pub const DATA_START: u64 = HEADER_LEN + 2 * SB_LEN;

/// 单个块索引项的序列化大小：offset(8) + clen(8) + flags(4) + block_crc(4) = 24 字节。
pub(crate) const INDEX_ENTRY_LEN: u64 = 24;

/// head 缓存的发现读窗口（= harness `Rv`，docs/02 §4.4）。首版默认 64KiB。
pub const HEAD_CACHE_BYTES: u64 = 65536;

/// 块 flags 位：原样存储（不可压缩启发式置位，读时跳过解压）。
pub const FLAG_VERBATIM: u32 = 0b0000_0001;

/// 一个块在 archive 中的索引项：物理位置、压缩长度、flags、块 CRC。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkEntry {
    /// 块压缩数据在文件中的字节偏移。
    pub offset: u64,
    /// 压缩后字节长度（verbatim 时即原始长度）。
    pub clen: u64,
    /// 标志位（当前仅 FLAG_VERBATIM）。
    pub flags: u32,
    /// 该块**存储字节**（压缩/verbatim 后）的 CRC32：read_block 校验封块静默错读（ROADMAP T1）。
    /// 仅覆盖封块；尾日志另有 rec_crc，head 缓存属可丢弃派生数据（M2 不 fail-closed）。
    pub block_crc: u32,
}

impl ChunkEntry {
    /// 是否原样存储（不需解压）。
    pub fn is_verbatim(&self) -> bool {
        self.flags & FLAG_VERBATIM != 0
    }
}

/// head 缓存（发现读快路径）：首 `rawlen` 逻辑字节的独立压缩流在 archive 中的位置。
///
/// `rawlen == min(HEAD_CACHE_BYTES, uncompressed_size)`，**不行对齐**（恒满窗口，避免部分命中）。
/// `verbatim` 同块语义：不可压缩则原样存、读时跳过解压。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadCache {
    /// 压缩流在文件中的字节偏移。
    pub offset: u64,
    /// 压缩后字节长度（verbatim 时即原始长度）。
    pub clen: u64,
    /// 解压后逻辑字节数（= 覆盖的前缀长度）。
    pub rawlen: u64,
    /// 是否原样存储。
    pub verbatim: bool,
}

/// archive 的 footer（解析后的视图）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    /// 逻辑块大小（定长，末块可不足）。
    pub chunk_size: u32,
    /// 逻辑（解压后）文件总大小。
    pub uncompressed_size: u64,
    /// 块数量。
    pub chunk_count: u64,
    /// 索引区起始偏移。
    pub index_offset: u64,
    /// 索引区的 CRC32，开包时校验，及早发现尾部损坏。
    pub crc: u32,
    /// head 缓存（无则 None）。发现读快路径用，docs/02。
    pub head_cache: Option<HeadCache>,
}

/// 跨子模块共享的测试辅助（仅 `#[cfg(test)]` 编译）：用公开 API 构造 fixture 字节 / 读活跃 SB /
/// 改写双槽。reader 与 updater 的测试模块共用（避免重复实现，且保证两侧对 SB 布局一致理解）。
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::io::Cursor;

    /// 用内存 Cursor 写一个 archive，返回字节缓冲。
    pub(crate) fn build_archive(chunk_size: u32, blocks: &[(Vec<u8>, bool, u64)]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut w = ArchiveWriter::new(cursor, chunk_size).unwrap();
        for (bytes, verbatim, raw_len) in blocks {
            w.append_block(bytes, *verbatim, *raw_len).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// 读活跃槽（seq 较大者）的 SuperBlock（测试辅助）。
    pub(crate) fn active_sb(buf: &[u8]) -> SuperBlock {
        let a = parse_superblock(&buf[SB_A_OFFSET as usize..]);
        let b = parse_superblock(&buf[SB_B_OFFSET as usize..]);
        match (a, b) {
            (Some(x), Some(y)) => {
                if y.seq > x.seq {
                    y
                } else {
                    x
                }
            }
            (Some(x), None) => x,
            (None, Some(y)) => y,
            (None, None) => panic!("两槽皆不可解析"),
        }
    }

    /// 用闭包改 SuperBlock 后，重新序列化写回 A、B 两槽（保持 sb_crc 自洽，测试辅助）。
    pub(crate) fn patch_both_sb(buf: &mut [u8], f: impl Fn(&mut SuperBlock)) {
        let mut sb = active_sb(buf);
        f(&mut sb);
        let b = serialize_superblock(&sb);
        buf[SB_A_OFFSET as usize..][..SB_LEN as usize].copy_from_slice(&b);
        buf[SB_B_OFFSET as usize..][..SB_LEN as usize].copy_from_slice(&b);
    }
}
