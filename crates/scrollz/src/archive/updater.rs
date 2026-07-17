//! ArchiveUpdater：在已存在 archive 上 append-only 更新 + 双 superblock 原子提交
//! （崩溃安全提交协议，docs/04 §12）
//!
//! 核心不变量（C2）：**写游标恒取物理 EOF**，新块/index/head 缓存一律 append 到文件末尾——
//! 绝不 `set_len` 截 live 数据、绝不原地覆盖任何 superblock 可达区间（由构造满足 C2）。
//! 这彻底删除了旧 `reuse_tail_slot` + `set_len`（原 durability bug 的发源地）。
//! 提交点 = 交替写两个固定 superblock 槽之一（带 seq+CRC），半截写总留另一槽完好 → 永远可恢复。
//! 被取代的旧块/旧 index 成空洞，仅压实回收（在线写从不就地覆盖）。

use std::io;
use std::path::Path;

use super::format::{corrupt, crc32, read_exact_at};
use super::journal::serialize_journal_record;
use super::reader::{footer_from_sb, load_active, read_head_cache_bytes, serialize_index};
use super::superblock::{serialize_superblock, SuperBlock};
use super::{
    ChunkEntry, HeadCache, DATA_START, FLAG_VERBATIM, HEADER_LEN, MAGIC, SB_A_OFFSET, SB_B_OFFSET,
    VERSION,
};
use crate::blockio::BlockIo;

/// 在已存在 archive 上 append-only 更新。`set_block`/`truncate` 改内存 index 并把新块写到 EOF，
/// `commit` append 新 index + 写非活跃 superblock 槽（双段 barrier）。
///
/// 泛型 `W: BlockIo` 是故障注入接缝（docs/05 §3）：生产为 `ArchiveUpdater<File>`（经
/// `impl BlockIo for File`，与改造前逐字节等价），测试经 `from_io(FaultIo)` 注入确定性崩溃。
/// 写路径全部经 `self.io.write_at`（绝对偏移 pwrite，与旧 `seek+write_all` 严格等价）+
/// `self.io.sync`（唯一 barrier）。
pub struct ArchiveUpdater<W: BlockIo> {
    io: W,
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
    /// 自上次 full commit 起 index 是否被 `set_block`/`truncate` 改过（评审 B3）。
    /// `commit_journal` 要求其为 false（否则新块不可达）；`commit` 收尾置回 false。
    index_dirty: bool,
}

impl ArchiveUpdater<std::fs::File> {
    /// 打开已存在的 archive 供更新（读写）。空/缺文件请先用 `ArchiveWriter` 建一个 0 块 archive。
    /// 生产入口：内部 `OpenOptions` 开 `File` → `from_io`（经 `impl BlockIo for File`）。
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        Self::from_io(file)
    }
}

