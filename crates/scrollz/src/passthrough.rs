//! P0：fuser 透传（passthrough，零压缩）。
//!
//! 把所有 FUSE 操作转发到一个底层目录（backing dir）。目的：在无压缩复杂度下打通
//! inode/句柄/并发/lookup-count/锁顺序骨架，把「FUSE 语义 bug」与「压缩/RMW bug」分离。
//! 对应 docs/01-zipfs-design.md §12 P0、§4「FUSE 层」。这是基准矩阵里的 B0（隔离纯 FUSE 税）。
//!
//! 实现策略：经典用户态透传——自维护 `ino ↔ 相对路径` 映射，每个操作对 backing 下对应路径
//! 或已打开 fd 做真实 syscall。不走 §5 的 Store 接缝（那是 P1+ 压缩布局才需要）。

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::core::system_time_from;

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, KernelConfig,
    OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use log::{debug, warn};

/// attr / entry 缓存 TTL。P0 取 1s；§4.1 要求 BV/BS 间固定同值，此处先给透传一个合理默认。
const TTL: Duration = Duration::from_secs(1);

/// 根 inode 编号，对齐 FUSE 约定（`INodeNo::ROOT == 1`）。
const ROOT_INO: u64 = 1;

// ===========================================================================
// InodeTable：ino ↔ 相对路径映射 + lookup-count（纯逻辑，可单测）
// ===========================================================================

/// 单个 inode 的记录：相对 backing 根的路径 + 内核侧 lookup 引用计数。
#[derive(Debug, Clone)]
struct InodeRecord {
    /// 相对 backing 根的路径（根 inode 为空路径 ""）。
    path: PathBuf,
    /// 内核 lookup 引用计数：每次 lookup/create/mkdir 成功 +1，forget(n) 减 n。
    ///
    /// TODO(§4 延迟回收)：P0 仅用它决定何时从表里丢弃 ino，尚未实现
    /// unlink-while-open 的 orphan 延迟回收（POSIX 要求最后一个 fd 关闭前仍可读写）。
    /// 完整语义在 P4：unlink 一个仍被打开或仍被内核引用的 inode 不能立即回收，
    /// 须置 orphan、待 forget + 句柄全关再回收。透传场景下底层 FS 已天然处理「已打开
    /// 文件被 unlink 后仍可读写」，故 P0 这里风险低，但映射表项的回收语义仍是简化的。
    lookup_count: u64,
}

/// inode 分配与双向映射。线程安全由外层 `Mutex` 保证，本结构本身不加锁。
#[derive(Debug)]
pub struct InodeTable {
    /// ino → 记录。
    by_ino: HashMap<u64, InodeRecord>,
    /// 相对路径 → ino（保证同一路径多次 lookup 拿到稳定 ino）。
    by_path: HashMap<PathBuf, u64>,
    /// 下一个待分配 ino（根占 1，从 2 开始）。
    next_ino: u64,
}

impl InodeTable {
    /// 新建表，预置根 inode（ino=1，路径 ""）。
    pub fn new() -> Self {
        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();
        by_ino.insert(
            ROOT_INO,
            InodeRecord {
                path: PathBuf::new(),
                lookup_count: 1, // 根永不被 forget 到 0
            },
        );
        by_path.insert(PathBuf::new(), ROOT_INO);
        Self {
            by_ino,
            by_path,
            next_ino: 2,
        }
    }

    /// 取某 ino 的相对路径。
    pub fn path_of(&self, ino: u64) -> Option<PathBuf> {
        self.by_ino.get(&ino).map(|r| r.path.clone())
    }

    /// 为 `parent` 下名为 `name` 的子项分配（或复用）ino，并 +1 lookup-count。
    ///
    /// 已存在同路径项则复用其 ino（不重复分配），符合 FUSE「同一对象同一 ino」要求。
    pub fn lookup_or_insert(&mut self, parent: u64, name: &OsStr) -> Option<u64> {
        let parent_path = self.by_ino.get(&parent)?.path.clone();
        let child_path = parent_path.join(name);

        if let Some(&ino) = self.by_path.get(&child_path) {
            // 复用：lookup-count +1
            if let Some(rec) = self.by_ino.get_mut(&ino) {
                rec.lookup_count += 1;
            }
            return Some(ino);
        }

        let ino = self.next_ino;
        self.next_ino += 1;
        self.by_ino.insert(
            ino,
            InodeRecord {
                path: child_path.clone(),
                lookup_count: 1,
            },
        );
        self.by_path.insert(child_path, ino);
        Some(ino)
    }

