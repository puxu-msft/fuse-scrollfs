//! ArchiveWriter：从已压缩块写出 footer 布局（仅供离线 fixture 工具）。

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use super::reader::serialize_index;
use super::superblock::{serialize_superblock, SuperBlock};
use super::{
    crc32, ChunkEntry, HeadCache, DATA_START, FLAG_VERBATIM, MAGIC, SB_A_OFFSET, SB_B_OFFSET,
    SB_LEN, VERSION,
};

/// 把一个 superblock 写到指定槽偏移（seek + 写定长 SB_LEN 字节）。Writer 内部用。
fn write_superblock_slot<W: Write + Seek>(w: &mut W, off: u64, sb: &SuperBlock) -> io::Result<()> {
    let bytes = serialize_superblock(sb);
    w.seek(SeekFrom::Start(off))?;
    w.write_all(&bytes)?;
    Ok(())
}

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
    /// 在 `path` 创建新 archive（覆盖语义：`File::create` = `O_CREAT|O_TRUNC`）。
    /// 离线 compact/seal 写残留 tmp、fixture 重建、ingest 写 dst 等**期望覆盖既有文件**的
    /// 调用方走此入口。**在线并发新建**（`ShadowStore::create`）须走 [`create_new`] 排他变体。
    ///
    /// [`create_new`]: ArchiveWriter::create_new
    pub fn create(path: &Path, chunk_size: u32) -> io::Result<Self> {
        let file = File::create(path)?;
        Self::new(file, chunk_size)
    }

    /// 排他新建（`O_CREAT|O_EXCL`，阶段 D3）：目标已存在则返回 `AlreadyExists`（映射 EEXIST），
    /// **绝不截断既有文件**。`ShadowStore::create` 专用——两个并发同名 create 时内核保证恰一个
    /// 成功建出文件、其余 EEXIST，杜绝「双成功 + 后者 O_TRUNC 截断前者」的损坏。覆盖语义的其余
    /// 调用方仍走 [`create`]（见其文档）。
    ///
    /// [`create`]: ArchiveWriter::create
    pub fn create_new(path: &Path, chunk_size: u32) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
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

    /// 设置 head 缓存(发现读快路径，docs/02)。`stored_bytes` 是 core::codec 对首
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
            block_crc: crc32(stored_bytes),
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
