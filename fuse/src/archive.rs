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
//! [chunk_index: (offset, clen, flags) × count]     ← 索引（footer 前）
//! [footer: chunk_size|uncompressed_size|chunk_count|index_offset|crc] ← 尾部固定大小
//! ```
//!
//! 本模块只做「格式读写」，不碰压缩（压缩在 core::codec，§2）。`ArchiveReader::read_block`
//! 返回压缩字节 + flags，由上层 Core 解压。`ArchiveWriter` 仅供离线 fixture 工具使用
//! （P1 无在线写路径），从已压缩块写出 footer 布局。

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// 文件头魔数，标识 zipfs 布局 S 的 archive。取 "ZIPFSAR1" 的字节。
pub const MAGIC: [u8; 8] = *b"ZIPFSAR\x01";

/// 当前格式版本。
pub const VERSION: u32 = 1;

/// 文件头大小：magic(8) + version(4)。
const HEADER_LEN: u64 = 12;

/// 单个块索引项的序列化大小：offset(8) + clen(8) + flags(4) = 20 字节。
const INDEX_ENTRY_LEN: u64 = 20;

/// footer 固定大小：
/// chunk_size(4) + uncompressed_size(8) + chunk_count(8) + index_offset(8) + crc(4) = 32 字节。
const FOOTER_LEN: u64 = 32;

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
}

// ===========================================================================
// CRC32（IEEE，逐位软算）—— 仅校验索引区完整性，避免引入额外依赖
// ===========================================================================

/// 计算 IEEE CRC32（多项式 0xEDB88320，逐位实现）。自带实现，避免为单一校验拉入额外 crate。
/// 索引区通常很小，逐位足够；若 index 增大成热点，可改查表法（留作后续优化）。
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
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

/// 只读 archive：打开即解析尾部 footer 与索引，后续 `read_block` O(1) 定位。
pub struct ArchiveReader {
    file: File,
    footer: Footer,
    index: Vec<ChunkEntry>,
}

impl ArchiveReader {
    /// 打开一个 archive 文件：校验 magic/version → 读尾部 footer → 读索引 → 校验 CRC。
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        Self::from_file(file)
    }

    /// 从已打开的 `File` 构造（测试与内部复用）。
    pub fn from_file(file: File) -> io::Result<Self> {
        let total_len = file.metadata()?.len();
        if total_len < HEADER_LEN + FOOTER_LEN {
            return Err(corrupt("文件太小，不足以容纳 header + footer"));
        }

        // 1) 校验文件头 magic + version。
        let mut header = [0u8; HEADER_LEN as usize];
        read_exact_at(&file, &mut header, 0)?;
        if header[..8] != MAGIC {
            return Err(corrupt("magic 不匹配，非 zipfs archive"));
        }
        let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(corrupt(&format!("不支持的 archive 版本：{version}")));
        }

        // 2) 读尾部固定大小 footer。
        let footer = Self::read_footer(&file, total_len)?;

        // 3) 据 index_offset 读索引区，校验范围。
        let index_bytes_len = footer
            .chunk_count
            .checked_mul(INDEX_ENTRY_LEN)
            .ok_or_else(|| corrupt("chunk_count 溢出"))?;
        let footer_start = total_len - FOOTER_LEN;
        if footer.index_offset < HEADER_LEN
            || footer.index_offset > footer_start
            || footer.index_offset + index_bytes_len != footer_start
        {
            return Err(corrupt("index_offset / chunk_count 与文件尺寸不自洽"));
        }

        let mut index_bytes = vec![0u8; index_bytes_len as usize];
        read_exact_at(&file, &mut index_bytes, footer.index_offset)?;

        // 4) 校验索引 CRC，及早发现尾部损坏。
        if crc32(&index_bytes) != footer.crc {
            return Err(corrupt("索引区 CRC 校验失败（尾部可能损坏）"));
        }

        let index = parse_index(&index_bytes, footer.chunk_count as usize);

        // 5) 逐项校验块在数据区 [HEADER_LEN, index_offset) 内自洽。
        // CRC 只保证索引「未被意外破坏」，挡不住「CRC 一致但 clen/offset 语义越界」——
        // 否则 read_block 会据不可信 clen 做无界分配（OOM）或 seek 越界。及早在 open 拒绝。
        let data_end = footer.index_offset;
        for (i, e) in index.iter().enumerate() {
            let end = e
                .offset
                .checked_add(e.clen)
                .ok_or_else(|| corrupt(&format!("块 {i} 的 offset+clen 溢出")))?;
            if e.offset < HEADER_LEN || end > data_end {
                return Err(corrupt(&format!("块 {i} 越出数据区")));
            }
        }

        Ok(Self {
            file,
            footer,
            index,
        })
    }

    fn read_footer(file: &File, total_len: u64) -> io::Result<Footer> {
        let mut buf = [0u8; FOOTER_LEN as usize];
        read_exact_at(file, &mut buf, total_len - FOOTER_LEN)?;
        let chunk_size = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let uncompressed_size = u64::from_le_bytes(buf[4..12].try_into().unwrap());
        let chunk_count = u64::from_le_bytes(buf[12..20].try_into().unwrap());
        let index_offset = u64::from_le_bytes(buf[20..28].try_into().unwrap());
        let crc = u32::from_le_bytes(buf[28..32].try_into().unwrap());
        if chunk_size == 0 {
            return Err(corrupt("chunk_size 不能为 0"));
        }
        Ok(Footer {
            chunk_size,
            uncompressed_size,
            chunk_count,
            index_offset,
            crc,
        })
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
}

