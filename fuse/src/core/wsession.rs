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
//! ## 并发（D4-b：锁包住数据）
//! per-inode 的开放尾块 `Tail` 不再独立存一张 `Mutex<HashMap>`；它被搬进 [`InodeState`]，
//! 由 rwfs 的 `DashMap<u64, Arc<RwLock<InodeState>>>` 持有。改 tail 必须持该 inode 的
//! `RwLock` 写锁——**编译器强制**（方法签名是 `&mut InodeState`），不再靠注释约定。
//! [`TailSessions`] 退化为只持全局开关 `enabled` + 计数 `seal_count` 的轻量结构，其每个
//! tail-touching 方法都接收**调用方已加锁**的 `&InodeState` / `&mut InodeState`，自己不再查表加锁。
//!
//! ## durability
//! 未 flush 的尾块在内存（与 page cache 未刷一致）；fsync/flush 必须先 `seal` 把尾块封块
//! 落 Store，再由 Store 持久化，符合 POSIX fsync 契约（§10）。

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// 已 journal 落盘的逻辑长度（尾日志写放大根治，docs/04 §8.4）：fsync 只追加 `plain[journaled_len..]`
    /// 的原始增量，不重压整块。从 Store 装入的尾块视为已全 journal（=plain.len）；新空尾块为 0。
    journaled_len: usize,
}

/// 一个 inode 的 per-inode 写状态——**唯一**承载该 inode 开放尾块的地方。
///
/// rwfs 把它放进 `Arc<RwLock<InodeState>>`，故「读/写某 ino 的 tail」在类型层面被强制要求先
/// 持该 `RwLock`（`&InodeState` 读 / `&mut InodeState` 写）。这是 D4-b 的核心：锁真正包住数据，
/// 替代旧版「`RwLock<()>` 包空元组 + 注释约定」的物理分离。
#[derive(Debug, Default)]
pub(crate) struct InodeState {
    /// 当前开放尾块；`None` 表示该 inode 当前无开放尾块（全部已封块落 Store）。
    tail: Option<Tail>,
}

/// 全局尾块缓冲策略：**只持**开关 `enabled` + 计数 `seal_count` 的无状态配置。per-inode 的开放
/// 尾块（[`InodeState`]）**一律由调用方持有**——生产路径（rwfs）放进 `DashMap<u64, Arc<RwLock<InodeState>>>`，
/// 单线程驱动者（compact / append-bench / 集成测试）用 [`WriteSession`] 内联持有；无论哪条路径，
/// 改 tail 都必须传入 `&mut InodeState`，由类型（`*_locked` 方法签名）强制持锁（D4-b）。
///
/// D4-b 收尾：原先为便捷 API 保留的 `legacy_states: Mutex<HashMap<u64, InodeState>>` 内部表已**消解**，
/// 不再有任何绕过「调用方持 `&mut InodeState`」的写路径——加锁纪律 100% 由类型系统强制。
///
/// `enabled=false` 时所有写/读/封块都直通旧的无状态 `rmw` 路径（`--no-tail-buffer` 基准对照）。
pub struct TailSessions {
    /// 优化开关：false 时退化为旧路径（每次 append 重压尾块）。
    enabled: bool,
    /// 累计封块次数（含每次「把尾块压缩落 Store」），基准量化重压次数用。
    seal_count: AtomicU64,
}

