//! 布局 S 的每文件分块压缩包格式（footer 在尾部）。
//!
//! 设计见 docs/01-zipfs-design.md §7。逻辑文件 = 定长逻辑块序列，每块独立压缩。
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

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// 文件头魔数，标识 zipfs 布局 S 的 archive。取 "ZIPFSAR1" 的字节。
pub const MAGIC: [u8; 8] = *b"ZIPFSAR\x01";

/// 当前格式版本。v2 = 崩溃安全双 superblock 提交协议（docs/04）。无历史 archive，不背兼容。
pub const VERSION: u32 = 2;

/// 文件头大小：magic(8) + version(4)。
const HEADER_LEN: u64 = 12;

/// 双 superblock 槽在文件头部的固定偏移（commit 点，docs/04 §2）。
const SB_A_OFFSET: u64 = HEADER_LEN;
const SB_B_OFFSET: u64 = HEADER_LEN + SB_LEN;
/// 数据区起点：header + 两个 superblock 槽。块/index/journal/head 缓存一律 append 到此后。
const DATA_START: u64 = HEADER_LEN + 2 * SB_LEN;

/// 单个块索引项的序列化大小：offset(8) + clen(8) + flags(4) = 20 字节。
const INDEX_ENTRY_LEN: u64 = 20;

/// head 缓存的发现读窗口（= harness `Rv`，docs/02 §4.4）。首版默认 64KiB。
pub const HEAD_CACHE_BYTES: u64 = 65536;

/// 块 flags 位：原样存储（不可压缩启发式置位，读时跳过解压）。
pub const FLAG_VERBATIM: u32 = 0b0000_0001;

/// 一个块在 archive 中的索引项：物理位置、压缩长度、flags。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkEntry {
    /// 块压缩数据在文件中的字节偏移。
    pub offset: u64,
    /// 压缩后字节长度（verbatim 时即原始长度）。
    pub clen: u64,
    /// 标志位（当前仅 FLAG_VERBATIM）。
    pub flags: u32,
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
// SuperBlock：崩溃安全提交协议的原子提交点（docs/04 §2.1/§4，TDD §8.1）
// ===========================================================================
//
// 两个定长 superblock 槽固定在文件头部（header 之后），交替写、带单调 seq + CRC。
// open 取「sb_magic+sb_crc 通过且 seq 最大」者（完整「槽可用」级联校验——再加 index_crc +
// 尾日志可重放——在 ArchiveReader::open 层做，见 docs/04 §4 M4）。本节只做 superblock 自身的
// 编解码与 seq 择优（纯函数，可隔离测试）。

/// superblock 魔数（"ZSB2" 小端），区分未初始化/损坏槽。
pub const SB_MAGIC: u32 = u32::from_le_bytes(*b"ZSB2");
/// 单个 superblock 槽的定长字节数（字段占 96B，[96..124] 零填充纳入 crc，[124..128] 为 sb_crc）。
pub const SB_LEN: u64 = 128;
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

#[inline]
fn put_u32(buf: &mut [u8], p: &mut usize, v: u32) {
    buf[*p..*p + 4].copy_from_slice(&v.to_le_bytes());
    *p += 4;
}
#[inline]
fn put_u64(buf: &mut [u8], p: &mut usize, v: u64) {
    buf[*p..*p + 8].copy_from_slice(&v.to_le_bytes());
    *p += 8;
}
#[inline]
fn get_u32(buf: &[u8], p: &mut usize) -> u32 {
    let v = u32::from_le_bytes(buf[*p..*p + 4].try_into().unwrap());
    *p += 4;
    v
}
#[inline]
fn get_u64(buf: &[u8], p: &mut usize) -> u64 {
    let v = u64::from_le_bytes(buf[*p..*p + 8].try_into().unwrap());
    *p += 8;
    v
}

// ===========================================================================
// 尾日志记录：未封尾块的原始字节增量（docs/04 §2.2/§4.4，TDD §8.2）
// ===========================================================================
//
// 每次 fsync 把「自上次 fsync 以来新追加的原始字节」作为一条记录 append 到尾日志区。
// 记录格式：[rec_len(4) | rec_crc(4) | raw_bytes(rec_len)]。重放 = 顺序拼接全部完整记录的
// payload（= 未封尾块的全量原始字节）。遇不完整/损坏记录即停（最近完整前缀，fail-closed 截断
// 尾部半截写）；rec_len 先与剩余字节 bounds 校验，防越界/OOM（H1）。

/// 尾日志记录头长度：rec_len(4) + rec_crc(4)。
pub const JOURNAL_REC_HEADER_LEN: usize = 8;

/// 序列化一条尾日志记录。`rec_crc = crc32(raw)`。
pub fn serialize_journal_record(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(JOURNAL_REC_HEADER_LEN + raw.len());
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32(raw).to_le_bytes());
    out.extend_from_slice(raw);
    out
}

/// 重放尾日志区，返回拼接的原始字节。遇不完整（半截头/半截 payload，rec_len 越界）或 rec_crc
/// 不符即停 = 最近完整前缀（docs/04 §4.4 H1：「遇坏即停」仅在不变量保证损坏必在尾部时正确）。
pub fn replay_journal(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + JOURNAL_REC_HEADER_LEN <= buf.len() {
        let rec_len = u32::from_le_bytes(buf[p..p + 4].try_into().unwrap()) as usize;
        let rec_crc = u32::from_le_bytes(buf[p + 4..p + 8].try_into().unwrap());
        let data_start = p + JOURNAL_REC_HEADER_LEN;
        // bounds：payload 必须完整落在 buf 内（防越界读 / 巨值 rec_len 截断尾部）。
        let Some(data_end) = data_start.checked_add(rec_len) else {
            break;
        };
        if data_end > buf.len() {
            break;
        }
        let payload = &buf[data_start..data_end];
        if crc32(payload) != rec_crc {
            break; // 损坏 → 停（截到上一条完整记录）。
        }
        out.extend_from_slice(payload);
        p = data_end;
    }
    out
}

// ===========================================================================
// 小工具：定长整数读写（小端），集中显式错误处理
// ===========================================================================

/// 定位读（pread）：用绝对偏移读，**不移动文件游标**。这让同一只读 `File`（如缓存的
/// `ArchiveReader`）可被多线程并发 `read_block` 而不发生 seek 竞争（fuser 多线程派发，
/// reader 缓存按 `Arc` 共享，见 store::shadow 的 per-fh reader 缓存）。
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    file.read_exact_at(buf, offset)
}

// ===========================================================================
// ArchiveReader：open → 读 footer/index；read_block(idx) → 压缩字节 + flags
// ===========================================================================

/// 只读 archive：打开即读双 superblock 选活跃 + 索引，后续 `read_block` O(1) 定位。
pub struct ArchiveReader {
    file: File,
    footer: Footer,
    index: Vec<ChunkEntry>,
    /// 未封尾块的尾日志区 `(offset, len)`（无则 None）。`read_tail` 重放它（docs/04 §8.4）。
    tail_journal: Option<(u64, u64)>,
}