    /// forget：lookup-count 减 `n`，归零则从映射中移除（根除外）。
    ///
    /// 返回是否已移除。TODO(§4)：移除即「回收」——P0 不区分「仍被打开」，
    /// 透传下底层 fd 仍有效故无数据风险，但 orphan 延迟回收语义待 P4 补全。
    pub fn forget(&mut self, ino: u64, n: u64) -> bool {
        if ino == ROOT_INO {
            return false;
        }
        let Some(rec) = self.by_ino.get_mut(&ino) else {
            return false;
        };
        rec.lookup_count = rec.lookup_count.saturating_sub(n);
        if rec.lookup_count == 0 {
            let path = rec.path.clone();
            self.by_ino.remove(&ino);
            self.by_path.remove(&path);
            true
        } else {
            false
        }
    }

    /// 路径改名时同步映射（rename 用）：把 `old` 子树的根从旧路径迁到新路径。
    ///
    /// P0 简化：只迁移精确匹配 `old` 的项与其直接记录；对已缓存的深层子孙路径，
    /// 依赖内核在 rename 后重新 lookup 来纠正（透传下可接受）。TODO：递归重写子孙缓存路径。
    pub fn rename_path(&mut self, old: &Path, new: &Path) {
        if let Some(ino) = self.by_path.remove(old) {
            if let Some(rec) = self.by_ino.get_mut(&ino) {
                rec.path = new.to_path_buf();
            }
            self.by_path.insert(new.to_path_buf(), ino);
        }
    }

    /// 当前已知 ino 数量（含根），测试用。
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.by_ino.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.by_ino.is_empty()
    }

    /// 读取某 ino 的 lookup-count，测试用。
    #[allow(dead_code)]
    pub fn lookup_count_of(&self, ino: u64) -> Option<u64> {
        self.by_ino.get(&ino).map(|r| r.lookup_count)
    }
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// PassthroughFs：impl fuser::Filesystem
// ===========================================================================

/// 透传文件系统。`backing` 为底层目录绝对路径；所有逻辑路径相对它解析。
pub struct PassthroughFs {
    backing: PathBuf,
    /// inode 映射表。`fuser` 0.17 多线程派发且回调签名为 `&self`，故用 Mutex 提供内部可变。
    inodes: Mutex<InodeTable>,
    /// 打开句柄表：fh → 已打开 File（持有底层 fd，read/write/fsync 走它）。
    ///
    /// 值用 `Arc<File>`：read/write/fsync 取出时 clone Arc（廉价）后**立刻释放表锁**，
    /// 再做阻塞 syscall，避免一把全局锁串行化所有文件的 I/O（见设计 §4 并发要求）。
    handles: Mutex<HashMap<u64, Arc<fs::File>>>,
    /// 下一个待分配 fh（单调递增，不复用）。
    next_fh: Mutex<u64>,
}

impl PassthroughFs {
    /// 用底层目录构造。`backing` 必须存在且为目录。
    pub fn new(backing: PathBuf) -> std::io::Result<Self> {
        let meta = fs::metadata(&backing)?;
        if !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("backing 不是目录：{}", backing.display()),
            ));
        }
        Ok(Self {
            backing,
            inodes: Mutex::new(InodeTable::new()),
            handles: Mutex::new(HashMap::new()),
            next_fh: Mutex::new(1),
        })
    }

    /// 把 ino 解析为 backing 下的绝对路径。
    fn abs_path(&self, ino: u64) -> Option<PathBuf> {
        let rel = self.inodes.lock().unwrap().path_of(ino)?;
        Some(self.backing.join(rel))
    }

    /// 分配一个新 fh。
    fn alloc_fh(&self) -> u64 {
        let mut g = self.next_fh.lock().unwrap();
        let fh = *g;
        *g += 1;
        fh
    }

    /// 取出 fh 对应的 File 引用（clone Arc 后立即释放表锁）。
    ///
    /// 关键：返回前锁已释放，调用方随后的阻塞 syscall 不再持表锁，
    /// 不会串行化其他 fh 的 I/O。
    fn handle(&self, fh: u64) -> Option<Arc<fs::File>> {
        self.handles.lock().unwrap().get(&fh).map(Arc::clone)
    }
}