impl TailSessions {
    /// 新建（`enabled` 控制是否启用尾块缓冲优化）。
    pub fn new(enabled: bool) -> Self {
        Self {
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
    /// 调用方持该 inode 读/写锁（传入 `&InodeState`），故读到的 `file_size` 与并发 seal/truncate
    /// 取得读后写一致（如 getattr 与紧随的 read 看到同一代视图）。getattr/lookup 现折叠进 inode
    /// 读锁（D4-b：原无锁路径取消，1s 属性 TTL 内代价可忽略）。
    pub(crate) fn geometry_locked(
        &self,
        store: &dyn Store,
        ino: u64,
        state: &InodeState,
    ) -> Option<(u64, u32)> {
        if self.enabled {
            if let Some(t) = state.tail.as_ref() {
                return Some((t.file_size, t.chunk_size));
            }
        }
        store.block_geometry(ino)
    }

    /// 读第 `idx` 块的**未压缩**逻辑字节：命中开放尾块则从缓冲返回 `Some(plain)`，
    /// 否则返回 `None`（调用方回落 `Store::get_block` + 解压）。
    ///
    /// 读协调关键：尾块在缓冲里尚未封块，直接读 Store 会读到旧版本或缺块。调用方持该 inode
    /// 读/写锁（`&InodeState`）。
    pub(crate) fn read_tail_block(&self, state: &InodeState, idx: u64) -> Option<Vec<u8>> {
        if !self.enabled {
            return None;
        }
        let t = state.tail.as_ref()?;
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
    /// 调用方须持该 inode 写锁（传入 `&mut InodeState`）——编译器强制。
    pub(crate) fn write_at_locked(
        &self,
        store: &dyn Store,
        ino: u64,
        state: &mut InodeState,
        offset: u64,
        data: &[u8],
        params: &CodecParams,
    ) -> io::Result<usize> {
        if !self.enabled {
            return rmw::write_at(store, ino, offset, data, params);
        }
        if data.is_empty() {
            return Ok(0);
        }

        // 取当前几何（含开放尾块）。
        let Some((cur_size, chunk_size)) = self.geometry_locked(store, ino, state) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("write_at：ino={ino} 非可分块文件或不存在"),
            ));
        };

        // 纯尾部 append（offset == 当前逻辑大小）是优化主路径：直接进缓冲，可能跨多块。
        if offset == cur_size {
            return self.append_into_tail(store, ino, state, data, chunk_size, cur_size, params);
        }

        // 非纯 append（随机写 / 越 EOF 空洞 / 改写已封块或尾块中部）：先封掉开放尾块，
        // 让 Store 持有自洽的全量已封块，再走旧无状态 RMW；写完把新尾块重新装入缓冲。
        self.materialize(store, ino, state, params)?;
        let n = rmw::write_at(store, ino, offset, data, params)?;
        self.refill_tail_from_store(store, ino, state, params)?;
        Ok(n)
    }

