//! ArchiveReader：open → 读 footer/index；read_block(idx) → 压缩字节 + flags。
//!
//! 同时容纳双 superblock 崩溃安全恢复读链（load_active/validate_and_load_index）与 index 编解码
//! （parse_index/serialize_index），后两者供 writer/updater 复用（pub(crate)）。

use std::fs::File;
use std::io;
use std::path::Path;

use super::format::{corrupt, crc32, read_exact_at};
use super::journal::replay_journal;
use super::superblock::{parse_superblock, SuperBlock};
use super::{
    ChunkEntry, Footer, HeadCache, DATA_START, HEADER_LEN, INDEX_ENTRY_LEN, MAGIC, SB_A_OFFSET,
    SB_B_OFFSET, SB_LEN, VERSION,
};
use crate::blockio::BlockIo;

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
            return Err(corrupt("magic 不匹配，非 scrollz archive"));
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
        // per-block CRC：校验封块存储字节，单块静默翻转即 fail-closed（ROADMAP T1）。head 缓存/
        // 尾日志走另路（前者可丢弃回退、后者 rec_crc），不经此校验。
        if crc32(&buf) != entry.block_crc {
            return Err(corrupt(&format!("块 {idx} CRC 不符（静默错读防护）")));
        }
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
pub(crate) fn footer_from_sb(sb: &SuperBlock) -> Footer {
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
fn read_sb_slot(io: &impl BlockIo, off: u64) -> io::Result<Option<SuperBlock>> {
    let mut buf = [0u8; SB_LEN as usize];
    read_exact_at(io, &mut buf, off)?;
    Ok(parse_superblock(&buf))
}

/// 级联校验并加载活跃 superblock：读两槽 → 候选按 seq 降序 → 逐个验证 index（bounds + index_crc +
/// 块 bounds）→ 取首个通过者，返回 `(活跃 SB, 活跃槽偏移, index)`；两槽皆不可用 → corrupt（M4）。
pub(crate) fn load_active(
    io: &impl BlockIo,
    total_len: u64,
) -> io::Result<(SuperBlock, u64, Vec<ChunkEntry>)> {
    let mut cands: Vec<(SuperBlock, u64)> = Vec::with_capacity(2);
    if let Some(sb) = read_sb_slot(io, SB_A_OFFSET)? {
        cands.push((sb, SB_A_OFFSET));
    }
    if let Some(sb) = read_sb_slot(io, SB_B_OFFSET)? {
        cands.push((sb, SB_B_OFFSET));
    }
    cands.sort_by_key(|c| std::cmp::Reverse(c.0.seq)); // seq 降序
    for (sb, off) in cands {
        if let Some(index) = validate_and_load_index(io, &sb, total_len)? {
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
    io: &impl BlockIo,
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
    read_exact_at(io, &mut index_bytes, sb.index_offset)?;
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
pub(crate) fn read_head_cache_bytes(
    io: &impl BlockIo,
    footer: &Footer,
    hc: HeadCache,
) -> Option<Vec<u8>> {
    let end = hc.offset.checked_add(hc.clen)?;
    if hc.offset < DATA_START || end > footer.index_offset {
        return None; // 越界 → 当作无缓存（优雅回退）。
    }
    let mut buf = vec![0u8; hc.clen as usize];
    read_exact_at(io, &mut buf, hc.offset).ok()?;
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
            let block_crc = u32::from_le_bytes(bytes[base + 20..base + 24].try_into().unwrap());
            ChunkEntry {
                offset,
                clen,
                flags,
                block_crc,
            }
        })
        .collect()
}

/// 序列化索引区为字节（与 `parse_index` 对偶）。Writer/Updater 共用，避免布局漂移。
pub(crate) fn serialize_index(index: &[ChunkEntry]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(index.len() * INDEX_ENTRY_LEN as usize);
    for e in index {
        bytes.extend_from_slice(&e.offset.to_le_bytes());
        bytes.extend_from_slice(&e.clen.to_le_bytes());
        bytes.extend_from_slice(&e.flags.to_le_bytes());
        bytes.extend_from_slice(&e.block_crc.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::testutil::{active_sb, build_archive, patch_both_sb};
    use crate::archive::ArchiveWriter;
    use std::io::{Cursor, Write};

    /// 把内存缓冲写到临时文件，open 成 ArchiveReader。
    fn reader_from_bytes(bytes: &[u8]) -> ArchiveReader {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(bytes).unwrap();
        tmp.flush().unwrap();
        ArchiveReader::open(tmp.path()).unwrap()
    }

    #[test]
    fn writer_reader_round_trip_single_block() {
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
    fn multi_block_offset_and_clen_correct() {
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
    fn out_of_bounds_block_returns_none() {
        let bytes = build_archive(64, &[(b"x".to_vec(), false, 1)]);
        let r = reader_from_bytes(&bytes);
        assert!(r.read_block(1).unwrap().is_none());
        assert!(r.entry(5).is_none());
    }

    #[test]
    fn zero_block_archive_valid() {
        let bytes = build_archive(64, &[]);
        let r = reader_from_bytes(&bytes);
        assert_eq!(r.chunk_count(), 0);
        assert_eq!(r.footer().uncompressed_size, 0);
        assert!(r.read_block(0).unwrap().is_none());
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bytes = build_archive(64, &[(b"y".to_vec(), false, 1)]);
        bytes[0] = b'X'; // 破坏 magic
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&bytes).unwrap();
        tmp.flush().unwrap();
        let err = expect_open_err(tmp.path());
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn index_crc_corruption_detected() {
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
    fn block_silent_byte_flip_detected_by_per_block_crc() {
        // index_crc/越界都自洽，仅某块数据字节翻转 → read_block 应 fail-closed（per-block CRC，T1）。
        let bytes = build_archive(64, &[(b"compressed-block-0".to_vec(), false, 50)]);
        let r = reader_from_bytes(&bytes);
        let off = r.entry(0).unwrap().offset as usize;
        let mut corrupted = bytes;
        corrupted[off] ^= 0xFF; // 翻转块0首字节（不动 index/SB，故 index_crc 仍自洽）
        let r2 = reader_from_bytes(&corrupted);
        let err = r2.read_block(0).unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidData,
            "块字节损坏应 fail-closed"
        );
    }

    #[test]
    fn out_of_bounds_clen_rejected_during_open() {
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

    // ----- head 缓存（发现读快路径，docs/02）-----

    #[test]
    fn head_cache_absent_reader_returns_none() {
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
    fn head_cache_verbatim_flag_preserved() {
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
    fn head_cache_out_of_bounds_graceful_fallback_none() {
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
}