impl ArchiveReader {
    /// 打开一个 archive 文件：校验 magic/version → 读尾部 footer → 读索引 → 校验 CRC。
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        Self::from_file(file)
    }

    /// 从已打开的 `File` 构造（测试与内部复用）。
    ///
    /// v2：校验 header → 读两个 superblock 槽 → 级联取活跃（seq 最大且 index 校验通过）→ 持 index。
    pub fn from_file(file: File) -> io::Result<Self> {
        let total_len = file.metadata()?.len();
        if total_len < DATA_START {
            return Err(corrupt("文件太小，不足 header + 双 superblock"));
        }
        let mut header = [0u8; HEADER_LEN as usize];
        read_exact_at(&file, &mut header, 0)?;
        if header[..8] != MAGIC {
            return Err(corrupt("magic 不匹配，非 zipfs archive"));
        }
        let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(corrupt(&format!("不支持的 archive 版本：{version}")));
        }
        let (sb, _active_off, index) = load_active(&file, total_len)?;
        let tail_journal = if sb.tail_journal_len > 0 {
            Some((sb.tail_journal_offset, sb.tail_journal_len))
        } else {
            None
        };
        Ok(Self {
            file,
            footer: footer_from_sb(&sb),
            index,
            tail_journal,
        })
    }

    /// 重放尾日志，返回未封尾块的**原始字节**（无未封尾则 None）。区间已在 `load_active` 校验 bounds，
    /// 故可信。上层把它当作「块 chunk_count」的 verbatim 尾块（docs/04 §8.4）。
    pub fn read_tail(&self) -> io::Result<Option<Vec<u8>>> {
        let Some((off, len)) = self.tail_journal else {
            return Ok(None);
        };
        let mut buf = vec![0u8; len as usize];
        read_exact_at(&self.file, &mut buf, off)?;
        Ok(Some(replay_journal(&buf)))
    }

    /// footer 视图（含 uncompressed_size / chunk_size，供 getattr 复用）。
    pub fn footer(&self) -> &Footer {
        &self.footer
    }

    /// 块数量。
    pub fn chunk_count(&self) -> u64 {
        self.footer.chunk_count
    }

    /// 第 `idx` 个块的索引项。
    pub fn entry(&self, idx: u64) -> Option<ChunkEntry> {
        self.index.get(idx as usize).copied()
    }

    /// 读第 `idx` 个块的**压缩字节**（不解压）+ 其索引项。
    ///
    /// 越界返回 `Ok(None)`（调用方据语义决定是零填充还是错误）。返回的字节是否需解压
    /// 由 `ChunkEntry::is_verbatim()` 决定，解压交给 Core（§2）。
    pub fn read_block(&self, idx: u64) -> io::Result<Option<(Vec<u8>, ChunkEntry)>> {
        let Some(entry) = self.entry(idx) else {
            return Ok(None);
        };
        let mut buf = vec![0u8; entry.clen as usize];
        read_exact_at(&self.file, &mut buf, entry.offset)?;
        Ok(Some((buf, entry)))
    }

    /// head 缓存覆盖的逻辑前缀字节数（无缓存则 0）。供 Core 读快路径判定 `off+len <= rawlen`。
    pub fn head_cache_rawlen(&self) -> u64 {
        self.footer.head_cache.map(|h| h.rawlen).unwrap_or(0)
    }

    /// 读 head 缓存的**压缩字节**（不解压）+ verbatim 标志；无缓存返回 `Ok(None)`。
    ///
    /// head 缓存是**可丢弃派生数据**（docs/04 §11 M2）：指针越界 / 溢出 → 当作无缓存返回 None
    /// （优雅回退逐块路径），绝不 fail-closed 整个文件。解压交给 Core（§2）。
    pub fn read_head_cache(&self) -> io::Result<Option<(Vec<u8>, bool)>> {
        let Some(hc) = self.footer.head_cache else {
            return Ok(None);
        };
        match read_head_cache_bytes(&self.file, &self.footer, hc) {
            Some(bytes) => Ok(Some((bytes, hc.verbatim))),
            None => Ok(None),
        }
    }
}

// ---- v2 superblock / 数据区读取 helper（崩溃安全提交协议，docs/04 §12）----

/// 从活跃 superblock 派生 `Footer` 视图（兼容旧调用面）。
fn footer_from_sb(sb: &SuperBlock) -> Footer {
    Footer {
        chunk_size: sb.chunk_size,
        uncompressed_size: sb.uncompressed_size,
        chunk_count: sb.chunk_count,
        index_offset: sb.index_offset,
        crc: sb.index_crc,
        head_cache: sb.head_cache,
    }
}

/// 读一个 superblock 槽并解析；magic/crc 不符（未初始化/半截写/损坏）→ None。
fn read_sb_slot(file: &File, off: u64) -> io::Result<Option<SuperBlock>> {
    let mut buf = [0u8; SB_LEN as usize];
    read_exact_at(file, &mut buf, off)?;
    Ok(parse_superblock(&buf))
}

/// 级联校验并加载活跃 superblock：读两槽 → 候选按 seq 降序 → 逐个验证 index（bounds + index_crc +
/// 块 bounds）→ 取首个通过者，返回 `(活跃 SB, 活跃槽偏移, index)`；两槽皆不可用 → corrupt（M4）。
fn load_active(file: &File, total_len: u64) -> io::Result<(SuperBlock, u64, Vec<ChunkEntry>)> {
    let mut cands: Vec<(SuperBlock, u64)> = Vec::with_capacity(2);
    if let Some(sb) = read_sb_slot(file, SB_A_OFFSET)? {
        cands.push((sb, SB_A_OFFSET));
    }
    if let Some(sb) = read_sb_slot(file, SB_B_OFFSET)? {
        cands.push((sb, SB_B_OFFSET));
    }
    cands.sort_by_key(|c| std::cmp::Reverse(c.0.seq)); // seq 降序
    for (sb, off) in cands {
        if let Some(index) = validate_and_load_index(file, &sb, total_len)? {
            return Ok((sb, off, index));
        }
    }
    Err(corrupt(
        "两个 superblock 槽均不可用（皆缺/损坏/index 校验失败）",
    ))
}

/// 校验并加载某 superblock 的 index 区；任一校验失败 → Ok(None)（让 load_active 回落另一槽）。
/// io 错误 → Err。校验：chunk_size!=0、index 区 bounds、index_crc、每块在数据区内。
fn validate_and_load_index(
    file: &File,
    sb: &SuperBlock,
    total_len: u64,
) -> io::Result<Option<Vec<ChunkEntry>>> {
    if sb.chunk_size == 0 {
        return Ok(None);
    }
    // index 区 bounds：长度 == chunk_count*ENTRY_LEN，且 [DATA_START, index_offset+index_len] ⊆ 文件。
    let expect_len = match sb.chunk_count.checked_mul(INDEX_ENTRY_LEN) {
        Some(v) => v,
        None => return Ok(None),
    };
    if sb.index_len != expect_len || sb.index_offset < DATA_START {
        return Ok(None);
    }
    let index_end = match sb.index_offset.checked_add(sb.index_len) {
        Some(v) => v,
        None => return Ok(None),
    };
    if index_end > total_len {
        return Ok(None);
    }
    let mut index_bytes = vec![0u8; sb.index_len as usize];
    read_exact_at(file, &mut index_bytes, sb.index_offset)?;
    if crc32(&index_bytes) != sb.index_crc {
        return Ok(None);
    }
    let index = parse_index(&index_bytes, sb.chunk_count as usize);
    // 每块在数据区 [DATA_START, index_offset) 内自洽（防 read_block 据不可信 clen OOM/越界）。
    let data_end = sb.index_offset;
    for e in &index {
        let end = match e.offset.checked_add(e.clen) {
            Some(v) => v,
            None => return Ok(None),
        };
        if e.offset < DATA_START || end > data_end {
            return Ok(None);
        }
    }
    // 尾日志区 bounds（docs/04 §8.4）：在 [DATA_START, total_len] 内，使 read_tail 可信。
    if sb.tail_journal_len > 0 {
        let jend = match sb.tail_journal_offset.checked_add(sb.tail_journal_len) {
            Some(v) => v,
            None => return Ok(None),
        };
        if sb.tail_journal_offset < DATA_START || jend > total_len {
            return Ok(None);
        }
    }
    Ok(Some(index))
}

/// 读 head 缓存压缩字节，越界/溢出 → None（可丢弃派生数据，M2）。`footer` 提供 index_offset 上界。
fn read_head_cache_bytes(file: &File, footer: &Footer, hc: HeadCache) -> Option<Vec<u8>> {
    let end = hc.offset.checked_add(hc.clen)?;
    if hc.offset < DATA_START || end > footer.index_offset {
        return None; // 越界 → 当作无缓存（优雅回退）。
    }
    let mut buf = vec![0u8; hc.clen as usize];
    read_exact_at(file, &mut buf, hc.offset).ok()?;
    Some(buf)
}

