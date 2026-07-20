//! 布局无关的**读写** FUSE 前端（P2/P3）。把 `fuser::Filesystem` 回调映射到任意 `Store`
//! + Core 写编排（core::rmw）+ codec。两布局（V 容器 / S 影子树）共用本前端，只换 `Store`。
//!
//! - read：算块范围 → 逐块 `get_block` → `decompress` → 拼接（顺序读跨块由块循环处理；
//!   缺块/越 EOF 零填充）。
//! - write：交给 `core::rmw::write_at`（RMW / append / 空洞零填充），持 per-inode 写锁保证原子。
//! - create/mkdir/unlink/rmdir/rename/setattr/fsync/flush：转 `Store` 对应方法。
//! - truncate（setattr 带 size）：走 `core::rmw::truncate`。
//!
//! 并发与锁（§4）：每 inode 一把写锁；FUSE 多线程派发，RMW 期间持锁避免交错。
//! 跨 inode 操作（rename）由 Store 内部事务/底层 FS 保证一致，前端不额外加全局锁
//! （首版：跨目录原子性以后端契约为准，§10）。

use parking_lot::RwLock;
use std::ffi::OsStr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use dashmap::DashMap;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, Notifier, OpenFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, Request, TimeOrNow,
};
use log::warn;

use crate::core::blockcache::BlockCache;
use crate::core::chunk::block_range;
use crate::core::codec::{decompress_block, Algo, SharedDict};
use crate::core::metrics::Metrics;
use crate::core::rmw::CodecParams;
use crate::core::wsession::{InodeState, TailSessions};
use crate::store::{Attr, Store};

const TTL: Duration = Duration::from_secs(1);

/// 读写 scrollz 前端。持一个 `Store` + codec 参数 + per-inode 状态表（含写锁与开放尾块）。
pub struct ScrollzRw {
    store: Arc<dyn Store>,
    params: CodecParams,
    default_chunk_size: u32,
    /// per-inode 状态表（§4 + D4-b）：`DashMap<u64, Arc<RwLock<InodeState>>>`。`RwLock<InodeState>`
    /// **真正包住该 inode 的开放尾块数据**（旧版是 `RwLock<()>` 包空元组、tail 另存一张 Mutex 表，
    /// 物理分离、靠注释约定加锁）。现在「读/写某 ino 的 tail」在类型层面被强制先持该 `RwLock`：
    /// read 取读锁（同 inode 多读并发），write/seal/truncate/fsync 取写锁（排他，仍堵 HIGH-1
    /// torn-read——seal 与读互斥）。DashMap 分片降全表锁争用，`entry/remove_if` 在同 key 同 shard
    /// 锁内串行 → 保持「同一 ino 任意时刻只有一把活 RwLock」不变量（与旧 std 版等价）。
    inodes: DashMap<u64, Arc<RwLock<InodeState>>>,
    /// 全局尾块缓冲策略（append 优化，§1.1）：只持 enabled 开关 + seal_count 计数，per-inode
    /// 尾块数据已搬进 `InodeState`（由上面 `inodes` 表托管）。
    tails: TailSessions,
    /// 协商的最大单次 write 字节数（init 时 set_max_write）。0=用 fuser 默认（128KiB）。大值减
    /// 内核拆分（2–4MiB 单行 append 少 8–32x 回调）；上限由 fuser 截到 16MiB。
    max_write: u32,
    /// 内核失效通知器（mount 后注入）：fsync/flush 提交后 `inval_inode` 失效只读 mmap 的陈旧
    /// page cache，堵跨 fd 一致性窗口。None=无（mount2 路径或注入前）。
    notifier: Arc<OnceLock<Notifier>>,
    /// 写回缓存（opt-in）：init 协商 FUSE_WRITEBACK_CACHE、写 fd 去 direct_io，内核合并小写降 p99。
    /// 默认 false（direct_io 求 RMW offset/size 精确）。
    writeback: bool,
    /// 解压块缓存（perf #1）：缓存已解压的**不可变内部块**明文，消除顺序读放大。只缓存
    /// `idx < tail_idx` 的块（见 [`BlockCache`] 模块文档「杠杆 A」）。失效靠 per-inode 写锁串行
    /// 化：变更路径持写锁调 `invalidate`，读路径持读锁 `get`/`insert`，二者对同 inode 互斥。
    block_cache: BlockCache,
    /// 统一指标注册表（per-op FUSE 埋点出口）。默认 `Metrics::new()`（空注册表，测试确定），
    /// main 经 `with_metrics` 注入全 crate 共享的 `Arc`，让 read/write/fsync/flush 计数进 .prom。
    metrics: Arc<Metrics>,
}

impl ScrollzRw {
    /// 取后端引用（main 后台 metrics 线程读 compression_stats 写 .prom）。
    pub fn store_handle(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store)
    }
}

impl ScrollzRw {
    pub fn new(store: Arc<dyn Store>, algo: Algo, level: i32, default_chunk_size: u32) -> Self {
        Self::with_tail_buffer(store, algo, level, default_chunk_size, true, None)
    }

    /// 同 `new`，但显式控制是否启用开放尾块缓冲（`--no-tail-buffer` → false 走旧路径），
    /// 并可注入共享字典（`dict=Some` 时所有块走字典压缩/解压，T3 研究项）。
    pub fn with_tail_buffer(
        store: Arc<dyn Store>,
        algo: Algo,
        level: i32,
        default_chunk_size: u32,
        tail_buffer: bool,
        dict: Option<Arc<SharedDict>>,
    ) -> Self {
        Self {
            store,
            params: CodecParams { algo, level, dict },
            default_chunk_size,
            inodes: DashMap::new(),
            tails: TailSessions::new(tail_buffer),
            max_write: 0,
            notifier: Arc::new(OnceLock::new()),
            writeback: false,
            // 默认禁用（cap=0 全 no-op），保持既有单测确定性、零行为变更；main 经 flag 注入。
            block_cache: BlockCache::new(0),
            // 默认空注册表；main 经 with_metrics 注入全 crate 共享 Arc。
            metrics: Metrics::new(),
        }
    }

    /// 设解压块缓存字节上限（main 据 `--block-cache-bytes` 注入；0 禁用）。返回自身便于链式。
    pub fn with_block_cache(mut self, cap_bytes: usize) -> Self {
        self.block_cache = BlockCache::new(cap_bytes);
        self
    }

    /// 注入全 crate 共享的指标注册表（main 据统一 `Arc<Metrics>` 注入）。返回自身便于链式。
    /// 除自身 read/write/fsync 计数外，也把同一 `Arc` 注入 `tails`，让尾会话封块（seal）计数进
    /// 统一 .prom（构造序：with_tail_buffer 先建默认注册表，此处再注入覆盖两者）。
    pub fn with_metrics(mut self, m: Arc<Metrics>) -> Self {
        self.tails.set_metrics(m.clone());
        self.metrics = m;
        self
    }

    /// 设协商最大 write（main 据 --max-write 注入；0 保持 fuser 默认）。返回自身便于链式。
    pub fn with_max_write(mut self, max_write: u32) -> Self {
        self.max_write = max_write;
        self
    }

    /// 开写回缓存（init 协商 FUSE_WRITEBACK_CACHE、写 fd 去 direct_io，内核合并小写降 p99）。
    pub fn with_writeback(mut self, writeback: bool) -> Self {
        self.writeback = writeback;
        self
    }

    /// 取内核通知器句柄（spawn_mount2 后 main 注入 Notifier，使 fsync 后失效只读 mmap 缓存）。
    pub fn notifier_slot(&self) -> Arc<OnceLock<Notifier>> {
        Arc::clone(&self.notifier)
    }