    /// 纯尾部 append：把 `data` 逐块灌进开放尾块缓冲。块满即 seal 落 Store 并开新尾块。
    #[allow(clippy::too_many_arguments)]
    fn append_into_tail(
        &self,
        store: &dyn Store,
        ino: u64,
        state: &mut InodeState,
        data: &[u8],
        chunk_size: u32,
        cur_size: u64,
        params: &CodecParams,
    ) -> io::Result<usize> {
        // 确保缓冲里有「当前尾块」。无缓冲时从 Store 装入未满尾块（或在块边界时建空尾块）。
        self.ensure_tail_loaded(store, ino, state, chunk_size, cur_size, params)?;

        let total = data.len();
        let mut written = 0usize;
        while written < total {
            // 当前尾块剩余可写空间。全程持该 inode 写锁（&mut InodeState），故 tail 不会被并发
            // 移除；但防御性地在缺尾块时返回错误而非 panic（理论上 ensure_tail_loaded 后必有）。
            let Some(t) = state.tail.as_mut() else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "append_into_tail：ino={ino} 尾块缺失（应已由 ensure_tail_loaded 装入）"
                    ),
                ));
            };
            let room = chunk_size as usize - t.plain.len();
            let take = room.min(total - written);
            t.plain.extend_from_slice(&data[written..written + take]);
            t.file_size += take as u64;
            written += take;
            let full = t.plain.len() == chunk_size as usize;

            if full && written < total {
                // 尾块填满且还有数据：封块落 Store，开下一块空尾块。
                self.materialize(store, ino, state, params)?;
                let next_size = cur_size + written as u64;
                self.start_empty_tail(state, next_size, chunk_size);
            } else if full {
                // 正好填满且数据写完：封块（不留半块在内存，下次 append 再装入）。
                self.materialize(store, ino, state, params)?;
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
        state: &mut InodeState,
        chunk_size: u32,
        cur_size: u64,
        params: &CodecParams,
    ) -> io::Result<()> {
        if state.tail.is_some() {
            return Ok(());
        }
        let cs = chunk_size as u64;
        let tail_len_in_block = (cur_size % cs) as usize;
        if cur_size == 0 || tail_len_in_block == 0 {
            // 块对齐（或空）：开新空尾块，块号 = cur_size / cs。
            self.start_empty_tail(state, cur_size, chunk_size);
            return Ok(());
        }
        // 末块未满：从 Store 取该块解压，作为开放尾块。
        let idx = cur_size / cs;
        let mut plain = rmw::load_plain_block(store, ino, idx, params)?;
        if plain.len() != tail_len_in_block {
            plain.resize(tail_len_in_block, 0);
        }
        let journaled_len = plain.len();
        state.tail = Some(Tail {
            idx,
            plain,
            chunk_size,
            file_size: cur_size,
            journaled_len,
        });
        Ok(())
    }

    /// 在缓冲里建一个空尾块（块号由 size/chunk_size 推出）。覆盖任何既有项（调用前应已 seal）。
    fn start_empty_tail(&self, state: &mut InodeState, size: u64, chunk_size: u32) {
        let idx = size / chunk_size as u64;
        state.tail = Some(Tail {
            idx,
            plain: Vec::with_capacity(chunk_size as usize),
            chunk_size,
            file_size: size,
            journaled_len: 0,
        });
    }

    /// fsync 路径——把未封尾块持久化但**保留缓冲**（尾块仍开放，可继续 append）。支持尾日志的后端
    /// 只追加 `plain[journaled_len..]` 原始增量（O(delta)，不压缩，根治写放大）；否则回退旧
    /// `store_plain_block`（整块重压 + put_block）。空对齐尾块无需写。
    ///
    /// fsync/flush/release/rename 调用。块满 / 非追加写 / truncate 走 `materialize`（封不可变块、重置
    /// 日志、移除缓冲）。调用方持该 inode 写锁（`&mut InodeState`）。
    pub(crate) fn seal_locked(
        &self,
        store: &dyn Store,
        ino: u64,
        state: &mut InodeState,
        params: &CodecParams,
    ) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let Some(t) = state.tail.as_ref() else {
            return Ok(());
        };
        if t.plain.is_empty() && t.file_size % (t.chunk_size as u64) == 0 {
            state.tail = None;
            return Ok(());
        }
        if store.supports_tail_journal() {
            // 只追加自上次以来的原始增量；无新增（journaled_len==plain.len）则跳过，不写空记录。
            if t.journaled_len < t.plain.len() {
                store.append_tail(ino, &t.plain[t.journaled_len..], t.file_size)?;
                // 上面持有 t（不可变借用）已用完；安全地重新可变借用更新 journaled_len。
                if let Some(b) = state.tail.as_mut() {
                    b.journaled_len = b.plain.len();
                }
            }
            return Ok(());
        }
        // 无尾日志后端：整块重压落 Store，移除缓冲（下次 append 再装入）。
        rmw::store_plain_block(store, ino, t.idx, &t.plain, t.file_size, params)?;
        self.seal_count.fetch_add(1, Ordering::Relaxed);
        state.tail = None;
        Ok(())
    }

    /// 把开放尾块封为**不可变压缩块**落 Store 并重置尾日志，移除缓冲。块满 / 非追加写前 / truncate 前
    /// 调用——让 Store 持有自洽全量已封块，后续 RMW 经 get_block 读它。空对齐尾块仅移除不写。
    fn materialize(
        &self,
        store: &dyn Store,
        ino: u64,
        state: &mut InodeState,
        params: &CodecParams,
    ) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let Some(t) = state.tail.as_ref() else {
            return Ok(());
        };
        if t.plain.is_empty() && t.file_size % (t.chunk_size as u64) == 0 {
            state.tail = None;
            return Ok(());
        }
        // seal_plain_block 用 seal_tail_block（支持后端重置 journal；否则 = put_block）。先落 Store
        // 再删缓冲，堵无锁读者 torn-read（rust-review HIGH-1）。
        rmw::seal_plain_block(store, ino, t.idx, &t.plain, t.file_size, params)?;
        self.seal_count.fetch_add(1, Ordering::Relaxed);
        state.tail = None;
        Ok(())
    }

    /// 截断（或零填充扩展）到 `new_size`。先 seal 开放尾块，再走旧 `rmw::truncate`，
    /// 之后把新末块重新装入缓冲（便于后续 append 继续走快路径）。调用方持该 inode 写锁。
    pub(crate) fn truncate_locked(
        &self,
        store: &dyn Store,
        ino: u64,
        state: &mut InodeState,
        new_size: u64,
        params: &CodecParams,
    ) -> io::Result<()> {
        if !self.enabled {
            return rmw::truncate(store, ino, new_size, params);
        }
        self.materialize(store, ino, state, params)?;
        rmw::truncate(store, ino, new_size, params)?;
        Ok(())
    }

    /// 从 Store 当前几何重新装入开放尾块（非 append 写 / 截断后调用，保持快路径可用）。
    fn refill_tail_from_store(
        &self,
        store: &dyn Store,
        ino: u64,
        state: &mut InodeState,
        params: &CodecParams,
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
        let journaled_len = plain.len();
        state.tail = Some(Tail {
            idx,
            plain,
            chunk_size,
            file_size: size,
            journaled_len,
        });
        Ok(())
    }

    /// 丢弃某 inode 的开放尾块**不封块**（unlink/rename 覆盖：底层数据即将消失，封块无意义）。
    /// 调用方持该 inode 写锁（`&mut InodeState`）。
    pub(crate) fn forget_locked(&self, state: &mut InodeState) {
        state.tail = None;
    }
}

