//! 布局 S —— 影子树 / 每文件压缩包（P1 只读 + P2/P3 写路径）。
//!
//! 设计见 docs/01-zipfs-design.md §7。底层目录树**镜像**逻辑树：逻辑 `/a/b.txt`
//! → 后端 `BACKING/a/b.txt`，该后端文件是一个**分块压缩包**（archive.rs 的 footer 布局）。
//!
//! ## 读侧（P1）
//! lookup / readdir / getattr 走底层镜像目录真实 stat；普通文件逻辑大小取 archive footer。
//! `get_block` 读 archive 块返回压缩字节 + flags，解压交给 Core（§2）。
//!
//! ## 写侧（P2/P3，本次）
//! - create/mkdir/unlink/rmdir/rename/setattr 落到底层镜像目录的真实 syscall。
//! - 写批处理（§6.1 契约）：`put_block`/`truncate_blocks` 累积进 **per-inode 脏会话**
//!   （内存 `WriteSession`），`get_block` read-through 脏块；`fsync`/`flush` 才用
//!   `ArchiveUpdater` 把脏块原地写到 archive 末尾 + 重写 footer（append 不重写前部数据，§7）。
//! - **性能**：一次写会话内复用脏块缓冲，避免每块重开 archive 重扫 footer（修 P1 性能债）。
//!
//! inode 映射：ShadowStore 自维护 `ino ↔ 相对路径` 表。

use super::{Attr, DirEntry, Store, StoredBlock};
use crate::archive::{ArchiveReader, ArchiveUpdater, ArchiveWriter};
use crate::blockio::BlockIo;
use crate::core::inode::Ino;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 根 inode 编号，对齐 FUSE 约定。
const ROOT_INO: u64 = 1;

/// ShadowStore 内部的 inode 映射：ino ↔ 相对 backing 根的路径。
#[derive(Debug)]
struct InodeMap {
    by_ino: HashMap<u64, PathBuf>,
    by_path: HashMap<PathBuf, u64>,
    next_ino: u64,
}

impl InodeMap {
    fn new() -> Self {
        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();
        by_ino.insert(ROOT_INO, PathBuf::new());
        by_path.insert(PathBuf::new(), ROOT_INO);
        Self {
            by_ino,
            by_path,
            next_ino: 2,
        }
    }

    fn path_of(&self, ino: u64) -> Option<PathBuf> {
        self.by_ino.get(&ino).cloned()
    }

    /// 为某相对路径分配或复用稳定 ino。
    fn intern(&mut self, path: PathBuf) -> u64 {
        if let Some(&ino) = self.by_path.get(&path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.by_ino.insert(ino, path.clone());
        self.by_path.insert(path, ino);
        ino
    }

    /// 解除一个相对路径的映射（unlink/rmdir/rename 源用）。
    fn remove_path(&mut self, path: &Path) {
        if let Some(ino) = self.by_path.remove(path) {
            self.by_ino.remove(&ino);
        }
    }

    /// 把 old 路径迁到 new（rename 用），保持 ino 稳定。
    fn rename_path(&mut self, old: &Path, new: &Path) {
        if let Some(ino) = self.by_path.remove(old) {
            self.by_ino.insert(ino, new.to_path_buf());
            self.by_path.insert(new.to_path_buf(), ino);
        }
    }
}

/// per-inode 写会话：累积脏块 + 当前逻辑大小 + 截断标记（写批处理，§6.1）。
#[derive(Debug, Default)]
struct WriteSession {
    /// 脏块：idx → 压缩块。fsync 时按 idx 升序应用到 ArchiveUpdater。
    dirty: HashMap<u64, StoredBlock>,
    /// 会话内当前逻辑大小（put_block/truncate 带入）。
    size: u64,
    /// chunk_size（建会话时从 footer / attr 取）。
    chunk_size: u32,
    /// 若 Some(keep_from)：提交时先把块数截到 keep_from。
    truncate_to: Option<u64>,
    /// 本会话内 Core 交来的新 head 缓存（已压缩字节, verbatim, 覆盖前缀长度），发现读快路径，
    /// docs/02。None = 本会话未更新 head 缓存（提交时 ArchiveUpdater 沿用盘上既有缓存）。
    head_cache: Option<(Vec<u8>, bool, u64)>,
}

/// 影子树后端（布局 S）。`backing` 为底层目录根（archive 树）。
pub struct ShadowStore {
    backing: PathBuf,
    inodes: Mutex<InodeMap>,
    /// per-inode 写会话表。键为 ino，值为挂起的脏块缓冲。fsync/flush 落盘后移除。
    sessions: Mutex<HashMap<u64, WriteSession>>,
    /// per-inode 已打开的 `ArchiveReader` 缓存（性能关键，FIRST-RUN §3.2 修复）。
    ///
    /// 修复前：每次 `get_block` 都 `ArchiveReader::open`（重读尾部 footer + 全量解析索引 +
    /// CRC 全扫 + 逐项越界校验）。一个 1GiB 文件索引约 320KiB，每个 4KiB 随机读都重解析一遍
    /// → 1.4 MiB/s 病态慢。改为缓存解析结果：首次 `get_block` 打开并解析一次，后续复用，
    /// 块定位降为内存 O(1) + 一次 pread。`ArchiveReader::read_block` 用定位读（pread，不移动
    /// 游标），故缓存的 reader 可被 fuser 多线程并发读而无 seek 竞争。
    ///
    /// **缓存失效（一致性关键）**：任何改动底层 archive 的操作都必须淘汰对应缓存项，否则会
    /// 读到陈旧 footer/index。失效点：写会话提交（commit_session）、unlink、rename 覆盖已存在
    /// 目标。`rename`（不覆盖）/ `setattr-perm` **不改 archive 内容**（rename 保持 ino 与文件内容
    /// 不变；setattr 只改底层 mode，size/footer 不动），故无需失效。写会话**未提交**时也无需失效
    /// ——挂起脏块由 get_block read-through 命中，未脏块仍读旧版本 archive，与缓存内容一致。
    ///
    /// **失效世代（epoch）**：仅 remove 缓存项不足以堵住「open→insert 之间被并发失效」的回填
    /// 竞态（rust-review H1）：读线程在锁外 open 出旧 reader 后，若期间发生 commit+失效，再
    /// `or_insert` 会把陈旧 reader 回填。故失效时递增 `reader_epoch`，回填前比对 open 前的快照，
    /// 世代变了就**不回填**（本次仍返回刚开的 reader 供当前读用，但不污染缓存）。
    readers: Mutex<HashMap<u64, Arc<ArchiveReader>>>,
    /// reader 缓存失效世代计数器，见 `readers` 文档（堵 open→insert 回填竞态，H1）。
    reader_epoch: AtomicU64,
    /// 新建文件采用的默认 chunk_size。
    default_chunk_size: u32,
    /// 故障注入（docs/05 §4 / 任务 2.6，仅 test/feature）：置位则下次 `commit_session` 走 `FaultIo`
    /// 并令末尾 `up.sync()` 返 EIO，验证「`invalidate_reader` 先于 `up.sync()`」不变量在 sync 失败时
    /// 仍成立。
    #[cfg(any(test, feature = "fault-injection"))]
    fault_commit_sync: std::sync::atomic::AtomicBool,
}

impl ShadowStore {
    /// 用底层 archive 树根构造（默认 chunk_size 取 Core 默认 64KiB）。
    pub fn open(backing: PathBuf) -> io::Result<Self> {
        Self::open_with_chunk_size(backing, crate::core::DEFAULT_CHUNK_SIZE as u32)
    }

