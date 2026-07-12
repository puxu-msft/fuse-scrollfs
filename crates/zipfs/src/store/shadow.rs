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

use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
#[derive(Clone, Debug, Default)]
enum HeadCacheUpdate {
    #[default]
    Keep,
    Set(Vec<u8>, bool, u64),
    Clear,
}

#[derive(Clone, Debug, Default)]
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
    head_cache: HeadCacheUpdate,
}

impl WriteSession {
    /// commit 前失败回滚：把旧 flushing 会话并回当前 active 会话。active 是在 flushing 可见期间
    /// 基于其逻辑几何产生的新操作，故 size 与同 idx 块优先；active 截断范围外的旧块不得复活。
    fn merge_from_flushing(&mut self, flushing: WriteSession) {
        let active_truncate = self.truncate_to;
        for (idx, block) in flushing.dirty {
            if active_truncate.is_none_or(|keep_from| idx < keep_from) {
                self.dirty.entry(idx).or_insert(block);
            }
        }
        if self.truncate_to.is_none() {
            self.truncate_to = flushing.truncate_to;
        }
        // HeadCacheUpdate 是操作而非可独立拼接的字段：active 显式 Set/Clear 一律优先；仅 Keep
        // 可继承 flushing。active 改块 0 时 put_block 会转为 Clear，避免旧前缀与最终块 0 不一致。
        if matches!(self.head_cache, HeadCacheUpdate::Keep) {
            self.head_cache = match flushing.head_cache {
                HeadCacheUpdate::Set(_, _, rawlen) if self.size < rawlen => HeadCacheUpdate::Clear,
                update => update,
            };
        }
    }
}

#[derive(Default)]
struct SessionBuffers {
    active: HashMap<u64, WriteSession>,
    flushing: HashMap<u64, WriteSession>,
}

/// 写会话提交失败发生在 durable commit 点之前还是之后。commit 前失败必须恢复会话；commit 后仅
/// `up.sync()` 失败时新版本已经 durable，不得恢复会话，否则下次 fsync 会重复追加。
enum CommitSessionError {
    BeforeCommit(io::Error),
    AfterCommit(io::Error),
}

/// 影子树后端（布局 S）。`backing` 为底层目录根（archive 树）。
pub struct ShadowStore {
    backing: PathBuf,
    /// backing 的跨进程排他锁（Bug A）。RAII 持有到 drop：守护退出（含 SIGKILL）时
    /// 内核自动释放，故第二个守护 open 同一 backing 必失败，杜绝双守护并发覆盖。
    /// 锁文件是 backing 外的 sibling `<backing>.zipfs.lock`，不进 readdir/写路径。
    _lock: std::fs::File,
    /// 命名空间锁（阶段 D3）：**仅** create/mkdir/unlink/rmdir/rename/symlink 在方法最开头
    /// 全程持有，把「查存在 → syscall → 改 inodes/sessions/readers 三表」整段串行化为原子。
    /// 数据路径（get_block/put_block/lookup/getattr/readdir/append_tail/seal/flush 等 by-ino、
    /// 不改命名空间）**不取** ns，故高频 get_block 不被慢目录 syscall 串行化。
    ///
    /// **锁序（严格遵守，防死锁）**：`ns < inodes < sessions < readers`。即持 ns 时才可再取
    /// 细锁；任何细锁路径**不得**反过来取 ns（数据路径不取 ns，自然满足）。目录方法持 ns 期间
    /// 调用的辅助（`rel_of`/`abs_of_ino`/`invalidate_reader` 等）只取细锁，与锁序一致。
    ns: Mutex<()>,
    inodes: Mutex<InodeMap>,
    /// 写会话双缓冲。active 接收新写；flushing 在 commit IO 期间继续供读路径查询。
    sessions: Mutex<SessionBuffers>,
    /// 串行化 archive 提交。与 sessions 分离，IO 期间不持 sessions 锁；全局锁比 per-inode 锁简单，
    /// 并同时保护直接改 archive 的 tail journal / seal 路径不与普通 commit 交错。
    commit_lock: Mutex<()>,
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
    /// 统一指标注册表（全 crate 共享 `Arc`）。默认自建私有实例；`with_metrics` 注入共享实例，
    /// `run_mount` 用其把 shadow 后端埋点接进统一 `.prom` 出口。埋点均为无锁 `Relaxed` 自增，
    /// 不参与任何锁序、不改控制流，与死锁不变量正交（见结构体各锁注释）。
    metrics: Arc<crate::core::metrics::Metrics>,
    /// 故障注入（docs/05 §4，仅 test/feature）：非零则下次 `commit_session` 走 `FaultIo`，
    /// 并令指定序号的 sync 返 EIO。1 = commit 内 barrier1；3 = 末尾 `up.sync()`。
    #[cfg(any(test, feature = "fault-injection"))]
    fault_commit_sync_at: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    last_fault_durable: Mutex<Option<Vec<u8>>>,
    #[cfg(test)]
    commit_pause: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
}