/// 解析索引区字节为 `ChunkEntry` 列表。
fn parse_index(bytes: &[u8], count: usize) -> Vec<ChunkEntry> {
    (0..count)
        .map(|i| {
            let base = i * INDEX_ENTRY_LEN as usize;
            let offset = u64::from_le_bytes(bytes[base..base + 8].try_into().unwrap());
            let clen = u64::from_le_bytes(bytes[base + 8..base + 16].try_into().unwrap());
            let flags = u32::from_le_bytes(bytes[base + 16..base + 20].try_into().unwrap());
            ChunkEntry {
                offset,
                clen,
                flags,
            }
        })
        .collect()
}

/// 序列化索引区为字节（与 `parse_index` 对偶）。Writer/Updater 共用，避免布局漂移。
fn serialize_index(index: &[ChunkEntry]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(index.len() * INDEX_ENTRY_LEN as usize);
    for e in index {
        bytes.extend_from_slice(&e.offset.to_le_bytes());
        bytes.extend_from_slice(&e.clen.to_le_bytes());
        bytes.extend_from_slice(&e.flags.to_le_bytes());
    }
    bytes
}

/// 把一个 superblock 写到指定槽偏移（seek + 写定长 SB_LEN 字节）。Writer/Updater 共用。
fn write_superblock_slot<W: Write + Seek>(w: &mut W, off: u64, sb: &SuperBlock) -> io::Result<()> {
    let bytes = serialize_superblock(sb);
    w.seek(SeekFrom::Start(off))?;
    w.write_all(&bytes)?;
    Ok(())
}

// ===========================================================================
// ArchiveWriter：从已压缩块写出 footer 布局（仅供离线 fixture 工具）
// ===========================================================================

/// 写 archive：先写 header，逐块 append 压缩数据并累积索引，`finish` 写索引 + footer。
///
/// 仅供离线 fixture 工具用（P1 无在线写路径）。块的压缩由调用方先用 core::codec 完成，
/// 这里只负责布局：header → 数据区 → 索引 → footer。
pub struct ArchiveWriter<W: Write + Seek> {
    inner: W,
    chunk_size: u32,
    uncompressed_size: u64,
    /// 下一个块写入位置（当前文件长度）。
    cursor: u64,
    index: Vec<ChunkEntry>,
    /// 可选 head 缓存：(已压缩字节, verbatim, 解压后逻辑长度)。`finish` 时写在索引之前。
    head_cache: Option<(Vec<u8>, bool, u64)>,
}

impl ArchiveWriter<File> {
    /// 在 `path` 创建新 archive。
    pub fn create(path: &Path, chunk_size: u32) -> io::Result<Self> {
        let file = File::create(path)?;
        Self::new(file, chunk_size)
    }
}

impl<W: Write + Seek> ArchiveWriter<W> {
    /// 用任意 `Write + Seek` 构造，写出 header + 两个 superblock 占位槽（finish 时填真实 SB）。
    pub fn new(mut inner: W, chunk_size: u32) -> io::Result<Self> {
        if chunk_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "chunk_size 不能为 0",
            ));
        }
        inner.seek(SeekFrom::Start(0))?;
        inner.write_all(&MAGIC)?;
        inner.write_all(&VERSION.to_le_bytes())?;
        // 两个 superblock 槽占位（共 2*SB_LEN 字节零）；finish 写真实 SB。数据从 DATA_START 起。
        inner.write_all(&[0u8; (2 * SB_LEN) as usize])?;
        Ok(Self {
            inner,
            chunk_size,
            uncompressed_size: 0,
            cursor: DATA_START,
            index: Vec::new(),
            head_cache: None,
        })
    }

    /// 设置 head 缓存（发现读快路径，docs/02）。`stored_bytes` 是 core::codec 对首
    /// `min(HEAD_CACHE_BYTES, 文件大小)` 字节的压缩输出；`verbatim` 为不可压缩 flag；
    /// `raw_len` 是覆盖的逻辑前缀长度。`finish` 时写在索引之前。
    pub fn set_head_cache(&mut self, stored_bytes: Vec<u8>, verbatim: bool, raw_len: u64) {
        self.head_cache = Some((stored_bytes, verbatim, raw_len));
    }

    /// 追加一个**已压缩**块。
    ///
    /// `stored_bytes` 是 core::codec::compress 的输出；`verbatim` 为不可压缩 flag；
    /// `raw_len` 是该块解压后长度（用于累加 uncompressed_size，末块可不足 chunk_size）。
    pub fn append_block(
        &mut self,
        stored_bytes: &[u8],
        verbatim: bool,
        raw_len: u64,
    ) -> io::Result<()> {
        self.inner.write_all(stored_bytes)?;
        let flags = if verbatim { FLAG_VERBATIM } else { 0 };
        self.index.push(ChunkEntry {
            offset: self.cursor,
            clen: stored_bytes.len() as u64,
            flags,
        });
        self.cursor += stored_bytes.len() as u64;
        self.uncompressed_size += raw_len;
        Ok(())
    }

    /// 写出 [head 缓存] + 索引区，再把 superblock（seq=0）写到 A、B 两槽，收尾。
    pub fn finish(mut self) -> io::Result<W> {
        // head 缓存（若有）写在索引之前、数据块之后（docs/02 §4.1 布局）。
        let head_cache = match &self.head_cache {
            Some((bytes, verbatim, raw_len)) => {
                let offset = self.cursor;
                self.inner.write_all(bytes)?;
                self.cursor += bytes.len() as u64;
                Some(HeadCache {
                    offset,
                    clen: bytes.len() as u64,
                    rawlen: *raw_len,
                    verbatim: *verbatim,
                })
            }
            None => None,
        };

        let index_offset = self.cursor;
        let index_bytes = serialize_index(&self.index);
        let index_crc = crc32(&index_bytes);
        self.inner.write_all(&index_bytes)?;

        let sb = SuperBlock {
            seq: 0,
            chunk_size: self.chunk_size,
            uncompressed_size: self.uncompressed_size,
            chunk_count: self.index.len() as u64,
            index_offset,
            index_len: index_bytes.len() as u64,
            index_crc,
            tail_journal_offset: 0,
            tail_journal_len: 0,
            head_cache,
        };
        // 新建 archive：两槽写同一 seq=0 SB（pick_active tie → A；后续 Updater 写 B=seq1 翻转）。
        write_superblock_slot(&mut self.inner, SB_A_OFFSET, &sb)?;
        write_superblock_slot(&mut self.inner, SB_B_OFFSET, &sb)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// 构造一个 InvalidData 错误，带统一前缀，便于排查。
fn corrupt(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("archive 损坏：{msg}"))
}

// ===========================================================================
// ArchiveUpdater：在已存在 archive 上 append-only 更新 + 双 superblock 原子提交
// （崩溃安全提交协议，docs/04 §12）
// ===========================================================================
//
// 核心不变量（C2）：**写游标恒取物理 EOF**，新块/index/head 缓存一律 append 到文件末尾——
// 绝不 `set_len` 截 live 数据、绝不原地覆盖任何 superblock 可达区间（由构造满足 C2）。
// 这彻底删除了旧 `reuse_tail_slot` + `set_len`（原 durability bug 的发源地）。
// 提交点 = 交替写两个固定 superblock 槽之一（带 seq+CRC），半截写总留另一槽完好 → 永远可恢复。
// 被取代的旧块/旧 index 成空洞，仅压实回收（在线写从不就地覆盖）。

/// 在已存在 archive 上 append-only 更新。`set_block`/`truncate` 改内存 index 并把新块写到 EOF，
/// `commit` append 新 index + 写非活跃 superblock 槽（双段 barrier）。
pub struct ArchiveUpdater {
    file: File,
    chunk_size: u32,
    index: Vec<ChunkEntry>,
    uncompressed_size: u64,
    /// 写游标 = 物理 EOF（append-only，C2 安全）。
    write_cursor: u64,
    /// head 缓存：(已压缩字节, verbatim, 解压后逻辑长度)。随每次 commit append 到 EOF。
    head_cache: Option<(Vec<u8>, bool, u64)>,
    /// 活跃 superblock 的 seq（commit 写 seq+1 到非活跃槽）。
    active_seq: u64,
    /// 下次 commit 要写的（非活跃）superblock 槽偏移（A/B 交替）。
    inactive_off: u64,
    /// 上次 full commit 落盘的 index 描述符 `(offset, len, crc)`。`commit_journal` 复用它（块集未变时
    /// index 稳定），使尾日志记录保持连续。`commit` 写新 index 后更新它。
    committed_index: (u64, u64, u32),
    /// 上次 commit 落盘的 head 缓存描述符（`commit_journal` 复用，不重写）。
    committed_head_cache: Option<HeadCache>,
    /// 尾日志（未封尾块的原始字节增量，docs/04 §8.4）：本封块周期内首条记录的偏移（None=无未封尾）。
    journal_offset: Option<u64>,
    /// 尾日志区累计字节长度（含记录头）。commit 写入 SB 的 `tail_journal_len`。
    journal_len: u64,
}