impl ArchiveWriter<File> {
    /// 在 `path` 创建新 archive。
    pub fn create(path: &Path, chunk_size: u32) -> io::Result<Self> {
        let file = File::create(path)?;
        Self::new(file, chunk_size)
    }
}

impl<W: Write + Seek> ArchiveWriter<W> {
    /// 用任意 `Write + Seek` 构造，并写出文件头。
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
        Ok(Self {
            inner,
            chunk_size,
            uncompressed_size: 0,
            cursor: HEADER_LEN,
            index: Vec::new(),
        })
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

    /// 写出索引区 + footer，收尾。返回内部 writer（便于 flush/同步）。
    pub fn finish(mut self) -> io::Result<W> {
        let index_offset = self.cursor;

        // 序列化索引区。
        let mut index_bytes = Vec::with_capacity(self.index.len() * INDEX_ENTRY_LEN as usize);
        for e in &self.index {
            index_bytes.extend_from_slice(&e.offset.to_le_bytes());
            index_bytes.extend_from_slice(&e.clen.to_le_bytes());
            index_bytes.extend_from_slice(&e.flags.to_le_bytes());
        }
        let crc = crc32(&index_bytes);
        self.inner.write_all(&index_bytes)?;

        // 写 footer（固定大小）。
        let mut footer = Vec::with_capacity(FOOTER_LEN as usize);
        footer.extend_from_slice(&self.chunk_size.to_le_bytes());
        footer.extend_from_slice(&self.uncompressed_size.to_le_bytes());
        footer.extend_from_slice(&(self.index.len() as u64).to_le_bytes());
        footer.extend_from_slice(&index_offset.to_le_bytes());
        footer.extend_from_slice(&crc.to_le_bytes());
        debug_assert_eq!(footer.len() as u64, FOOTER_LEN);
        self.inner.write_all(&footer)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// 构造一个 InvalidData 错误，带统一前缀，便于排查。
fn corrupt(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("archive 损坏：{msg}"))
}

// ===========================================================================
// ArchiveUpdater：在已存在 archive 上**原地更新**（P2/P3 在线写路径）
// ===========================================================================
//
// 设计见 §7。核心是「不就地毁旧数据」：
// - **append 尾块 / RMW 任意块**：把新压缩块写到**当前文件末尾**（旧 footer 之后），更新内存
//   index 指向新 offset（被覆盖块的旧位置成包内空洞，留待后续压实），`commit` 在新块之后写新
//   index + 新 footer，并 `set_len` 截掉残留。
// - **崩溃安全（首版，诚实边界）**：
//   - **已提交** 的数据是持久的：`commit` 内部「写 index → sync_data → 写 footer → set_len →
//     sync_data」，返回即新版本落盘。footer 含 index CRC，尾部不自洽会被 open 拒绝。
//   - **提交中崩溃**（写了新块/新 index 但新 footer 未落盘）：因 footer 必须在 EOF，半截尾部
//     不是合法 footer → open **检测为损坏并报错**（见单测 updater_未提交即崩溃_*），绝不静默
//     错读。但此时旧版本也无法经简单尾读恢复——完整恢复（扫描 / 截回最近合法 footer，或双 footer
//     交替）属后续工作。§10 已将 S 的一致性定级为「较弱、须文档化」，此即其边界。
// - 旧 index+footer 留在文件中部成空洞，不影响读（读走新 footer 的 index_offset）。

/// live 数据区末尾：所有索引项 `offset+clen` 的最大值；空 index 则为 `HEADER_LEN`。
/// 提交后 archive 是紧凑布局（数据块紧随 header、index+footer 在尾），故该值即「可回收空洞
/// 起点」——尾部超出此值的字节都是上次提交遗留的已死 index/footer/旧块版本（碎片化修复 §A）。
fn live_data_end(index: &[ChunkEntry]) -> u64 {
    index
        .iter()
        .map(|e| e.offset + e.clen)
        .max()
        .unwrap_or(HEADER_LEN)
}

/// 在已存在 archive 上做原地更新。打开即读 footer + index（复用 ArchiveReader 的解析），
/// 之后 `set_block` / `truncate` 改内存 index 并把新块写到文件末尾，`commit` 落 index + footer。
pub struct ArchiveUpdater {
    file: File,
    chunk_size: u32,
    index: Vec<ChunkEntry>,
    uncompressed_size: u64,
    /// 下一个新块的写入位置。初始 = **live 数据区末尾**（回收尾部已死 index/footer/旧块版本
    /// 空洞），随 `set_block` 追加推进。见 `live_data_end` 与 `open` 文档。
    write_cursor: u64,
}

impl ArchiveUpdater {
    /// 打开已存在的 archive 供更新（读写）。空/缺文件请先用 `ArchiveWriter` 建一个 0 块 archive。
    pub fn open(path: &Path) -> io::Result<Self> {
        let reader = ArchiveReader::open(path)?;
        let footer = *reader.footer();
        let index = reader.index.clone();
        // 复用同一 fd 读写：以读写模式重开（ArchiveReader 持只读 fd）。
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        // 写游标置于**当前 live 数据区末尾**（= 所有索引项 offset+clen 的最大值，空 archive 则
        // 紧随 header），而非物理文件末尾。这样上次提交遗留在尾部的「已被取代的旧尾块版本 +
        // 旧 index + 旧 footer」空洞会在本次提交时被新数据覆盖回收（碎片化修复 §A）。
        //
        // 崩溃安全（承接 rust-review C3/CRITICAL 的诚实边界，§10 已将 S 一致性定级为「较弱、须文档化」）：
        // 两类写各自 fail-closed：
        // (1) **追加到 live 数据末尾**（idx==count 或非尾 RMW）：覆盖的是已死旧 index/footer 空洞，
        //     不动任何 live 块；提交中崩溃 → 旧 footer 仍在 EOF 且自洽，open 取旧一致版本。
        // (2) **reuse 原地覆盖最末 live 块**（set_block 的 reuse_tail_slot，碎片化修复主路径）：会就地
        //     改写盘上字节，故在覆盖**前**先 set_len 截掉该 slot 之后的旧 index/footer 再 sync——崩溃
        //     窗口内 EOF 不再是合法 footer，open 检测损坏报错，绝不静默读出「新前缀+旧残尾」。
        // 提交内仍是「写 index → sync_data → 写 footer → sync_data」两段 barrier。两路均 fail-closed
        // （见 updater_未提交即崩溃_* 与 updater_reuse_尾块原地覆盖中崩溃_* 测试）。
        let live_data_end = live_data_end(&index);
        Ok(Self {
            file,
            chunk_size: footer.chunk_size,
            index,
            uncompressed_size: footer.uncompressed_size,
            write_cursor: live_data_end,
        })
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

    /// 写第 `idx` 块的新内容：把压缩字节追加到数据区末尾，index[idx] 改指新位置。
    ///
    /// `idx` 可等于当前块数（append 新块）、小于（RMW 覆盖已有块），或**大于**当前块数——
    /// 后者说明中间存在纯空洞块（上层 Core 对稀疏写未物化中间块）：此处自动用「整块零字节
    /// verbatim 块」补齐缺口，保持 archive 块索引连续。`new_size` 是更新后逻辑大小。
    pub fn set_block(
        &mut self,
        idx: u64,
        stored_bytes: &[u8],
        verbatim: bool,
        new_size: u64,
    ) -> io::Result<()> {
        // 缺口补零块（[count, idx)）。大跨度时复用同一零缓冲，避免每块重复分配（rust-review L1）。
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
        let count = self.index.len() as u64;
        // 写入位置：默认追加到 live 数据末尾（write_cursor）。**例外**：当本次是「重写当前最末
        // live 块」（即被改块的物理位置恰好紧贴 write_cursor，是数据区最高 offset 的块）时，
        // **复用它自己的 slot**（原地覆盖），从而在 append 主负载下保持 archive 紧凑——否则
        // 每次 fsync 把渐增的尾块追加一遍、旧版本成永久空洞，物理文件随 fsync 次数线性膨胀
        // （碎片化修复 §A）。复用安全性：被覆盖区间起点 = 该块自身 offset，其后只有「上次提交
        // 遗留的已死 index/footer 空洞」，绝不触碰其它 live 块（它们 offset 更小）。
        let reuse_tail_slot = idx < count && {
            let e = &self.index[idx as usize];
            e.offset + e.clen == self.write_cursor
        };
        let offset = if reuse_tail_slot {
            self.index[idx as usize].offset
        } else {
            self.write_cursor
        };
        // **崩溃 fail-closed（rust-review CRITICAL）**：reuse 原地覆盖会就地改写最末 live 块的盘上
        // 字节。其后紧跟着上次提交遗留的旧 index + 旧 footer（仍在 EOF、CRC 自洽）。若新压缩长度
        // <= 旧长度，覆盖只触及该块前缀、不碰旧 index/footer——崩溃后 open 会据旧 footer 读出
        // 「新前缀 + 旧残尾」的 Frankenstein 块（无 per-block 校验拦不住）。为恢复 append-only 的
        // fail-closed 语义：原地覆盖**前**先把该 slot 之后的遗留尾部（旧块残尾 + 旧 index + 旧 footer）
        // 物理截掉并 sync——此后 EOF 不再是合法 footer，崩溃窗口内 open 检测损坏报错，绝不静默错读。
        // 该 slot 是最末 live 块（offset+clen==write_cursor），截到 offset 不丢任何 live 数据。
        if reuse_tail_slot {
            self.file.set_len(offset)?;
            self.file.sync_data()?;
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(stored_bytes)?;
        self.write_cursor = offset + stored_bytes.len() as u64;
        let entry = ChunkEntry {
            offset,
            clen: stored_bytes.len() as u64,
            flags: if verbatim { FLAG_VERBATIM } else { 0 },
        };
        if idx == count {
            self.index.push(entry);
        } else {
            self.index[idx as usize] = entry; // 非尾块原地复用时此处仍为覆盖；尾块复用时位置不变
        }
        self.uncompressed_size = new_size;
        Ok(())
    }

    /// 截断到 `keep_from` 块（丢弃其后所有块）+ 设新逻辑大小。
    pub fn truncate(&mut self, keep_from: u64, new_size: u64) {
        if (keep_from as usize) < self.index.len() {
            self.index.truncate(keep_from as usize);
        }
        self.uncompressed_size = new_size;
    }

    /// 提交：在数据区末尾写新 index + footer，截断文件到 footer 末尾，fsync 落盘。
    ///
    /// 崩溃安全：先写 index + fsync，再写 footer + fsync。footer 的 CRC 覆盖 index，
    /// open 时不一致即拒绝（不会用到半截 index）。前部数据块从不被覆盖，故旧数据安全。
    pub fn commit(&mut self) -> io::Result<()> {
        let index_offset = self.write_cursor;

        // 序列化 index。
        let mut index_bytes = Vec::with_capacity(self.index.len() * INDEX_ENTRY_LEN as usize);
        for e in &self.index {
            index_bytes.extend_from_slice(&e.offset.to_le_bytes());
            index_bytes.extend_from_slice(&e.clen.to_le_bytes());
            index_bytes.extend_from_slice(&e.flags.to_le_bytes());
        }
        let crc = crc32(&index_bytes);

        self.file.seek(SeekFrom::Start(index_offset))?;
        self.file.write_all(&index_bytes)?;
        // 阶段一 barrier：确保 index 已落盘，再写 footer 指向它。
        self.file.sync_data()?;

        // footer（固定大小）。
        let mut footer = Vec::with_capacity(FOOTER_LEN as usize);
        footer.extend_from_slice(&self.chunk_size.to_le_bytes());
        footer.extend_from_slice(&self.uncompressed_size.to_le_bytes());
        footer.extend_from_slice(&(self.index.len() as u64).to_le_bytes());
        footer.extend_from_slice(&index_offset.to_le_bytes());
        footer.extend_from_slice(&crc.to_le_bytes());
        debug_assert_eq!(footer.len() as u64, FOOTER_LEN);
        self.file.write_all(&footer)?;

        // 截断掉可能残留的旧尾部（若新文件比旧文件短）。
        let total_len = index_offset + index_bytes.len() as u64 + FOOTER_LEN;
        self.file.set_len(total_len)?;
        // 阶段二 barrier：footer 落盘后返回，确保 commit 返回即新版本持久（rust-review C3）。
        // 旧 index+footer 仍在文件中间（成空洞），但新 footer 在尾部、CRC 自洽，open 取新版本。
        self.file.sync_data()?;
        Ok(())
    }

    /// fsync 后端文件（fsync 回调用）。
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
        // 第一个块紧跟 header。
        assert_eq!(r.entry(0).unwrap().offset, HEADER_LEN);
        assert_eq!(r.entry(0).unwrap().clen, 10);
        assert_eq!(r.entry(1).unwrap().offset, HEADER_LEN + 10);
        assert_eq!(r.entry(2).unwrap().offset, HEADER_LEN + 10 + 25);
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

    #[test]
    fn 索引_crc_损坏被检出() {
        let bytes = build_archive(64, &[(b"abc".to_vec(), false, 3)]);
        // 索引区紧接数据区、footer 之前。翻转索引区某字节（offset 字段）。
        let mut corrupted = bytes.clone();
        let footer_start = corrupted.len() - FOOTER_LEN as usize;
        // index_offset 在 footer 的 [20..28]。
        let index_off = u64::from_le_bytes(
            corrupted[footer_start + 20..footer_start + 28]
                .try_into()
                .unwrap(),
        ) as usize;
        corrupted[index_off] ^= 0xFF;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&corrupted).unwrap();
        tmp.flush().unwrap();
        let err = expect_open_err(tmp.path());
        assert!(err.to_string().contains("CRC"), "应报 CRC 错误：{err}");
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
        // 构造一个 footer-CRC 自洽、但索引项 clen 被改成越界值的 archive。
        // 期望：open 阶段就拒绝（防止 read_block 据不可信 clen 无界分配）。
        let bytes = build_archive(64, &[(b"abc".to_vec(), false, 3)]);
        let mut corrupted = bytes.clone();
        let footer_start = corrupted.len() - FOOTER_LEN as usize;
        let index_off = u64::from_le_bytes(
            corrupted[footer_start + 20..footer_start + 28]
                .try_into()
                .unwrap(),
        ) as usize;
        // 索引项布局：offset(8) | clen(8) | flags(4)。把 clen 改成巨值。
        let huge = u64::MAX.to_le_bytes();
        corrupted[index_off + 8..index_off + 16].copy_from_slice(&huge);
        // 重算索引区 CRC，使 footer 自洽（模拟「CRC 一致但语义越界」）。
        let index_bytes = &corrupted[index_off..footer_start];
        let new_crc = crc32(index_bytes);
        corrupted[footer_start + 28..footer_start + 32].copy_from_slice(&new_crc.to_le_bytes());

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
        // 文件应增长（append 增量），但块0 物理位置不变（offset 仍是 HEADER_LEN）。
        assert!(std::fs::metadata(&path).unwrap().len() > len_before);
        assert_eq!(r.entry(0).unwrap().offset, HEADER_LEN);
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
    fn updater_未提交即崩溃_被检测为损坏而非静默错读() {
        // 崩溃安全（C3，诚实版）：新块写到文件末尾。若「写了新块但 footer 还没落盘就崩溃」，
        // 文件尾部不再是合法 footer（旧 footer 已不在 EOF）。此时 open **必须检测出损坏并报错**，
        // 绝不能静默返回半截/错误数据。完整恢复（截回最近合法 footer）属后续工作。
        let (_d, path) = build_archive_file(8, &[(b"AAAAAAAA".to_vec(), false, 8)]);
        {
            let mut up = ArchiveUpdater::open(&path).unwrap();
            up.set_block(1, b"BBBB", false, 12).unwrap();
            // 故意不 commit（drop），模拟 footer 落盘前崩溃：尾部追加了新块但无新 footer。
            drop(up);
        }
        // open 应失败（尾部不自洽被 footer/index 校验挡下），而非误读。
        assert!(
            ArchiveReader::open(&path).is_err(),
            "未提交的尾部追加应被检测为损坏，不能静默错读"
        );
    }

    #[test]
    fn updater_reuse_尾块原地覆盖中崩溃_不静默错读旧版本() {
        // CRITICAL（rust-review，append-opt §A 复用路径崩溃语义）：reuse_tail_slot 把最末 live
        // 块**原地覆盖**。若新压缩长度 <= 旧压缩长度，就地覆盖只触及该块旧字节区间的前缀，
        // **不碰**其后遗留的旧 index / 旧 footer——若此刻崩溃（footer 未更新），open 会读到完好的
        // 旧 footer→旧 index，据旧 clen 从该块 offset 读出「新前缀 + 旧残尾」的 Frankenstein 块
        // （无 per-block 校验拦不住），把损坏字节当合法数据交给 decompress。这违反 fail-closed。
        //
        // 修复后契约：reuse 原地覆盖前先把盘上遗留的旧 index/footer 物理截掉（截到该 slot 起点）
        // 并 sync，使崩溃窗口里 EOF 不再是合法 footer → open fail-closed（报错），绝不静默错读。
        //
        // 用 verbatim 块精确控制压缩长度：旧块 8 字节，新块 4 字节（L_new < L_old），命中危险子情形。
        let (_d, path) = build_archive_file(64, &[(b"AAAAAAAA".to_vec(), true, 8)]);
        {
            let mut up = ArchiveUpdater::open(&path).unwrap();
            // 块0 是最末 live 块 → reuse_tail_slot 原地覆盖。新内容更短，命中 L_new < L_old。
            up.set_block(0, b"BBBB", true, 4).unwrap();
            // 故意不 commit（drop），模拟原地覆盖后、footer 更新前崩溃。
            drop(up);
        }
        // open 必须 fail-closed：要么报损坏，要么读回**一致的旧版本 AAAAAAAA**；
        // 绝不能返回「新前缀 + 旧残尾」的 Frankenstein 块（如 b"BBBBAAAA"）。
        match ArchiveReader::open(&path) {
            Err(_) => { /* 合格：尾部不自洽被校验挡下 */ }
            Ok(r) => {
                let got = r.read_block(0).unwrap().unwrap().0;
                assert!(
                    got == b"AAAAAAAA" || got == b"BBBB",
                    "崩溃后只能读到一致的旧版本或新版本，不得读到 Frankenstein 块：got={got:?}"
                );
            }
        }
    }

    #[test]
    fn updater_反复重写尾块跨提交_不累积空洞() {
        // 碎片化修复（append-opt §A）：模拟「fsync 频繁、尾块未满即封、随后续写同一 idx」
        // —— 每次提交把渐增的尾块重写一遍。旧实现把每个版本追加到末尾、旧版本成永久空洞
        // → 文件随提交次数线性膨胀。修复后：重写尾块复用其物理 slot（先 durable 落新版本再
        // 反映入 footer），文件大小只取决于「当前 live 块」，与提交次数无关。
        let (_d, path) = build_archive_file(64, &[(b"AAAAAAAA".to_vec(), false, 8)]);

        // 反复把块0（尾块）重写为渐增内容，每次单独提交（模拟一次 fsync）。
        let mut sizes = Vec::new();
        for grow in 1..=20u64 {
            let len = (grow * 3).min(60) as usize;
            let content = vec![b'Z'; len];
            let mut up = ArchiveUpdater::open(&path).unwrap();
            up.set_block(0, &content, false, len as u64).unwrap();
            up.commit().unwrap();
            up.sync().unwrap();
            // 每次都能正确读回最新内容（durable + 正确）。
            let r = ArchiveReader::open(&path).unwrap();
            assert_eq!(r.chunk_count(), 1);
            assert_eq!(r.read_block(0).unwrap().unwrap().0, content);
            sizes.push(std::fs::metadata(&path).unwrap().len());
        }
        // 关键断言：文件不随提交次数线性膨胀。最终大小应接近「单个满块 + index + footer」，
        // 而非 20 个递增版本的累积。给出宽松上界：< 3 个满块的量级（旧实现会 >> 20 个版本累积）。
        let final_len = *sizes.last().unwrap();
        let one_block_envelope = HEADER_LEN + 60 + INDEX_ENTRY_LEN + FOOTER_LEN;
        assert!(
            final_len < one_block_envelope * 3,
            "尾块反复重写后文件应保持紧凑（不累积空洞）：final_len={final_len}，单块封套≈{one_block_envelope}，sizes={sizes:?}"
        );
    }
}