/// backing 的锁文件路径由 [`super::lock::backing_lock_path`] 统一提供（守护 open 与离线
/// compact/seal 共用同一互斥域真值，评审 A3）。
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
        // Bug A：取 backing 的跨进程排他锁，挡住第二个守护并发持有同一 backing。
        // 锁路径由 `lock::backing_lock_path` 统一计算（与离线 compact/seal 同一互斥域，评审 A3）。
        let lock = super::lock::acquire_backing(&backing)?;
        Ok(Self {
            backing,
            _lock: lock,
            ns: Mutex::new(()),
            inodes: Mutex::new(InodeMap::new()),
            sessions: Mutex::new(SessionBuffers::default()),
            commit_lock: Mutex::new(()),
            readers: Mutex::new(HashMap::new()),
            reader_epoch: AtomicU64::new(0),
            default_chunk_size,
            metrics: crate::core::metrics::Metrics::new(),
            #[cfg(any(test, feature = "fault-injection"))]
            fault_commit_sync_at: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            last_fault_durable: Mutex::new(None),
            #[cfg(test)]
            commit_pause: Mutex::new(None),
        })
    }

    /// 链式注入共享指标注册表（全 crate 单一 `Arc<Metrics>`）。默认自建私有实例，
    /// `run_mount` 用本方法把 shadow 后端埋点接进统一 `.prom` 出口（与 container 对称）。
    pub fn with_metrics(mut self, m: Arc<crate::core::metrics::Metrics>) -> Self {
        self.metrics = m;
        self
    }

    fn abs_of(&self, rel: &Path) -> PathBuf {
        self.backing.join(rel)
    }

    fn rel_of(&self, ino: Ino) -> Option<PathBuf> {
        self.inodes.lock().path_of(ino)
    }

    fn abs_of_ino(&self, ino: Ino) -> Option<PathBuf> {
        self.rel_of(ino).map(|rel| self.abs_of(&rel))
    }

    /// 由底层 metadata + 相对路径构造 Store 层 `Attr`。普通文件 size 取逻辑大小
    /// （优先脏会话内大小，其次 archive footer），目录用底层 size。
    fn attr_from_meta(&self, ino: Ino, meta: &fs::Metadata, abs: &Path) -> Attr {
        let kind = filetype_from_meta(meta);
        let (size, chunk_size) = if kind == fuser::FileType::RegularFile {
            // 脏会话优先（写后读一致）。**先快照再放锁**：绝不在持 `sessions` 时调
            // `read_footer_geometry`（含 `ArchiveReader::open` 阻塞 IO）——否则每次冷元数据读都
            // 把全局 `sessions` 锁按住一次 archive 解析，饿死所有并发写者。
            let dirty = {
                let sessions = self.sessions.lock();
                sessions
                    .active
                    .get(&ino)
                    .or_else(|| sessions.flushing.get(&ino))
                    .map(|s| (s.size, s.chunk_size))
            };
            match dirty {
                Some(geom) => geom,
                None => read_footer_geometry(abs)
                    .unwrap_or_else(|| (meta.size(), self.default_chunk_size)),
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

    /// 取 `ino` 写会话的可变引用并执行 `f`，返回其结果。**双检懒建**（性能）：会话已存在——
    /// 即热路径主体（文件首次写后会话一直在，直到 fsync/commit 清空）——时只锁一次 `sessions`，
    /// **不碰 `inodes`、不算 `abs`、不堆分配**；仅首次写（Vacant）才付 `abs`：放 `sessions` 后取
    /// `inodes` 算好，再重锁 `sessions` 用 `entry` 懒建（处理并发插入竞态）。
    ///
    /// **锁序纪律**：慢路径先释 `sessions` 再取 `inodes`（算 abs），全程绝不同持两把 store 锁
    /// （死锁不变量，见 [`Self::ensure_session`]）。`f` 只做内存脏缓冲改动，**绝不在其内做 IO /
    /// 取其它锁**（它在持 `sessions` 时运行）。
    fn with_session<R>(&self, ino: Ino, f: impl FnOnce(&mut WriteSession) -> R) -> io::Result<R> {
        // 快路径：会话已存在，单锁 sessions，零 inodes 流量、零分配。
        {
            let mut sessions = self.sessions.lock();
            if let Some(s) = sessions.active.get_mut(&ino) {
                return Ok(f(s));
            }
        }
        // 慢路径（首次写）：sessions 已释，算 abs（inodes 锁），再重锁 sessions 懒建。
        let abs = self
            .abs_of_ino(ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ino 无映射"))?;
        let mut sessions = self.sessions.lock();
        if !sessions.active.contains_key(&ino) {
            let session = sessions.flushing.get(&ino).cloned().unwrap_or_else(|| {
                let (size, chunk_size) =
                    read_footer_geometry(&abs).unwrap_or((0, self.default_chunk_size));
                WriteSession {
                    size,
                    chunk_size,
                    ..WriteSession::default()
                }
            });
            sessions.active.insert(
                ino,
                WriteSession {
                    // flushing 已由三层读路径提供旧块；active 只记录更新操作，避免失败合并时
                    // 把复制的旧状态误判成新写。但几何必须继承 flushing，保证 new_size 基于最新状态。
                    dirty: HashMap::new(),
                    size: session.size,
                    chunk_size: session.chunk_size,
                    truncate_to: None,
                    head_cache: HeadCacheUpdate::Keep,
                },
            );
        }
        Ok(f(sessions.active.get_mut(&ino).expect("active 已插入")))
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
        if let Some(r) = self.readers.lock().get(&ino) {
            self.metrics.record_reader_hit();
            return Ok(Some(r.clone()));
        }
        // 未命中：即将打开并解析一个新 reader。记 miss 落在 readers 锁作用域**之外**（上面 if 块
        // 已随其守卫 drop 释放锁），与「埋点不落持锁段」纪律一致——虽原子自增本身 lock-free。
        self.metrics.record_reader_miss();
        let epoch_before = self.reader_epoch.load(Ordering::Acquire);
        let Some(abs) = self.abs_of_ino(ino) else {
            return Ok(None);
        };
        let reader = match ArchiveReader::open(&abs) {
            Ok(r) => Arc::new(r),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut cache = self.readers.lock();
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
        self.readers.lock().remove(&ino);
    }

    /// 把某 ino 的 active 会话移入 flushing 后提交。`commit_lock` 串行化 archive 修改；IO 期间
    /// 不持 `sessions` 锁，读路径仍从 flushing 看见该代。commit 未确认时合并回 active，已确认后清除。
    fn commit_session(&self, ino: Ino) -> io::Result<()> {
        let _commit_guard = self.commit_lock.lock();
        let session = {
            let mut sessions = self.sessions.lock();
            let Some(session) = sessions.active.remove(&ino) else {
                drop(sessions);
                // 无脏数据：仍对底层文件 fsync（POSIX fsync 语义）。
                if let Some(abs) = self.abs_of_ino(ino) {
                    if let Ok(f) = fs::File::open(&abs) {
                        f.sync_all()?;
                    }
                }
                return Ok(());
            };
            debug_assert!(!sessions.flushing.contains_key(&ino));
            sessions.flushing.insert(ino, session.clone());
            session
        };

        #[cfg(test)]
        if let Some((entered, resume)) = self.commit_pause.lock().take() {
            entered.wait();
            resume.wait();
        }

        let result = (|| {
            let abs = self.abs_of_ino(ino).ok_or_else(|| {
                CommitSessionError::BeforeCommit(io::Error::new(
                    io::ErrorKind::NotFound,
                    "ino 无映射",
                ))
            })?;

            #[cfg(any(test, feature = "fault-injection"))]
            {
                let fail_sync_at = self
                    .fault_commit_sync_at
                    .swap(0, std::sync::atomic::Ordering::AcqRel);
                if fail_sync_at != 0 {
                    let bytes = fs::read(&abs).map_err(CommitSessionError::BeforeCommit)?;
                    let fio = crate::blockio::FaultIo::from_bytes(bytes);
                    fio.fail_sync_in(fail_sync_at);
                    let mut up = ArchiveUpdater::from_io(fio.clone())
                        .map_err(CommitSessionError::BeforeCommit)?;
                    let result = self.commit_with_updater(ino, &session, &mut up);
                    #[cfg(test)]
                    {
                        *self.last_fault_durable.lock() = Some(fio.durable_bytes());
                    }
                    return result;
                }
            }

            let mut up = ArchiveUpdater::open(&abs).map_err(CommitSessionError::BeforeCommit)?;
            self.commit_with_updater(ino, &session, &mut up)
        })();

        let mut sessions = self.sessions.lock();
        let flushing = sessions
            .flushing
            .remove(&ino)
            .expect("当前提交代必须仍在 flushing");
        match result {
            Ok(()) => Ok(()),
            Err(CommitSessionError::BeforeCommit(error)) => {
                match sessions.active.entry(ino) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().merge_from_flushing(flushing);
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(flushing);
                    }
                }
                Err(error)
            }
            Err(CommitSessionError::AfterCommit(error)) => Err(error),
        }
    }

    /// 把脏会话应用到 updater 并提交：截断 → 升序应用脏块 → head 缓存 → `commit` → **失效 reader
    /// 缓存** → `up.sync()`。泛型于 `BlockIo`（生产为 `File`，故障注入为 `FaultIo`）。
    /// **失效点必须在 `up.sync()` 之前**：即便 sync 失败提前返回，盘上已是新版本（commit 内部 barrier
    /// 已落），缓存也不残留旧 reader（rust-review L3）。
    fn commit_with_updater<W: BlockIo>(
        &self,
        ino: Ino,
        session: &WriteSession,
        up: &mut ArchiveUpdater<W>,
    ) -> Result<(), CommitSessionError> {
        // 先截断（若有）。
        if let Some(keep_from) = session.truncate_to {
            up.truncate(keep_from, session.size);
        }
        // 按 idx 升序应用脏块（set_block 不允许空洞，须连续）。
        let mut idxs: Vec<u64> = session.dirty.keys().copied().collect();
        idxs.sort_unstable();
        for idx in idxs {
            let blk = &session.dirty[&idx];
            up.set_block(idx, &blk.bytes, blk.stored_verbatim, session.size)
                .map_err(CommitSessionError::BeforeCommit)?;
        }
        // 本会话若更新了 head 缓存（Core 在块 0 封块时交来），写入 updater；否则 updater 沿用
        // open 时从盘上 footer 载入的既有缓存（块 0 未变时保持有效）。docs/02 §4.3。
        match &session.head_cache {
            HeadCacheUpdate::Keep => {}
            HeadCacheUpdate::Set(bytes, verbatim, rawlen) => {
                up.set_head_cache(bytes.clone(), *verbatim, *rawlen);
            }
            HeadCacheUpdate::Clear => up.clear_head_cache(),
        }
        up.commit().map_err(CommitSessionError::BeforeCommit)?;
        // 一个脏会话已真正提交（commit 内部 barrier 已落新 footer/index）。埋点放在 commit 成功之后、
        // invalidate_reader 之前——纯 Relaxed 原子自增，不改锁序、不改控制流，绝不影响
        // 「invalidate_reader 先于 up.sync()」这一 durability 不变量的相对顺序。
        self.metrics.record_shadow_commit();
        // 底层 archive 已变更（commit 内部已 sync 落新 footer/index）。在 up.sync() 之前就失效缓存：
        // 即便随后的 sync() 失败提前返回，盘上已是新版本，缓存也不会残留旧 reader（rust-review L3）。
        self.invalidate_reader(ino);
        up.sync().map_err(CommitSessionError::AfterCommit)?;
        Ok(())
    }

    /// 故障注入（任务 2.6，仅 test/feature）：令下一次 `commit_session` 走 FaultIo 并使末尾
    /// `up.sync()` 返 EIO。`pub` 以便 feature 构建下集成测试亦可调用（与导出的 `FaultIo` 一致）。
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn fault_next_commit_sync(&self) {
        self.fault_commit_sync_at
            .store(3, std::sync::atomic::Ordering::Release);
    }

    /// 故障注入（仅 test/feature）：令下一次 `commit_session` 的 commit barrier1 返 EIO。
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn fault_next_commit_barrier1(&self) {
        self.fault_commit_sync_at
            .store(1, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    fn test_pause_next_commit(&self) -> (Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *self.commit_pause.lock() = Some((entered.clone(), resume.clone()));
        (entered, resume)
    }

    #[cfg(test)]
    fn test_move_active_to_flushing(&self, ino: Ino) {
        let mut sessions = self.sessions.lock();
        let session = sessions
            .active
            .remove(&ino)
            .expect("测试前置：active 会话存在");
        sessions.flushing.insert(ino, session);
    }

    #[cfg(test)]
    fn test_fail_flushing_commit(&self, ino: Ino) {
        let mut sessions = self.sessions.lock();
        let flushing = sessions
            .flushing
            .remove(&ino)
            .expect("测试前置：flushing 会话存在");
        match sessions.active.entry(ino) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().merge_from_flushing(flushing);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(flushing);
            }
        }
    }
}

/// 读 archive footer 取 (uncompressed_size, chunk_size)；非 archive / 打不开则 None。
fn read_footer_geometry(abs: &Path) -> Option<(u64, u32)> {
    ArchiveReader::open(abs)
        .ok()
        .map(|r| (r.footer().uncompressed_size, r.footer().chunk_size))
}

/// 把 atime/mtime 落到底层文件，见 [`crate::core::set_file_times`]（共享时间写入原语）。
use crate::core::set_file_times;

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
        let ino = self.inodes.lock().intern(child_rel);
        Some(self.attr_from_meta(ino, &meta, &abs))
    }

    fn create(&self, parent: Ino, name: &str, attr: Attr) -> io::Result<Ino> {
        // ns 锁全程持有：使「O_EXCL 建文件 → fsync → 改 inodes 表」对同目录并发原子（阶段 D3）。
        let _ns = self.ns.lock();
        super::validate_name(name)?;
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
        // O_EXCL 排他新建（阶段 D3）：并发同名 create 时内核保证恰一个成功、其余 AlreadyExists
        // （映射 EEXIST），绝不「双成功 + 后者 O_TRUNC 截断前者」。覆盖语义的离线工具仍走 create。
        let w = ArchiveWriter::create_new(&abs, chunk_size)?;
        let f = w.finish()?;
        f.sync_all()?;
        // 评审 A2：fsync 文件本身不保证 dentry durable，崩溃后新文件可能整体消失（后续
        // commit_session 的 open 会失败）。与 seal/compact 的 rename 后 fsync 父目录一致。
        crate::core::fsync_dir_of(&abs);
        // 应用权限。失败不致命（文件已建好）但不可静默吞——记日志（用户规则：不静默吞错误）。
        if let Err(e) = fs::set_permissions(&abs, fs::Permissions::from_mode(attr.perm as u32)) {
            log::warn!("create：设置 {} 权限失败：{e}", abs.display());
        }
        let ino = self.inodes.lock().intern(child_rel);
        Ok(ino)
    }

    fn mkdir(&self, parent: Ino, name: &str, attr: Attr) -> io::Result<Ino> {
        let _ns = self.ns.lock();
        super::validate_name(name)?;
        let parent_rel = self
            .rel_of(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "父目录不存在"))?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        fs::create_dir(&abs)?;
        if let Err(e) = fs::set_permissions(&abs, fs::Permissions::from_mode(attr.perm as u32)) {
            log::warn!("mkdir：设置 {} 权限失败：{e}", abs.display());
        }
        let ino = self.inodes.lock().intern(child_rel);
        Ok(ino)
    }

    fn unlink(&self, parent: Ino, name: &str) -> io::Result<()> {
        // ns 锁全程持有：使「remove_file → 清 sessions/readers/inodes 三表」原子（阶段 D3）。
        let _ns = self.ns.lock();
        let _commit_guard = self.commit_lock.lock();
        let parent_rel = self
            .rel_of(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "父目录不存在"))?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        fs::remove_file(&abs)?;
        // 丢弃可能残留的写会话 + 缓存 reader + 映射项。
        // **锁序纪律**：把 `by_path` 查询绑到 `let`，令 `inodes` 守卫在语句末即 drop——绝不在持
        // `inodes` 时取 `sessions`（否则与数据路径 put_block 的 sessions→inodes 构成 AB-BA）。
        let victim = self.inodes.lock().by_path.get(&child_rel).copied();
        if let Some(ino) = victim {
            {
                let mut sessions = self.sessions.lock();
                sessions.active.remove(&ino);
                sessions.flushing.remove(&ino);
            }
            self.invalidate_reader(ino);
        }
        self.inodes.lock().remove_path(&child_rel);
        Ok(())
    }

    fn rmdir(&self, parent: Ino, name: &str) -> io::Result<()> {
        let _ns = self.ns.lock();
        let parent_rel = self
            .rel_of(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "父目录不存在"))?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        fs::remove_dir(&abs)?;
        self.inodes.lock().remove_path(&child_rel);
        Ok(())
    }

    fn rename(&self, old: (Ino, &str), new: (Ino, &str)) -> io::Result<()> {
        // ns 锁全程持有：overwritten_ino 快照与 fs::rename 同临界区取得（消除快照过期），
        // 「失效 victim 三表 → rename_path」整段原子（阶段 D3）。
        let _ns = self.ns.lock();
        let _commit_guard = self.commit_lock.lock();
        super::validate_name(old.1)?;
        super::validate_name(new.1)?;
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
        let overwritten_ino = self.inodes.lock().by_path.get(&new_rel).copied();
        fs::rename(self.abs_of(&old_rel), self.abs_of(&new_rel))?;
        if let Some(victim) = overwritten_ino {
            {
                let mut sessions = self.sessions.lock();
                sessions.active.remove(&victim);
                sessions.flushing.remove(&victim);
            }
            self.invalidate_reader(victim);
            self.inodes.lock().remove_path(&new_rel);
        }
        self.inodes.lock().rename_path(&old_rel, &new_rel);
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
            let ino = self.inodes.lock().intern(child_rel);
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
        let _ns = self.ns.lock();
        super::validate_name(name)?;
        let parent_rel = self
            .rel_of(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "父目录不存在"))?;
        let child_rel = parent_rel.join(name);
        let abs = self.abs_of(&child_rel);
        std::os::unix::fs::symlink(target, &abs)?;
        let meta = fs::symlink_metadata(&abs)?;
        let ino = self.inodes.lock().intern(child_rel);
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
        // 1) read-through 双缓冲：active 更新优先，随后 flushing，最后 archive。
        {
            let sessions = self.sessions.lock();
            for s in [sessions.active.get(&ino), sessions.flushing.get(&ino)]
                .into_iter()
                .flatten()
            {
                if let Some(blk) = s.dirty.get(&idx) {
                    return Ok(Some(blk.clone()));
                }
                if s.truncate_to.is_some_and(|keep_from| idx >= keep_from) {
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
        // active 优先于 flushing。
        {
            let sessions = self.sessions.lock();
            if let Some(s) = sessions
                .active
                .get(&ino)
                .or_else(|| sessions.flushing.get(&ino))
            {
                return Some((s.size, s.chunk_size));
            }
        }
        // 经缓存 reader 取 footer 几何，避免每次 read 都重开 archive（rwfs::read_range 每读一次）。
        let reader = self.cached_reader(ino).ok().flatten()?;
        let f = reader.footer();
        Some((f.uncompressed_size, f.chunk_size))
    }

    fn put_block(&self, ino: Ino, idx: u64, blk: StoredBlock, new_size: u64) -> io::Result<()> {
        // 双检懒建（性能）：会话已存在则单锁 sessions；仅首次写才付 abs（inodes 锁）。
        // 锁序：绝不持 `sessions` 取 `inodes`（见 with_session）。
        self.with_session(ino, |s| {
            s.dirty.insert(idx, blk);
            s.size = new_size;
            if idx == 0 {
                s.head_cache = HeadCacheUpdate::Clear;
            }
        })
    }

    fn truncate_blocks(&self, ino: Ino, keep_from: u64, new_size: u64) -> io::Result<()> {
        self.with_session(ino, |s| {
            // 丢弃脏块中 >= keep_from 的，并记录截断点（提交时一并应用到底层）。
            s.dirty.retain(|&i, _| i < keep_from);
            s.truncate_to = Some(match s.truncate_to {
                Some(prev) => prev.min(keep_from),
                None => keep_from,
            });
            s.size = new_size;
            if keep_from == 0 {
                s.head_cache = HeadCacheUpdate::Clear;
            }
        })
    }

    // ---- in-archive 尾日志（写放大根治，docs/04 §8.4）----
    fn supports_tail_journal(&self) -> bool {
        true
    }

    /// fsync 路径：把未封尾块原始增量追加进 archive 尾日志并 durable。开 updater →
    /// append_journal(delta) → commit_journal（不重写 index、双段 barrier）。绕开脏会话直接落盘，
    /// 故 reader 缓存随之失效（盘上 SB/journal 已变）。
    fn append_tail(&self, ino: Ino, delta: &[u8], new_size: u64) -> io::Result<()> {
        let _commit_guard = self.commit_lock.lock();
        let abs = self
            .abs_of_ino(ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ino 无映射"))?;
        let mut up = ArchiveUpdater::open(&abs)?;
        up.append_journal(delta)?;
        up.set_size(new_size);
        up.commit_journal()?;
        self.invalidate_reader(ino);
        self.metrics.record_tail_append();
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
        let _commit_guard = self.commit_lock.lock();
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
        self.with_session(ino, |s| {
            s.head_cache = HeadCacheUpdate::Set(stored_bytes, verbatim, rawlen);
        })
    }

    fn read_head_cache(&self, ino: Ino, off: u64, len: u64) -> io::Result<Option<(Vec<u8>, bool)>> {
        // 有挂起写会话时跳过快路径：脏块 0 可能与盘上 head 缓存不一致（如块 0 刚被 RMW 尚未
        // 提交）→ 回退逐块路径（get_block read-through 脏块）。fsync 后会话移除、快路径恢复。
        {
            let sessions = self.sessions.lock();
            if sessions.active.contains_key(&ino) || sessions.flushing.contains_key(&ino) {
                return Ok(None);
            }
        }
        let Some(reader) = self.cached_reader(ino)? else {
            return Ok(None);
        };
        let rawlen = reader.head_cache_rawlen();
        // 仅当请求区间 [off, off+len) 完全落在缓存覆盖前缀内才命中（避免部分命中拼接）。
        // 评审 B2：覆盖前缀额外 clamp 到当前逻辑文件大小——纵深防御，即便陈旧缓存（rawlen >
        // 实际大小）漏过 B1 的失效，也绝不返回超过 EOF 的旧字节。
        let effective = rawlen.min(reader.footer().uncompressed_size);
        let covered = effective > 0
            && off
                .checked_add(len)
                .map(|end| end <= effective)
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
        let inos: Vec<u64> = self.sessions.lock().active.keys().copied().collect();
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
    fn open_second_on_same_backing_rejected_by_lock() {
        // Bug A：两个守护同时持有同一 shadow backing 是数据损坏直接成因
        // （孤儿守护用空视图周期性覆盖）。open 须取跨进程排他锁，第二个 open 失败。
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("proj");
        std::fs::create_dir(&backing).unwrap();
        let s1 = ShadowStore::open_with_chunk_size(backing.clone(), 65536).unwrap();
        let s2 = ShadowStore::open_with_chunk_size(backing.clone(), 65536);
        assert!(
            s2.is_err(),
            "第二个守护 open 同一 backing 应被排他锁拒绝（防双守护覆盖）"
        );
        drop(s1); // 释放锁（drop / 进程退出含 SIGKILL 内核自动释放）
        let s3 = ShadowStore::open_with_chunk_size(backing.clone(), 65536);
        assert!(s3.is_ok(), "锁释放后应可重新 open");
    }

    #[test]
    fn lock_file_lives_outside_backing() {
        // 锁文件必须放 backing 外 sibling，否则被 readdir 暴露成幽灵文件、
        // 被 compact/seal/ingest 误当数据（与 .zipfs.meta 同理）。
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("proj");
        std::fs::create_dir(&backing).unwrap();
        let _s = ShadowStore::open_with_chunk_size(backing.clone(), 65536).unwrap();
        assert!(
            !backing.join(".zipfs.lock").exists(),
            "lock 绝不能在 backing 内（否则被 readdir 暴露）"
        );
        assert!(
            dir.path().join("proj.zipfs.lock").exists(),
            "lock 应在 backing 同级 sibling: proj.zipfs.lock"
        );
    }

    #[test]
    fn inode_map_root_is_1_path_empty() {
        let m = InodeMap::new();
        assert_eq!(m.path_of(ROOT_INO), Some(PathBuf::new()));
    }

    #[test]
    fn intern_same_path_reuses_ino() {
        let mut m = InodeMap::new();
        let a = m.intern(PathBuf::from("a/b.txt"));
        let b = m.intern(PathBuf::from("a/b.txt"));
        assert_eq!(a, b);
        let c = m.intern(PathBuf::from("a/c.txt"));
        assert_ne!(a, c);
        assert_eq!(m.path_of(a), Some(PathBuf::from("a/b.txt")));
    }

    #[test]
    fn rename_path_keeps_ino_stable() {
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

    #[test]
    fn failed_session_merge_preserves_newer_writes_and_fills_missing_state() {
        let mut failed = WriteSession {
            size: 16,
            chunk_size: 8,
            truncate_to: Some(2),
            head_cache: HeadCacheUpdate::Set(b"old-head".to_vec(), false, 8),
            ..WriteSession::default()
        };
        failed.dirty.insert(0, mk_block(b"OLD00000"));
        failed.dirty.insert(1, mk_block(b"OLD11111"));

        let mut active = WriteSession {
            size: 24,
            chunk_size: 8,
            truncate_to: Some(3),
            head_cache: HeadCacheUpdate::Set(b"new-head".to_vec(), true, 8),
            ..WriteSession::default()
        };
        active.dirty.insert(1, mk_block(b"NEW11111"));
        active.merge_from_flushing(failed);

        assert_eq!(active.size, 24, "并发新会话的 size 优先");
        assert_eq!(active.truncate_to, Some(3), "并发新截断意图优先");
        assert!(
            matches!(&active.head_cache, HeadCacheUpdate::Set(bytes, true, 8) if bytes == b"new-head"),
            "并发新 head cache 优先"
        );
        let block = &active.dirty[&1];
        assert_eq!(
            decompress(&block.bytes, Algo::Zstd, block.stored_verbatim).unwrap(),
            b"NEW11111",
            "同 idx 的并发新块不得被失败会话覆盖"
        );
        assert!(active.dirty.contains_key(&0), "失败会话独有的块必须补回");

        let mut empty_active = WriteSession {
            size: 16,
            chunk_size: 8,
            ..WriteSession::default()
        };
        let failed = WriteSession {
            size: 16,
            chunk_size: 8,
            truncate_to: Some(2),
            head_cache: HeadCacheUpdate::Set(b"old-head".to_vec(), false, 8),
            ..WriteSession::default()
        };
        empty_active.merge_from_flushing(failed);
        assert_eq!(empty_active.truncate_to, Some(2));
        assert!(matches!(
            empty_active.head_cache,
            HeadCacheUpdate::Set(bytes, false, 8) if bytes == b"old-head"
        ));
    }

    #[test]
    fn concurrent_fsync_waits_while_first_commit_is_in_flushing_io() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);
        let store = Arc::new(store);
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        let (entered, resume) = store.test_pause_next_commit();

        let first = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || store.fsync(ino))
        };
        entered.wait();
        assert!(store.sessions.lock().flushing.contains_key(&ino));
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..])
        );

        store.put_block(ino, 0, mk_block(b"BBBBBBBB"), 8).unwrap();
        let second_started = Arc::new(std::sync::Barrier::new(2));
        let second_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second = {
            let store = Arc::clone(&store);
            let started = Arc::clone(&second_started);
            let done = Arc::clone(&second_done);
            std::thread::spawn(move || {
                started.wait();
                let result = store.fsync(ino);
                done.store(true, std::sync::atomic::Ordering::Release);
                result
            })
        };
        second_started.wait();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            !second_done.load(std::sync::atomic::Ordering::Acquire),
            "第二个同 inode fsync 必须被 commit_lock 阻塞"
        );

        resume.wait();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        let reader = ArchiveReader::open(&store.abs_of_ino(ino).unwrap()).unwrap();
        let (bytes, entry) = reader.read_block(0).unwrap().unwrap();
        assert_eq!(
            decompress(&bytes, Algo::Zstd, entry.is_verbatim()).unwrap(),
            b"BBBBBBBB"
        );
    }

    #[test]
    fn flushing_interleaving_is_visible_and_failure_merge_preserves_operation_order() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.put_block(ino, 1, mk_block(b"BBBBBBBB"), 16).unwrap();

        // 确定性交错 T1：提交线程已把 active 移入 flushing，但尚未开始 archive IO。
        store.test_move_active_to_flushing(ino);
        assert_eq!(
            read_plain(&store, ino, 1).as_deref(),
            Some(&b"BBBBBBBB"[..])
        );
        assert_eq!(store.block_geometry(ino), Some((16, cs)));
        assert!(store.read_head_cache(ino, 0, 1).unwrap().is_none());

        // T2：并发新操作基于 flushing 几何建 active，并截断掉块 1。
        store.truncate_blocks(ino, 1, 8).unwrap();
        // T3：另一个 fsync 会被 commit_lock 串行；这里确定性模拟 T1 commit 前失败后的协调锁合并。
        store.test_fail_flushing_commit(ino);
        assert!(
            store.get_block(ino, 1).unwrap().is_none(),
            "较新 truncate 不得复活旧高位块"
        );
        assert_eq!(store.block_geometry(ino), Some((8, cs)));
        store.fsync(ino).unwrap();
        assert!(ArchiveReader::open(&store.abs_of_ino(ino).unwrap())
            .unwrap()
            .read_block(1)
            .unwrap()
            .is_none());
    }

    #[test]
    fn failed_extension_then_active_overwrite_keeps_flushing_geometry() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.fsync(ino).unwrap();
        store.put_block(ino, 1, mk_block(b"BBBBBBBB"), 16).unwrap();
        store.test_move_active_to_flushing(ino);

        // active 首次建立必须继承 flushing size=16，而不是旧 archive size=8。
        store.put_block(ino, 0, mk_block(b"ZZZZZZZZ"), 16).unwrap();
        store.test_fail_flushing_commit(ino);
        assert_eq!(store.block_geometry(ino), Some((16, cs)));
        store.fsync(ino).unwrap();
        let reader = ArchiveReader::open(&store.abs_of_ino(ino).unwrap()).unwrap();
        assert_eq!(reader.footer().uncompressed_size, 16);
        assert!(reader.read_block(1).unwrap().is_some());
    }

    #[test]
    fn active_block0_overwrite_clears_flushing_head_cache() {
        let mut flushing = WriteSession {
            size: 16,
            chunk_size: 8,
            head_cache: HeadCacheUpdate::Set(b"OLD-HEAD".to_vec(), true, 8),
            ..WriteSession::default()
        };
        flushing.dirty.insert(0, mk_block(b"OLD00000"));
        let mut active = WriteSession {
            size: 16,
            chunk_size: 8,
            head_cache: HeadCacheUpdate::Clear,
            ..WriteSession::default()
        };
        active.dirty.insert(0, mk_block(b"NEW00000"));
        active.merge_from_flushing(flushing);
        assert!(matches!(active.head_cache, HeadCacheUpdate::Clear));
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
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let store = ShadowStore::open_with_chunk_size(backing, chunk_size).unwrap();
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
    fn reader_cache_write_then_read_visible_after_commit_and_cache_hit() {
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
            store.readers.lock().contains_key(&ino),
            "首次读后应缓存 reader"
        );
        // 再读应命中同一缓存（内容仍正确）。
        assert_eq!(read_plain(&store, ino, 1).as_deref(), Some(&b"BBBB"[..]));
        assert_eq!(store.block_geometry(ino), Some((12, cs)));
    }

    #[test]
    fn reader_cache_invalidated_after_commit_no_stale_read() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);

        // 第一次写 + 提交 + 读（填充缓存）。
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.fsync(ino).unwrap();
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..])
        );
        assert!(store.readers.lock().contains_key(&ino));

        // 第二次写覆盖块0，提交：提交应淘汰缓存。
        store.put_block(ino, 0, mk_block(b"ZZZZZZZZ"), 8).unwrap();
        store.fsync(ino).unwrap();
        assert!(
            !store.readers.lock().contains_key(&ino),
            "提交后缓存 reader 必须失效"
        );
        // 再读必须看到新数据（绝不读陈旧 footer/index）。
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"ZZZZZZZZ"[..])
        );
    }

    #[test]
    fn reader_cache_release_frees() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.fsync(ino).unwrap();
        let _ = read_plain(&store, ino, 0);
        assert!(store.readers.lock().contains_key(&ino));
        store.release(ino);
        assert!(
            !store.readers.lock().contains_key(&ino),
            "release 后应释放缓存 reader"
        );
    }

    #[test]
    fn reader_cache_uncommitted_dirty_block_read_through_does_not_pollute_cache() {
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
    fn reader_cache_rename_overwrite_target_invalidates_old_cache() {
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
        assert!(store.readers.lock().contains_key(&dst_ino));

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
            !store.readers.lock().contains_key(&dst_ino),
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

    // ----- 故障注入：commit_session 各 durability 阶段失败后的会话与缓存不变量 -----

    #[test]
    fn commit_session_barrier1_failure_preserves_session_for_retry() {
        let cs = 8u32;
        let (store, _d, ino) = store_with_file(cs);

        // 先提交并读取 baseline A，建立可辨识的旧版本和 reader cache。
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.fsync(ino).unwrap();
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..])
        );
        assert!(store.readers.lock().contains_key(&ino));
        let abs = store.abs_of_ino(ino).unwrap();

        // 写入 B 后令下一次 commit 的 barrier1 失败。此时旧 superblock 仍是 active，B 尚未提交。
        store.put_block(ino, 0, mk_block(b"BBBBBBBB"), 8).unwrap();
        store.fault_next_commit_barrier1();
        let res = store.fsync(ino);
        assert!(res.is_err(), "注入 barrier1 故障后 fsync 应返回 Err");
        assert!(
            store.sessions.lock().active.contains_key(&ino),
            "commit 前失败必须保留脏会话供下次 fsync 重试"
        );
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"BBBBBBBB"[..]),
            "失败后读路径仍应从保留的会话读到已确认写入 B"
        );
        assert!(
            store.readers.lock().contains_key(&ino),
            "barrier1 失败不得失效旧 reader"
        );
        let disk = ArchiveReader::open(&abs).unwrap();
        let (bytes, entry) = disk.read_block(0).unwrap().unwrap();
        assert_eq!(
            decompress(&bytes, Algo::Zstd, entry.is_verbatim()).unwrap(),
            b"AAAAAAAA",
            "barrier1 失败后真实 archive 必须仍是 A"
        );

        // 不再注入故障：同一会话应可重试提交，随后直接从 archive 读回 durable 的 B。
        store.fsync(ino).unwrap();
        assert!(!store.sessions.lock().active.contains_key(&ino));
        let disk = ArchiveReader::open(&abs).unwrap();
        let (bytes, entry) = disk.read_block(0).unwrap().unwrap();
        assert_eq!(
            decompress(&bytes, Algo::Zstd, entry.is_verbatim()).unwrap(),
            b"BBBBBBBB",
            "重试 fsync 成功后真实 archive 应 durable 为 B"
        );
    }

    #[test]
    fn commit_session_sync_failure_still_invalidates_reader_cache_no_stale() {
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
        assert!(store.readers.lock().contains_key(&ino), "读后应缓存 reader");

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
            !store.readers.lock().contains_key(&ino),
            "sync 失败后旧 reader 缓存必已失效（invalidate 先于 up.sync）"
        );
        assert!(
            !store.sessions.lock().active.contains_key(&ino),
            "commit 已 durable 后的末尾 sync 失败不得恢复会话"
        );
        let durable = store.last_fault_durable.lock().take().unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), durable).unwrap();
        let reader = ArchiveReader::open(tmp.path()).unwrap();
        let (bytes, entry) = reader.read_block(0).unwrap().unwrap();
        assert_eq!(
            decompress(&bytes, Algo::Zstd, entry.is_verbatim()).unwrap(),
            b"ZZZZZZZZ",
            "sync#3 失败时 FaultIo durable 版本必须已是 Z"
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
    fn head_cache_built_when_block0_sealed_as_body_fast_path_decompress_equals_prefix() {
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
    fn head_cache_not_built_for_single_block_small_file() {
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
    fn head_cache_request_beyond_covered_prefix_returns_none() {
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
    fn head_cache_falls_back_per_block_during_dirty_session() {
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
    fn head_cache_preserved_across_append_commit() {
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

    // ----- 阶段 D3：目录操作并发原子性（ns 锁 + create O_EXCL） -----

    use std::sync::Barrier;
    use std::thread;

    /// 一个普通文件 Attr（测试便捷）。
    fn reg_attr(chunk_size: u32) -> Attr {
        Attr {
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
        }
    }

    /// 在空 backing 上建 ShadowStore（无预置文件）。
    fn empty_store(chunk_size: u32) -> (Arc<ShadowStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let store = ShadowStore::open_with_chunk_size(backing, chunk_size).unwrap();
        (Arc::new(store), dir)
    }

    /// inodes 三/双向表自洽断言：by_ino 与 by_path 互为逆，无悬挂项。
    fn assert_inode_map_consistent(store: &ShadowStore) {
        let m = store.inodes.lock();
        for (ino, path) in m.by_ino.iter() {
            assert_eq!(
                m.by_path.get(path).copied(),
                Some(*ino),
                "by_ino[{ino}]={path:?} 在 by_path 中应反向映射回 {ino}"
            );
        }
        for (path, ino) in m.by_path.iter() {
            assert_eq!(
                m.by_ino.get(ino).cloned(),
                Some(path.clone()),
                "by_path[{path:?}]={ino} 在 by_ino 中应反向映射回 {path:?}"
            );
        }
    }

    #[test]
    fn concurrent_same_name_create_only_one_file_no_mutual_truncation() {
        // 缺陷：create 无 O_EXCL（底层 File::create = O_TRUNC），两个并发同名 create
        // 双成功且第二个截断第一个。加 O_EXCL 后：底层只一个文件被建出，恰一个 create 成功，
        // 其余得 AlreadyExists；且 by_ino/by_path 双向一致。
        let n = 8usize;
        let iters = 40usize;
        for it in 0..iters {
            let (store, _dir) = empty_store(64);
            let name = format!("c{it}.bin");
            let barrier = Arc::new(Barrier::new(n));
            let mut handles = Vec::new();
            for _ in 0..n {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let name = name.clone();
                handles.push(thread::spawn(move || {
                    barrier.wait();
                    store.create(ROOT_INO, &name, reg_attr(64))
                }));
            }
            let mut ok = 0usize;
            let mut eexist = 0usize;
            for h in handles {
                match h.join().unwrap() {
                    Ok(_) => ok += 1,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => eexist += 1,
                    Err(e) => panic!("意外错误：{e}"),
                }
            }
            assert_eq!(ok, 1, "并发同名 create 应恰一个成功（O_EXCL）");
            assert_eq!(eexist, n - 1, "其余应得 AlreadyExists");
            assert_inode_map_consistent(&store);
        }
    }

    #[test]
    fn concurrent_create_and_unlink_same_name_no_dangling_inode_table() {
        // 缺陷：unlink 的「查 ino → remove_file → 清 sessions/readers/inodes」跨多次独立加锁、
        // syscall 夹在中间，与并发 create 交错会留孤儿映射 / 双向失配。ns 锁覆盖整段后表恒自洽。
        let iters = 60usize;
        for it in 0..iters {
            let (store, _dir) = empty_store(64);
            let name = format!("u{it}.bin");
            let barrier = Arc::new(Barrier::new(2));
            let s1 = Arc::clone(&store);
            let s2 = Arc::clone(&store);
            let b1 = Arc::clone(&barrier);
            let b2 = Arc::clone(&barrier);
            let n1 = name.clone();
            let n2 = name.clone();
            let hc = thread::spawn(move || {
                b1.wait();
                let _ = s1.create(ROOT_INO, &n1, reg_attr(64));
            });
            let hu = thread::spawn(move || {
                b2.wait();
                let _ = s2.unlink(ROOT_INO, &n2);
            });
            hc.join().unwrap();
            hu.join().unwrap();
            // 不论交错先后，inodes 双向表必自洽（无悬挂 ino / 无失配 path）。
            assert_inode_map_consistent(&store);
        }
    }

    #[test]
    fn concurrent_rename_overwrite_no_dangling_inode_table_no_orphan_session() {
        // 缺陷：rename 的 overwritten_ino 快照在 fs::rename 之前、与并发 create 交错可能漏失效，
        // 且清 victim 三表与 rename_path 跨多次加锁非原子。ns 锁内一气呵成后表自洽、无孤儿会话。
        let iters = 50usize;
        for it in 0..iters {
            let (store, _dir) = empty_store(64);
            let src = format!("s{it}.bin");
            let dst = format!("d{it}.bin");
            store.create(ROOT_INO, &src, reg_attr(64)).unwrap();
            store.create(ROOT_INO, &dst, reg_attr(64)).unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let s1 = Arc::clone(&store);
            let s2 = Arc::clone(&store);
            let b1 = Arc::clone(&barrier);
            let b2 = Arc::clone(&barrier);
            let src1 = src.clone();
            let dst1 = dst.clone();
            let dst2 = dst.clone();
            // 线程 A：rename src -> dst（覆盖 dst）。线程 B：并发往 dst 写会话后再 unlink。
            let ha = thread::spawn(move || {
                b1.wait();
                let _ = s1.rename((ROOT_INO, &src1), (ROOT_INO, &dst1));
            });
            let hb = thread::spawn(move || {
                b2.wait();
                let _ = s2.unlink(ROOT_INO, &dst2);
            });
            ha.join().unwrap();
            hb.join().unwrap();
            assert_inode_map_consistent(&store);
            // 残存映射项对应的 ino 不应留有孤儿写会话（被清/被覆盖的 ino 不得悬挂在 sessions）。
            let live: std::collections::HashSet<u64> =
                store.inodes.lock().by_ino.keys().copied().collect();
            let sess: Vec<u64> = store.sessions.lock().active.keys().copied().collect();
            for ino in sess {
                assert!(
                    live.contains(&ino),
                    "sessions 中的 ino={ino} 应仍在 inodes 表（无孤儿会话）"
                );
            }
        }
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

    // ----- 后端级可观测指标：commit / reader 缓存命中率 / 尾日志追加（走共享 Metrics 注册表）-----

    #[test]
    fn shadow_backend_metrics_record_commit_reader_hit_and_tail_append() {
        use crate::core::metrics::Metrics;
        let cs = 8u32;
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let metrics = Metrics::new();
        let store = ShadowStore::open_with_chunk_size(backing, cs)
            .unwrap()
            .with_metrics(metrics.clone());
        let ino = store.create(ROOT_INO, "f.bin", reg_attr(cs)).unwrap();

        // 写块0 + fsync：一次真正的脏会话提交 → record_shadow_commit。
        store.put_block(ino, 0, mk_block(b"AAAAAAAA"), 8).unwrap();
        store.fsync(ino).unwrap();

        // 首次 get_block：reader 缓存未命中（打开并解析新 reader）→ record_reader_miss。
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..])
        );
        // 第二次 get_block：命中已缓存 reader → record_reader_hit。
        assert_eq!(
            read_plain(&store, ino, 0).as_deref(),
            Some(&b"AAAAAAAA"[..])
        );

        // 尾日志增量追加 → record_tail_append（绕开脏会话直接落尾日志）。
        store.append_tail(ino, b"tail-delta", 8 + 10).unwrap();

        let mut out = String::new();
        metrics.write_prometheus(&mut out);
        assert!(
            out.contains("zipfs_shadow_commits_total 1"),
            "一次 fsync 提交应记 1 次 shadow_commit：\n{out}"
        );
        assert!(
            out.contains("zipfs_shadow_reader_hits_total 1"),
            "第二次读应命中缓存记 1 次 hit：\n{out}"
        );
        assert!(
            out.contains("zipfs_shadow_reader_misses_total 1"),
            "首次读应未命中记 1 次 miss：\n{out}"
        );
        assert!(
            out.contains("zipfs_shadow_tail_appends_total 1"),
            "一次 append_tail 应记 1 次 tail_append：\n{out}"
        );
    }
}