impl ArchiveUpdater {
    /// 打开已存在的 archive 供更新（读写）。空/缺文件请先用 `ArchiveWriter` 建一个 0 块 archive。
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let total_len = file.metadata()?.len();
        if total_len < DATA_START {
            return Err(corrupt("文件太小，不足 header + 双 superblock"));
        }
        let mut header = [0u8; HEADER_LEN as usize];
        read_exact_at(&file, &mut header, 0)?;
        if header[..8] != MAGIC || u32::from_le_bytes(header[8..12].try_into().unwrap()) != VERSION
        {
            return Err(corrupt("非 v2 archive"));
        }
        let (sb, active_off, index) = load_active(&file, total_len)?;
        let footer = footer_from_sb(&sb);
        // 载入既有 head 缓存字节（越界 → None，可丢弃派生数据 M2）。
        let head_cache = sb.head_cache.and_then(|hc| {
            read_head_cache_bytes(&file, &footer, hc).map(|b| (b, hc.verbatim, hc.rawlen))
        });
        let inactive_off = if active_off == SB_A_OFFSET {
            SB_B_OFFSET
        } else {
            SB_A_OFFSET
        };
        Ok(Self {
            file,
            chunk_size: sb.chunk_size,
            index,
            uncompressed_size: sb.uncompressed_size,
            write_cursor: total_len, // EOF：append-only，C2 安全
            head_cache,
            active_seq: sb.seq,
            inactive_off,
            committed_index: (sb.index_offset, sb.index_len, sb.index_crc),
            committed_head_cache: sb.head_cache,
            journal_offset: if sb.tail_journal_len > 0 {
                Some(sb.tail_journal_offset)
            } else {
                None
            },
            journal_len: sb.tail_journal_len,
        })
    }

    /// 翻转活跃槽（commit / commit_journal 收尾共用）。
    fn flip_active(&mut self, new_seq: u64) {
        self.active_seq = new_seq;
        self.inactive_off = if self.inactive_off == SB_A_OFFSET {
            SB_B_OFFSET
        } else {
            SB_A_OFFSET
        };
    }

    /// 仅提交尾日志（fsync 路径，docs/04 §8.4）：**不重写 index**（块集未变 → index 稳定，
    /// 复用上次 full commit 的 index_offset，使 journal 记录保持连续），只更新 SB 的尾日志指针。
    /// 调用前应已 `append_journal`。双段 barrier 同 `commit`。**契约**：自上次 full commit 起未发生
    /// `set_block`/`truncate`（否则 index 已变、必须走 `commit`）。
    pub fn commit_journal(&mut self) -> io::Result<()> {
        // barrier 1：journal 记录已落盘（append_journal 已写，这里确保 durable）。
        self.file.sync_all()?;
        let (index_offset, index_len, index_crc) = self.committed_index;
        let new_seq = self.active_seq + 1;
        let sb = SuperBlock {
            seq: new_seq,
            chunk_size: self.chunk_size,
            uncompressed_size: self.uncompressed_size,
            chunk_count: self.index.len() as u64,
            index_offset,
            index_len,
            index_crc,
            tail_journal_offset: self.journal_offset.unwrap_or(0),
            tail_journal_len: self.journal_len,
            head_cache: self.committed_head_cache,
        };
        write_superblock_slot(&mut self.file, self.inactive_off, &sb)?;
        self.file.sync_all()?; // barrier 2
        self.flip_active(new_seq);
        Ok(())
    }

    /// 追加一条尾日志记录（未封尾块的原始字节增量，docs/04 §8.4）。**不压缩、不动 index**——
    /// fsync 路径调用，成本 O(delta)。首条记录确立 journal 区起点。commit 时写入 SB 的尾日志指针。
    pub fn append_journal(&mut self, raw_delta: &[u8]) -> io::Result<()> {
        let rec = serialize_journal_record(raw_delta);
        if self.journal_offset.is_none() {
            self.journal_offset = Some(self.write_cursor);
        }
        self.file.seek(SeekFrom::Start(self.write_cursor))?;
        self.file.write_all(&rec)?;
        self.write_cursor += rec.len() as u64;
        self.journal_len += rec.len() as u64;
        Ok(())
    }

    /// 重置尾日志（封块时调用）：新 SB 的 `tail_journal_len=0`，旧 journal 记录成空洞（压实回收，
    /// **不物理清零** H2）。调用顺序：先 `set_block` 把封块写入，再 `reset_journal`，再 `commit`。
    pub fn reset_journal(&mut self) {
        self.journal_offset = None;
        self.journal_len = 0;
    }

    /// 设置 / 更新 head 缓存（块 0 首次封存或头区 RMW 后由上层调用，docs/02 §4.3）。
    pub fn set_head_cache(&mut self, stored_bytes: Vec<u8>, verbatim: bool, raw_len: u64) {
        self.head_cache = Some((stored_bytes, verbatim, raw_len));
    }

    /// 当前 head 缓存覆盖的逻辑前缀长度（无则 0）。
    pub fn head_cache_rawlen(&self) -> u64 {
        self.head_cache.as_ref().map(|(_, _, r)| *r).unwrap_or(0)
    }

    /// 当前块数。
    pub fn chunk_count(&self) -> u64 {
        self.index.len() as u64
    }

    /// chunk_size。
    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// 逻辑大小。
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// 写第 `idx` 块的新内容：**恒把压缩字节 append 到 EOF**（write_cursor），index[idx] 改指新位置。
    /// 旧块物理位置成空洞（压实回收）。`idx` 大于当前块数时用零块补齐缺口（稀疏空洞物化）。
    pub fn set_block(
        &mut self,
        idx: u64,
        stored_bytes: &[u8],
        verbatim: bool,
        new_size: u64,
    ) -> io::Result<()> {
        // 缺口补零块（[count, idx)）。
        if (self.index.len() as u64) < idx {
            let zeros = vec![0u8; self.chunk_size as usize];
            while (self.index.len() as u64) < idx {
                let offset = self.write_cursor;
                self.file.seek(SeekFrom::Start(offset))?;
                self.file.write_all(&zeros)?;
                self.write_cursor += zeros.len() as u64;
                self.index.push(ChunkEntry {
                    offset,
                    clen: zeros.len() as u64,
                    flags: FLAG_VERBATIM,
                });
            }
        }
        // 恒 append 到 EOF（append-only，C2：绝不覆写任何 SB 可达区间，删除 reuse/set_len）。
        let offset = self.write_cursor;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(stored_bytes)?;
        self.write_cursor = offset + stored_bytes.len() as u64;
        let entry = ChunkEntry {
            offset,
            clen: stored_bytes.len() as u64,
            flags: if verbatim { FLAG_VERBATIM } else { 0 },
        };
        let count = self.index.len() as u64;
        if idx == count {
            self.index.push(entry);
        } else {
            self.index[idx as usize] = entry; // 旧块成空洞
        }
        self.uncompressed_size = new_size;
        Ok(())
    }

    /// 截断到 `keep_from` 块（丢弃其后所有块）+ 设新逻辑大小（仅改内存 index，旧块成空洞）。
    pub fn truncate(&mut self, keep_from: u64, new_size: u64) {
        if (keep_from as usize) < self.index.len() {
            self.index.truncate(keep_from as usize);
        }
        self.uncompressed_size = new_size;
    }

    /// 原子提交（docs/04 §3）：append [head 缓存] + 新 index 到 EOF → **barrier 1 fsync** →
    /// 写非活跃 superblock 槽（seq+1）→ **barrier 2 fsync（原子提交点）** → 翻转活跃槽。
    pub fn commit(&mut self) -> io::Result<()> {
        // 1) append head 缓存（若有）+ 新 index 到 EOF。
        self.file.seek(SeekFrom::Start(self.write_cursor))?;
        let head_cache = match &self.head_cache {
            Some((bytes, verbatim, raw_len)) => {
                let offset = self.write_cursor;
                self.file.write_all(bytes)?;
                self.write_cursor += bytes.len() as u64;
                Some(HeadCache {
                    offset,
                    clen: bytes.len() as u64,
                    rawlen: *raw_len,
                    verbatim: *verbatim,
                })
            }
            None => None,
        };
        let index_offset = self.write_cursor;
        let index_bytes = serialize_index(&self.index);
        let index_crc = crc32(&index_bytes);
        self.file.seek(SeekFrom::Start(index_offset))?;
        self.file.write_all(&index_bytes)?;
        self.write_cursor += index_bytes.len() as u64;
        // barrier 1：数据 + index 落盘（检查返回值；失败则不写/不推进 superblock，旧活跃槽不受损）。
        self.file.sync_all()?;

        // 2) 写非活跃 superblock 槽（seq+1）= 原子提交点。
        let new_seq = self.active_seq + 1;
        let sb = SuperBlock {
            seq: new_seq,
            chunk_size: self.chunk_size,
            uncompressed_size: self.uncompressed_size,
            chunk_count: self.index.len() as u64,
            index_offset,
            index_len: index_bytes.len() as u64,
            index_crc,
            tail_journal_offset: self.journal_offset.unwrap_or(0),
            tail_journal_len: self.journal_len,
            head_cache,
        };
        write_superblock_slot(&mut self.file, self.inactive_off, &sb)?;
        // barrier 2：superblock 落盘 → 新版本原子生效。
        self.file.sync_all()?;

        // 更新已提交 index / head 缓存描述符（供后续 commit_journal 复用），翻转活跃槽。
        self.committed_index = (index_offset, index_bytes.len() as u64, index_crc);
        self.committed_head_cache = head_cache;
        self.flip_active(new_seq);
        Ok(())
    }

    /// fsync 后端文件（commit 已含 barrier，保留兼容 shadow 的 commit→sync 调用序）。
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
    fn superblock_round_trip_无head缓存() {
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
    fn superblock_round_trip_带head缓存() {
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
    fn superblock_crc_检出任意单字节翻转() {
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
    fn superblock_坏magic_返回none() {
        let mut bytes = serialize_superblock(&sample_sb(1, None));
        bytes[0] ^= 0xFF; // 破坏 magic
        assert_eq!(parse_superblock(&bytes), None);
    }

    #[test]
    fn superblock_短buffer_返回none() {
        assert_eq!(parse_superblock(&[0u8; 10]), None);
        assert_eq!(parse_superblock(&[]), None);
    }

    #[test]
    fn pick_active_取seq最大者_与顺序无关() {
        let a = sample_sb(5, None);
        let b = sample_sb(3, None);
        assert_eq!(pick_active(Some(a), Some(b)).unwrap().seq, 5);
        assert_eq!(pick_active(Some(b), Some(a)).unwrap().seq, 5);
    }

    #[test]
    fn pick_active_一槽损坏取另一槽_皆坏取none() {
        let a = sample_sb(9, None);
        assert_eq!(pick_active(Some(a), None), Some(a));
        assert_eq!(pick_active(None, Some(a)), Some(a));
        assert_eq!(pick_active(None, None), None);
    }

    #[test]
    fn pick_active_seq相等取a_确定性() {
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

    // ---- 尾日志记录编解码 + 重放（docs/04 §8.2，TDD）----

    #[test]
    fn journal_单条_round_trip() {
        let raw = b"hello world payload";
        let rec = serialize_journal_record(raw);
        assert_eq!(rec.len(), JOURNAL_REC_HEADER_LEN + raw.len());
        assert_eq!(replay_journal(&rec), raw);
    }

    #[test]
    fn journal_多条拼接_顺序还原() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&serialize_journal_record(b"line1\n"));
        buf.extend_from_slice(&serialize_journal_record(b"line2\n"));
        buf.extend_from_slice(&serialize_journal_record(b"line3\n"));
        assert_eq!(replay_journal(&buf), b"line1\nline2\nline3\n");
    }

    #[test]
    fn journal_空区_空记录() {
        assert_eq!(replay_journal(&[]), Vec::<u8>::new());
        // 空 payload 记录：合法，贡献 0 字节。
        let rec = serialize_journal_record(b"");
        assert_eq!(rec.len(), JOURNAL_REC_HEADER_LEN);
        assert_eq!(replay_journal(&rec), Vec::<u8>::new());
    }

    #[test]
    fn journal_半截头_返回上一完整前缀() {
        let mut buf = serialize_journal_record(b"complete");
        // 追加一段不足 8 字节的半截头（崩溃于写记录头中途）。
        buf.extend_from_slice(&[0u8; 5]);
        assert_eq!(replay_journal(&buf), b"complete");
    }

    #[test]
    fn journal_半截payload_返回上一完整前缀() {
        let mut buf = serialize_journal_record(b"first");
        // 第二条：头声明 100 字节但只写 3 字节 payload（半截写）。
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&crc32(&[0u8; 100]).to_le_bytes());
        buf.extend_from_slice(b"abc");
        assert_eq!(
            replay_journal(&buf),
            b"first",
            "rec_len 越界 → 截到上一完整记录"
        );
    }

    #[test]
    fn journal_payload_损坏_crc检出即停() {
        let mut buf = serialize_journal_record(b"good");
        buf.extend_from_slice(&serialize_journal_record(b"willcorrupt"));
        // 翻转第二条 payload 的一个字节（在 buf 末尾区）。
        let last = buf.len() - 1;
        buf[last] ^= 0xFF;
        assert_eq!(
            replay_journal(&buf),
            b"good",
            "第二条 crc 不符 → 停在第一条"
        );
    }

    #[test]
    fn journal_rec_len_巨值_不越界不panic() {
        // rec_len = u32::MAX，bounds 校验应直接停，不 panic / 不分配。
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(b"x");
        assert_eq!(replay_journal(&buf), Vec::<u8>::new());
    }

    /// 用内存 Cursor 写一个 archive，返回字节缓冲。
    fn build_archive(chunk_size: u32, blocks: &[(Vec<u8>, bool, u64)]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut w = ArchiveWriter::new(cursor, chunk_size).unwrap();
        for (bytes, verbatim, raw_len) in blocks {
            w.append_block(bytes, *verbatim, *raw_len).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// 把内存缓冲写到临时文件，open 成 ArchiveReader。
    fn reader_from_bytes(bytes: &[u8]) -> ArchiveReader {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(bytes).unwrap();
        tmp.flush().unwrap();
        ArchiveReader::open(tmp.path()).unwrap()
    }

    #[test]
    fn writer_reader_round_trip_单块() {
        let bytes = build_archive(64, &[(b"compressed-block-0".to_vec(), false, 50)]);
        let r = reader_from_bytes(&bytes);
        assert_eq!(r.chunk_count(), 1);
        assert_eq!(r.footer().chunk_size, 64);
        assert_eq!(r.footer().uncompressed_size, 50);
        let (data, entry) = r.read_block(0).unwrap().unwrap();
        assert_eq!(data, b"compressed-block-0");
        assert!(!entry.is_verbatim());
    }

    #[test]
    fn 多块_offset_与_clen_正确() {
        let b0 = vec![1u8; 10];
        let b1 = vec![2u8; 25];
        let b2 = vec![3u8; 7];
        let bytes = build_archive(
            16,
            &[
                (b0.clone(), false, 16),
                (b1.clone(), false, 16),
                (b2.clone(), true, 9),
            ],
        );
        let r = reader_from_bytes(&bytes);
        assert_eq!(r.chunk_count(), 3);
        // 第一个块紧跟 header + 双 superblock 槽（DATA_START）。
        assert_eq!(r.entry(0).unwrap().offset, DATA_START);
        assert_eq!(r.entry(0).unwrap().clen, 10);
        assert_eq!(r.entry(1).unwrap().offset, DATA_START + 10);
        assert_eq!(r.entry(2).unwrap().offset, DATA_START + 10 + 25);
        assert_eq!(r.read_block(1).unwrap().unwrap().0, b1);
        // 末块 verbatim flag 应翻转。
        assert!(r.entry(2).unwrap().is_verbatim());
        // uncompressed_size = 16+16+9。
        assert_eq!(r.footer().uncompressed_size, 41);
    }

    #[test]
    fn 越界块返回_none() {
        let bytes = build_archive(64, &[(b"x".to_vec(), false, 1)]);
        let r = reader_from_bytes(&bytes);
        assert!(r.read_block(1).unwrap().is_none());
        assert!(r.entry(5).is_none());
    }

    #[test]
    fn 零块_archive_合法() {
        let bytes = build_archive(64, &[]);
        let r = reader_from_bytes(&bytes);
        assert_eq!(r.chunk_count(), 0);
        assert_eq!(r.footer().uncompressed_size, 0);
        assert!(r.read_block(0).unwrap().is_none());
    }

    #[test]
    fn 坏_magic_被拒() {
        let mut bytes = build_archive(64, &[(b"y".to_vec(), false, 1)]);
        bytes[0] = b'X'; // 破坏 magic
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&bytes).unwrap();
        tmp.flush().unwrap();
        let err = expect_open_err(tmp.path());
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// 读活跃槽（seq 较大者）的 SuperBlock（测试辅助）。
    fn active_sb(buf: &[u8]) -> SuperBlock {
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
    fn patch_both_sb(buf: &mut [u8], f: impl Fn(&mut SuperBlock)) {
        let mut sb = active_sb(buf);
        f(&mut sb);
        let b = serialize_superblock(&sb);
        buf[SB_A_OFFSET as usize..][..SB_LEN as usize].copy_from_slice(&b);
        buf[SB_B_OFFSET as usize..][..SB_LEN as usize].copy_from_slice(&b);
    }

    #[test]
    fn 索引_crc_损坏被检出() {
        let bytes = build_archive(64, &[(b"abc".to_vec(), false, 3)]);
        // 破坏 index 区某字节 → 两槽 index_crc 均不符 → 级联校验失败 → open 报损坏。
        let mut corrupted = bytes.clone();
        let index_off = active_sb(&corrupted).index_offset as usize;
        corrupted[index_off] ^= 0xFF;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&corrupted).unwrap();
        tmp.flush().unwrap();
        let err = expect_open_err(tmp.path());
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidData,
            "index 损坏应报损坏：{err}"
        );
    }

    /// open 应失败并返回错误（ArchiveReader 不实现 Debug，故不能 unwrap_err）。
    fn expect_open_err(path: &std::path::Path) -> io::Error {
        match ArchiveReader::open(path) {
            Ok(_) => panic!("预期 open 失败，却成功了"),
            Err(e) => e,
        }
    }

    #[test]
    fn crc32_已知向量() {
        // "123456789" 的 IEEE CRC32 标准向量 = 0xCBF43926。
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn 越界的_clen_在_open_期被拒() {
        // 构造一个「两槽 index_crc 自洽、但索引项 clen 越界」的 archive：
        // open 阶段 bounds 校验应拒绝（防 read_block 据不可信 clen 无界分配）。
        let bytes = build_archive(64, &[(b"abc".to_vec(), false, 3)]);
        let mut corrupted = bytes.clone();
        let sb = active_sb(&corrupted);
        let index_off = sb.index_offset as usize;
        let index_len = sb.index_len as usize;
        // 索引项布局：offset(8) | clen(8) | flags(4)。把 clen 改成巨值。
        corrupted[index_off + 8..index_off + 16].copy_from_slice(&u64::MAX.to_le_bytes());
        // 重算 index_crc 写回两槽（sb_crc 由 serialize 重算），模拟「CRC 一致但语义越界」。
        let new_crc = crc32(&corrupted[index_off..index_off + index_len]);
        patch_both_sb(&mut corrupted, |s| s.index_crc = new_crc);
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&corrupted).unwrap();
        tmp.flush().unwrap();
        let err = expect_open_err(tmp.path());
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidData,
            "越界 clen 应被拒：{err}"
        );
    }

    // ----- ArchiveUpdater：原地更新（append / RMW / truncate / 原子 footer） -----

    /// 在临时目录建一个初始 archive，返回 (tempdir, path)。
    fn build_archive_file(
        chunk_size: u32,
        blocks: &[(Vec<u8>, bool, u64)],
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.archive");
        let mut w = ArchiveWriter::create(&path, chunk_size).unwrap();
        for (bytes, verbatim, raw_len) in blocks {
            w.append_block(bytes, *verbatim, *raw_len).unwrap();
        }
        w.finish().unwrap().sync_all().unwrap();
        (dir, path)
    }

    /// 读回所有块的压缩字节（不解压），供更新后校验。
    fn read_all_raw(path: &std::path::Path) -> Vec<(Vec<u8>, ChunkEntry)> {
        let r = ArchiveReader::open(path).unwrap();
        (0..r.chunk_count())
            .map(|i| r.read_block(i).unwrap().unwrap())
            .collect()
    }

    #[test]
    fn updater_append_尾块不重写前部数据() {
        let (_d, path) = build_archive_file(8, &[(b"AAAAAAAA".to_vec(), false, 8)]);
        let len_before = std::fs::metadata(&path).unwrap().len();
        let block0_before = read_all_raw(&path)[0].0.clone();

        let mut up = ArchiveUpdater::open(&path).unwrap();
        up.set_block(1, b"BBBB", false, 12).unwrap();
        up.commit().unwrap();

        let blocks = read_all_raw(&path);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, block0_before, "块0 字节不应被改动");
        assert_eq!(blocks[1].0, b"BBBB");
        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(r.footer().uncompressed_size, 12);
        // 文件应增长（append 增量），但块0 物理位置不变（offset 仍是 DATA_START）。
        assert!(std::fs::metadata(&path).unwrap().len() > len_before);
        assert_eq!(r.entry(0).unwrap().offset, DATA_START);
    }

    #[test]
    fn updater_rmw_中间块追加新版本_旧块成空洞() {
        let (_d, path) = build_archive_file(
            8,
            &[
                (b"AAAAAAAA".to_vec(), false, 8),
                (b"BBBBBBBB".to_vec(), false, 8),
            ],
        );
        let old_block1_offset = ArchiveReader::open(&path).unwrap().entry(1).unwrap().offset;

        let mut up = ArchiveUpdater::open(&path).unwrap();
        // RMW 块0：新内容更长，放不回原位 → 追加到末尾。
        up.set_block(0, b"XXXXXXXX", false, 16).unwrap();
        up.commit().unwrap();

        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(r.chunk_count(), 2);
        // 块0 现在指向末尾（offset > 旧块1 offset），旧块0 位置成空洞。
        assert!(r.entry(0).unwrap().offset > old_block1_offset);
        assert_eq!(r.read_block(0).unwrap().unwrap().0, b"XXXXXXXX");
        assert_eq!(r.read_block(1).unwrap().unwrap().0, b"BBBBBBBB");
    }

    #[test]
    fn updater_truncate_丢弃尾块() {
        let (_d, path) = build_archive_file(
            8,
            &[
                (b"AAAAAAAA".to_vec(), false, 8),
                (b"BBBBBBBB".to_vec(), false, 8),
                (b"CCCC".to_vec(), false, 4),
            ],
        );
        let mut up = ArchiveUpdater::open(&path).unwrap();
        up.truncate(1, 8); // 只保留块0
        up.commit().unwrap();
        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(r.chunk_count(), 1);
        assert_eq!(r.footer().uncompressed_size, 8);
        assert_eq!(r.read_block(0).unwrap().unwrap().0, b"AAAAAAAA");
    }

    #[test]
    fn updater_提交后_crc_仍自洽可重开() {
        let (_d, path) = build_archive_file(8, &[(b"AAAAAAAA".to_vec(), false, 8)]);
        let mut up = ArchiveUpdater::open(&path).unwrap();
        up.set_block(1, b"BBBB", false, 12).unwrap();
        up.commit().unwrap();
        // 连续多次更新仍自洽。
        let mut up2 = ArchiveUpdater::open(&path).unwrap();
        up2.set_block(0, b"ZZZZZZZZ", false, 12).unwrap();
        up2.commit().unwrap();
        // open 成功即说明 magic/footer/index CRC/越界校验全过。
        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(r.read_block(0).unwrap().unwrap().0, b"ZZZZZZZZ");
        assert_eq!(r.read_block(1).unwrap().unwrap().0, b"BBBB");
    }

    #[test]
    fn updater_未提交即崩溃_恢复上一致版本() {
        // append-only + 双 SB（崩溃安全根治）：set_block 把新块 append 到 EOF，但**未 commit**
        // （无 SB 更新）即崩溃。旧设计这里 fail-closed-不可恢复；新设计 open 取活跃 SB（仍 seq0、
        // 指向旧 index）→ **恢复上一致版本**，未提交的尾部追加被忽略。这正是根治：丢未提交写，
        // 而非毁已提交数据。
        let (_d, path) = build_archive_file(8, &[(b"AAAAAAAA".to_vec(), false, 8)]);
        {
            let mut up = ArchiveUpdater::open(&path).unwrap();
            up.set_block(1, b"BBBB", false, 12).unwrap(); // append 到 EOF，未 commit
            drop(up);
        }
        let r = ArchiveReader::open(&path).expect("应恢复上一致版本，而非报损坏");
        assert_eq!(r.chunk_count(), 1, "未提交的 append 应被忽略");
        assert_eq!(r.read_block(0).unwrap().unwrap().0, b"AAAAAAAA");
    }

    #[test]
    fn updater_活跃sb损坏_回落另一槽恢复() {
        // 双 superblock 的核心保证（构造性崩溃测试，docs/04 §8.5）：commit 写非活跃槽（seq+1）。
        // 若该 SB 写半截/损坏（崩溃窗口），open 回落另一槽（上一 seq）→ 恢复上一致版本，绝不丢
        // 已提交数据。这取代了旧 reuse-tail-slot 的「fail-closed 不可恢复」边界。
        let (_d, path) = build_archive_file(8, &[(b"AAAAAAAA".to_vec(), false, 8)]);
        {
            let mut up = ArchiveUpdater::open(&path).unwrap();
            up.set_block(0, b"CCCCCCCC", false, 8).unwrap();
            up.commit().unwrap(); // 写 SB_B=seq1（活跃），SB_A=seq0（旧 AAAAAAAA）
        }
        assert_eq!(
            ArchiveReader::open(&path)
                .unwrap()
                .read_block(0)
                .unwrap()
                .unwrap()
                .0,
            b"CCCCCCCC",
            "commit 后应读到新版本"
        );
        // 模拟「活跃 SB 写半截/损坏」：翻转 seq 较大（活跃）槽的一个非 magic 字节 → 命中 sb_crc。
        let mut bytes = std::fs::read(&path).unwrap();
        let sa = parse_superblock(&bytes[SB_A_OFFSET as usize..]);
        let sb = parse_superblock(&bytes[SB_B_OFFSET as usize..]);
        let active_off = match (sa, sb) {
            (Some(x), Some(y)) if y.seq > x.seq => SB_B_OFFSET,
            _ => SB_A_OFFSET,
        };
        bytes[active_off as usize + 4] ^= 0xFF; // 破坏活跃槽（seq 字段区，非 magic）
        std::fs::write(&path, &bytes).unwrap();
        // open 回落另一槽 → 恢复一致版本，不报损坏、不丢数据。
        let r = ArchiveReader::open(&path).expect("活跃槽损坏应回落另一槽，不报损坏");
        let got = r.read_block(0).unwrap().unwrap().0;
        assert!(
            got == b"AAAAAAAA" || got == b"CCCCCCCC",
            "应恢复某个一致版本（不丢已提交数据）：got={got:?}"
        );
    }

    #[test]
    fn updater_反复重写尾块跨提交_读回正确() {
        // append-only + 双 SB：每次提交把渐增尾块 append 到 EOF（旧版本成空洞，压实回收）。
        // 文件单调增长（与旧 reuse 的「紧凑」不同——空洞由压实而非在线覆盖回收）；本测验证的是
        // **跨提交每次都读回正确且 durable**（崩溃安全的正确性，不再断言文件不增长）。
        let (_d, path) = build_archive_file(64, &[(b"AAAAAAAA".to_vec(), false, 8)]);
        for grow in 1..=20u64 {
            let len = (grow * 3).min(60) as usize;
            let content = vec![b'Z'; len];
            let mut up = ArchiveUpdater::open(&path).unwrap();
            up.set_block(0, &content, false, len as u64).unwrap();
            up.commit().unwrap();
            up.sync().unwrap();
            let r = ArchiveReader::open(&path).unwrap();
            assert_eq!(r.chunk_count(), 1);
            assert_eq!(
                r.read_block(0).unwrap().unwrap().0,
                content,
                "第 {grow} 次提交后应读回最新内容"
            );
        }
    }

    // ----- head 缓存（发现读快路径，docs/02）-----

    #[test]
    fn head_cache_无时_reader_返回_none() {
        // 不设 head 缓存 → footer 字段全 0 → read_head_cache None、rawlen 0。
        let bytes = build_archive(64, &[(b"blk0".to_vec(), false, 4)]);
        let r = reader_from_bytes(&bytes);
        assert!(r.footer().head_cache.is_none());
        assert_eq!(r.head_cache_rawlen(), 0);
        assert!(r.read_head_cache().unwrap().is_none());
    }

    #[test]
    fn head_cache_writer_round_trip() {
        // Writer 设 head 缓存 → reader 读回同样字节 + rawlen + verbatim。
        let cursor = Cursor::new(Vec::new());
        let mut w = ArchiveWriter::new(cursor, 64).unwrap();
        w.append_block(b"compressed-block-0", false, 50).unwrap();
        w.set_head_cache(b"HEADCACHE-COMPRESSED".to_vec(), false, 64);
        let bytes = w.finish().unwrap().into_inner();

        let r = reader_from_bytes(&bytes);
        assert_eq!(r.head_cache_rawlen(), 64);
        let (hc, verbatim) = r.read_head_cache().unwrap().unwrap();
        assert_eq!(hc, b"HEADCACHE-COMPRESSED");
        assert!(!verbatim);
        // 块仍正常可读（head 缓存不干扰块索引）。
        assert_eq!(r.read_block(0).unwrap().unwrap().0, b"compressed-block-0");
    }

    #[test]
    fn head_cache_verbatim_flag_保真() {
        let cursor = Cursor::new(Vec::new());
        let mut w = ArchiveWriter::new(cursor, 64).unwrap();
        w.append_block(b"x", false, 1).unwrap();
        w.set_head_cache(b"RAWHEAD".to_vec(), true, 7); // verbatim head
        let bytes = w.finish().unwrap().into_inner();
        let r = reader_from_bytes(&bytes);
        let (_, verbatim) = r.read_head_cache().unwrap().unwrap();
        assert!(verbatim, "verbatim head 缓存 flag 应保真");
    }

    #[test]
    fn head_cache_越界_优雅回退_none() {
        // head 缓存是可丢弃派生数据（docs/04 §11 M2）：superblock 里 head_cache_offset 越界
        // → open **成功**，read_head_cache 返回 None（优雅回退逐块），**绝不** fail-closed 整文件。
        let cursor = Cursor::new(Vec::new());
        let mut w = ArchiveWriter::new(cursor, 64).unwrap();
        w.append_block(b"abc", false, 3).unwrap();
        w.set_head_cache(b"HEAD".to_vec(), false, 64);
        let mut bytes = w.finish().unwrap().into_inner();
        // 把 SB 里 head_cache.offset 改成越界巨值（> index_offset），两槽重序列化保持 sb_crc 自洽。
        patch_both_sb(&mut bytes, |s| {
            if let Some(hc) = s.head_cache.as_mut() {
                hc.offset = u64::MAX - 100;
            }
        });
        let r = reader_from_bytes(&bytes);
        assert!(
            r.read_head_cache().unwrap().is_none(),
            "越界 head 缓存应优雅回退 None（M2），不 fail-closed"
        );
        // 文件本身仍可正常打开、块可读（缓存损坏不拖垮文件）。
        assert_eq!(r.read_block(0).unwrap().unwrap().0, b"abc");
    }

    #[test]
    fn updater_保留_head_cache_跨提交_append() {
        // build 一个带 head 缓存的 archive，open updater append 一块（不动 head 缓存），
        // commit 后 head 缓存仍在（rawlen + 字节保真）—— 验证 updater 跨提交重写元数据尾区时
        // 从 footer 载入并重新落盘 head 缓存。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.archive");
        {
            let mut w = ArchiveWriter::create(&path, 8).unwrap();
            w.append_block(b"AAAAAAAA", false, 8).unwrap();
            w.set_head_cache(b"HC-COMPRESSED-BYTES".to_vec(), false, 8);
            w.finish().unwrap().sync_all().unwrap();
        }
        let mut up = ArchiveUpdater::open(&path).unwrap();
        assert_eq!(up.head_cache_rawlen(), 8, "open 应从 footer 载入 head 缓存");
        up.set_block(1, b"BBBB", false, 12).unwrap();
        up.commit().unwrap();

        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(r.chunk_count(), 2);
        assert_eq!(r.head_cache_rawlen(), 8, "append 后 head 缓存应保留");
        assert_eq!(
            r.read_head_cache().unwrap().unwrap().0,
            b"HC-COMPRESSED-BYTES"
        );
        assert_eq!(r.read_block(1).unwrap().unwrap().0, b"BBBB");
    }

    #[test]
    fn updater_set_head_cache_后提交可读回() {
        // 无 head 缓存的 archive，open updater 设 head 缓存并 commit → reader 读回。
        let (_d, path) = build_archive_file(8, &[(b"AAAAAAAA".to_vec(), false, 8)]);
        let mut up = ArchiveUpdater::open(&path).unwrap();
        assert_eq!(up.head_cache_rawlen(), 0);
        up.set_head_cache(b"NEW-HEAD".to_vec(), false, 8);
        up.commit().unwrap();
        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(r.head_cache_rawlen(), 8);
        assert_eq!(r.read_head_cache().unwrap().unwrap().0, b"NEW-HEAD");
        // 块数据不受影响。
        assert_eq!(r.read_block(0).unwrap().unwrap().0, b"AAAAAAAA");
    }

    #[test]
    fn updater_含head_cache_未提交即崩溃_恢复上一致版本() {
        // 崩溃安全（M2 变体）：set_head_cache + set_block 后不 commit（drop），模拟「写了新尾区但
        // SB 未落盘即崩溃」。新设计 open 取活跃 SB（仍 seq0）→ 恢复上一致版本（1 块，无 head 缓存），
        // 未提交的 head 缓存 + 尾块追加被忽略。不报损坏、不丢已提交数据。
        let (_d, path) = build_archive_file(8, &[(b"AAAAAAAA".to_vec(), false, 8)]);
        {
            let mut up = ArchiveUpdater::open(&path).unwrap();
            up.set_head_cache(b"HEAD-PENDING".to_vec(), false, 8);
            up.set_block(1, b"BBBB", false, 12).unwrap();
            drop(up); // 不 commit
        }
        let r = ArchiveReader::open(&path).expect("应恢复上一致版本，而非报损坏");
        assert_eq!(r.chunk_count(), 1, "未提交的尾块应被忽略");
        assert_eq!(r.read_block(0).unwrap().unwrap().0, b"AAAAAAAA");
        assert!(
            r.read_head_cache().unwrap().is_none(),
            "未提交的 head 缓存不应可见"
        );
    }

    // ----- 尾日志接线（docs/04 §8.4，TDD）-----

    #[test]
    fn journal_append_commit_重开_重放尾块() {
        // 0 块 archive（空文件）。逐次 append_journal（原始增量）+ commit_journal（不动 index）→
        // 重开 read_tail 应重放出全部原始字节。模拟 fsync 路径：不压缩、只追加 delta。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.archive");
        {
            let mut w = ArchiveWriter::create(&path, 64).unwrap();
            w.finish().unwrap().sync_all().unwrap();
        }
        {
            let mut up = ArchiveUpdater::open(&path).unwrap();
            for line in [b"line1\n".as_ref(), b"line2\n", b"line3\n"] {
                up.append_journal(line).unwrap();
                up.commit_journal().unwrap(); // 每次 fsync 一提交
            }
        }
        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(r.chunk_count(), 0, "尾日志不增加封块数");
        assert_eq!(
            r.read_tail().unwrap().as_deref(),
            Some(b"line1\nline2\nline3\n".as_ref()),
            "重开应重放出全部尾日志原始字节"
        );
    }

    #[test]
    fn journal_未提交即崩溃_丢未提交delta_保住已提交() {
        // append_journal 两条都 commit_journal（durable），第三条 append 后不 commit（drop=崩溃）。
        // 重开应只见前两条（已提交），第三条未提交被忽略——崩溃安全的尾日志。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.archive");
        {
            let mut w = ArchiveWriter::create(&path, 64).unwrap();
            w.finish().unwrap().sync_all().unwrap();
        }
        {
            let mut up = ArchiveUpdater::open(&path).unwrap();
            up.append_journal(b"committed-A").unwrap();
            up.commit_journal().unwrap();
            up.append_journal(b"committed-B").unwrap();
            up.commit_journal().unwrap();
            up.append_journal(b"UNCOMMITTED").unwrap(); // 不 commit
            drop(up);
        }
        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(
            r.read_tail().unwrap().as_deref(),
            Some(b"committed-Acommitted-B".as_ref()),
            "未提交的第三条 delta 应被忽略，已提交的保住"
        );
    }

    #[test]
    fn journal_封块后重置_尾块迁入压缩块() {
        // 攒了尾日志后「封块」：set_block 写压缩块（这里 verbatim 模拟）+ reset_journal + commit。
        // 重开：read_tail 应为 None（journal 已重置），内容改由块 0 承载。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.archive");
        {
            let mut w = ArchiveWriter::create(&path, 64).unwrap();
            w.finish().unwrap().sync_all().unwrap();
        }
        {
            let mut up = ArchiveUpdater::open(&path).unwrap();
            up.append_journal(b"AAAA").unwrap();
            up.commit_journal().unwrap();
            up.append_journal(b"BBBB").unwrap();
            up.commit_journal().unwrap();
            // 封块：把累积的 "AAAABBBB" 作为块0（verbatim 模拟压缩）写入，重置 journal。
            up.set_block(0, b"AAAABBBB", true, 8).unwrap();
            up.reset_journal();
            up.commit().unwrap();
        }
        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(r.chunk_count(), 1, "封块后应有 1 个块");
        assert!(r.read_tail().unwrap().is_none(), "封块后尾日志应已重置为空");
        assert_eq!(r.read_block(0).unwrap().unwrap().0, b"AAAABBBB");
    }

    #[test]
    fn journal_越界_load_active拒绝该槽() {
        // SB 的 tail_journal 区越界 → 该槽不可用（级联校验拒绝）。两槽都坏则报损坏。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.archive");
        {
            let mut w = ArchiveWriter::create(&path, 64).unwrap();
            w.finish().unwrap().sync_all().unwrap();
        }
        {
            let mut up = ArchiveUpdater::open(&path).unwrap();
            up.append_journal(b"data").unwrap();
            up.commit_journal().unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        // 把两槽的 tail_journal_offset 改成越界巨值（保持 sb_crc 自洽）。
        patch_both_sb(&mut bytes, |s| s.tail_journal_offset = u64::MAX - 100);
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&bytes).unwrap();
        tmp.flush().unwrap();
        assert!(
            ArchiveReader::open(tmp.path()).is_err(),
            "尾日志越界的槽应被级联校验拒绝；两槽皆坏 → 报损坏"
        );
    }
}