    /// fsync/flush 提交后失效该 inode 内核缓存（off=0,len=0 表整 inode）：只读 mmap 跨 fd 见新数据。
    fn invalidate_kernel_cache(&self, ino: u64) {
        if let Some(n) = self.notifier.get() {
            if let Err(e) = n.inval_inode(INodeNo(ino), 0, 0) {
                // best-effort：失效失败不阻断 fsync，但记日志（只读 mmap 可能短暂读到旧页）。
                warn!("inval_inode ino={ino} 失败：{e}");
            }
        }
    }

    /// 取（或建）某 inode 的状态锁句柄 `Arc<RwLock<InodeState>>`。
    ///
    /// **死锁规避（D4-b 关键）**：先 `entry().or_default()` 拿到 DashMap 的 `Ref`，立即 `clone`
    /// 出内层 `Arc` 并让 `Ref` 在本函数返回时 drop（释放 shard 锁）。调用方拿到 `Arc` **之后**
    /// 才对其取内层 `.read()/.write()`——绝不在持 DashMap `Ref`（shard 锁）时锁内层 `RwLock`，
    /// 杜绝 shard 锁 × 内层锁的嵌套持有死锁面。
    fn lock_for(&self, ino: u64) -> Arc<RwLock<InodeState>> {
        self.inodes.entry(ino).or_default().clone()
    }

    /// 回收某 inode 的状态项（unlink/rmdir/forget 成功后调用，避免表无界增长，rust-review H1）。
    /// `remove_if` 仅当无其他持有者（`strong_count==1`，即只剩表内这一份 Arc）时移除，防止误删
    /// 正在用的锁。`remove_if` 与 `lock_for` 的 `entry` 对同一 key 在同一 shard 锁内串行，故保持
    /// 「同一 ino 任意时刻只有一把活 RwLock」不变量（与旧 std Mutex<HashMap> 版等价）。
    fn evict_lock(&self, ino: u64) {
        self.inodes
            .remove_if(&ino, |_, arc| Arc::strong_count(arc) == 1);
    }

    /// 在**调用方已持该 inode 写锁**（传入 `&mut InodeState`）的前提下执行 append/RMW 写。先无
    /// 条件失效块缓存（评审 CRITICAL-1：失效在改写之前、不依赖每条返回路径都记得失效），再走尾块
    /// 缓冲/RMW 写编排。抽成独立方法既给 `fn write` 复用，也便于单测直接驱动（绕开 fuser）。
    fn write_at_locked(
        &self,
        ino: u64,
        state: &mut InodeState,
        offset: u64,
        data: &[u8],
    ) -> std::io::Result<usize> {
        self.block_cache.invalidate(ino);
        self.tails
            .write_at_locked(self.store.as_ref(), ino, state, offset, data, &self.params)
    }

    /// 在持该 inode 写锁的前提下丢弃其开放尾块（不封块），再回收锁项。
    /// unlink/rmdir/rename-覆盖用：与并发的同 inode write/seal 串行，堵 rust-review MEDIUM-3
    /// 的「ensure_tail_loaded 后尾块被并发 forget 移除」panic 窗口。
    fn forget_inode_locked(&self, ino: u64) {
        {
            let lock = self.lock_for(ino);
            let mut guard = lock.write();
            self.block_cache.invalidate(ino);
            self.tails.forget_locked(&mut guard);
        }
        // 出锁作用域后再 evict（evict 要求 strong_count==1，持 guard 时计数为 2 会漏删）。
        self.evict_lock(ino);
    }

    /// 内核 forget（lookup 引用归零，此时无打开句柄、release 已封尾）：回收 per-inode 锁项 +
    /// 尾缓冲 + 后端缓存，杜绝 locks/tails 映射随只读/只追加不删除的会话**无界增长**（评审 D1）。
    ///
    /// 与 [`Self::forget_inode_locked`]（unlink 用，直接丢弃尾块）区别：此处先 `seal` 把任何未
    /// journal 的增量刷入 Store，**再**丢内存缓冲——绝不丢数据。seal 失败则保留缓冲与锁、下次重试。
    fn forget_inode_flush(&self, ino: u64) {
        let sealed = {
            let lock = self.lock_for(ino);
            let mut guard = lock.write();
            self.block_cache.invalidate(ino);
            match self
                .tails
                .seal_locked(self.store.as_ref(), ino, &mut guard, &self.params)
            {
                Ok(()) => {
                    // 封尾只追加 journal 增量；须 flush 提交 SB 尾指针，否则丢弃内存缓冲后
                    // 新 reader 看不到未提交的尾字节（与 release 一致）。flush 失败 == 尾字节
                    // 已 seal 但未提交 SB 尾指针：此时 forget 会丢失已 seal 的尾缓冲 → 静默数据
                    // 丢失 + 零填充。故视同 seal 失败——保留尾缓冲与锁、不 forget、下次重试。
                    if let Err(e) = self.store.flush(ino) {
                        warn!("forget：flush ino={ino} 失败：{e}，保留尾缓冲与锁待重试");
                        false
                    } else {
                        self.tails.forget_locked(&mut guard);
                        true
                    }
                }
                Err(e) => {
                    warn!("forget：封 ino={ino} 尾块失败：{e}，保留尾缓冲与锁待重试");
                    false
                }
            }
        };
        if sealed {
            self.store.release(ino);
            self.evict_lock(ino);
        }
    }

    fn to_file_attr(&self, a: &Attr) -> FileAttr {
        FileAttr {
            ino: INodeNo(a.ino),
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: a.atime,
            mtime: a.mtime,
            ctime: a.ctime,
            // 无真实 btime 来源（两后端都不存创建时间）；置 epoch（同 passthrough），
            // 避免 crtime=ctime 在 chmod 后误随 ctime 前跳（review M3）。
            crtime: SystemTime::UNIX_EPOCH,
            kind: a.kind,
            perm: a.perm,
            nlink: 1,
            uid: a.uid,
            gid: a.gid,
            rdev: 0,
            flags: 0,
            // 广告真实最优 IO 单元 = 文件块大小（封顶 1MiB 防应用按 st_blksize 分配巨缓冲），
            // 非误导的 4KiB——后者让 honor st_blksize 的工具（cat/cp/缓冲读）按 4K 读，每次
            // 触发整压缩块重解压（chunk/4K 倍放大，全靠 block_cache 兜）。见 docs/08-observability。
            blksize: if a.chunk_size > 0 {
                a.chunk_size.min(1 << 20)
            } else {
                self.default_chunk_size.min(1 << 20)
            },
        }
    }

    /// 把开放尾块的逻辑大小覆盖进 `Attr`（getattr/lookup 须反映未封尾块，写后读一致）。
    ///
    /// **D4-b 语义变更**：原版走无锁路径（tail 另存的表锁），现折叠进该 inode **读锁**——`geometry`
    /// 经 `&InodeState` 读，类型上要求先持锁。getattr/lookup 低频 + 1s 属性 TTL，正确性收益（与并发
    /// seal/truncate 取得读后写一致视图）> 一次读锁代价（rust-review MEDIUM-2 的滞后窗口也随之消失）。
    fn overlay_tail_size(&self, mut a: Attr) -> Attr {
        if a.kind == FileType::RegularFile {
            let lock = self.lock_for(a.ino);
            let guard = lock.read();
            if let Some((size, _cs)) =
                self.tails
                    .geometry_locked(self.store.as_ref(), a.ino, &guard)
            {
                a.size = size;
            }
        }
        a
    }