    /// 指定默认 chunk_size 构造（测试 / 小块场景用）。
    pub fn open_with_chunk_size(backing: PathBuf, default_chunk_size: u32) -> io::Result<Self> {
        let meta = fs::metadata(&backing)?;
        if !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("backing 不是目录：{}", backing.display()),
            ));
        }
        if default_chunk_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "default_chunk_size 不能为 0",
            ));
        }
        Ok(Self {
            backing,
            inodes: Mutex::new(InodeMap::new()),
            sessions: Mutex::new(HashMap::new()),
            readers: Mutex::new(HashMap::new()),
            reader_epoch: AtomicU64::new(0),
            default_chunk_size,
            #[cfg(any(test, feature = "fault-injection"))]
            fault_commit_sync: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn abs_of(&self, rel: &Path) -> PathBuf {
        self.backing.join(rel)
    }

    fn rel_of(&self, ino: Ino) -> Option<PathBuf> {
        self.inodes.lock().unwrap().path_of(ino)
    }

    fn abs_of_ino(&self, ino: Ino) -> Option<PathBuf> {
        self.rel_of(ino).map(|rel| self.abs_of(&rel))
    }

    /// 由底层 metadata + 相对路径构造 Store 层 `Attr`。普通文件 size 取逻辑大小
    /// （优先脏会话内大小，其次 archive footer），目录用底层 size。
    fn attr_from_meta(&self, ino: Ino, meta: &fs::Metadata, abs: &Path) -> Attr {
        let kind = filetype_from_meta(meta);
        let (size, chunk_size) = if kind == fuser::FileType::RegularFile {
            // 脏会话优先（写后读一致）。
            if let Some(s) = self.sessions.lock().unwrap().get(&ino) {
                (s.size, s.chunk_size)
            } else {
                read_footer_geometry(abs).unwrap_or_else(|| (meta.size(), self.default_chunk_size))
            }
        } else {
            (meta.size(), self.default_chunk_size)
        };
        Attr {
            ino,
            size,
            kind,
            perm: (meta.mode() & 0o7777) as u16,
            uid: meta.uid(),
            gid: meta.gid(),
            // 由底层文件真实时间填充（修复挂载点全 1970 的根因：旧实现丢弃 meta 时间）。
            mtime: crate::core::system_time_from(meta.mtime(), meta.mtime_nsec()),
            atime: crate::core::system_time_from(meta.atime(), meta.atime_nsec()),
            ctime: crate::core::system_time_from(meta.ctime(), meta.ctime_nsec()),
            chunk_size,
        }
    }

    /// 确保某 ino 有写会话，没有则从 archive footer 初始化（懒建）。返回该会话的可变借用守卫。
    fn ensure_session<'a>(
        &'a self,
        ino: Ino,
        sessions: &'a mut HashMap<u64, WriteSession>,
    ) -> io::Result<&'a mut WriteSession> {
        use std::collections::hash_map::Entry;
        match sessions.entry(ino) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(slot) => {
                let abs = self
                    .abs_of_ino(ino)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ino 无映射"))?;
                let (size, chunk_size) =
                    read_footer_geometry(&abs).unwrap_or((0, self.default_chunk_size));
                Ok(slot.insert(WriteSession {
                    dirty: HashMap::new(),
                    size,
                    chunk_size,
                    truncate_to: None,
                    head_cache: None,
                }))
            }
        }
    }

    /// 取该 ino 的缓存 `ArchiveReader`；未缓存则打开+解析一次并存入。`NotFound`（文件不存在）
    /// 返回 `Ok(None)`，与原 get_block 的越界语义一致。其余打开/解析错误向上传递。
    ///
    /// 并发：先在锁内查缓存命中即返回；未命中则**释放锁**再打开（打开含 IO，不可持锁）。打开
    /// **前**快照 `reader_epoch`；打开后取锁，仅当世代未变（期间无失效）且槽位仍空时才回填——
    /// 世代变了说明本次 open 读到的可能已是被取代的旧版本，不回填以免污染缓存（rust-review H1）。
    /// 无论是否回填，本次都返回刚开的 reader 供当前读使用（其引用的块仍在文件内，append-only +
    /// pread 保证读到自洽旧版本字节）。
    fn cached_reader(&self, ino: Ino) -> io::Result<Option<Arc<ArchiveReader>>> {
        if let Some(r) = self.readers.lock().unwrap().get(&ino) {
            return Ok(Some(r.clone()));
        }
        let epoch_before = self.reader_epoch.load(Ordering::Acquire);
        let Some(abs) = self.abs_of_ino(ino) else {
            return Ok(None);
        };
        let reader = match ArchiveReader::open(&abs) {
            Ok(r) => Arc::new(r),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut cache = self.readers.lock().unwrap();
        // 世代未变且槽位仍空才回填；否则沿用已存在项（若有）或直接返回本次 reader 不缓存。
        if self.reader_epoch.load(Ordering::Acquire) == epoch_before {
            return Ok(Some(cache.entry(ino).or_insert(reader).clone()));
        }
        if let Some(existing) = cache.get(&ino) {
            return Ok(Some(existing.clone()));
        }
        Ok(Some(reader))
    }

    /// 淘汰某 ino 的缓存 reader（底层 archive 被改动后调用，防读到陈旧 footer/index）。
    /// 同时递增失效世代，使「正在 open 但尚未回填」的并发读不把陈旧 reader 写回缓存（H1）。
    fn invalidate_reader(&self, ino: Ino) {
        self.reader_epoch.fetch_add(1, Ordering::AcqRel);
        self.readers.lock().unwrap().remove(&ino);
    }

    /// 把某 ino 的脏会话落盘到底层 archive，并移除会话。无会话则只 fsync 文件。
    fn commit_session(&self, ino: Ino) -> io::Result<()> {
        let session = self.sessions.lock().unwrap().remove(&ino);
        let Some(session) = session else {
            // 无脏数据：仍对底层文件 fsync（POSIX fsync 语义）。
            // 注意：这条 `fs::File::open + sync_all` 是一条**独立的 durable 写点**，**有意留在
            // `BlockIo` 接缝之外**（不经 `ArchiveUpdater`）——它不参与 archive 字节流提交协议，
            // 故障注入归 crash-test.sh / Tier 2 覆盖，勿误以为 shadow 全部 durable 写已收口 BlockIo
            // （docs/05 §9 / 任务 1.3）。
            if let Some(abs) = self.abs_of_ino(ino) {
                if let Ok(f) = fs::File::open(&abs) {
                    f.sync_all()?;
                }
            }
            return Ok(());
        };
        let abs = self
            .abs_of_ino(ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ino 无映射"))?;

        // 故障注入钩子（docs/05 §4 / 任务 2.6，仅 test/feature）：用 FaultIo 重放 commit 并令末尾
        // `up.sync()` 返 EIO，验证「invalidate_reader 先于 up.sync()」不变量在 sync 失败时仍成立。
        // FaultIo 为内存盘面（不落真实 archive），仅检验缓存失效时序，故此分支不更新真实文件内容。
        #[cfg(any(test, feature = "fault-injection"))]
        if self
            .fault_commit_sync
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            let bytes = fs::read(&abs)?;
            let fio = crate::blockio::FaultIo::from_bytes(bytes);
            fio.fail_sync_in(3); // commit 内 barrier1/2（sync #1/#2）+ 末尾 up.sync()（#3）失败
            let mut up = ArchiveUpdater::from_io(fio)?;
            return self.commit_with_updater(ino, session, &mut up);
        }

        let mut up = ArchiveUpdater::open(&abs)?;
        self.commit_with_updater(ino, session, &mut up)
    }

    /// 把脏会话应用到 updater 并提交：截断 → 升序应用脏块 → head 缓存 → `commit` → **失效 reader
    /// 缓存** → `up.sync()`。泛型于 `BlockIo`（生产为 `File`，任务 2.6 故障注入为 `FaultIo`）。
    /// **失效点必须在 `up.sync()` 之前**：即便 sync 失败提前返回，盘上已是新版本（commit 内部 barrier
    /// 已落），缓存也不残留旧 reader（rust-review L3）。
    fn commit_with_updater<W: BlockIo>(
        &self,
        ino: Ino,
        session: WriteSession,
        up: &mut ArchiveUpdater<W>,
    ) -> io::Result<()> {
        // 先截断（若有）。
        if let Some(keep_from) = session.truncate_to {
            up.truncate(keep_from, session.size);
        }
        // 按 idx 升序应用脏块（set_block 不允许空洞，须连续）。
        let mut idxs: Vec<u64> = session.dirty.keys().copied().collect();
        idxs.sort_unstable();
        for idx in idxs {
            let blk = &session.dirty[&idx];
            up.set_block(idx, &blk.bytes, blk.stored_verbatim, session.size)?;
        }
        // 本会话若更新了 head 缓存（Core 在块 0 封块时交来），写入 updater；否则 updater 沿用
        // open 时从盘上 footer 载入的既有缓存（块 0 未变时保持有效）。docs/02 §4.3。
        if let Some((bytes, verbatim, rawlen)) = session.head_cache {
            up.set_head_cache(bytes, verbatim, rawlen);
        }
        up.commit()?;
        // 底层 archive 已变更（commit 内部已 sync 落新 footer/index）。在 up.sync() 之前就失效缓存：
        // 即便随后的 sync() 失败提前返回，盘上已是新版本，缓存也不会残留旧 reader（rust-review L3）。
        self.invalidate_reader(ino);
        up.sync()?;
        Ok(())
    }

    /// 故障注入（任务 2.6，仅 test/feature）：令下一次 `commit_session` 走 FaultIo 并使末尾
    /// `up.sync()` 返 EIO。`pub` 以便 feature 构建下集成测试亦可调用（与导出的 `FaultIo` 一致）。
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn fault_next_commit_sync(&self) {
        self.fault_commit_sync
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// 读 archive footer 取 (uncompressed_size, chunk_size)；非 archive / 打不开则 None。
fn read_footer_geometry(abs: &Path) -> Option<(u64, u32)> {
    ArchiveReader::open(abs)
        .ok()
        .map(|r| (r.footer().uncompressed_size, r.footer().chunk_size))
}

/// 把 atime/mtime 落到底层文件（`utimensat`，不跟随符号链接，对齐 symlink_metadata 语义）。
/// epoch 之前的时间 clamp 到 0。ctime 不可由用户态直接设定，故不处理。
fn set_file_times(
    abs: &Path,
    atime: std::time::SystemTime,
    mtime: std::time::SystemTime,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    fn to_timespec(t: std::time::SystemTime) -> libc::timespec {
        let (secs, nsec) = match t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as libc::time_t, d.subsec_nanos() as i64),
            Err(_) => (0, 0),
        };
        libc::timespec {
            tv_sec: secs,
            tv_nsec: nsec as _,
        }
    }
    let times = [to_timespec(atime), to_timespec(mtime)];
    let c_path = std::ffi::CString::new(abs.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径含 NUL"))?;
    // SAFETY: c_path 是有效的 NUL 结尾 C 字符串；times 指向本栈上长度为 2 的合法数组。
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn filetype_from_meta(meta: &fs::Metadata) -> fuser::FileType {
    let ft = meta.file_type();
    if ft.is_dir() {
        fuser::FileType::Directory
    } else if ft.is_symlink() {
        fuser::FileType::Symlink
    } else {
        fuser::FileType::RegularFile
    }
}

impl Store for ShadowStore {
    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
        let parent_rel = self.rel_of(parent)?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        let meta = fs::symlink_metadata(&abs).ok()?;
        let ino = self.inodes.lock().unwrap().intern(child_rel);
        Some(self.attr_from_meta(ino, &meta, &abs))
    }

    fn create(&self, parent: Ino, name: &str, attr: Attr) -> io::Result<Ino> {
        let parent_rel = self
            .rel_of(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "父目录不存在"))?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        // 新建一个 0 块合法 archive（footer 在、uncompressed_size=0）。
        let chunk_size = if attr.chunk_size == 0 {
            self.default_chunk_size
        } else {
            attr.chunk_size
        };
        let w = ArchiveWriter::create(&abs, chunk_size)?;
        let f = w.finish()?;
        f.sync_all()?;
        // 应用权限。失败不致命（文件已建好）但不可静默吞——记日志（用户规则：不静默吞错误）。
        if let Err(e) = fs::set_permissions(&abs, fs::Permissions::from_mode(attr.perm as u32)) {
            log::warn!("create：设置 {} 权限失败：{e}", abs.display());
        }
        let ino = self.inodes.lock().unwrap().intern(child_rel);
        Ok(ino)
    }

    fn mkdir(&self, parent: Ino, name: &str, attr: Attr) -> io::Result<Ino> {
        let parent_rel = self
            .rel_of(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "父目录不存在"))?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        fs::create_dir(&abs)?;
        if let Err(e) = fs::set_permissions(&abs, fs::Permissions::from_mode(attr.perm as u32)) {
            log::warn!("mkdir：设置 {} 权限失败：{e}", abs.display());
        }
        let ino = self.inodes.lock().unwrap().intern(child_rel);
        Ok(ino)
    }

    fn unlink(&self, parent: Ino, name: &str) -> io::Result<()> {
        let parent_rel = self
            .rel_of(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "父目录不存在"))?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        fs::remove_file(&abs)?;
        // 丢弃可能残留的写会话 + 缓存 reader + 映射项。
        if let Some(ino) = self.inodes.lock().unwrap().by_path.get(&child_rel).copied() {
            self.sessions.lock().unwrap().remove(&ino);
            self.invalidate_reader(ino);
        }
        self.inodes.lock().unwrap().remove_path(&child_rel);
        Ok(())
    }

    fn rmdir(&self, parent: Ino, name: &str) -> io::Result<()> {
        let parent_rel = self
            .rel_of(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "父目录不存在"))?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        fs::remove_dir(&abs)?;
        self.inodes.lock().unwrap().remove_path(&child_rel);
        Ok(())
    }

    fn rename(&self, old: (Ino, &str), new: (Ino, &str)) -> io::Result<()> {
        let old_parent_rel = self
            .rel_of(old.0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "源父目录不存在"))?;
        let new_parent_rel = self
            .rel_of(new.0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "目标父目录不存在"))?;
        let old_rel = old_parent_rel.join(old.1);
        let new_rel = new_parent_rel.join(new.1);
        // 若目标已存在（被 rename 原子覆盖），其旧 ino 的缓存 reader / 写会话指向的内容即将被
        // old 的内容替换，须一并失效 + 清映射，防陈旧读（rust-review M2）。
        let overwritten_ino = self.inodes.lock().unwrap().by_path.get(&new_rel).copied();
        fs::rename(self.abs_of(&old_rel), self.abs_of(&new_rel))?;
        if let Some(victim) = overwritten_ino {
            self.sessions.lock().unwrap().remove(&victim);
            self.invalidate_reader(victim);
            self.inodes.lock().unwrap().remove_path(&new_rel);
        }
        self.inodes.lock().unwrap().rename_path(&old_rel, &new_rel);
        Ok(())
    }

    fn readdir(&self, dir: Ino) -> Vec<DirEntry> {
        let Some(dir_rel) = self.rel_of(dir) else {
            return Vec::new();
        };
        let abs = self.abs_of(&dir_rel);
        let Ok(rd) = fs::read_dir(&abs) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for dent in rd.flatten() {
            let name = dent.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let child_rel = dir_rel.join(&name);
            let ino = self.inodes.lock().unwrap().intern(child_rel);
            let kind = match dent.file_type() {
                Ok(ft) if ft.is_dir() => fuser::FileType::Directory,
                Ok(ft) if ft.is_symlink() => fuser::FileType::Symlink,
                _ => fuser::FileType::RegularFile,
            };
            out.push(DirEntry {
                ino,
                name: name_str.to_string(),
                kind,
            });
        }
        out
    }

    fn readlink(&self, ino: Ino) -> io::Result<PathBuf> {
        let abs = self
            .abs_of_ino(ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ino 无映射"))?;
        fs::read_link(&abs)
    }

    fn symlink(&self, parent: Ino, name: &str, target: &Path) -> io::Result<Attr> {
        let parent_rel = self
            .rel_of(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "父目录不存在"))?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        std::os::unix::fs::symlink(target, &abs)?;
        let meta = fs::symlink_metadata(&abs)?;
        let ino = self.inodes.lock().unwrap().intern(child_rel);
        Ok(self.attr_from_meta(ino, &meta, &abs))
    }

    fn setattr(&self, ino: Ino, attr: Attr) -> io::Result<()> {
        let abs = self
            .abs_of_ino(ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ino 无映射"))?;
        // perm 落到底层文件（mode）。uid/gid/size 的截断由 Core::truncate 走 truncate_blocks，
        // 这里只处理元数据 perm（size 由分块路径维护，不在 setattr 直接改物理大小）。
        fs::set_permissions(&abs, fs::Permissions::from_mode(attr.perm as u32))?;
        // atime/mtime 落到底层文件（utimensat）。前端已把 TimeOrNow 解析为绝对时间，attr 始终
        // 携带有效时间（未改的字段为 getattr 读到的现值），故无条件回写。ctime 在 Linux 无法
        // 直接设定——元数据变更即由内核置「now」，跳过。
        set_file_times(&abs, attr.atime, attr.mtime)?;
        Ok(())
    }

    fn getattr_ino(&self, ino: Ino) -> Option<Attr> {
        let rel = self.rel_of(ino)?;
        let abs = self.abs_of(&rel);
        let meta = fs::symlink_metadata(&abs).ok()?;
        Some(self.attr_from_meta(ino, &meta, &abs))
    }

    fn get_block(&self, ino: Ino, idx: u64) -> io::Result<Option<StoredBlock>> {
        // 1) read-through 脏会话。
        if let Some(s) = self.sessions.lock().unwrap().get(&ino) {
            if let Some(blk) = s.dirty.get(&idx) {
                return Ok(Some(blk.clone()));
            }
            // 会话内若已 truncate 掉该块，视作越界。
            if let Some(keep_from) = s.truncate_to {
                if idx >= keep_from {
                    return Ok(None);
                }
            }
        }
        // 2) 落底层 archive，经 per-inode reader 缓存（避免每块重开重解析 footer，FIRST-RUN §3.2）。
        let Some(reader) = self.cached_reader(ino)? else {
            return Ok(None);
        };
        match reader.read_block(idx)? {
            Some((bytes, entry)) => Ok(Some(StoredBlock {
                bytes,
                stored_verbatim: entry.is_verbatim(),
            })),
            // idx == 封块数 且有尾日志 → 重放未封尾块原始字节，包成 verbatim 尾块（docs/04 §8.4）。
            None => match reader.read_tail()? {
                Some(raw) if idx == reader.chunk_count() => Ok(Some(StoredBlock {
                    bytes: raw,
                    stored_verbatim: true,
                })),
                _ => Ok(None),
            },
        }
    }

    fn block_geometry(&self, ino: Ino) -> Option<(u64, u32)> {
        // 脏会话优先。
        if let Some(s) = self.sessions.lock().unwrap().get(&ino) {
            return Some((s.size, s.chunk_size));
        }
        // 经缓存 reader 取 footer 几何，避免每次 read 都重开 archive（rwfs::read_range 每读一次）。
        let reader = self.cached_reader(ino).ok().flatten()?;
        let f = reader.footer();
        Some((f.uncompressed_size, f.chunk_size))
    }

    fn put_block(&self, ino: Ino, idx: u64, blk: StoredBlock, new_size: u64) -> io::Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = self.ensure_session(ino, &mut sessions)?;
        s.dirty.insert(idx, blk);
        s.size = new_size;
        Ok(())
    }

    fn truncate_blocks(&self, ino: Ino, keep_from: u64, new_size: u64) -> io::Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = self.ensure_session(ino, &mut sessions)?;
        // 丢弃脏块中 >= keep_from 的，并记录截断点（提交时一并应用到底层）。
        s.dirty.retain(|&i, _| i < keep_from);
        s.truncate_to = Some(match s.truncate_to {
            Some(prev) => prev.min(keep_from),
            None => keep_from,
        });
        s.size = new_size;
        Ok(())
    }

    // ---- in-archive 尾日志（写放大根治，docs/04 §8.4）----
    fn supports_tail_journal(&self) -> bool {
        true
    }

    /// fsync 路径：把未封尾块原始增量追加进 archive 尾日志并 durable。开 updater →
    /// append_journal(delta) → commit_journal（不重写 index、双段 barrier）。绕开脏会话直接落盘，
    /// 故 reader 缓存随之失效（盘上 SB/journal 已变）。
    fn append_tail(&self, ino: Ino, delta: &[u8], new_size: u64) -> io::Result<()> {
        let abs = self
            .abs_of_ino(ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ino 无映射"))?;
        let mut up = ArchiveUpdater::open(&abs)?;
        up.append_journal(delta)?;
        up.set_size(new_size);
        up.commit_journal()?;
        self.invalidate_reader(ino);
        Ok(())
    }

    /// 封块：把累积尾块作为压缩块 idx 落盘 + 重置尾日志。开 updater → set_block → reset_journal →
    /// commit。绕开脏会话直接落盘，失效 reader 缓存。
    fn seal_tail_block(
        &self,
        ino: Ino,
        idx: u64,
        blk: StoredBlock,
        new_size: u64,
    ) -> io::Result<()> {
        let abs = self
            .abs_of_ino(ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ino 无映射"))?;
        let mut up = ArchiveUpdater::open(&abs)?;
        up.set_block(idx, &blk.bytes, blk.stored_verbatim, new_size)?;
        up.reset_journal();
        up.commit()?;
        self.invalidate_reader(ino);
        Ok(())
    }

    fn set_head_cache(
        &self,
        ino: Ino,
        stored_bytes: Vec<u8>,
        verbatim: bool,
        rawlen: u64,
    ) -> io::Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = self.ensure_session(ino, &mut sessions)?;
        s.head_cache = Some((stored_bytes, verbatim, rawlen));
        Ok(())
    }

    fn read_head_cache(&self, ino: Ino, off: u64, len: u64) -> io::Result<Option<(Vec<u8>, bool)>> {
        // 有挂起写会话时跳过快路径：脏块 0 可能与盘上 head 缓存不一致（如块 0 刚被 RMW 尚未
        // 提交）→ 回退逐块路径（get_block read-through 脏块）。fsync 后会话移除、快路径恢复。
        if self.sessions.lock().unwrap().contains_key(&ino) {
            return Ok(None);
        }
        let Some(reader) = self.cached_reader(ino)? else {
            return Ok(None);
        };
        let rawlen = reader.head_cache_rawlen();
        // 仅当请求区间 [off, off+len) 完全落在缓存覆盖前缀内才命中（避免部分命中拼接）。
        let covered = rawlen > 0
            && off
                .checked_add(len)
                .map(|end| end <= rawlen)
                .unwrap_or(false);
        if covered {
            reader.read_head_cache()
        } else {
            Ok(None)
        }
    }

    fn fsync(&self, ino: Ino) -> io::Result<()> {
        self.commit_session(ino)
    }

    fn sync_all(&self) -> io::Result<()> {
        let inos: Vec<u64> = self.sessions.lock().unwrap().keys().copied().collect();
        for ino in inos {
            self.commit_session(ino)?;
        }
        Ok(())
    }

    /// 最后一个 fd 关闭：释放该 inode 的缓存 reader（关闭其底层 fd，回收内存）。
    /// 持久化已由 rwfs 在 release 前的 flush 完成，此处仅释放缓存资源。
    fn release(&self, ino: Ino) {
        self.invalidate_reader(ino);
    }

    /// 遍历 backing 树聚合 (物理字节=archive 文件实际占用, 逻辑字节=Σ footer uncompressed_size)。
    /// statfs 据此让 `df` 显压缩比；遍历 O(文件数)，statfs 罕调，可接受。
    fn compression_stats(&self) -> Option<(u64, u64)> {
        let mut phys = 0u64;
        let mut logical = 0u64;
        let mut stack = vec![self.backing.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for ent in rd.flatten() {
                let p = ent.path();
                match ent.file_type() {
                    Ok(t) if t.is_dir() => stack.push(p),
                    Ok(t) if t.is_file() => {
                        phys += fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                        logical += read_footer_geometry(&p).map(|(s, _)| s).unwrap_or(0);
                    }
                    _ => {}
                }
            }
        }
        Some((phys, logical))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inode_map_根为_1_路径为空() {
        let m = InodeMap::new();
        assert_eq!(m.path_of(ROOT_INO), Some(PathBuf::new()));
    }

    #[test]
    fn intern_同路径复用_ino() {
        let mut m = InodeMap::new();
        let a = m.intern(PathBuf::from("a/b.txt"));
        let b = m.intern(PathBuf::from("a/b.txt"));
        assert_eq!(a, b);
        let c = m.intern(PathBuf::from("a/c.txt"));
        assert_ne!(a, c);
        assert_eq!(m.path_of(a), Some(PathBuf::from("a/b.txt")));
    }

    #[test]
    fn rename_path_保持_ino_稳定() {
        let mut m = InodeMap::new();
        let a = m.intern(PathBuf::from("old.txt"));
        m.rename_path(Path::new("old.txt"), Path::new("new.txt"));
        assert_eq!(m.by_path.get(Path::new("new.txt")).copied(), Some(a));
        assert!(!m.by_path.contains_key(Path::new("old.txt")));
        assert_eq!(m.path_of(a), Some(PathBuf::from("new.txt")));
    }

    // ----- reader 缓存：正确性 + 写后失效（FIRST-RUN §3.2 修复） -----

    use crate::core::codec::{compress, decompress, Algo};

    /// 压缩一个逻辑块，构造 StoredBlock（测试便捷）。
    fn mk_block(plain: &[u8]) -> StoredBlock {
        let (bytes, verbatim) = compress(plain, Algo::Zstd, 3).unwrap();
        StoredBlock {
            bytes,
            stored_verbatim: verbatim,
        }
    }

    /// 经 Store API 读回某块的逻辑字节（解压）。
    fn read_plain(store: &ShadowStore, ino: u64, idx: u64) -> Option<Vec<u8>> {
        store
            .get_block(ino, idx)
            .unwrap()
            .map(|b| decompress(&b.bytes, Algo::Zstd, b.stored_verbatim).unwrap())
    }

    /// 在临时 backing 下建一个 ShadowStore + 一个普通文件，返回 (store, tmpdir, ino)。
    fn store_with_file(chunk_size: u32) -> (ShadowStore, tempfile::TempDir, u64) {
        let dir = tempfile::tempdir().unwrap();
        let store =
            ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), chunk_size).unwrap();
        let attr = Attr {
            ino: 0,
            size: 0,
            kind: fuser::FileType::RegularFile,
            perm: 0o644,
            uid: 0,
            gid: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            atime: std::time::SystemTime::UNIX_EPOCH,
            ctime: std::time::SystemTime::UNIX_EPOCH,
            chunk_size,
        };
        let ino = store.create(ROOT_INO, "f.bin", attr).unwrap();
        (store, dir, ino)
    }

    #[test]
    fn symlink_creates_with_symlink_kind_and_readlink_round_trips() {
        let (store, _dir, _file_ino) = store_with_file(4096);
        // 指向 mount 外的软链（Claude `memory` 外链即此类）：target 原样存取。
        let target = Path::new("/some/external/memory");
        let a = store.symlink(ROOT_INO, "memory", target).unwrap();
        assert_eq!(
            a.kind,
            fuser::FileType::Symlink,
            "新条目类型应为 Symlink（否则内核不会发 readlink）"
        );
        // readlink 原样返回 target，不暴露 backing 绝对路径。
        assert_eq!(store.readlink(a.ino).unwrap(), target);
        // 经 lookup 再取也应是 Symlink（symlink_metadata 不跟随）。
        assert_eq!(
            store.lookup(ROOT_INO, "memory").unwrap().kind,
            fuser::FileType::Symlink
        );
    }

    #[test]
    fn readlink_on_regular_file_is_einval() {
        let (store, _dir, file_ino) = store_with_file(4096);
        let e = store.readlink(file_ino).unwrap_err();
        // readlink(2) 对非链接返回 EINVAL → std 映射 InvalidInput。
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "对普通文件应 EINVAL");
    }

    #[test]
    fn reader_缓存_写后读经提交可见且命中缓存() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);

        // 写块0 + 块1，fsync 落盘（提交时失效缓存）。
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.put_block(ino, 1, mk_block(b"BBBB"), 12).unwrap();
        store.fsync(ino).unwrap();

        // 读：首次 get_block 应打开并缓存 reader。
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..])
        );
        assert!(
            store.readers.lock().unwrap().contains_key(&ino),
            "首次读后应缓存 reader"
        );
        // 再读应命中同一缓存（内容仍正确）。
        assert_eq!(read_plain(&store, ino, 1).as_deref(), Some(&b"BBBB"[..]));
        assert_eq!(store.block_geometry(ino), Some((12, cs)));
    }

    #[test]
    fn reader_缓存_提交后失效_不读陈旧数据() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);

        // 第一次写 + 提交 + 读（填充缓存）。
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.fsync(ino).unwrap();
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..])
        );
        assert!(store.readers.lock().unwrap().contains_key(&ino));

        // 第二次写覆盖块0，提交：提交应淘汰缓存。
        store.put_block(ino, 0, mk_block(b"ZZZZZZZZ"), 8).unwrap();
        store.fsync(ino).unwrap();
        assert!(
            !store.readers.lock().unwrap().contains_key(&ino),
            "提交后缓存 reader 必须失效"
        );
        // 再读必须看到新数据（绝不读陈旧 footer/index）。
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"ZZZZZZZZ"[..])
        );
    }

    #[test]
    fn reader_缓存_release_释放() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.fsync(ino).unwrap();
        let _ = read_plain(&store, ino, 0);
        assert!(store.readers.lock().unwrap().contains_key(&ino));
        store.release(ino);
        assert!(
            !store.readers.lock().unwrap().contains_key(&ino),
            "release 后应释放缓存 reader"
        );
    }

    #[test]
    fn reader_缓存_未提交脏块_read_through_不污染缓存() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);
        // 先提交块0，建立缓存。
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.fsync(ino).unwrap();
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..])
        );

        // 开一个新脏会话写块1（未提交）。读块1走脏 read-through；读块0走缓存（旧已提交版本）。
        store.put_block(ino, 1, mk_block(b"CCCC"), 12).unwrap();
        assert_eq!(
            read_plain(&store, ino, 1).as_deref(),
            Some(&b"CCCC"[..]),
            "脏块 read-through"
        );
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..]),
            "未脏块仍读已提交版本"
        );
    }

    #[test]
    fn reader_缓存_rename_覆盖目标_失效旧缓存() {
        let cs = 8u32;
        let (store, _d, _ino) = store_with_file(cs);
        // 建第二个文件 dst.bin，写入并提交后读一次（填充其缓存）。
        let dst_attr = Attr {
            ino: 0,
            size: 0,
            kind: fuser::FileType::RegularFile,
            perm: 0o644,
            uid: 0,
            gid: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            atime: std::time::SystemTime::UNIX_EPOCH,
            ctime: std::time::SystemTime::UNIX_EPOCH,
            chunk_size: cs,
        };
        let dst_ino = store.create(ROOT_INO, "dst.bin", dst_attr).unwrap();
        store
            .put_block(dst_ino, 0, mk_block(b"OLDOLDOL"), 8)
            .unwrap();
        store.fsync(dst_ino).unwrap();
        assert_eq!(
            read_plain(&store, dst_ino, 0).as_deref(),
            Some(&b"OLDOLDOL"[..])
        );
        assert!(store.readers.lock().unwrap().contains_key(&dst_ino));

        // 源文件 f.bin（store_with_file 建的）写入新内容并提交。
        let src = store.lookup(ROOT_INO, "f.bin").unwrap();
        store
            .put_block(src.ino, 0, mk_block(b"NEWNEWNE"), 8)
            .unwrap();
        store.fsync(src.ino).unwrap();

        // rename f.bin -> dst.bin（覆盖）。被覆盖的 dst 旧 ino 缓存须失效。
        store
            .rename((ROOT_INO, "f.bin"), (ROOT_INO, "dst.bin"))
            .unwrap();
        assert!(
            !store.readers.lock().unwrap().contains_key(&dst_ino),
            "被覆盖目标的旧缓存 reader 必须失效"
        );
        // 经新路径读 dst.bin，必须看到 f.bin 的内容（NEW），不得读到旧 OLD。
        let now = store.lookup(ROOT_INO, "dst.bin").unwrap();
        assert_eq!(
            read_plain(&store, now.ino, 0).as_deref(),
            Some(&b"NEWNEWNE"[..]),
            "rename 覆盖后读到新内容"
        );
    }

    // ----- 故障注入：commit_session 的 up.sync() 失败后 reader 缓存仍失效（docs/05 §4 / 任务 2.6）-----

    #[test]
    fn commit_session_sync失败_仍失效reader缓存_不读陈旧() {
        // 历史 reuse-tail-slot durability 洞高发区：commit_session 把 invalidate_reader 放在
        // up.sync() **之前**，故即便 sync 失败提前返回，缓存也已失效。三层中只有 FaultIo 能在
        // 进程内令 up.sync() 返 EIO（docs/05 §8）。
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);
        // 写 + 提交 + 读，填充 reader 缓存。
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.fsync(ino).unwrap();
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..])
        );
        assert!(
            store.readers.lock().unwrap().contains_key(&ino),
            "读后应缓存 reader"
        );

        // 再写块0，武装「下次 commit_session 的 up.sync() 返 EIO」。
        store.put_block(ino, 0, mk_block(b"ZZZZZZZZ"), 8).unwrap();
        store.fault_next_commit_sync();
        let res = store.fsync(ino); // commit ok → invalidate_reader → up.sync() EIO → Err
        assert!(
            res.is_err(),
            "注入 sync 失败应使 fsync 返回 Err（非静默吞）"
        );

        // 不变量：sync 失败提前返回，但 reader 缓存已在 sync 前失效，后续读不命中陈旧 footer。
        assert!(
            !store.readers.lock().unwrap().contains_key(&ino),
            "sync 失败后旧 reader 缓存必已失效（invalidate 先于 up.sync）"
        );
    }

    // ----- head 缓存端到端：Core(rmw) 封块时建 → Store 读快路径（docs/02）-----
    use crate::archive::HEAD_CACHE_BYTES;
    use crate::core::rmw::{self, CodecParams};

    fn codec_params() -> CodecParams {
        CodecParams {
            algo: Algo::Zstd,
            level: 3,
            dict: None,
        }
    }

    /// 确定性可压缩字节（非随机，保证压缩启发式不置 verbatim、贴近 jsonl 文本特性）。
    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| b"abcdefghij \n"[i % 12]).collect()
    }

    #[test]
    fn head_cache_块0封为正文块时建_读快路径解压等于前缀() {
        // 块大小 128KiB（> HEAD_CACHE_BYTES 64KiB）。写 200KiB（2 块）：块0 满封为不可变正文块
        // （new_size 200K > 块0 128K）→ Core 建 head 缓存。fsync 落盘后 read_head_cache 命中。
        let cs = 128 * 1024u32;
        let (store, _d, ino) = store_with_file(cs);
        let data = patterned(200 * 1024);
        rmw::write_at(&store, ino, 0, &data, &codec_params()).unwrap();
        store.fsync(ino).unwrap();

        let hc = store.read_head_cache(ino, 0, HEAD_CACHE_BYTES).unwrap();
        assert!(hc.is_some(), "块0 封为正文块后应建 head 缓存");
        let (bytes, verbatim) = hc.unwrap();
        let plain = decompress(&bytes, Algo::Zstd, verbatim).unwrap();
        assert_eq!(
            plain.len() as u64,
            HEAD_CACHE_BYTES,
            "head 缓存解压长度应为 HEAD_CACHE_BYTES"
        );
        assert_eq!(
            &plain[..],
            &data[..HEAD_CACHE_BYTES as usize],
            "head 缓存解压应逐字节等于文件前 64KB"
        );
    }

    #[test]
    fn head_cache_单块小文件不建() {
        // 写 100KiB（< 128KiB 块，单块、块0 即末块）→ 不建缓存（无放大、块0 仍可变）。
        let cs = 128 * 1024u32;
        let (store, _d, ino) = store_with_file(cs);
        let data = patterned(100 * 1024);
        rmw::write_at(&store, ino, 0, &data, &codec_params()).unwrap();
        store.fsync(ino).unwrap();
        assert!(
            store
                .read_head_cache(ino, 0, HEAD_CACHE_BYTES)
                .unwrap()
                .is_none(),
            "单块文件不建 head 缓存"
        );
    }

    #[test]
    fn head_cache_请求越出覆盖前缀返回_none() {
        let cs = 128 * 1024u32;
        let (store, _d, ino) = store_with_file(cs);
        let data = patterned(200 * 1024);
        rmw::write_at(&store, ino, 0, &data, &codec_params()).unwrap();
        store.fsync(ino).unwrap();
        // 请求 [0, 64KB+1) 越出缓存覆盖前缀（rawlen=64KB）→ 不命中，回退逐块。
        assert!(
            store
                .read_head_cache(ino, 0, HEAD_CACHE_BYTES + 1)
                .unwrap()
                .is_none(),
            "越出覆盖前缀应回退（避免部分命中拼接）"
        );
    }

    #[test]
    fn head_cache_脏会话期间回退逐块() {
        // 块0 封块建缓存 + fsync。再开新写会话（脏块）→ read_head_cache 回退 None（脏块0 可能
        // 与盘上缓存不一致），保证读快路径不读陈旧前缀。fsync 后恢复命中。
        let cs = 128 * 1024u32;
        let (store, _d, ino) = store_with_file(cs);
        let data = patterned(200 * 1024);
        rmw::write_at(&store, ino, 0, &data, &codec_params()).unwrap();
        store.fsync(ino).unwrap();
        assert!(store
            .read_head_cache(ino, 0, HEAD_CACHE_BYTES)
            .unwrap()
            .is_some());

        // 开脏会话（append 块2 的一部分，制造未提交写）。
        store
            .put_block(ino, 2, mk_block(b"tail"), 200 * 1024 + 4)
            .unwrap();
        assert!(
            store
                .read_head_cache(ino, 0, HEAD_CACHE_BYTES)
                .unwrap()
                .is_none(),
            "挂起写会话期间应回退逐块"
        );
        store.fsync(ino).unwrap();
        assert!(
            store
                .read_head_cache(ino, 0, HEAD_CACHE_BYTES)
                .unwrap()
                .is_some(),
            "fsync 后快路径恢复"
        );
    }

    #[test]
    fn head_cache_跨_append_提交保留() {
        // 块0 封块建缓存后，继续 append 增长（不动块0）。多次 fsync 后 head 缓存仍在且内容不变
        // ——验证 ArchiveUpdater 跨提交从 footer 载入并重写既有缓存。
        let cs = 128 * 1024u32;
        let (store, _d, ino) = store_with_file(cs);
        let data = patterned(200 * 1024);
        rmw::write_at(&store, ino, 0, &data, &codec_params()).unwrap();
        store.fsync(ino).unwrap();
        let before = store
            .read_head_cache(ino, 0, HEAD_CACHE_BYTES)
            .unwrap()
            .unwrap();

        // 再 append 一段（落在末块之后，不触块0）。
        let more = patterned(50 * 1024);
        rmw::write_at(&store, ino, 200 * 1024, &more, &codec_params()).unwrap();
        store.fsync(ino).unwrap();

        let after = store
            .read_head_cache(ino, 0, HEAD_CACHE_BYTES)
            .unwrap()
            .unwrap();
        assert_eq!(
            before.0, after.0,
            "append 后 head 缓存字节应不变（块0 未动）"
        );
        let plain = decompress(&after.0, Algo::Zstd, after.1).unwrap();
        assert_eq!(&plain[..], &data[..HEAD_CACHE_BYTES as usize]);
    }

    // ----- 时间戳：读路径透传底层真实 mtime + setattr 写回（修复挂载点全 1970） -----

    /// 一个非 epoch 的确定性参照时间（2026-06-24 04:47:00 UTC 附近）。
    fn ref_time() -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(1_750_740_420, 123_456_789)
    }

    #[test]
    fn getattr_propagates_backing_mtime_not_epoch() {
        let (store, _dir, ino) = store_with_file(64 * 1024);
        let abs = store.abs_of_ino(ino).unwrap();
        // 直接给底层文件盖一个已知 mtime（atime 同步设），模拟真实会话文件。
        set_file_times(&abs, ref_time(), ref_time()).unwrap();

        let a = store.getattr_ino(ino).unwrap();
        assert_ne!(
            a.mtime,
            std::time::SystemTime::UNIX_EPOCH,
            "mtime 不应退化为 1970"
        );
        // 允许文件系统纳秒精度差异，按秒比较。
        let got = a
            .mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        assert_eq!(got.as_secs(), 1_750_740_420);
    }

    #[test]
    fn setattr_writes_mtime_back_to_backing() {
        let (store, _dir, ino) = store_with_file(64 * 1024);
        let mut a = store.getattr_ino(ino).unwrap();
        a.mtime = ref_time();
        a.atime = ref_time();
        store.setattr(ino, a).unwrap();

        // 重新 getattr：mtime 应反映写回值（往返一致）。
        let a2 = store.getattr_ino(ino).unwrap();
        let secs = a2
            .mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, 1_750_740_420, "setattr 写回的 mtime 应被持久化");
        // 底层文件本身也应同步（不止内存视图）。
        let abs = store.abs_of_ino(ino).unwrap();
        let backing_secs = fs::metadata(&abs).unwrap().mtime();
        assert_eq!(backing_secs, 1_750_740_420);
    }
}
