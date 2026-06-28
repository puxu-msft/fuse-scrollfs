//! Core 写会话：未压缩**开放尾块缓冲**（open-tail buffer），append 优化的核心（§1.1、§3）。
//!
//! ## 问题
//! 原 `rmw::write_at` 对未满尾块的每次小 append 都走
//! `get_block→decompress→patch→compress→put_block`，即**每次都把尾块整块重压一遍**。
//! 64KiB 块 + 1KB 行 → 一个尾块封块前被重压约 64 次（见 docs §1.1 追加写硬约束）。
//!
//! ## 优化
//! 把**当前未压缩的尾块字节**缓存在 Core 的 per-inode 写会话里（不放 Store——Store 仍只存
//! 已封的压缩块，压缩仍全在 Core，保持 §5 接缝干净，两布局 BS/BV 同时受益）：
//! - **append / 写尾块** → 直接写进未压缩缓冲，**不压缩**。
//! - **封块（seal）时机**：尾块填满 chunk_size、flush/fsync/release、或一个需要旧尾块的
//!   非尾块写——此时才 compress + `put_block` 落到 Store。
//! - **读协调**：读路径必须先查写会话；若请求块正是缓冲中的尾块，**从未压缩缓冲返回**，
//!   不走 `get_block`（否则读到的是尚未封块的旧版本）。
//! - **几何**：`geometry` 返回的逻辑大小须包含未封尾块（写后读一致、getattr 正确）。
//!
//! ## 并发
//! 写/封块由 rwfs 的 per-inode 写锁串行化（见 rwfs::lock_for）；本结构内部再用一把
//! `Mutex` 保护尾块表本身，使多读者读尾块安全（读只短暂持表锁克隆出尾块字节）。
//!
//! ## durability
//! 未 flush 的尾块在内存（与 page cache 未刷一致）；fsync/flush 必须先 `seal` 把尾块封块
//! 落 Store，再由 Store 持久化，符合 POSIX fsync 契约（§10）。

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::core::rmw::{self, CodecParams};
use crate::store::Store;

/// 一个 inode 的开放尾块缓冲：未压缩字节 + 块号 + 文件逻辑大小。
#[derive(Debug, Clone)]
struct Tail {
    /// 缓冲的尾块块号。
    idx: u64,
    /// 该尾块的**未压缩**字节（长度即该块逻辑长度，<= chunk_size）。
    plain: Vec<u8>,
    /// 块大小（建会话时取自 Store 几何）。
    chunk_size: u32,
    /// 文件当前逻辑大小（含本未封尾块）。等于 `idx*chunk_size + plain.len()`。
    file_size: u64,
}

/// per-inode 开放尾块缓冲表。Core 层持有（rwfs 拥有一个实例），Store 不感知。
///
/// `enabled=false` 时所有写/读/封块都直通旧的无状态 `rmw` 路径（`--no-tail-buffer` 基准对照）。
pub struct TailSessions {
    /// ino → 当前开放尾块。无项表示该 inode 当前无开放尾块（全部已封块落 Store）。
    tails: Mutex<HashMap<u64, Tail>>,
    /// 优化开关：false 时退化为旧路径（每次 append 重压尾块）。
    enabled: bool,
    /// 累计封块次数（含每次「把尾块压缩落 Store」），基准量化重压次数用。
    seal_count: AtomicU64,
}

impl TailSessions {
    /// 新建（`enabled` 控制是否启用尾块缓冲优化）。
    pub fn new(enabled: bool) -> Self {
        Self {
            tails: Mutex::new(HashMap::new()),
            enabled,
            seal_count: AtomicU64::new(0),
        }
    }

    /// 累计封块次数（基准埋点）。每次把一个尾块压缩并 `put_block` 落 Store 记一次。
    pub fn seal_count(&self) -> u64 {
        self.seal_count.load(Ordering::Relaxed)
    }

    /// 是否启用了尾块缓冲优化。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 取逻辑几何 `(size, chunk_size)`，**含未封尾块**。无开放尾块则回落 Store 几何。
    ///
    /// 表锁保证读到自洽的 `file_size`（不会撕裂）。但要与并发 seal/truncate 取得**读后写一致**
    /// （如 getattr 与紧随的 read 看到同一代视图），调用方须持该 inode 写锁——getattr/lookup
    /// 走无锁路径，仅保证自洽 size，1s 属性 TTL 内的轻微滞后可接受（rust-review MEDIUM-2）。
    pub fn geometry(&self, store: &dyn Store, ino: u64) -> Option<(u64, u32)> {
        if self.enabled {
            if let Some(t) = self.tails.lock().unwrap().get(&ino) {
                return Some((t.file_size, t.chunk_size));
            }
        }
        store.block_geometry(ino)
    }