    /// 组装 `[offset, offset+size)` 区间逻辑字节（同 P1 读路径，缺块零填充）。
    /// 读协调：尾块若在开放缓冲中，从未压缩缓冲取，不走 `get_block`（否则读到旧封块/缺块）。
    ///
    /// **并发（rust-review HIGH-1）**：持该 inode 写锁，使 `geometry`+`read_tail_block`+`get_block`
    /// 序列对同 inode 的 write/seal/truncate 原子——否则无锁读者可能在「seal 移除缓冲尾块」与
    /// 「sealed 块落 Store」之间观察到空窗，把有数据的块零填充（torn read）。append 主负载下
    /// 同 inode 的读写并发少，串行化代价可忽略；正确性优先（§10）。
    fn read_range(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let lock = self.lock_for(ino);
        let guard = lock.read();
        let Some((uncompressed_size, chunk_size)) =
            self.tails.geometry_locked(self.store.as_ref(), ino, &guard)
        else {
            return Err(Errno::ENOENT);
        };
        if offset >= uncompressed_size || size == 0 {
            return Ok(Vec::new());
        }
        let end = (offset + size as u64).min(uncompressed_size);
        let want = (end - offset) as usize;
        let cs = chunk_size as u64;

        // 发现读快路径（docs/02）：整个请求区间落在 head 缓存覆盖前缀内时，解压一小段 head
        // 缓存切片返回，跳过整块（1MiB）解压。Store 仅在「完全覆盖 + 无挂起写会话」时返回 Some
        // （脏块 0 时回退逐块）；不支持的后端默认 None。命中即免去块循环。
        if let Some((bytes, verbatim)) = self
            .store
            .read_head_cache(ino, offset, want as u64)
            .map_err(|e| io_to_errno(&e))?
        {
            let plain = decompress_block(
                &bytes,
                self.params.algo,
                verbatim,
                self.params.dict.as_deref(),
            )
            .map_err(|e| io_to_errno(&e))?;
            let start = offset as usize;
            let stop = start + want;
            if stop <= plain.len() {
                return Ok(plain[start..stop].to_vec());
            }
            // 解压长度与缓存 rawlen 承诺不符（理论不应发生）：回退逐块路径，不返回错误切片。
        }

        let (first, last) = block_range(offset, (end - offset).max(1), cs);

        // 块缓存只收「严格内部块」`idx < tail_idx`：排除可变尾块 / 尾日志重放块（`get_block` 对
        // `idx == chunk_count` 返回可变 verbatim 尾块），append/seal 只动尾块、内部块恒不可变。
        // tail_idx = 含文件末字节的块号；uncompressed_size>0（offset<size 已保证）。
        let tail_idx = (uncompressed_size - 1) / cs;

        let mut out = Vec::with_capacity(want);
        for idx in first..=last {
            let block_start = idx * cs;
            let block_end = block_start + cs;
            let copy_start = offset.max(block_start);
            let copy_end = end.min(block_end);
            if copy_start >= copy_end {
                continue;
            }
            // 读协调：先查开放尾块缓冲（未压缩字节）。命中则直接切片，不解压、不读 Store。
            if let Some(plain) = self.tails.read_tail_block(&guard, idx) {
                let in_block_start = (copy_start - block_start) as usize;
                let in_block_end = ((copy_end - block_start) as usize).min(plain.len());
                if in_block_start < in_block_end {
                    out.extend_from_slice(&plain[in_block_start..in_block_end]);
                }
                let produced = in_block_end.saturating_sub(in_block_start);
                let expected = (copy_end - copy_start) as usize;
                if produced < expected {
                    // 尾块逻辑长度即文件末尾，不足部分零填充（与下方封块路径一致）。
                    out.resize(out.len() + (expected - produced), 0);
                }
                continue;
            }
            // 块缓存：命中免整块解压（顺序读放大的主因）。未命中走 Store + 解压，仅严格内部块
            // （idx < tail_idx）回填缓存。空洞块（get_block=None）零填充、不缓存。
            let plain: std::sync::Arc<[u8]> = if let Some(cached) = self.block_cache.get(ino, idx) {
                self.metrics.record_cache_hit();
                cached
            } else {
                self.metrics.record_cache_miss();
                let stored = match self
                    .store
                    .get_block(ino, idx)
                    .map_err(|e| io_to_errno(&e))?
                {
                    Some(b) => b,
                    None => {
                        out.resize(out.len() + (copy_end - copy_start) as usize, 0);
                        continue;
                    }
                };
                let decoded = decompress_block(
                    &stored.bytes,
                    self.params.algo,
                    stored.stored_verbatim,
                    self.params.dict.as_deref(),
                )
                .map_err(|e| io_to_errno(&e))?;
                let arc: std::sync::Arc<[u8]> = std::sync::Arc::from(decoded.into_boxed_slice());
                if idx < tail_idx {
                    self.block_cache.insert(ino, idx, arc.clone());
                }
                arc
            };
            let in_block_start = (copy_start - block_start) as usize;
            let in_block_end = ((copy_end - block_start) as usize).min(plain.len());
            if in_block_start < in_block_end {
                out.extend_from_slice(&plain[in_block_start..in_block_end]);
            }
            let produced = in_block_end.saturating_sub(in_block_start);
            let expected = (copy_end - copy_start) as usize;
            if produced < expected {
                let is_last_logical_block = block_end >= uncompressed_size;
                if is_last_logical_block {
                    out.resize(out.len() + (expected - produced), 0);
                } else {
                    warn!("ino={ino} 块 {idx} 解压长度不足，疑似损坏");
                    return Err(Errno::EIO);
                }
            }
        }
        Ok(out)
    }
}

/// io::Error → Errno，无 raw_os_error 时回退 EIO。
pub fn io_to_errno(e: &std::io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

/// 把 FUSE `setattr` 的 `TimeOrNow` 解析为绝对 `SystemTime`：`Now` → 当前时钟，
/// `SpecificTime(t)` → t。供 atime/mtime 写回（utimensat 的 UTIME_NOW 语义在前端折叠）。
fn resolve_time_or_now(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::Now => SystemTime::now(),
        TimeOrNow::SpecificTime(t) => t,
    }
}