impl<W: BlockIo> ArchiveUpdater<W> {
    /// 从注入的 `BlockIo` 构造 updater（生产为 `File`，故障注入测试为 `FaultIo`，docs/05 §3）。
    /// 读 header（经 BlockIo）→ `load_active`（读双 SB + index + bounds）→ 载入既有 head 缓存。
    /// 这条**打开/恢复读链**正是双 SB 崩溃安全最该测的路径，故必须经 `BlockIo` 可注入。
    pub fn from_io(io: W) -> io::Result<Self> {
        let total_len = io.len()?;
        if total_len < DATA_START {
            return Err(corrupt("文件太小，不足 header + 双 superblock"));
        }
        let mut header = [0u8; HEADER_LEN as usize];
        read_exact_at(&io, &mut header, 0)?;
        if header[..8] != MAGIC || u32::from_le_bytes(header[8..12].try_into().unwrap()) != VERSION
        {
            return Err(corrupt("非 v2 archive"));
        }
        let (sb, active_off, index) = load_active(&io, total_len)?;
        let footer = footer_from_sb(&sb);
        // 载入既有 head 缓存字节（越界 → None，可丢弃派生数据 M2）。
        let head_cache = sb.head_cache.and_then(|hc| {
            read_head_cache_bytes(&io, &footer, hc).map(|b| (b, hc.verbatim, hc.rawlen))
        });
        let inactive_off = if active_off == SB_A_OFFSET {
            SB_B_OFFSET
        } else {
            SB_A_OFFSET
        };
        Ok(Self {
            io,
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
            index_dirty: false,
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
        // 评审 B3：commit_journal 复用 committed_index，要求自上次 full commit 起 index 未变。
        // 否则新块写到 EOF 但 SB 仍指旧 index → 新块不可达（数据丢失）。误用时 debug 崩溃定位。
        debug_assert!(
            !self.index_dirty,
            "commit_journal 在 index 变更后被调用——应改用 commit（否则新块不可达）"
        );
        // barrier 1：journal 记录已落盘（append_journal 已写，这里确保 durable）。
        self.io.sync()?;
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
        self.io
            .write_at(self.inactive_off, &serialize_superblock(&sb))?;
        self.io.sync()?; // barrier 2
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
        self.io.write_at(self.write_cursor, &rec)?;
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

    /// 设置逻辑文件大小（尾日志追加未封尾块原始字节后，逻辑大小增长但块集不变，commit_journal
    /// 须随之更新 SB.uncompressed_size，否则重开看不到新增尾字节，docs/04 §8.4）。
    pub fn set_size(&mut self, new_size: u64) {
        self.uncompressed_size = new_size;
    }

    /// 设置 / 更新 head 缓存（块 0 首次封存或头区 RMW 后由上层调用，docs/02 §4.3）。
    pub fn set_head_cache(&mut self, stored_bytes: Vec<u8>, verbatim: bool, raw_len: u64) {
        self.head_cache = Some((stored_bytes, verbatim, raw_len));
    }

    /// 明确清除 head 缓存。用于块 0 已改变但调用方没有提供对应新缓存的提交，避免沿用旧前缀。
    pub fn clear_head_cache(&mut self) {
        self.head_cache = None;
        self.committed_head_cache = None;
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
            let zeros_crc = crc32(&zeros);
            while (self.index.len() as u64) < idx {
                let offset = self.write_cursor;
                self.io.write_at(offset, &zeros)?;
                self.write_cursor += zeros.len() as u64;
                self.index.push(ChunkEntry {
                    offset,
                    clen: zeros.len() as u64,
                    flags: FLAG_VERBATIM,
                    block_crc: zeros_crc,
                });
            }
        }
        // 恒 append 到 EOF（append-only，C2：绝不覆写任何 SB 可达区间，删除 reuse/set_len）。
        let offset = self.write_cursor;
        self.io.write_at(offset, stored_bytes)?;
        self.write_cursor = offset + stored_bytes.len() as u64;
        let entry = ChunkEntry {
            offset,
            clen: stored_bytes.len() as u64,
            flags: if verbatim { FLAG_VERBATIM } else { 0 },
            block_crc: crc32(stored_bytes),
        };
        let count = self.index.len() as u64;
        if idx == count {
            self.index.push(entry);
        } else {
            self.index[idx as usize] = entry; // 旧块成空洞
        }
        self.uncompressed_size = new_size;
        self.index_dirty = true; // 评审 B3：index 已变，commit_journal 不再可用
        Ok(())
    }

    /// 截断到 `keep_from` 块（丢弃其后所有块）+ 设新逻辑大小（仅改内存 index，旧块成空洞）。
    pub fn truncate(&mut self, keep_from: u64, new_size: u64) {
        if (keep_from as usize) < self.index.len() {
            self.index.truncate(keep_from as usize);
        }
        self.uncompressed_size = new_size;
        self.index_dirty = true; // 评审 B3：index 已变
                                 // 评审 B1：head 缓存覆盖块 0 的前 rawlen 字节。若截断使文件短于该前缀，缓存即越界
                                 // （发现读会返回已被截掉的陈旧字节）。文件仍长于 rawlen 时块 0 前缀不变、缓存仍有效。
                                 // 同时清 committed_head_cache，杜绝 commit/commit_journal 任一路径重写陈旧指针。
        let rawlen = self
            .head_cache
            .as_ref()
            .map(|(_, _, r)| *r)
            .or_else(|| self.committed_head_cache.map(|h| h.rawlen))
            .unwrap_or(0);
        if rawlen > 0 && new_size < rawlen {
            self.head_cache = None;
            self.committed_head_cache = None;
        }
    }

    /// 原子提交（docs/04 §3）：append [head 缓存] + 新 index 到 EOF → **barrier 1 fsync** →
    /// 写非活跃 superblock 槽（seq+1）→ **barrier 2 fsync（原子提交点）** → 翻转活跃槽。
    pub fn commit(&mut self) -> io::Result<()> {
        // 评审 B3：index 已变时 journal 必须已重置（封块契约 set_block→reset_journal→commit），
        // 否则新 SB 同时指向新 index + 旧 journal 区间 → read_tail 把陈旧 raw delta 叠加到新封块
        // 逻辑尾部 → 静默逻辑损坏（journal 有自己的 rec_crc，检不出）。误用时 debug 崩溃定位。
        debug_assert!(
            !(self.index_dirty && self.journal_len > 0),
            "commit 时 index 已变但 journal 未重置——封块前须 reset_journal，否则重放污染封块"
        );
        // 1) append head 缓存（若有）+ 新 index 到 EOF。
        let head_cache = match &self.head_cache {
            Some((bytes, verbatim, raw_len)) => {
                let offset = self.write_cursor;
                self.io.write_at(offset, bytes)?;
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
        self.io.write_at(index_offset, &index_bytes)?;
        self.write_cursor += index_bytes.len() as u64;
        // barrier 1：数据 + index 落盘（检查返回值；失败则不写/不推进 superblock，旧活跃槽不受损）。
        self.io.sync()?;

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
        self.io
            .write_at(self.inactive_off, &serialize_superblock(&sb))?;
        // barrier 2：superblock 落盘 → 新版本原子生效。
        self.io.sync()?;

        // 更新已提交 index / head 缓存描述符（供后续 commit_journal 复用），翻转活跃槽。
        self.committed_index = (index_offset, index_bytes.len() as u64, index_crc);
        self.committed_head_cache = head_cache;
        self.index_dirty = false; // 评审 B3：新 index 已 durable，commit_journal 可再次复用
        self.flip_active(new_seq);
        Ok(())
    }

    /// fsync 后端文件（commit 已含 barrier，保留兼容 shadow 的 commit→sync 调用序）。
    pub fn sync(&self) -> io::Result<()> {
        self.io.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::testutil::{active_sb, build_archive, patch_both_sb};
    use crate::archive::{parse_superblock, ArchiveReader, ArchiveWriter};
    use std::io::Write;

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
    fn updater_append_tail_block_does_not_rewrite_prior_data() {
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
    fn updater_rmw_interior_block_appends_new_version_old_block_becomes_hole() {
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
    fn updater_truncate_discards_tail_block() {
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
    fn updater_after_commit_crc_stays_consistent_reopenable() {
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
    fn updater_crash_before_commit_recovers_last_consistent_version() {
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
    fn updater_active_sb_corrupt_falls_back_to_other_slot_recovers() {
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
    fn updater_repeated_tail_rewrite_across_commits_readback_correct() {
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
    fn updater_preserves_head_cache_across_commits_append() {
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
    fn updater_truncate_discards_out_of_bounds_head_cache() {
        // 评审 B1：head 缓存覆盖块 0 前缀 rawlen 字节。若 truncate 使文件短于 rawlen，
        // 旧码不动 head_cache → commit 原样重写陈旧缓存 → 发现读返回已被截掉的旧前缀。
        // 截断到短于 rawlen 后，缓存必须失效（rawlen 归 0）。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.archive");
        {
            let mut w = ArchiveWriter::create(&path, 8).unwrap();
            w.append_block(b"AAAAAAAA", false, 8).unwrap();
            w.append_block(b"BBBBBBBB", false, 8).unwrap();
            w.set_head_cache(b"HEADCACHE-16B".to_vec(), false, 16); // 缓存覆盖前 16 字节
            w.finish().unwrap().sync_all().unwrap();
        }
        let mut up = ArchiveUpdater::open(&path).unwrap();
        assert_eq!(up.head_cache_rawlen(), 16);
        // 截断到 4 字节（< rawlen=16）：缓存越界，必须丢弃。
        up.truncate(0, 4);
        up.set_block(0, b"AAAA", false, 4).unwrap();
        up.commit().unwrap();

        let r = ArchiveReader::open(&path).unwrap();
        assert_eq!(r.footer().uncompressed_size, 4);
        assert_eq!(
            r.head_cache_rawlen(),
            0,
            "截断到短于 rawlen 后，陈旧 head 缓存必须失效（否则发现读越界返回旧数据）"
        );
    }

    #[test]
    fn updater_set_head_cache_after_commit_readback() {
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
    fn updater_with_head_cache_crash_before_commit_recovers_last_consistent_version() {
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
    fn journal_append_commit_reopen_replay_tail_block() {
        // 0 块 archive（空文件）。逐次 append_journal（原始增量）+ commit_journal（不动 index）→
        // 重开 read_tail 应重放出全部原始字节。模拟 fsync 路径：不压缩、只追加 delta。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.archive");
        {
            let w = ArchiveWriter::create(&path, 64).unwrap();
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
    fn journal_crash_before_commit_drop_uncommitted_delta_keep_committed() {
        // append_journal 两条都 commit_journal（durable），第三条 append 后不 commit（drop=崩溃）。
        // 重开应只见前两条（已提交），第三条未提交被忽略——崩溃安全的尾日志。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.archive");
        {
            let w = ArchiveWriter::create(&path, 64).unwrap();
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
    fn journal_reset_after_sealing_tail_migrated_into_compressed_block() {
        // 攒了尾日志后「封块」：set_block 写压缩块（这里 verbatim 模拟）+ reset_journal + commit。
        // 重开：read_tail 应为 None（journal 已重置），内容改由块 0 承载。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.archive");
        {
            let w = ArchiveWriter::create(&path, 64).unwrap();
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
    fn journal_out_of_bounds_load_active_rejects_slot() {
        // SB 的 tail_journal 区越界 → 该槽不可用（级联校验拒绝）。两槽都坏则报损坏。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.archive");
        {
            let w = ArchiveWriter::create(&path, 64).unwrap();
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

    // ----- 字节等价回归网（spy IO，docs/05 任务 1.2c）-----
    //
    // 现有 updater 测试只经 reader 间接验最终字节正确，offset 错位可能因 CRC 自洽蒙混。本网用
    // in-memory BlockIo 镜像与真实 File 路径逐字节比对：钉死「FaultIo 盘面模型字节级保真」——
    // Tier 1「crash() 镜像 → ArchiveReader::from_file」全建在这条保真之上。

    /// 记录所有 `write_at` 的 spy `BlockIo`：内部 `Vec<u8>` 模拟盘面（绝对偏移写、越界零填充扩展，
    /// 仿 `File` 语义），`Clone` 共享内部状态，使 updater 消费后仍能取出镜像与写记录。
    #[derive(Clone)]
    struct RecordingIo {
        inner: std::sync::Arc<std::sync::Mutex<RecState>>,
    }
    struct RecState {
        data: Vec<u8>,
        writes: Vec<(u64, usize)>,
    }
    impl RecordingIo {
        fn new(initial: Vec<u8>) -> Self {
            Self {
                inner: std::sync::Arc::new(std::sync::Mutex::new(RecState {
                    data: initial,
                    writes: Vec::new(),
                })),
            }
        }
        fn image(&self) -> Vec<u8> {
            self.inner.lock().unwrap().data.clone()
        }
        fn writes(&self) -> Vec<(u64, usize)> {
            self.inner.lock().unwrap().writes.clone()
        }
    }
    impl BlockIo for RecordingIo {
        fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<()> {
            let mut st = self.inner.lock().unwrap();
            let end = off as usize + buf.len();
            if st.data.len() < end {
                st.data.resize(end, 0); // 越界写自动零填充扩展，仿 File 语义。
            }
            st.data[off as usize..end].copy_from_slice(buf);
            st.writes.push((off, buf.len()));
            Ok(())
        }
        fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
            let st = self.inner.lock().unwrap();
            let end = off as usize + buf.len();
            if end > st.data.len() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read 越界"));
            }
            buf.copy_from_slice(&st.data[off as usize..end]);
            Ok(())
        }
        fn sync(&self) -> io::Result<()> {
            Ok(())
        }
        fn len(&self) -> io::Result<u64> {
            Ok(self.inner.lock().unwrap().data.len() as u64)
        }
        fn set_len(&self, len: u64) -> io::Result<()> {
            self.inner.lock().unwrap().data.resize(len as usize, 0);
            Ok(())
        }
    }

    #[test]
    fn updater_spy_io_byte_mirror_equals_file_path_byte_for_byte() {
        // 基线档 1 块；两条路径从同一初始字节出发跑同一工作负载（两次 set_block + commit），
        // 比对最终盘面逐字节相等——证 in-memory 基底与真实 File 字节级保真。
        let base = build_archive(8, &[(b"AAAAAAAA".to_vec(), false, 8)]);

        // golden = File 路径（生产，经 impl BlockIo for File；已被全套 reader 测试 + crash-test.sh 验证）。
        let golden = {
            let mut tmp = tempfile::NamedTempFile::new().unwrap();
            tmp.write_all(&base).unwrap();
            tmp.flush().unwrap();
            let mut up = ArchiveUpdater::open(tmp.path()).unwrap();
            up.set_block(1, b"BBBB", false, 12).unwrap();
            up.set_block(0, b"XXXXXXXX", false, 16).unwrap();
            up.commit().unwrap();
            std::fs::read(tmp.path()).unwrap()
        };

        // mirror = spy 路径（in-memory Vec 基底，记录 write）。
        let spy = RecordingIo::new(base.clone());
        let probe = spy.clone();
        let mut up = ArchiveUpdater::from_io(spy).unwrap();
        up.set_block(1, b"BBBB", false, 12).unwrap();
        up.set_block(0, b"XXXXXXXX", false, 16).unwrap();
        up.commit().unwrap();
        let mirror = probe.image();

        assert_eq!(
            mirror, golden,
            "spy in-memory 盘面应与 File 路径逐字节等价（offset 错位会在此暴露）"
        );

        // 独立结构校验：活跃 SB 的 index 区由测试侧定位，断言镜像该处字节自洽，且 index append 在
        // 基线档之后（不靠 reader 自证；堵「写错 offset 但 SB 指向别处仍 CRC 自洽」）。
        let sb = active_sb(&mirror);
        assert!(
            sb.index_offset as usize >= base.len(),
            "index 应 append 在基线档之后"
        );
        let idx = &mirror[sb.index_offset as usize..(sb.index_offset + sb.index_len) as usize];
        assert_eq!(
            crc32(idx),
            sb.index_crc,
            "镜像 index 区应与活跃 SB 的 index_crc 自洽"
        );
        // spy 确实记录了写（commit 至少写了 index + 一个 SB 槽）。
        assert!(probe.writes().len() >= 2, "spy 应记录 commit 期间的写");
    }
}