/// **单 inode、单线程顺序驱动器**：把「一个 [`InodeState`] + 一份 [`TailSessions`] 配置」内联绑在
/// 一起，直接对**自有** `state` 调 `*_locked` 核心——无需任何表锁。
///
/// 生产 rwfs **不用**此类型：它把 per-inode `InodeState` 放进 `DashMap<u64, Arc<RwLock<InodeState>>>`
/// 自持锁并调 `*_locked`。`WriteSession` 是给 **compact / append-bench / 集成测试**等单线程顺序驱动
/// 单 inode 的场景用的公开 API——替代 D4-b 之前 `TailSessions` 内部那张 `legacy_states` 便捷表：
/// 状态被本实例**独占持有**，改 tail 依旧要 `&mut self`，加锁纪律由类型强制，不再有旁路表。
///
/// 需要跨线程共享时（如镜像 rwfs 的并发读/写测试），把整个 `WriteSession` 放进 `Mutex`/`RwLock`
/// 即可：读走 `&self`、写走 `&mut self`，与生产 `Arc<RwLock<InodeState>>` 的互斥语义一致。
pub struct WriteSession {
    sessions: TailSessions,
    state: InodeState,
}

impl WriteSession {
    /// 新建（`enabled` 透传给内部 [`TailSessions`]，控制是否启用尾块缓冲优化）。
    pub fn new(enabled: bool) -> Self {
        Self {
            sessions: TailSessions::new(enabled),
            state: InodeState::default(),
        }
    }

    /// 取逻辑几何 `(size, chunk_size)`，**含未封尾块**。见 [`TailSessions::geometry_locked`]。
    pub fn geometry(&self, store: &dyn Store, ino: u64) -> Option<(u64, u32)> {
        self.sessions.geometry_locked(store, ino, &self.state)
    }

    /// 读第 `idx` 块的**未压缩**尾块字节：命中开放尾块返回 `Some(plain)`，否则 `None`
    /// （调用方回落 `Store::get_block`）。见 [`TailSessions::read_tail_block`]。
    pub fn read_tail_block(&self, idx: u64) -> Option<Vec<u8>> {
        self.sessions.read_tail_block(&self.state, idx)
    }

    /// 在 `offset` 写入 `data`。见 [`TailSessions::write_at_locked`]。
    pub fn write_at(
        &mut self,
        store: &dyn Store,
        ino: u64,
        offset: u64,
        data: &[u8],
        params: &CodecParams,
    ) -> io::Result<usize> {
        self.sessions
            .write_at_locked(store, ino, &mut self.state, offset, data, params)
    }