impl Filesystem for ScrollzRw {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        // 协商更大 max_write，减大行 append 的内核拆分（fuser 默认 128KiB→ 可到 16MiB）。
        if self.max_write > 0 {
            if let Err(nearest) = config.set_max_write(self.max_write) {
                warn!(
                    "set_max_write({}) 失败，回退最近值 {nearest}",
                    self.max_write
                );
                let _ = config.set_max_write(nearest);
            }
        }
        // writeback 缓存：内核合并小写、async 回刷，降写尾 p99（须配合 open 去 direct_io）。
        if self.writeback {
            if let Err(missing) = config.add_capabilities(fuser::InitFlags::FUSE_WRITEBACK_CACHE) {
                warn!("内核不支持 FUSE_WRITEBACK_CACHE（{missing:?}），仍走 direct_io");
                self.writeback = false;
            }
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.store.lookup(parent.0, name) {
            Some(a) => reply.entry(
                &TTL,
                &self.to_file_attr(&self.overlay_tail_size(a)),
                Generation(0),
            ),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.store.getattr_ino(ino.0) {
            Some(a) => reply.attr(&TTL, &self.to_file_attr(&self.overlay_tail_size(a))),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, _nlookup: u64) {
        // 内核丢弃对该 inode 的 lookup 引用 → 回收锁/尾缓冲，杜绝映射无界增长（评审 D1）。
        // 无打开句柄保证（release 已封尾）；forget_inode_flush 仍先 seal 再丢弃，双保险不丢数据。
        self.forget_inode_flush(ino.0);
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        use std::os::unix::ffi::OsStrExt;
        match self.store.readlink(ino.0) {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(e) => reply.error(io_to_errno(&e)),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let Some(name) = link_name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.store.symlink(parent.0, name, target) {
            Ok(a) => reply.entry(&TTL, &self.to_file_attr(&a), Generation(0)),
            Err(e) => reply.error(io_to_errno(&e)),
        }
    }

    /// hardlink 正式不支持：布局 S 一文件 = 一 archive、布局 V 的 inode 无多名命名层，
    /// 无法表达「多个目录项指向同一 inode」（docs/01 §4、ROADMAP T1 定调）。显式返回 ENOTSUP
    /// 给 `cp -al` / git 明确语义，而非未实现 handler 时内核 VFS 回退的 EPERM。
    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::from_i32(libc::ENOTSUP));
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if self.store.getattr_ino(ino.0).is_some() {
            // 只读 open：用 page cache（FOPEN_KEEP_CACHE）→ 支持只读 mmap（T2）；read 仍精确切片。
            // 读写/只写 open：默认 direct_io 求 RMW offset/size 精确；--writeback 下去 direct_io 用
            // page cache（内核合并小写、async 回刷，降 p99），KEEP_CACHE 保读缓存。
            let fopen = if flags.acc_mode() == fuser::OpenAccMode::O_RDONLY || self.writeback {
                fuser::FopenFlags::FOPEN_KEEP_CACHE
            } else {
                fuser::FopenFlags::FOPEN_DIRECT_IO
            };
            reply.opened(FileHandle(0), fopen);
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        // 计时开销 ~数十 ns（两次 Instant::now），可接受——Prometheus 直方图标准做法。
        let t0 = std::time::Instant::now();
        match self.read_range(ino.0, offset, size) {
            Ok(buf) => {
                self.metrics.record_read(buf.len() as u64);
                self.metrics
                    .observe_read_latency_us(t0.elapsed().as_micros() as u64);
                reply.data(&buf)
            }
            Err(e) => {
                self.metrics.record_fuse_error();
                // 错误路径也观测：失败同样消耗了时间，延迟分布应含错误尾（p99 更真实）。
                self.metrics
                    .observe_read_latency_us(t0.elapsed().as_micros() as u64);
                reply.error(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        // t0 在取锁前起，量整个 write（含取锁等待 + RMW）。计时开销 ~数十 ns，可接受。
        let t0 = std::time::Instant::now();
        let lock = self.lock_for(ino.0);
        let result = {
            let mut guard = lock.write();
            self.write_at_locked(ino.0, &mut guard, offset, data)
        }; // 写锁在此 drop —— 之后再做纯原子埋点，绝不持锁自增。
        match result {
            Ok(n) => {
                self.metrics.record_write(n as u64);
                self.metrics
                    .observe_write_latency_us(t0.elapsed().as_micros() as u64);
                reply.written(n as u32)
            }
            Err(e) => {
                self.metrics.record_fuse_error();
                self.metrics
                    .observe_write_latency_us(t0.elapsed().as_micros() as u64);
                reply.error(io_to_errno(&e))
            }
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let now = SystemTime::now();
        let attr = Attr {
            ino: 0,
            size: 0,
            kind: FileType::RegularFile,
            perm: ((mode & !umask) & 0o7777) as u16,
            uid: req.uid(),
            gid: req.gid(),
            mtime: now,
            atime: now,
            ctime: now,
            chunk_size: self.default_chunk_size,
        };
        match self.store.create(parent.0, name, attr) {
            Ok(ino) => match self.store.getattr_ino(ino) {
                Some(a) => reply.created(
                    &TTL,
                    &self.to_file_attr(&a),
                    Generation(0),
                    FileHandle(0),
                    // 与 open 一致：writeback 下用 page cache 合并写，否则 direct_io 求精确。
                    if self.writeback {
                        fuser::FopenFlags::FOPEN_KEEP_CACHE
                    } else {
                        fuser::FopenFlags::FOPEN_DIRECT_IO
                    },
                ),
                None => reply.error(Errno::EIO),
            },
            Err(e) => reply.error(io_to_errno(&e)),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let now = SystemTime::now();
        let attr = Attr {
            ino: 0,
            size: 0,
            kind: FileType::Directory,
            perm: ((mode & !umask) & 0o7777) as u16,
            uid: req.uid(),
            gid: req.gid(),
            mtime: now,
            atime: now,
            ctime: now,
            chunk_size: self.default_chunk_size,
        };
        match self.store.mkdir(parent.0, name, attr) {
            Ok(ino) => match self.store.getattr_ino(ino) {
                Some(a) => reply.entry(&TTL, &self.to_file_attr(&a), Generation(0)),
                None => reply.error(Errno::EIO),
            },
            Err(e) => reply.error(io_to_errno(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        // 删前取 ino，删成功后丢弃尾块 + 回收其锁项（持锁，H1 + MEDIUM-3）。
        let victim = self.store.lookup(parent.0, name).map(|a| a.ino);
        match self.store.unlink(parent.0, name) {
            Ok(()) => {
                if let Some(ino) = victim {
                    self.forget_inode_locked(ino);
                }
                reply.ok()
            }
            Err(e) => reply.error(io_to_errno(&e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        let victim = self.store.lookup(parent.0, name).map(|a| a.ino);
        match self.store.rmdir(parent.0, name) {
            Ok(()) => {
                if let Some(ino) = victim {
                    self.forget_inode_locked(ino);
                }
                reply.ok()
            }
            Err(e) => reply.error(io_to_errno(&e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        // 被 rename 覆盖的目标若存在，其底层数据即将被替换，须丢弃其开放尾块（不封块，否则
        // 会把陈旧尾块封进即将消失的旧 inode）。源文件 ino 不变、内容跟随，尾块继续有效。
        let overwritten = self.store.lookup(newparent.0, newname).map(|a| a.ino);
        // 源文件若有开放尾块，rename 不改其内容，但保险起见在 rename 前先封块，避免后续对
        // 同 ino 的尾块缓冲与底层路径变动产生不一致（封块是幂等的安全操作）。
        if let Some(src) = self.store.lookup(parent.0, name).map(|a| a.ino) {
            let lock = self.lock_for(src);
            let mut guard = lock.write();
            self.block_cache.invalidate(src);
            if let Err(e) =
                self.tails
                    .seal_locked(self.store.as_ref(), src, &mut guard, &self.params)
            {
                // 非致命（rename 仍可进行，源内容由底层路径承载），但不静默吞——记日志。
                warn!("rename：封源 ino={src} 尾块失败：{e}");
            }
        }
        match self.store.rename((parent.0, name), (newparent.0, newname)) {
            Ok(()) => {
                if let Some(victim) = overwritten {
                    self.forget_inode_locked(victim);
                }
                reply.ok()
            }
            Err(e) => reply.error(io_to_errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(mut a) = self.store.getattr_ino(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        // truncate / extend：走 Core 写编排（持 inode 写锁）。先封开放尾块再截断。
        if let Some(new_size) = size {
            let lock = self.lock_for(ino.0);
            let mut guard = lock.write();
            self.block_cache.invalidate(ino.0);
            if let Err(e) = self.tails.truncate_locked(
                self.store.as_ref(),
                ino.0,
                &mut guard,
                new_size,
                &self.params,
            ) {
                reply.error(io_to_errno(&e));
                return;
            }
        }
        // 元数据更新（perm/uid/gid + atime/mtime/ctime）。时间把 `TimeOrNow` 解析为绝对
        // `SystemTime`（Now → 当前时钟），写回 store；shadow 落到底层文件，container 存进行。
        let has_time = atime.is_some() || mtime.is_some() || ctime.is_some();
        if mode.is_some() || uid.is_some() || gid.is_some() || has_time {
            if let Some(m) = mode {
                a.perm = (m & 0o7777) as u16;
            }
            if let Some(u) = uid {
                a.uid = u;
            }
            if let Some(g) = gid {
                a.gid = g;
            }
            if let Some(t) = atime {
                a.atime = resolve_time_or_now(t);
            }
            if let Some(t) = mtime {
                a.mtime = resolve_time_or_now(t);
            }
            if let Some(t) = ctime {
                a.ctime = t;
            }
            if let Err(e) = self.store.setattr(ino.0, a) {
                reply.error(io_to_errno(&e));
                return;
            }
        }
        match self.store.getattr_ino(ino.0) {
            Some(a) => reply.attr(&TTL, &self.to_file_attr(&self.overlay_tail_size(a))),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        // 持 inode 写锁再封块 + 提交，避免与并发 write/truncate 的 RMW 序列交错（rust-review C1）。
        // **锁序纪律（notify 重入根治）**：`inval_inode` 是同步内核往返，会取 inode 页缓存锁；若在持
        // per-inode 写锁时调用，与「持内核页锁、其 FUSE read 又堵在本写锁」的并发读构成跨层 AB-BA。
        // 故把封块+提交圈在内层作用域、**出锁后再** `invalidate_kernel_cache`。
        // t0 在取锁前起，量整个 flush handler（含取锁 + seal + store.flush）。计时开销 ~数十 ns，可接受。
        let t0 = std::time::Instant::now();
        let lock = self.lock_for(ino.0);
        let flush_result = {
            let mut guard = lock.write();
            self.block_cache.invalidate(ino.0);
            if let Err(e) =
                self.tails
                    .seal_locked(self.store.as_ref(), ino.0, &mut guard, &self.params)
            {
                // 封块失败早返回：出锁后观测（同样消耗了时间），保延迟分布含此错误路径。
                drop(guard);
                self.metrics
                    .observe_fsync_latency_us(t0.elapsed().as_micros() as u64);
                reply.error(io_to_errno(&e));
                return;
            }
            self.store.flush(ino.0)
        }; // 写锁在此 drop —— 之后再通知内核，绝不持锁跨 inval_inode。
        match flush_result {
            Ok(()) => {
                self.invalidate_kernel_cache(ino.0);
                self.metrics.record_fsync();
                self.metrics
                    .observe_fsync_latency_us(t0.elapsed().as_micros() as u64);
                reply.ok()
            }
            Err(e) => {
                self.metrics.record_fuse_error();
                self.metrics
                    .observe_fsync_latency_us(t0.elapsed().as_micros() as u64);
                reply.error(io_to_errno(&e))
            }
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // 持 inode 写锁再封块 + 提交（rust-review C1）：fsync 须先把开放尾块封块落 Store，
        // 再让 Store 持久化，符合 POSIX fsync 契约（§10），且不能与同 inode 的 RMW 交错。
        // **锁序纪律（notify 重入根治，同 flush）**：出锁作用域后再 `invalidate_kernel_cache`。
        // t0 在取锁前起，量整个 fsync handler（含取锁 + seal + store.fsync）。计时开销 ~数十 ns，可接受。
        let t0 = std::time::Instant::now();
        let lock = self.lock_for(ino.0);
        let fsync_result = {
            let mut guard = lock.write();
            self.block_cache.invalidate(ino.0);
            if let Err(e) =
                self.tails
                    .seal_locked(self.store.as_ref(), ino.0, &mut guard, &self.params)
            {
                // 封块失败早返回：出锁后观测，保延迟分布含此错误路径。
                drop(guard);
                self.metrics
                    .observe_fsync_latency_us(t0.elapsed().as_micros() as u64);
                reply.error(io_to_errno(&e));
                return;
            }
            self.store.fsync(ino.0)
        }; // 写锁在此 drop —— 之后再通知内核。
        match fsync_result {
            Ok(()) => {
                self.invalidate_kernel_cache(ino.0);
                self.metrics.record_fsync();
                self.metrics
                    .observe_fsync_latency_us(t0.elapsed().as_micros() as u64);
                reply.ok()
            }
            Err(e) => {
                self.metrics.record_fuse_error();
                self.metrics
                    .observe_fsync_latency_us(t0.elapsed().as_micros() as u64);
                reply.error(io_to_errno(&e))
            }
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // close 时封开放尾块 + 落盘该 inode 的挂起写（保证 close 后再 open 能读到）。持锁同 fsync。
        // FUSE release 无错误回传通道（内核忽略），但失败不能静默吞——记日志（rust-review MEDIUM-1）。
        // 注意：close 不保证 durability（那是 fsync 的职责），此处尽力而为。
        {
            let lock = self.lock_for(ino.0);
            let mut guard = lock.write();
            self.block_cache.invalidate(ino.0);
            if let Err(e) =
                self.tails
                    .seal_locked(self.store.as_ref(), ino.0, &mut guard, &self.params)
            {
                warn!("release：封 ino={} 尾块失败：{e}", ino.0);
            }
            if let Err(e) = self.store.flush(ino.0) {
                warn!("release：flush ino={} 失败：{e}", ino.0);
            }
        }
        // 提示后端释放 per-inode 缓存资源（布局 S 的 ArchiveReader 缓存）。落盘已在上面完成，
        // 故缓存释放与持久化无序依赖；不持写锁，避免无谓串行化 release。
        self.store.release(ino.0);
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (ino.0, FileType::Directory, "..".to_string()),
        ];
        for de in self.store.readdir(ino.0) {
            entries.push((de.ino, de.kind, de.name));
        }
        for (i, (eino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(eino), (i + 1) as u64, kind, name.as_str()) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        // df 显压缩比：blocks=逻辑总量、bavail/bfree=逻辑−物理（已省空间作可用），bsize=4KiB。
        const BS: u64 = 4096;
        let (phys, logical) = self.store.compression_stats().unwrap_or((0, 0));
        let blocks = logical / BS;
        let used = phys / BS;
        let free = blocks.saturating_sub(used);
        reply.statfs(blocks, free, free, 0, 0, BS as u32, 255, BS as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::HEAD_CACHE_BYTES;
    use crate::core::rmw;
    use crate::store::shadow::ShadowStore;
    use crate::store::Attr;

    #[test]
    fn resolve_time_or_now_maps_specific_and_now() {
        let t = SystemTime::UNIX_EPOCH + Duration::new(1_750_740_420, 5);
        assert_eq!(resolve_time_or_now(TimeOrNow::SpecificTime(t)), t);
        // Now → 当前时钟（晚于 epoch，且非固定值）。
        let now = resolve_time_or_now(TimeOrNow::Now);
        assert!(now > SystemTime::UNIX_EPOCH);
    }

    const ROOT: u64 = 1;

    /// 建 shadow ScrollzRw + 一个 200KiB 文件（128KiB 块 → 块0 满封建 head 缓存），返回 (fs, 内容, ino)。
    fn fs_with_head_cache() -> (ScrollzRw, Vec<u8>, u64) {
        let dir = tempfile::tempdir().unwrap();
        let cs = 128 * 1024u32;
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let store = Arc::new(ShadowStore::open_with_chunk_size(backing, cs).unwrap());
        let attr = Attr {
            ino: 0,
            size: 0,
            kind: FileType::RegularFile,
            perm: 0o644,
            uid: 0,
            gid: 0,
            mtime: SystemTime::UNIX_EPOCH,
            atime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            chunk_size: cs,
        };
        let ino = store.create(ROOT, "f.bin", attr).unwrap();
        let data: Vec<u8> = (0..200 * 1024).map(|i| b"abcde \n"[i % 7]).collect();
        let fs = ScrollzRw::new(store.clone(), Algo::Zstd, 3, cs);
        rmw::write_at(store.as_ref(), ino, 0, &data, &fs.params).unwrap();
        store.fsync(ino).unwrap();
        std::mem::forget(dir); // 测试期保留 backing
        (fs, data, ino)
    }

    #[test]
    fn read_range_head_cache_hit_byte_for_byte_correct() {
        let (fs, data, ino) = fs_with_head_cache();
        // 区间完全落在 head 缓存前缀（< 64KiB）→ 走快路径，免整块解压。
        let got = fs.read_range(ino, 100, 4000).unwrap();
        assert_eq!(got, &data[100..4100], "head 缓存切片逐字节一致");
    }

    #[test]
    fn read_range_beyond_cached_prefix_falls_back_per_block_still_correct() {
        let (fs, data, ino) = fs_with_head_cache();
        // 跨越 head 缓存末端（HEAD_CACHE_BYTES 附近）→ 回退逐块路径，仍逐字节正确。
        let off = HEAD_CACHE_BYTES - 50;
        let got = fs.read_range(ino, off, 200).unwrap();
        assert_eq!(
            got,
            &data[off as usize..off as usize + 200],
            "回退逐块逐字节一致"
        );
    }

    #[test]
    fn forget_evicts_lock_and_tail_preserving_data() {
        // 评审 D1：只追加不删除的会话负载下，旧码无 forget → per-inode 锁与尾缓冲永久驻留、
        // 无界增长。forget 须回收锁/尾缓冲，且先封尾再丢弃、绝不丢未落盘数据。
        let dir = tempfile::tempdir().unwrap();
        let cs = 65536u32;
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let store = Arc::new(ShadowStore::open_with_chunk_size(backing, cs).unwrap());
        let attr = Attr {
            ino: 0,
            size: 0,
            kind: FileType::RegularFile,
            perm: 0o644,
            uid: 0,
            gid: 0,
            mtime: SystemTime::UNIX_EPOCH,
            atime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            chunk_size: cs,
        };
        let ino = store.create(ROOT, "f.bin", attr).unwrap();
        let fs = ScrollzRw::new(store.clone(), Algo::Zstd, 3, cs);
        // 写恰好一个满块（cs 字节）并封块，使后续 append 落在 cs 边界的新开放尾块（生产真实
        // append 路径：尾日志记录对应 idx==chunk_count 的新尾块）。
        let base: Vec<u8> = (0..cs as usize).map(|i| b"abcde \n"[i % 7]).collect();
        {
            let lock = fs.lock_for(ino);
            let mut g = lock.write();
            fs.tails
                .write_at_locked(store.as_ref(), ino, &mut g, 0, &base, &fs.params)
                .unwrap();
            fs.tails
                .seal_locked(store.as_ref(), ino, &mut g, &fs.params)
                .unwrap();
        }
        store.fsync(ino).unwrap();
        // 经尾缓冲追加一行（建立 tails 缓冲 + 锁项）。
        let extra = b"appended-session-line\n";
        {
            let lock = fs.lock_for(ino);
            let mut g = lock.write();
            fs.tails
                .write_at_locked(store.as_ref(), ino, &mut g, cs as u64, extra, &fs.params)
                .unwrap();
        }
        assert!(fs.inodes.contains_key(&ino), "追加后应有锁项");

        fs.forget_inode_flush(ino);

        assert!(
            !fs.inodes.contains_key(&ino),
            "forget 后锁项应回收（评审 D1：杜绝锁表无界增长）"
        );
        // 数据未丢：重读追加区间仍得追加内容（forget 先 seal 刷盘再丢弃内存缓冲）。
        let got = fs.read_range(ino, cs as u64, extra.len() as u32).unwrap();
        assert_eq!(got, extra, "forget 先封尾再丢弃，追加数据不丢");
        std::mem::forget(dir);
    }

    /// C-4：forget 路径里 seal 成功但 store.flush 失败时——尾字节已 seal 进 journal 却未提交
    /// SB 尾指针。旧码仅 warn 仍 forget 内存缓冲 → 新 reader 读不到这些尾字节（静默丢数据 +
    /// 零填充）。修复后须视同 seal 失败：保留尾缓冲与锁、不 forget、下次重试。
    #[test]
    fn forget_flush_failure_preserves_tail_and_lock_no_data_loss() {
        let dir = tempfile::tempdir().unwrap();
        let cs = 65536u32;
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let inner = Arc::new(ShadowStore::open_with_chunk_size(backing, cs).unwrap());
        let attr = Attr {
            ino: 0,
            size: 0,
            kind: FileType::RegularFile,
            perm: 0o644,
            uid: 0,
            gid: 0,
            mtime: SystemTime::UNIX_EPOCH,
            atime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            chunk_size: cs,
        };
        let ino = inner.create(ROOT, "f.bin", attr).unwrap();
        // flush 注入失败的装饰器（seal/append_tail 仍转发到 inner → seal 成功、flush 报错）。
        let store = Arc::new(FlushFailStore::new(inner));
        let fs = ScrollzRw::new(store.clone(), Algo::Zstd, 3, cs);

        // 写满一个块并封块，使后续 append 落在新开放尾块（同 forget_evicts 测试的真实 append 路径）。
        let base: Vec<u8> = (0..cs as usize).map(|i| b"abcde \n"[i % 7]).collect();
        {
            let lock = fs.lock_for(ino);
            let mut g = lock.write();
            fs.tails
                .write_at_locked(store.as_ref(), ino, &mut g, 0, &base, &fs.params)
                .unwrap();
            fs.tails
                .seal_locked(store.as_ref(), ino, &mut g, &fs.params)
                .unwrap();
        }
        store.fsync(ino).unwrap();
        // 经尾缓冲追加（建立 tails 缓冲 + 锁项）。
        let extra = b"appended-session-line\n";
        {
            let lock = fs.lock_for(ino);
            let mut g = lock.write();
            fs.tails
                .write_at_locked(store.as_ref(), ino, &mut g, cs as u64, extra, &fs.params)
                .unwrap();
        }

        // 此后 flush 注入失败。forget 不得丢弃尾缓冲与锁。
        store.set_flush_fail(true);
        fs.forget_inode_flush(ino);

        assert!(
            fs.inodes.contains_key(&ino),
            "flush 失败时锁项须保留（视同 seal 失败、下次重试）"
        );
        // 尾缓冲仍在：read_range 仍读到追加内容（数据未被丢弃）。
        let got = fs.read_range(ino, cs as u64, extra.len() as u32).unwrap();
        assert_eq!(got, extra, "flush 失败不得丢弃已写入的尾字节");

        // 恢复 flush 后重试 forget 成功收尾（锁项回收、数据仍在）。
        store.set_flush_fail(false);
        fs.forget_inode_flush(ino);
        assert!(
            !fs.inodes.contains_key(&ino),
            "flush 恢复后重试 forget 应成功回收锁项"
        );
        let got2 = fs.read_range(ino, cs as u64, extra.len() as u32).unwrap();
        assert_eq!(got2, extra, "重试成功后数据仍完整");
        std::mem::forget(dir);
    }

    /// D4-b 不变量：并发 write + forget 同 ino 不 panic、不丢锁项、不丢 tail。
    ///
    /// 旧版 tail 与锁物理分离（`RwLock<()>` + 另一张 Mutex 表），靠注释约定加锁。本测验证重构后
    /// 锁真正包住 `InodeState`：多线程对同 ino 反复 write，期间穿插 forget（丢弃尾块 + evict 锁项），
    /// 全程串行化于该 inode 写锁 → 无数据竞争 panic；末轮全静默后 forget 应把该 ino 从 DashMap 移除。
    #[test]
    fn concurrent_write_and_forget_same_ino_no_panic_and_evicted_ino_absent_after() {
        use std::sync::atomic::{AtomicBool, Ordering as AOrd};
        use std::thread;

        let cs = 4096u32;
        let mem: Arc<dyn Store> = Arc::new(MemStore::new(cs));
        let attr = Attr {
            ino: 0,
            size: 0,
            kind: FileType::RegularFile,
            perm: 0o644,
            uid: 0,
            gid: 0,
            mtime: SystemTime::UNIX_EPOCH,
            atime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            chunk_size: cs,
        };
        let ino = mem.create(ROOT, "f.bin", attr).unwrap();
        let fs = Arc::new(ScrollzRw::new(mem, Algo::Zstd, 3, cs));

        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();

        // 4 个写线程：各自反复在文件尾 append（持该 ino 写锁，经 write_at_locked）。
        for _ in 0..4 {
            let fs = Arc::clone(&fs);
            let stop = Arc::clone(&stop);
            handles.push(thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(AOrd::Relaxed) {
                    let lock = fs.lock_for(ino);
                    let mut g = lock.write();
                    // 追加到当前几何尾部（含未封尾块），保持纯 append 快路径。
                    let off = fs
                        .tails
                        .geometry_locked(fs.store.as_ref(), ino, &g)
                        .map(|(s, _)| s)
                        .unwrap_or(0);
                    fs.write_at_locked(ino, &mut g, off, b"x").unwrap();
                    n += 1;
                    if n > 2000 {
                        break;
                    }
                }
            }));
        }

        // 2 个 forget 线程：穿插丢弃尾块 + evict（与写线程串行于同 ino 写锁，堵 MEDIUM-3 panic 窗口）。
        for _ in 0..2 {
            let fs = Arc::clone(&fs);
            let stop = Arc::clone(&stop);
            handles.push(thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(AOrd::Relaxed) {
                    fs.forget_inode_locked(ino);
                    n += 1;
                    if n > 2000 {
                        break;
                    }
                }
            }));
        }

        // 让线程跑一小段后收尾。
        thread::sleep(Duration::from_millis(50));
        stop.store(true, AOrd::Relaxed);
        for h in handles {
            h.join()
                .expect("线程不应 panic（锁包住 InodeState，无数据竞争）");
        }

        // 终态一致性：读不 panic（读路径持读锁取 InodeState）。
        let _ = fs.read_range(ino, 0, 16);

        // 末轮 forget：全部线程已停，此后单 forget 应把该 ino 从 DashMap 彻底回收（无活句柄、
        // strong_count==1）。验证 evict 后表不含该 ino——锁表不泄漏。
        fs.forget_inode_locked(ino);
        assert!(
            !fs.inodes.contains_key(&ino),
            "末轮 forget 后该 ino 应从 inodes 表移除（evict 不泄漏锁项）"
        );
    }

    // ---- 块缓存集成回归（perf #1）----

    use crate::core::inode::Ino;
    use crate::store::tests_support::MemStore;
    use crate::store::{DirEntry, Store, StoredBlock};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// C-4 用：透明转发全部 Store 方法，但可注入 `flush` 失败。`seal_tail_block`/`append_tail`
    /// 转发到 inner（seal 成功），仅 `flush` 在开关打开时返回错误——精确复现「seal 成功、flush
    /// 失败」分支。
    struct FlushFailStore {
        inner: Arc<dyn Store>,
        flush_fail: std::sync::atomic::AtomicBool,
    }
    impl FlushFailStore {
        fn new(inner: Arc<dyn Store>) -> Self {
            Self {
                inner,
                flush_fail: std::sync::atomic::AtomicBool::new(false),
            }
        }
        fn set_flush_fail(&self, v: bool) {
            self.flush_fail.store(v, Ordering::SeqCst);
        }
    }
    impl Store for FlushFailStore {
        fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
            self.inner.lookup(parent, name)
        }
        fn create(&self, parent: Ino, name: &str, attr: Attr) -> std::io::Result<Ino> {
            self.inner.create(parent, name, attr)
        }
        fn mkdir(&self, parent: Ino, name: &str, attr: Attr) -> std::io::Result<Ino> {
            self.inner.mkdir(parent, name, attr)
        }
        fn unlink(&self, parent: Ino, name: &str) -> std::io::Result<()> {
            self.inner.unlink(parent, name)
        }
        fn rmdir(&self, parent: Ino, name: &str) -> std::io::Result<()> {
            self.inner.rmdir(parent, name)
        }
        fn rename(&self, old: (Ino, &str), new: (Ino, &str)) -> std::io::Result<()> {
            self.inner.rename(old, new)
        }
        fn readdir(&self, dir: Ino) -> Vec<DirEntry> {
            self.inner.readdir(dir)
        }
        fn setattr(&self, ino: Ino, attr: Attr) -> std::io::Result<()> {
            self.inner.setattr(ino, attr)
        }
        fn getattr_ino(&self, ino: Ino) -> Option<Attr> {
            self.inner.getattr_ino(ino)
        }
        fn get_block(&self, ino: Ino, idx: u64) -> std::io::Result<Option<StoredBlock>> {
            self.inner.get_block(ino, idx)
        }
        fn block_geometry(&self, ino: Ino) -> Option<(u64, u32)> {
            self.inner.block_geometry(ino)
        }
        fn put_block(
            &self,
            ino: Ino,
            idx: u64,
            blk: StoredBlock,
            new_size: u64,
        ) -> std::io::Result<()> {
            self.inner.put_block(ino, idx, blk, new_size)
        }
        fn truncate_blocks(&self, ino: Ino, keep_from: u64, new_size: u64) -> std::io::Result<()> {
            self.inner.truncate_blocks(ino, keep_from, new_size)
        }
        fn supports_tail_journal(&self) -> bool {
            self.inner.supports_tail_journal()
        }
        fn append_tail(&self, ino: Ino, delta: &[u8], new_size: u64) -> std::io::Result<()> {
            self.inner.append_tail(ino, delta, new_size)
        }
        fn seal_tail_block(
            &self,
            ino: Ino,
            idx: u64,
            blk: StoredBlock,
            new_size: u64,
        ) -> std::io::Result<()> {
            self.inner.seal_tail_block(ino, idx, blk, new_size)
        }
        fn set_head_cache(
            &self,
            ino: Ino,
            stored_bytes: Vec<u8>,
            verbatim: bool,
            rawlen: u64,
        ) -> std::io::Result<()> {
            self.inner
                .set_head_cache(ino, stored_bytes, verbatim, rawlen)
        }
        fn read_head_cache(
            &self,
            ino: Ino,
            off: u64,
            len: u64,
        ) -> std::io::Result<Option<(Vec<u8>, bool)>> {
            self.inner.read_head_cache(ino, off, len)
        }
        fn fsync(&self, ino: Ino) -> std::io::Result<()> {
            self.inner.fsync(ino)
        }
        fn flush(&self, ino: Ino) -> std::io::Result<()> {
            if self.flush_fail.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("注入的 flush 失败"));
            }
            self.inner.flush(ino)
        }
        fn release(&self, ino: Ino) {
            self.inner.release(ino)
        }
        fn sync_all(&self) -> std::io::Result<()> {
            self.inner.sync_all()
        }
    }

    /// 透明转发全部 Store 方法、统计 `get_block` 调用次数的装饰器。默认方法（flush/seal_tail_block/
    /// head_cache 等）经 trait 默认实现转调 `self.*` → 仍落到 inner，故无需逐一覆写。
    struct CountingStore {
        inner: Arc<dyn Store>,
        get_block_calls: AtomicUsize,
    }
    impl CountingStore {
        fn new(inner: Arc<dyn Store>) -> Self {
            Self {
                inner,
                get_block_calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.get_block_calls.load(Ordering::SeqCst)
        }
        fn reset(&self) {
            self.get_block_calls.store(0, Ordering::SeqCst);
        }
    }
    impl Store for CountingStore {
        fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
            self.inner.lookup(parent, name)
        }
        fn create(&self, parent: Ino, name: &str, attr: Attr) -> std::io::Result<Ino> {
            self.inner.create(parent, name, attr)
        }
        fn mkdir(&self, parent: Ino, name: &str, attr: Attr) -> std::io::Result<Ino> {
            self.inner.mkdir(parent, name, attr)
        }
        fn unlink(&self, parent: Ino, name: &str) -> std::io::Result<()> {
            self.inner.unlink(parent, name)
        }
        fn rmdir(&self, parent: Ino, name: &str) -> std::io::Result<()> {
            self.inner.rmdir(parent, name)
        }
        fn rename(&self, old: (Ino, &str), new: (Ino, &str)) -> std::io::Result<()> {
            self.inner.rename(old, new)
        }
        fn readdir(&self, dir: Ino) -> Vec<DirEntry> {
            self.inner.readdir(dir)
        }
        fn setattr(&self, ino: Ino, attr: Attr) -> std::io::Result<()> {
            self.inner.setattr(ino, attr)
        }
        fn getattr_ino(&self, ino: Ino) -> Option<Attr> {
            self.inner.getattr_ino(ino)
        }
        fn get_block(&self, ino: Ino, idx: u64) -> std::io::Result<Option<StoredBlock>> {
            self.get_block_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.get_block(ino, idx)
        }
        fn block_geometry(&self, ino: Ino) -> Option<(u64, u32)> {
            self.inner.block_geometry(ino)
        }
        fn put_block(
            &self,
            ino: Ino,
            idx: u64,
            blk: StoredBlock,
            new_size: u64,
        ) -> std::io::Result<()> {
            self.inner.put_block(ino, idx, blk, new_size)
        }
        fn truncate_blocks(&self, ino: Ino, keep_from: u64, new_size: u64) -> std::io::Result<()> {
            self.inner.truncate_blocks(ino, keep_from, new_size)
        }
        fn fsync(&self, ino: Ino) -> std::io::Result<()> {
            self.inner.fsync(ino)
        }
        fn sync_all(&self) -> std::io::Result<()> {
            self.inner.sync_all()
        }
    }

    /// 建 CountingStore(MemStore) 后端 + 一个 `nbytes` 字节的多块已封存文件（经 rmw 直写 committed
    /// 块，绕开尾缓冲），返回 (fs, 计数 store, 内容, ino)。`cap` 为块缓存字节上限。
    fn fs_counting(
        cs: u32,
        nbytes: usize,
        cap: usize,
    ) -> (ScrollzRw, Arc<CountingStore>, Vec<u8>, u64) {
        let mem: Arc<dyn Store> = Arc::new(MemStore::new(cs));
        let ino = {
            // 经 MemStore 便捷入口在根下建匿名文件。
            let attr = Attr {
                ino: 0,
                size: 0,
                kind: FileType::RegularFile,
                perm: 0o644,
                uid: 0,
                gid: 0,
                mtime: SystemTime::UNIX_EPOCH,
                atime: SystemTime::UNIX_EPOCH,
                ctime: SystemTime::UNIX_EPOCH,
                chunk_size: cs,
            };
            mem.create(ROOT, "f.bin", attr).unwrap()
        };
        let store = Arc::new(CountingStore::new(mem));
        let fs = ScrollzRw::new(store.clone(), Algo::Zstd, 3, cs).with_block_cache(cap);
        let data: Vec<u8> = (0..nbytes).map(|i| b"abcde \n"[i % 7]).collect();
        rmw::write_at(store.as_ref(), ino, 0, &data, &fs.params).unwrap();
        store.fsync(ino).unwrap();
        (fs, store, data, ino)
    }

    #[test]
    fn block_cache_multiple_small_reads_same_interior_block_fetch_once() {
        let cs = 4096u32;
        // 4 满块 + 100B 尾块 → tail_idx=4，块 0..3 为可缓存内部块。
        let (fs, store, data, ino) = fs_counting(cs, 4 * cs as usize + 100, 1 << 20);
        store.reset();
        let b1 = cs as u64; // 块 1 起点。
        let r1 = fs.read_range(ino, b1, 100).unwrap();
        let r2 = fs.read_range(ino, b1 + 100, 100).unwrap();
        let r3 = fs.read_range(ino, b1 + 1000, 50).unwrap();
        assert_eq!(r1, &data[b1 as usize..b1 as usize + 100], "首读逐字节正确");
        assert_eq!(
            r2,
            &data[b1 as usize + 100..b1 as usize + 200],
            "缓存命中切片正确"
        );
        assert_eq!(r3, &data[b1 as usize + 1000..b1 as usize + 1050]);
        assert_eq!(
            store.calls(),
            1,
            "内部块只取/解压一次，其余命中缓存（消除顺序读放大）"
        );
    }

    #[test]
    fn block_cache_tail_block_not_cached_fetch_every_read() {
        let cs = 4096u32;
        let nbytes = 4 * cs as usize + 100; // tail_idx=4，尾块部分 100B。
        let (fs, store, data, ino) = fs_counting(cs, nbytes, 1 << 20);
        store.reset();
        let tail = 4 * cs as u64; // 尾块（idx==tail_idx）起点。
        let _ = fs.read_range(ino, tail, 50).unwrap();
        let _ = fs.read_range(ino, tail + 10, 40).unwrap();
        assert_eq!(
            store.calls(),
            2,
            "尾块 idx==tail_idx 不进缓存（防可变尾日志重放陈旧，杠杆 A），每次读都取块"
        );
        let g = fs.read_range(ino, tail, 100).unwrap();
        assert_eq!(
            g,
            &data[tail as usize..tail as usize + 100],
            "尾块内容仍正确"
        );
    }

    #[test]
    fn block_cache_invalidated_after_write_interior_block_reads_new_bytes_not_stale() {
        let cs = 4096u32;
        let (fs, store, _data, ino) = fs_counting(cs, 4 * cs as usize + 100, 1 << 20);
        let b1 = cs as u64;
        let before = fs.read_range(ino, b1, 100).unwrap(); // 缓存块 1。
        let newbytes = vec![0xABu8; 100];
        {
            // 经 write_at_locked（持写锁、写前无条件失效）改写块 1 内一段。
            let lock = fs.lock_for(ino);
            let mut g = lock.write();
            fs.write_at_locked(ino, &mut g, b1, &newbytes).unwrap();
        }
        let after = fs.read_range(ino, b1, 100).unwrap();
        assert_ne!(after, before, "写后缓存须失效，不得返回陈旧旧值");
        assert_eq!(after, newbytes, "读到新写入字节");
        let _ = store;
    }

    #[test]
    fn block_cache_hit_miss_counts_recorded_in_metrics() {
        let cs = 4096u32;
        // 4 满块 + 100B 尾块 → tail_idx=4，块 0..3 为可缓存内部块。
        let (fs, _store, _data, ino) = fs_counting(cs, 4 * cs as usize + 100, 1 << 20);
        let b1 = cs as u64; // 内部块 1 起点。

        fn read_metrics(fs: &ScrollzRw) -> String {
            let mut out = String::new();
            fs.metrics.write_prometheus(&mut out);
            out
        }
        fn counter(out: &str, name: &str) -> u64 {
            out.lines()
                .find_map(|l| l.strip_prefix(name).and_then(|r| r.trim().parse().ok()))
                .unwrap_or_else(|| panic!("找不到指标 {name}：\n{out}"))
        }

        // 首读块 1：块缓存未命中（走 Store + 解压 + 回填）。
        let _ = fs.read_range(ino, b1, 100).unwrap();
        let after_first = read_metrics(&fs);
        let miss1 = counter(&after_first, "scrollz_blockcache_misses_total");
        let hit1 = counter(&after_first, "scrollz_blockcache_hits_total");
        assert_eq!(miss1, 1, "首读同一内部块记一次未命中：\n{after_first}");
        assert_eq!(hit1, 0, "首读无命中：\n{after_first}");

        // 第二遍读同一内部块：命中计数上升、未命中不变。
        let _ = fs.read_range(ino, b1 + 100, 100).unwrap();
        let after_second = read_metrics(&fs);
        assert_eq!(
            counter(&after_second, "scrollz_blockcache_hits_total"),
            1,
            "第二遍读同块命中计数上升：\n{after_second}"
        );
        assert_eq!(
            counter(&after_second, "scrollz_blockcache_misses_total"),
            1,
            "命中不再增未命中：\n{after_second}"
        );
    }
}