    /// 读第 `idx` 块的**未压缩**逻辑字节：命中开放尾块则从缓冲返回 `Some(plain)`，
    /// 否则返回 `None`（调用方回落 `Store::get_block` + 解压）。
    ///
    /// 读协调关键：尾块在缓冲里尚未封块，直接读 Store 会读到旧版本或缺块。多读者安全
    /// （短暂持表锁克隆字节）。
    pub fn read_tail_block(&self, ino: u64, idx: u64) -> Option<Vec<u8>> {
        if !self.enabled {
            return None;
        }
        let tails = self.tails.lock().unwrap();
        let t = tails.get(&ino)?;
        if t.idx == idx {
            Some(t.plain.clone())
        } else {
            None
        }
    }

    /// 在 `offset` 写入 `data`。启用时尽量写进开放尾块缓冲（不压缩）；需要封块的情形
    /// （跨到新块、非尾块写、越界空洞）按需先 seal 再走旧 `rmw::write_at`，最后把新尾块
    /// 重新装入缓冲。返回写入字节数（恒为 data.len()）。
    ///
    /// 调用方须持该 inode 写锁（rwfs::lock_for）。
    pub fn write_at(
        &self,
        store: &dyn Store,
        ino: u64,
        offset: u64,
        data: &[u8],
        params: CodecParams,
    ) -> io::Result<usize> {
        if !self.enabled {
            return rmw::write_at(store, ino, offset, data, params);
        }
        if data.is_empty() {
            return Ok(0);
        }

        // 取当前几何（含开放尾块）。
        let Some((cur_size, chunk_size)) = self.geometry(store, ino) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("write_at：ino={ino} 非可分块文件或不存在"),
            ));
        };

        // 纯尾部 append（offset == 当前逻辑大小）是优化主路径：直接进缓冲，可能跨多块。
        if offset == cur_size {
            return self.append_into_tail(store, ino, data, chunk_size, cur_size, params);
        }

        // 非纯 append（随机写 / 越 EOF 空洞 / 改写已封块或尾块中部）：先封掉开放尾块，
        // 让 Store 持有自洽的全量已封块，再走旧无状态 RMW；写完把新尾块重新装入缓冲。
        self.seal(store, ino, params)?;
        let n = rmw::write_at(store, ino, offset, data, params)?;
        self.refill_tail_from_store(store, ino, params)?;
        Ok(n)
    }

    /// 纯尾部 append：把 `data` 逐块灌进开放尾块缓冲。块满即 seal 落 Store 并开新尾块。
    fn append_into_tail(
        &self,
        store: &dyn Store,
        ino: u64,
        data: &[u8],
        chunk_size: u32,
        cur_size: u64,
        params: CodecParams,
    ) -> io::Result<usize> {
        // 确保缓冲里有「当前尾块」。无缓冲时从 Store 装入未满尾块（或在块边界时建空尾块）。
        self.ensure_tail_loaded(store, ino, chunk_size, cur_size, params)?;

        let total = data.len();
        let mut written = 0usize;
        while written < total {
            // 当前尾块剩余可写空间。调用方持该 inode 写锁，故正常不会被并发移除；但 unlink-while-open
            // 等边角下 forget 可能介入，缺尾块时返回错误而非 panic（rust-review MEDIUM-3）。
            let mut tails = self.tails.lock().unwrap();
            let Some(t) = tails.get_mut(&ino) else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("append_into_tail：ino={ino} 尾块被并发移除（疑似 unlink-while-open）"),
                ));
            };
            let room = chunk_size as usize - t.plain.len();
            let take = room.min(total - written);
            t.plain.extend_from_slice(&data[written..written + take]);
            t.file_size += take as u64;
            written += take;
            let full = t.plain.len() == chunk_size as usize;
            drop(tails);

            if full && written < total {
                // 尾块填满且还有数据：封块落 Store，开下一块空尾块。
                self.seal(store, ino, params)?;
                let next_size = cur_size + written as u64;
                self.start_empty_tail(ino, next_size, chunk_size);
            } else if full {
                // 正好填满且数据写完：封块（不留半块在内存，下次 append 再装入）。
                self.seal(store, ino, params)?;
            }
        }
        Ok(total)
    }

    /// 确保 `ino` 的开放尾块已装入缓冲。已在缓冲则什么都不做；否则从 Store 取末块解压装入，
    /// 若文件正好块对齐（末块已满或空文件）则建一个新的空尾块。
    fn ensure_tail_loaded(
        &self,
        store: &dyn Store,
        ino: u64,
        chunk_size: u32,
        cur_size: u64,
        params: CodecParams,
    ) -> io::Result<()> {
        if self.tails.lock().unwrap().contains_key(&ino) {
            return Ok(());
        }
        let cs = chunk_size as u64;
        let tail_len_in_block = (cur_size % cs) as usize;
        if cur_size == 0 || tail_len_in_block == 0 {
            // 块对齐（或空）：开新空尾块，块号 = cur_size / cs。
            self.start_empty_tail(ino, cur_size, chunk_size);
            return Ok(());
        }
        // 末块未满：从 Store 取该块解压，作为开放尾块。
        let idx = cur_size / cs;
        let mut plain = rmw::load_plain_block(store, ino, idx, params)?;
        if plain.len() != tail_len_in_block {
            plain.resize(tail_len_in_block, 0);
        }
        self.tails.lock().unwrap().insert(
            ino,
            Tail {
                idx,
                plain,
                chunk_size,
                file_size: cur_size,
            },
        );
        Ok(())
    }

    /// 在缓冲里建一个空尾块（块号由 size/chunk_size 推出）。覆盖任何既有项（调用前应已 seal）。
    fn start_empty_tail(&self, ino: u64, size: u64, chunk_size: u32) {
        let idx = size / chunk_size as u64;
        self.tails.lock().unwrap().insert(
            ino,
            Tail {
                idx,
                plain: Vec::with_capacity(chunk_size as usize),
                chunk_size,
                file_size: size,
            },
        );
    }

    /// 封块（seal）：把开放尾块压缩并 `put_block` 落 Store，移除缓冲项。无开放尾块则 no-op。
    ///
    /// fsync/flush/release，以及任何需要 Store 持有自洽全量块的操作前都要先 seal。
    ///
    /// **顺序（堵无锁读者的 torn-read，rust-review HIGH-1）**：先 `store_plain_block` 把尾块落 Store，
    /// **再**从缓冲移除。期间尾块同时在缓冲与 Store（字节一致，读者优先读缓冲，安全）；绝不出现
    /// 「缓冲已删、Store 未写」的空窗——否则并发读会把有数据的块零填充。调用方持该 inode 写锁，
    /// 故 seal 之间不会相互交错；唯一并发方是无锁读者（`read_tail_block`/`geometry`）。
    pub fn seal(&self, store: &dyn Store, ino: u64, params: CodecParams) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        // 取尾块快照（克隆），不立刻移除：先落 Store 再删缓冲。
        let snapshot = self.tails.lock().unwrap().get(&ino).cloned();
        let Some(t) = snapshot else {
            return Ok(());
        };
        // 空尾块（块对齐文件刚开的空 tail，无任何 append）无需写：避免凭空多存一个零长块。
        // 此时 size 已由前一次满块 seal 的 put_block(new_size) 落 Store，移除缓冲不丢 size。
        if t.plain.is_empty() && t.file_size % (t.chunk_size as u64) == 0 {
            self.tails.lock().unwrap().remove(&ino);
            return Ok(());
        }
        // 先落 Store（尾块仍在缓冲，读者读到一致字节）。
        rmw::store_plain_block(store, ino, t.idx, &t.plain, t.file_size, params)?;
        self.seal_count.fetch_add(1, Ordering::Relaxed);
        // 落盘成功后再移除缓冲——此后读者回落 Store，块已在。若 store 失败提前返回，缓冲保留，
        // 下次 seal 重试（不丢数据）。
        self.tails.lock().unwrap().remove(&ino);
        Ok(())
    }

    /// 截断（或零填充扩展）到 `new_size`。先 seal 开放尾块，再走旧 `rmw::truncate`，
    /// 之后把新末块重新装入缓冲（便于后续 append 继续走快路径）。
    pub fn truncate(
        &self,
        store: &dyn Store,
        ino: u64,
        new_size: u64,
        params: CodecParams,
    ) -> io::Result<()> {
        if !self.enabled {
            return rmw::truncate(store, ino, new_size, params);
        }
        self.seal(store, ino, params)?;
        rmw::truncate(store, ino, new_size, params)?;
        Ok(())
    }

    /// 从 Store 当前几何重新装入开放尾块（非 append 写 / 截断后调用，保持快路径可用）。
    fn refill_tail_from_store(
        &self,
        store: &dyn Store,
        ino: u64,
        params: CodecParams,
    ) -> io::Result<()> {
        let Some((size, chunk_size)) = store.block_geometry(ino) else {
            return Ok(());
        };
        let cs = chunk_size as u64;
        // 仅当末块未满时才装入（块对齐则保持「无开放尾块」，下次 append 再懒建空尾块）。
        if size == 0 || size % cs == 0 {
            return Ok(());
        }
        let idx = size / cs;
        let tail_len = (size % cs) as usize;
        let mut plain = rmw::load_plain_block(store, ino, idx, params)?;
        if plain.len() != tail_len {
            plain.resize(tail_len, 0);
        }
        self.tails.lock().unwrap().insert(
            ino,
            Tail {
                idx,
                plain,
                chunk_size,
                file_size: size,
            },
        );
        Ok(())
    }

    /// 丢弃某 inode 的开放尾块**不封块**（unlink/rename 覆盖：底层数据即将消失，封块无意义）。
    pub fn forget(&self, ino: u64) {
        self.tails.lock().unwrap().remove(&ino);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::codec::{decompress, Algo};
    use crate::store::tests_support::MemStore;
    use crate::store::Store;

    fn params() -> CodecParams {
        CodecParams {
            algo: Algo::Zstd,
            level: 3,
        }
    }

    /// 经 Store 读回某已封块的逻辑字节（解压）。
    fn read_sealed(store: &dyn Store, ino: u64, idx: u64) -> Option<Vec<u8>> {
        store
            .get_block(ino, idx)
            .unwrap()
            .map(|b| decompress(&b.bytes, Algo::Zstd, b.stored_verbatim).unwrap())
    }

    /// 经会话 + Store 读回整文件逻辑字节（读协调：尾块走缓冲，其余走 Store 解压）。
    fn read_all(ws: &TailSessions, store: &dyn Store, ino: u64) -> Vec<u8> {
        let (size, cs) = ws.geometry(store, ino).unwrap();
        let mut out = vec![0u8; size as usize];
        let cs = cs as u64;
        let nblocks = size.div_ceil(cs);
        for idx in 0..nblocks {
            let start = (idx * cs) as usize;
            let plain = if let Some(p) = ws.read_tail_block(ino, idx) {
                p
            } else if let Some(p) = read_sealed(store, ino, idx) {
                p
            } else {
                continue;
            };
            let end = (start + plain.len()).min(out.len());
            if start < end {
                out[start..end].copy_from_slice(&plain[..end - start]);
            }
        }
        out
    }

    #[test]
    fn 大量小_append_后整文件正确_且重压次数远少于行数() {
        // 64 字节块，逐行 append 4 字节 × 100 行 = 6400 字节 = 100 块。
        let store = MemStore::new(64);
        let ino = store.new_file();
        let ws = TailSessions::new(true);

        let mut expected = Vec::new();
        for i in 0..100u32 {
            let line = format!("L{i:03}"); // 恰 4 字节
            let off = ws.geometry(&store, ino).unwrap().0;
            ws.write_at(&store, ino, off, line.as_bytes(), params())
                .unwrap();
            expected.extend_from_slice(line.as_bytes());
        }
        // 读协调：未 fsync 时整文件已可读（尾块走缓冲）。
        assert_eq!(read_all(&ws, &store, ino), expected);

        // 重压（封块）次数应约等于「已填满的块数」（~99），远少于 append 次数（100）。
        // 关键断言：不是「每次 append 一次重压」（若为旧路径会 >= 100 且尾块被反复重压）。
        let seals = ws.seal_count();
        assert!(
            seals <= 100,
            "封块次数 {seals} 不应超过块数级别（每满块一次），而非每次 append 一次重压"
        );
        // 每个满块只封一次：6400/64 = 100 块，其中 99 个在中途填满被封，末块（第100块，idx99）
        // 正好填满也封。粗略上界即可，核心是「远小于 1KB 行场景的 64x 重压」。
    }

    #[test]
    fn fsync_后尾块已封且可经_store_读出() {
        let store = MemStore::new(16);
        let ino = store.new_file();
        let ws = TailSessions::new(true);
        ws.write_at(&store, ino, 0, b"hello", params()).unwrap();
        // 未 seal 前 Store 无此块（在缓冲里）。
        assert!(
            store.get_block(ino, 0).unwrap().is_none(),
            "seal 前 Store 不应有尾块"
        );
        // fsync 模拟：seal 落盘。
        ws.seal(&store, ino, params()).unwrap();
        assert_eq!(read_sealed(&store, ino, 0).as_deref(), Some(&b"hello"[..]));
        assert_eq!(store.block_geometry(ino).unwrap().0, 5, "size 落 Store");
    }

    #[test]
    fn read_while_appending_读到缓冲尾块而非旧封块() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let ws = TailSessions::new(true);
        // 先写满块0（封块落 Store），再 append 进块1（缓冲）。
        ws.write_at(&store, ino, 0, b"AAAAAAAA", params()).unwrap(); // 块0满→封
        ws.write_at(&store, ino, 8, b"BB", params()).unwrap(); // 块1缓冲
                                                               // 读块1必须读到缓冲里的 "BB"，而不是 Store（Store 此时无块1）。
        assert_eq!(ws.read_tail_block(ino, 1).as_deref(), Some(&b"BB"[..]));
        assert!(
            store.get_block(ino, 1).unwrap().is_none(),
            "块1仍在缓冲未封"
        );
        assert_eq!(read_all(&ws, &store, ino), b"AAAAAAAABB");
    }

    #[test]
    fn append_与随机写混合_保持正确() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let ws = TailSessions::new(true);
        ws.write_at(&store, ino, 0, b"abcdefghij", params())
            .unwrap(); // 10 字节, 块0满+块1("ij")
                       // 随机改写块0中部 [2,5)（非尾块写 → 先 seal 尾块再 RMW）。
        ws.write_at(&store, ino, 2, b"XYZ", params()).unwrap();
        // 再继续 append。
        let off = ws.geometry(&store, ino).unwrap().0;
        ws.write_at(&store, ino, off, b"KL", params()).unwrap();
        assert_eq!(read_all(&ws, &store, ino), b"abXYZfghijKL");
    }

    #[test]
    fn 越_eof_空洞写仍正确() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let ws = TailSessions::new(true);
        ws.write_at(&store, ino, 0, b"ab", params()).unwrap();
        // 越过空洞在 offset 20 写（非 append → seal + RMW 零填充）。
        ws.write_at(&store, ino, 20, b"Z", params()).unwrap();
        let got = read_all(&ws, &store, ino);
        assert_eq!(got.len(), 21);
        assert_eq!(&got[0..2], b"ab");
        assert!(got[2..20].iter().all(|&b| b == 0), "空洞零填充");
        assert_eq!(got[20], b'Z');
    }

    #[test]
    fn truncate_后再_append_正确() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let ws = TailSessions::new(true);
        ws.write_at(&store, ino, 0, b"0123456789AB", params())
            .unwrap(); // 12 字节
        ws.truncate(&store, ino, 5, params()).unwrap();
        assert_eq!(read_all(&ws, &store, ino), b"01234");
        // 截断后继续 append。
        let off = ws.geometry(&store, ino).unwrap().0;
        ws.write_at(&store, ino, off, b"xyz", params()).unwrap();
        assert_eq!(read_all(&ws, &store, ino), b"01234xyz");
    }

    #[test]
    fn 关闭优化时退化为旧路径_仍正确() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let ws = TailSessions::new(false);
        ws.write_at(&store, ino, 0, b"hello", params()).unwrap();
        ws.write_at(&store, ino, 5, b"world", params()).unwrap();
        // 关闭时每次 append 直接落 Store（无缓冲），读 Store 即可。
        assert_eq!(
            read_sealed(&store, ino, 0).as_deref(),
            Some(&b"hellowor"[..])
        );
        assert_eq!(ws.seal_count(), 0, "关闭优化时不经 seal 计数");
    }

    #[test]
    fn forget_丢弃尾块不封块() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let ws = TailSessions::new(true);
        ws.write_at(&store, ino, 0, b"abc", params()).unwrap();
        ws.forget(ino);
        // forget 后缓冲应空；Store 也无块（从未 seal）。
        assert!(ws.read_tail_block(ino, 0).is_none());
        assert!(store.get_block(ino, 0).unwrap().is_none());
    }
}
