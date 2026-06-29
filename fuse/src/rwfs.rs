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

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, Notifier, OpenFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, Request, TimeOrNow,
};
use log::warn;

use crate::core::chunk::block_range;
use crate::core::codec::{decompress_block, Algo, SharedDict};
use crate::core::rmw::CodecParams;
use crate::core::wsession::TailSessions;
use crate::store::{Attr, Store};

const TTL: Duration = Duration::from_secs(1);

/// 读写 zipfs 前端。持一个 `Store` + codec 参数 + per-inode 写锁表 + 开放尾块缓冲。
pub struct ZipfsRw {
    store: Arc<dyn Store>,
    params: CodecParams,
    default_chunk_size: u32,
    /// per-inode 写锁（§4）：保证同一文件的 RMW / truncate / 封块 / fsync 串行、原子。
    /// `RwLock`：read 取读锁（同 inode 多读并发），write/seal/truncate/fsync 取写锁（排他，仍堵
    /// HIGH-1 torn-read——seal 与读互斥）。
    locks: Mutex<HashMap<u64, Arc<RwLock<()>>>>,
    /// 开放尾块缓冲（append 优化，§1.1）。写/封块由 per-inode 写锁串行；读尾块多读者安全。
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
}

impl ZipfsRw {
    /// 取后端引用（main 后台 metrics 线程读 compression_stats 写 .prom）。
    pub fn store_handle(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store)
    }
}

impl ZipfsRw {
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
            locks: Mutex::new(HashMap::new()),
            tails: TailSessions::new(tail_buffer),
            max_write: 0,
            notifier: Arc::new(OnceLock::new()),
            writeback: false,
        }
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

    /// 取（或建）某 inode 的写锁句柄。
    fn lock_for(&self, ino: u64) -> Arc<RwLock<()>> {
        let mut g = self.locks.lock().unwrap();
        g.entry(ino).or_default().clone()
    }

    /// 回收某 inode 的写锁项（unlink/rmdir 成功后调用，避免锁表无界增长，rust-review H1）。
    /// 仅当无其他持有者（strong_count==1，即只剩表内这一份）时移除，防止误删正在用的锁。
    fn evict_lock(&self, ino: u64) {
        let mut g = self.locks.lock().unwrap();
        if let Some(arc) = g.get(&ino) {
            if Arc::strong_count(arc) == 1 {
                g.remove(&ino);
            }
        }
    }

    /// 在持该 inode 写锁的前提下丢弃其开放尾块（不封块），再回收锁项。
    /// unlink/rmdir/rename-覆盖用：与并发的同 inode write/seal 串行，堵 rust-review MEDIUM-3
    /// 的「ensure_tail_loaded 后尾块被并发 forget 移除」panic 窗口。
    fn forget_inode_locked(&self, ino: u64) {
        {
            let lock = self.lock_for(ino);
            let _guard = lock.write().unwrap();
            self.tails.forget(ino);
        }
        // 出锁作用域后再 evict（evict 要求 strong_count==1，持 guard 时计数为 2 会漏删）。
        self.evict_lock(ino);
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
            blksize: 4096,
        }
    }

    /// 把开放尾块的逻辑大小覆盖进 `Attr`（getattr/lookup 须反映未封尾块，写后读一致）。
    /// 持有该 inode 写锁期间调用最准；无锁的 getattr 仍读到自洽的「含尾块」大小（短暂持表锁）。
    fn overlay_tail_size(&self, mut a: Attr) -> Attr {
        if a.kind == FileType::RegularFile {
            if let Some((size, _cs)) = self.tails.geometry(self.store.as_ref(), a.ino) {
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
        let _guard = lock.read().unwrap();
        let Some((uncompressed_size, chunk_size)) = self.tails.geometry(self.store.as_ref(), ino)
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
            if let Some(plain) = self.tails.read_tail_block(ino, idx) {
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
            let plain = decompress_block(
                &stored.bytes,
                self.params.algo,
                stored.stored_verbatim,
                self.params.dict.as_deref(),
            )
            .map_err(|e| io_to_errno(&e))?;
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

impl Filesystem for ZipfsRw {
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
        match self.read_range(ino.0, offset, size) {
            Ok(buf) => reply.data(&buf),
            Err(e) => reply.error(e),
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
        let lock = self.lock_for(ino.0);
        let _guard = lock.write().unwrap();
        match self
            .tails
            .write_at(self.store.as_ref(), ino.0, offset, data, &self.params)
        {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(io_to_errno(&e)),
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
            let _guard = lock.write().unwrap();
            if let Err(e) = self.tails.seal(self.store.as_ref(), src, &self.params) {
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
            let _guard = lock.write().unwrap();
            if let Err(e) = self
                .tails
                .truncate(self.store.as_ref(), ino.0, new_size, &self.params)
            {
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
        let lock = self.lock_for(ino.0);
        let _guard = lock.write().unwrap();
        if let Err(e) = self.tails.seal(self.store.as_ref(), ino.0, &self.params) {
            reply.error(io_to_errno(&e));
            return;
        }
        match self.store.flush(ino.0) {
            Ok(()) => {
                self.invalidate_kernel_cache(ino.0);
                reply.ok()
            }
            Err(e) => reply.error(io_to_errno(&e)),
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
        let lock = self.lock_for(ino.0);
        let _guard = lock.write().unwrap();
        if let Err(e) = self.tails.seal(self.store.as_ref(), ino.0, &self.params) {
            reply.error(io_to_errno(&e));
            return;
        }
        match self.store.fsync(ino.0) {
            Ok(()) => {
                self.invalidate_kernel_cache(ino.0);
                reply.ok()
            }
            Err(e) => reply.error(io_to_errno(&e)),
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
            let _guard = lock.write().unwrap();
            if let Err(e) = self.tails.seal(self.store.as_ref(), ino.0, &self.params) {
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

    /// 建 shadow ZipfsRw + 一个 200KiB 文件（128KiB 块 → 块0 满封建 head 缓存），返回 (fs, 内容, ino)。
    fn fs_with_head_cache() -> (ZipfsRw, Vec<u8>, u64) {
        let dir = tempfile::tempdir().unwrap();
        let cs = 128 * 1024u32;
        let store =
            Arc::new(ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), cs).unwrap());
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
        let fs = ZipfsRw::new(store.clone(), Algo::Zstd, 3, cs);
        rmw::write_at(store.as_ref(), ino, 0, &data, &fs.params).unwrap();
        store.fsync(ino).unwrap();
        std::mem::forget(dir); // 测试期保留 backing
        (fs, data, ino)
    }

    #[test]
    fn read_range_head_缓存命中_逐字节正确() {
        let (fs, data, ino) = fs_with_head_cache();
        // 区间完全落在 head 缓存前缀（< 64KiB）→ 走快路径，免整块解压。
        let got = fs.read_range(ino, 100, 4000).unwrap();
        assert_eq!(got, &data[100..4100], "head 缓存切片逐字节一致");
    }

    #[test]
    fn read_range_越缓存前缀_回退逐块仍正确() {
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
}
