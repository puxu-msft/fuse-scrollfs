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
use std::io::{self, Read, Seek, SeekFrom, Write};
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
// CRC32（IEEE，查表法）—— 仅校验索引区完整性，避免引入额外依赖
// ===========================================================================

/// 计算 IEEE CRC32。自带实现，避免为单一校验拉入额外 crate。
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

fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut f = file;
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(buf)
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
}