/// 把底层 `fs::Metadata` 转成 FUSE `FileAttr`，inode 用我们分配的 `ino`。
fn metadata_to_attr(ino: u64, meta: &fs::Metadata) -> FileAttr {
    let kind = mode_to_filetype(meta.mode());
    FileAttr {
        ino: INodeNo(ino),
        size: meta.size(),
        blocks: meta.blocks(),
        atime: system_time_from(meta.atime(), meta.atime_nsec()),
        mtime: system_time_from(meta.mtime(), meta.mtime_nsec()),
        ctime: system_time_from(meta.ctime(), meta.ctime_nsec()),
        crtime: SystemTime::UNIX_EPOCH,
        kind,
        perm: (meta.mode() & 0o7777) as u16,
        nlink: meta.nlink() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev() as u32,
        flags: 0,
        blksize: meta.blksize() as u32,
    }
}

/// 由 st_mode 推 FUSE FileType。
fn mode_to_filetype(mode: u32) -> FileType {
    match mode & libc::S_IFMT {
        libc::S_IFREG => FileType::RegularFile,
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

/// 把 `io::Error` 映射成 `Errno`，无 raw_os_error 时回退 EIO。
fn io_errno(e: &std::io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

impl Filesystem for PassthroughFs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        debug!("init: backing={}", self.backing.display());
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_abs) = self.abs_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child_abs = parent_abs.join(name);
        match fs::symlink_metadata(&child_abs) {
            Ok(meta) => {
                let ino = match self.inodes.lock().unwrap().lookup_or_insert(parent.0, name) {
                    Some(ino) => ino,
                    None => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                };
                reply.entry(&TTL, &metadata_to_attr(ino, &meta), Generation(0));
            }
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.inodes.lock().unwrap().forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(abs) = self.abs_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match fs::symlink_metadata(&abs) {
            Ok(meta) => reply.attr(&TTL, &metadata_to_attr(ino.0, &meta)),
            Err(e) => reply.error(io_errno(&e)),
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
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(abs) = self.abs_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // chown 先于 chmod：POSIX chown 会清掉 setuid/setgid 位，若先 chmod 再 chown
        // 会把刚设的 setuid 抹掉，故先 chown。
        if uid.is_some() || gid.is_some() {
            if let Err(e) = chown(&abs, uid, gid) {
                reply.error(io_errno(&e));
                return;
            }
        }
        // chmod
        if let Some(mode) = mode {
            if let Err(e) = chmod(&abs, mode) {
                reply.error(io_errno(&e));
                return;
            }
        }
        // truncate / extend
        if let Some(size) = size {
            match fs::OpenOptions::new().write(true).open(&abs) {
                Ok(f) => {
                    if let Err(e) = f.set_len(size) {
                        reply.error(io_errno(&e));
                        return;
                    }
                }
                Err(e) => {
                    reply.error(io_errno(&e));
                    return;
                }
            }
        }
        // TODO(§4.1)：atime/mtime 设定（futimens）P0 暂略，待 P4 元数据阶段补。

        match fs::symlink_metadata(&abs) {
            Ok(meta) => reply.attr(&TTL, &metadata_to_attr(ino.0, &meta)),
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(parent_abs) = self.abs_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child_abs = parent_abs.join(name);
        if let Err(e) = fs::create_dir(&child_abs) {
            reply.error(io_errno(&e));
            return;
        }
        // 应用 umask：POSIX 创建语义为 mode & !umask（create_dir 不带 mode，故创建后 chmod）。
        let _ = chmod(&child_abs, (mode & !umask) & 0o7777);

        match fs::symlink_metadata(&child_abs) {
            Ok(meta) => {
                let ino = self
                    .inodes
                    .lock()
                    .unwrap()
                    .lookup_or_insert(parent.0, name)
                    .unwrap_or(0);
                if ino == 0 {
                    reply.error(Errno::EIO);
                    return;
                }
                reply.entry(&TTL, &metadata_to_attr(ino, &meta), Generation(0));
            }
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_abs) = self.abs_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child_abs = parent_abs.join(name);
        // TODO(§4 unlink-while-open)：若该文件仍被打开，POSIX 要求关闭前可继续读写。
        // 透传下底层 FS 天然保证（已打开 fd 在 unlink 后仍有效），故 P0 直接转发；
        // 但我们的 inode 映射表项可能仍残留——依赖 forget 回收，未做 orphan 显式追踪。
        match fs::remove_file(&child_abs) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_abs) = self.abs_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child_abs = parent_abs.join(name);
        match fs::remove_dir(&child_abs) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(io_errno(&e)),
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
        let (Some(parent_abs), Some(newparent_abs)) =
            (self.abs_path(parent.0), self.abs_path(newparent.0))
        else {
            reply.error(Errno::ENOENT);
            return;
        };
        let old_abs = parent_abs.join(name);
        let new_abs = newparent_abs.join(newname);
        match fs::rename(&old_abs, &new_abs) {
            Ok(()) => {
                // 同步 inode 映射的相对路径。
                let mut tbl = self.inodes.lock().unwrap();
                if let (Some(old_rel), Some(new_rel)) =
                    (tbl.path_of(parent.0), tbl.path_of(newparent.0))
                {
                    let old = old_rel.join(name);
                    let new = new_rel.join(newname);
                    tbl.rename_path(&old, &new);
                }
                reply.ok();
            }
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(abs) = self.abs_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        // 用底层 open flags 打开 backing 文件，fd 存进句柄表。
        let mut opts = fs::OpenOptions::new();
        opts.custom_flags(flags.0 & !libc::O_CREAT & !libc::O_EXCL);
        // 由访问模式决定读写；custom_flags 已带 O_ACCMODE，但 std 要求显式 read/write。
        let acc = flags.0 & libc::O_ACCMODE;
        opts.read(acc == libc::O_RDONLY || acc == libc::O_RDWR);
        opts.write(acc == libc::O_WRONLY || acc == libc::O_RDWR);

        match opts.open(&abs) {
            Ok(file) => {
                let fh = self.alloc_fh();
                self.handles.lock().unwrap().insert(fh, Arc::new(file));
                // §4.1：P0 首版用 direct_io 求正确（语义简单、offset/size 精确）。
                reply.opened(FileHandle(fh), fuser::FopenFlags::FOPEN_DIRECT_IO);
            }
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let Some(file) = self.handle(fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        // 锁已释放，下面的 pread 不持表锁。
        let mut buf = vec![0u8; size as usize];
        match pread(&file, &mut buf, offset) {
            Ok(n) => {
                buf.truncate(n);
                reply.data(&buf);
            }
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some(file) = self.handle(fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        // 锁已释放，下面的 pwrite 不持表锁。
        match pwrite(&file, data, offset) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        // flush 不强制落盘（§4 注释）；透传下数据已经过 pwrite 落到 backing fd，
        // P0 仅校验 fh 有效后直接 ok。
        if self.handles.lock().unwrap().contains_key(&fh.0) {
            reply.ok();
        } else {
            reply.error(Errno::EBADF);
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // 从句柄表移除，最后一个 Arc drop 时关闭底层 fd。
        self.handles.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let Some(file) = self.handle(fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        // 锁已释放，sync 可能耗时但不阻塞其他 fh 的 I/O。
        let res = if datasync {
            file.sync_data()
        } else {
            file.sync_all()
        };
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(abs) = self.abs_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // 组装条目列表：. 与 .. 在前，随后是底层目录项。offset 是「下一个条目下标」。
        let mut entries: Vec<(u64, FileType, OsString)> = vec![
            (ino.0, FileType::Directory, OsString::from(".")),
            (ino.0, FileType::Directory, OsString::from("..")),
        ];

        let rd = match fs::read_dir(&abs) {
            Ok(rd) => rd,
            Err(e) => {
                reply.error(io_errno(&e));
                return;
            }
        };
        for dent in rd {
            let Ok(dent) = dent else { continue };
            let name = dent.file_name();
            let kind = match dent.file_type() {
                Ok(ft) => filetype_from_std(&ft),
                Err(_) => FileType::RegularFile,
            };
            // 为每个子项分配/复用 ino，保证 readdir 给出的 ino 与后续 lookup 一致。
            let child_ino = self
                .inodes
                .lock()
                .unwrap()
                .lookup_or_insert(ino.0, &name)
                .unwrap_or(0);
            entries.push((child_ino, kind, name));
        }

        for (i, (eino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            // 下一个条目的 offset = i + 1。reply.add 返回 true 表示缓冲已满。
            if reply.add(INodeNo(eino), (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_abs) = self.abs_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child_abs = parent_abs.join(name);

        let mut opts = fs::OpenOptions::new();
        let acc = flags & libc::O_ACCMODE;
        opts.read(acc == libc::O_RDONLY || acc == libc::O_RDWR);
        opts.write(acc == libc::O_WRONLY || acc == libc::O_RDWR);
        opts.create(true);
        // O_EXCL / O_TRUNC 等透传，但去掉 O_CREAT（由 .create(true) 表达）。
        opts.custom_flags(flags & !libc::O_CREAT);
        // 应用 umask：FUSE 把调用方 umask 单独传来，守护进程自身 umask 通常为 0，须显式扣除。
        opts.mode((mode & !umask) & 0o7777);

        match opts.open(&child_abs) {
            Ok(file) => {
                let meta = match file.metadata() {
                    Ok(m) => m,
                    Err(e) => {
                        reply.error(io_errno(&e));
                        return;
                    }
                };
                let ino = self
                    .inodes
                    .lock()
                    .unwrap()
                    .lookup_or_insert(parent.0, name)
                    .unwrap_or(0);
                if ino == 0 {
                    reply.error(Errno::EIO);
                    return;
                }
                let fh = self.alloc_fh();
                self.handles.lock().unwrap().insert(fh, Arc::new(file));
                reply.created(
                    &TTL,
                    &metadata_to_attr(ino, &meta),
                    Generation(0),
                    FileHandle(fh),
                    fuser::FopenFlags::FOPEN_DIRECT_IO,
                );
            }
            Err(e) => reply.error(io_errno(&e)),
        }
    }

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        let abs = self.abs_path(ino.0).unwrap_or_else(|| self.backing.clone());
        match statvfs(&abs) {
            Ok(s) => reply.statfs(
                s.f_blocks,
                s.f_bfree,
                s.f_bavail,
                s.f_files,
                s.f_ffree,
                s.f_bsize as u32,
                s.f_namemax as u32,
                s.f_frsize as u32,
            ),
            Err(e) => {
                warn!("statfs 失败：{e}");
                reply.error(io_errno(&e));
            }
        }
    }
}

/// std `FileType` → fuser `FileType`。
fn filetype_from_std(ft: &fs::FileType) -> FileType {
    if ft.is_dir() {
        FileType::Directory
    } else if ft.is_symlink() {
        FileType::Symlink
    } else if ft.is_file() {
        FileType::RegularFile
    } else {
        // FIFO / socket / device：用底层 mode 难直接拿，透传下少见，回退普通文件。
        FileType::RegularFile
    }
}

// ===========================================================================
// 低层 syscall 包装（pread/pwrite/chmod/chown/statvfs），集中显式错误处理
// ===========================================================================

fn pread(file: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    // 在 direct_io 下内核不会替我们补齐/重试，须自行处理 EINTR。
    // 短读因 EOF 是合法结果（直接返回真实读到的字节数），故不强行填满缓冲。
    loop {
        // SAFETY：buf 指针与长度成对传入，pread 至多写 buf.len() 字节；
        // fd 来自仍存活的 File（Arc 保活）；errno 仅在 ret<0 时读取。
        let ret = unsafe {
            libc::pread(
                file.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                offset as libc::off_t,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        return Ok(ret as usize);
    }
}

fn pwrite(file: &fs::File, data: &[u8], offset: u64) -> std::io::Result<usize> {
    // 循环写满：pwrite 可能短写（信号/部分落盘），须推进偏移继续写，并对 EINTR 重试，
    // 否则会静默丢数据。EOF 不适用于写。
    let mut written = 0usize;
    while written < data.len() {
        let chunk = &data[written..];
        // SAFETY：chunk 指针与长度成对传入，pwrite 至多读 chunk.len() 字节；
        // fd 来自仍存活的 File；errno 仅在 ret<0 时读取。
        let ret = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                chunk.as_ptr() as *const libc::c_void,
                chunk.len(),
                (offset + written as u64) as libc::off_t,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // 已写出部分则返回已写字节数（内核会按需重发剩余）；否则返回错误。
            if written > 0 {
                return Ok(written);
            }
            return Err(err);
        }
        if ret == 0 {
            // 理论上不应发生；避免死循环。
            break;
        }
        written += ret as usize;
    }
    Ok(written)
}

fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    let c = path_to_cstring(path)?;
    // SAFETY：c 是有效的以 NUL 结尾的 C 字符串，存活到 chmod 返回；errno 仅在 ret<0 时读取。
    let ret = unsafe { libc::chmod(c.as_ptr(), (mode & 0o7777) as libc::mode_t) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn chown(path: &Path, uid: Option<u32>, gid: Option<u32>) -> std::io::Result<()> {
    let c = path_to_cstring(path)?;
    // None → (uid_t)-1，即「不改」（Linux uid_t 为 u32，全 1 位即 -1）。
    let uid = uid.unwrap_or(u32::MAX) as libc::uid_t;
    let gid = gid.unwrap_or(u32::MAX) as libc::gid_t;
    // SAFETY：c 是有效的以 NUL 结尾的 C 字符串，存活到 chown 返回；errno 仅在 ret<0 时读取。
    let ret = unsafe { libc::chown(c.as_ptr(), uid, gid) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn statvfs(path: &Path) -> std::io::Result<libc::statvfs> {
    let c = path_to_cstring(path)?;
    // SAFETY：先零初始化输出结构，再传可写指针；c 是有效 C 字符串；errno 仅在 ret<0 时读取。
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(s)
    }
}

fn path_to_cstring(path: &Path) -> std::io::Result<std::ffi::CString> {
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "路径含 NUL 字节"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_inode_preset_to_1_and_path_empty() {
        let t = InodeTable::new();
        assert_eq!(t.path_of(ROOT_INO), Some(PathBuf::new()));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn assigns_monotonically_increasing_ino_to_children() {
        let mut t = InodeTable::new();
        let a = t.lookup_or_insert(ROOT_INO, OsStr::new("a.txt")).unwrap();
        let b = t.lookup_or_insert(ROOT_INO, OsStr::new("b.txt")).unwrap();
        assert_eq!(a, 2);
        assert_eq!(b, 3);
        assert_eq!(t.path_of(a), Some(PathBuf::from("a.txt")));
    }

    #[test]
    fn repeated_lookup_same_path_reuses_ino_and_accumulates_refcount() {
        let mut t = InodeTable::new();
        let first = t.lookup_or_insert(ROOT_INO, OsStr::new("x")).unwrap();
        let second = t.lookup_or_insert(ROOT_INO, OsStr::new("x")).unwrap();
        assert_eq!(first, second);
        assert_eq!(t.lookup_count_of(first), Some(2));
    }

    #[test]
    fn forget_to_zero_removes_from_table_except_root() {
        let mut t = InodeTable::new();
        let ino = t.lookup_or_insert(ROOT_INO, OsStr::new("y")).unwrap();
        // 计数为 1，forget(1) 应移除
        assert!(t.forget(ino, 1));
        assert_eq!(t.path_of(ino), None);
        // 根永不被移除
        assert!(!t.forget(ROOT_INO, 100));
        assert_eq!(t.path_of(ROOT_INO), Some(PathBuf::new()));
    }

    #[test]
    fn forget_partial_refcount_keeps_entry() {
        let mut t = InodeTable::new();
        let ino = t.lookup_or_insert(ROOT_INO, OsStr::new("z")).unwrap();
        t.lookup_or_insert(ROOT_INO, OsStr::new("z")).unwrap(); // 计数=2
        assert!(!t.forget(ino, 1)); // 仍为 1，不移除
        assert_eq!(t.lookup_count_of(ino), Some(1));
    }

    #[test]
    fn nested_path_joined_correctly() {
        let mut t = InodeTable::new();
        let dir = t.lookup_or_insert(ROOT_INO, OsStr::new("sub")).unwrap();
        let file = t.lookup_or_insert(dir, OsStr::new("inner.txt")).unwrap();
        assert_eq!(t.path_of(file), Some(PathBuf::from("sub/inner.txt")));
    }

    #[test]
    fn rename_path_updates_mapping() {
        let mut t = InodeTable::new();
        let ino = t.lookup_or_insert(ROOT_INO, OsStr::new("old")).unwrap();
        t.rename_path(&PathBuf::from("old"), &PathBuf::from("new"));
        assert_eq!(t.path_of(ino), Some(PathBuf::from("new")));
    }

    #[test]
    fn mode_derives_filetype() {
        assert_eq!(
            mode_to_filetype(libc::S_IFREG | 0o644),
            FileType::RegularFile
        );
        assert_eq!(mode_to_filetype(libc::S_IFDIR | 0o755), FileType::Directory);
        assert_eq!(mode_to_filetype(libc::S_IFLNK | 0o777), FileType::Symlink);
    }
}