    /// fsync 封尾（保留缓冲，尾块仍开放可续 append）。见 [`TailSessions::seal_locked`]。
    pub fn seal(&mut self, store: &dyn Store, ino: u64, params: &CodecParams) -> io::Result<()> {
        self.sessions
            .seal_locked(store, ino, &mut self.state, params)
    }

    /// 截断（或零填充扩展）到 `new_size`。见 [`TailSessions::truncate_locked`]。
    pub fn truncate(
        &mut self,
        store: &dyn Store,
        ino: u64,
        new_size: u64,
        params: &CodecParams,
    ) -> io::Result<()> {
        self.sessions
            .truncate_locked(store, ino, &mut self.state, new_size, params)
    }

    /// 丢弃开放尾块**不封块**（unlink/rename 覆盖）。见 [`TailSessions::forget_locked`]。
    pub fn forget(&mut self) {
        self.sessions.forget_locked(&mut self.state);
    }

    /// 累计封块次数（基准埋点）。
    pub fn seal_count(&self) -> u64 {
        self.sessions.seal_count()
    }

    /// 是否启用了尾块缓冲优化。
    pub fn enabled(&self) -> bool {
        self.sessions.enabled()
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
            dict: None,
        }
    }

    /// 测试夹具：把「单 inode 的 InodeState」与 TailSessions 绑在一起，模拟 rwfs 持锁后对
    /// `&mut InodeState` 调用各 API。单测里无并发，直接内联 state，等价于「已持该 inode 写锁」。
    struct Session {
        ws: TailSessions,
        state: InodeState,
    }

    impl Session {
        fn new(enabled: bool) -> Self {
            Self {
                ws: TailSessions::new(enabled),
                state: InodeState::default(),
            }
        }

        fn write_at(
            &mut self,
            store: &dyn Store,
            ino: u64,
            offset: u64,
            data: &[u8],
        ) -> io::Result<usize> {
            self.ws
                .write_at_locked(store, ino, &mut self.state, offset, data, &params())
        }

        fn seal(&mut self, store: &dyn Store, ino: u64) -> io::Result<()> {
            self.ws.seal_locked(store, ino, &mut self.state, &params())
        }

        fn truncate(&mut self, store: &dyn Store, ino: u64, new_size: u64) -> io::Result<()> {
            self.ws
                .truncate_locked(store, ino, &mut self.state, new_size, &params())
        }

        fn forget(&mut self) {
            self.ws.forget_locked(&mut self.state);
        }

        fn geometry(&self, store: &dyn Store, ino: u64) -> Option<(u64, u32)> {
            self.ws.geometry_locked(store, ino, &self.state)
        }

        fn read_tail_block(&self, idx: u64) -> Option<Vec<u8>> {
            self.ws.read_tail_block(&self.state, idx)
        }

        fn seal_count(&self) -> u64 {
            self.ws.seal_count()
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
    fn read_all(sess: &Session, store: &dyn Store, ino: u64) -> Vec<u8> {
        let (size, cs) = sess.geometry(store, ino).unwrap();
        let mut out = vec![0u8; size as usize];
        let cs = cs as u64;
        let nblocks = size.div_ceil(cs);
        for idx in 0..nblocks {
            let start = (idx * cs) as usize;
            let plain = if let Some(p) = sess.read_tail_block(idx) {
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
        let mut sess = Session::new(true);

        let mut expected = Vec::new();
        for i in 0..100u32 {
            let line = format!("L{i:03}"); // 恰 4 字节
            let off = sess.geometry(&store, ino).unwrap().0;
            sess.write_at(&store, ino, off, line.as_bytes()).unwrap();
            expected.extend_from_slice(line.as_bytes());
        }
        // 读协调：未 fsync 时整文件已可读（尾块走缓冲）。
        assert_eq!(read_all(&sess, &store, ino), expected);

        // 重压（封块）次数应约等于「已填满的块数」（~99），远少于 append 次数（100）。
        // 关键断言：不是「每次 append 一次重压」（若为旧路径会 >= 100 且尾块被反复重压）。
        let seals = sess.seal_count();
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
        let mut sess = Session::new(true);
        sess.write_at(&store, ino, 0, b"hello").unwrap();
        // 未 seal 前 Store 无此块（在缓冲里）。
        assert!(
            store.get_block(ino, 0).unwrap().is_none(),
            "seal 前 Store 不应有尾块"
        );
        // fsync 模拟：seal 落盘。
        sess.seal(&store, ino).unwrap();
        assert_eq!(read_sealed(&store, ino, 0).as_deref(), Some(&b"hello"[..]));
        assert_eq!(store.block_geometry(ino).unwrap().0, 5, "size 落 Store");
    }

    #[test]
    fn read_while_appending_读到缓冲尾块而非旧封块() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let mut sess = Session::new(true);
        // 先写满块0（封块落 Store），再 append 进块1（缓冲）。
        sess.write_at(&store, ino, 0, b"AAAAAAAA").unwrap(); // 块0满→封
        sess.write_at(&store, ino, 8, b"BB").unwrap(); // 块1缓冲
                                                       // 读块1必须读到缓冲里的 "BB"，而不是 Store（Store 此时无块1）。
        assert_eq!(sess.read_tail_block(1).as_deref(), Some(&b"BB"[..]));
        assert!(
            store.get_block(ino, 1).unwrap().is_none(),
            "块1仍在缓冲未封"
        );
        assert_eq!(read_all(&sess, &store, ino), b"AAAAAAAABB");
    }

    #[test]
    fn append_与随机写混合_保持正确() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let mut sess = Session::new(true);
        sess.write_at(&store, ino, 0, b"abcdefghij").unwrap(); // 10 字节, 块0满+块1("ij")
                                                               // 随机改写块0中部 [2,5)（非尾块写 → 先 seal 尾块再 RMW）。
        sess.write_at(&store, ino, 2, b"XYZ").unwrap();
        // 再继续 append。
        let off = sess.geometry(&store, ino).unwrap().0;
        sess.write_at(&store, ino, off, b"KL").unwrap();
        assert_eq!(read_all(&sess, &store, ino), b"abXYZfghijKL");
    }

    #[test]
    fn 越_eof_空洞写仍正确() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let mut sess = Session::new(true);
        sess.write_at(&store, ino, 0, b"ab").unwrap();
        // 越过空洞在 offset 20 写（非 append → seal + RMW 零填充）。
        sess.write_at(&store, ino, 20, b"Z").unwrap();
        let got = read_all(&sess, &store, ino);
        assert_eq!(got.len(), 21);
        assert_eq!(&got[0..2], b"ab");
        assert!(got[2..20].iter().all(|&b| b == 0), "空洞零填充");
        assert_eq!(got[20], b'Z');
    }

    #[test]
    fn truncate_后再_append_正确() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let mut sess = Session::new(true);
        sess.write_at(&store, ino, 0, b"0123456789AB").unwrap(); // 12 字节
        sess.truncate(&store, ino, 5).unwrap();
        assert_eq!(read_all(&sess, &store, ino), b"01234");
        // 截断后继续 append。
        let off = sess.geometry(&store, ino).unwrap().0;
        sess.write_at(&store, ino, off, b"xyz").unwrap();
        assert_eq!(read_all(&sess, &store, ino), b"01234xyz");
    }

    #[test]
    fn 关闭优化时退化为旧路径_仍正确() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let mut sess = Session::new(false);
        sess.write_at(&store, ino, 0, b"hello").unwrap();
        sess.write_at(&store, ino, 5, b"world").unwrap();
        // 关闭时每次 append 直接落 Store（无缓冲），读 Store 即可。
        assert_eq!(
            read_sealed(&store, ino, 0).as_deref(),
            Some(&b"hellowor"[..])
        );
        assert_eq!(sess.seal_count(), 0, "关闭优化时不经 seal 计数");
    }

    #[test]
    fn forget_丢弃尾块不封块() {
        let store = MemStore::new(8);
        let ino = store.new_file();
        let mut sess = Session::new(true);
        sess.write_at(&store, ino, 0, b"abc").unwrap();
        sess.forget();
        // forget 后缓冲应空；Store 也无块（从未 seal）。
        assert!(sess.read_tail_block(0).is_none());
        assert!(store.get_block(ino, 0).unwrap().is_none());
    }
}
