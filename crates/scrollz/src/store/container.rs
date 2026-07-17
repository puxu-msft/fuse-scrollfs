//! 布局 V —— 单容器 / 虚拟盘（P2/P3：redb 全包写路径）。
//!
//! 设计见 docs/01-zipfs-design.md §6。整棵树（元数据 + 数据块）落进一个 redb 容器文件。
//! 复用 redb 的 ACID B-tree 作「变长 blob 分配器 + 空闲管理 + 事务」，而非手搓（§6.0）。
//!
//! 表：
//! - `inodes`:  key=ino(u64) → 序列化 InodeRow（kind/size/perm/uid/gid/chunk_size）。
//! - `dirents`: key="<parent_ino>/<name>"(&str) → child_ino(u64)。
//! - `blocks`:  key=(ino, idx)((u64,u64)) → 压缩块 blob（首字节 flags + 压缩字节）。
//!
//! **写批处理（§6.1 必备项）**：一次 FUSE `write` 回调内多块 `put_block` 累积进一个
//! **挂起写事务**（内存暂存），仅在 `fsync`/`flush`/`sync_all` 时 commit——否则每块一事务，
//! microbench 证实慢 8–18x。`get_block` read-through 挂起暂存，保证写后读可见。

use super::{Attr, DirEntry, Store, StoredBlock};
use crate::core::inode::Ino;
use crate::core::metrics::Metrics;

use parking_lot::Mutex;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

const ROOT_INO: u64 = 1;

/// inode 表：ino → 序列化行（见 InodeRow 编码）。
const INODES: TableDefinition<u64, &[u8]> = TableDefinition::new("inodes");
/// 目录项表：key="parent/name" → child ino。
const DIRENTS: TableDefinition<&str, u64> = TableDefinition::new("dirents");
/// 数据块表：key=(ino, idx) → flags(1B) + 压缩字节。
const BLOCKS: TableDefinition<(u64, u64), &[u8]> = TableDefinition::new("blocks");

/// inode 行的内存视图。序列化为定长小端字节（见 encode/decode）。
#[derive(Debug, Clone)]
struct InodeRow {
    /// 1=目录 2=普通文件。
    kind: u8,
    size: u64,
    perm: u16,
    uid: u32,
    gid: u32,
    chunk_size: u32,
    /// 修改 / 访问 / 状态变更时间。V 布局自身持久化（无底层文件可问），create/setattr 维护。
    mtime: std::time::SystemTime,
    atime: std::time::SystemTime,
    ctime: std::time::SystemTime,
}

/// 旧布局长度（无时间字段）：kind(1)+size(8)+perm(2)+uid(4)+gid(4)+chunk_size(4)。
const INODE_ROW_LEN_V1: usize = 1 + 8 + 2 + 4 + 4 + 4; // 23
/// 当前布局：在 V1 基础上追加 mtime/atime/ctime，各 i64 秒 + u32 纳秒（12B×3）。
const INODE_ROW_LEN: usize = INODE_ROW_LEN_V1 + 12 * 3; // 59

/// SystemTime → (秒 i64, 纳秒 u32)。epoch 之前（罕见）clamp 到 0，与 `system_time_from` 对称。
fn time_to_parts(t: std::time::SystemTime) -> (i64, u32) {
    match t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(_) => (0, 0),
    }
}

impl InodeRow {
    fn encode(&self) -> [u8; INODE_ROW_LEN] {
        let mut b = [0u8; INODE_ROW_LEN];
        b[0] = self.kind;
        b[1..9].copy_from_slice(&self.size.to_le_bytes());
        b[9..11].copy_from_slice(&self.perm.to_le_bytes());
        b[11..15].copy_from_slice(&self.uid.to_le_bytes());
        b[15..19].copy_from_slice(&self.gid.to_le_bytes());
        b[19..23].copy_from_slice(&self.chunk_size.to_le_bytes());
        // 时间三元组，各 (secs i64, nsec u32)。
        for (off, t) in [(23usize, self.mtime), (35, self.atime), (47, self.ctime)] {
            let (secs, nsec) = time_to_parts(t);
            b[off..off + 8].copy_from_slice(&secs.to_le_bytes());
            b[off + 8..off + 12].copy_from_slice(&nsec.to_le_bytes());
        }
        b
    }

    fn decode(b: &[u8]) -> io::Result<Self> {
        // 长度容忍：V1(23B) 旧档时间退化为 UNIX_EPOCH；当前布局(59B) 解出真实时间。
        if b.len() != INODE_ROW_LEN && b.len() != INODE_ROW_LEN_V1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("inode 行长度异常：{}", b.len()),
            ));
        }
        let read_time = |off: usize| -> std::time::SystemTime {
            let secs = i64::from_le_bytes(b[off..off + 8].try_into().unwrap());
            let nsec = u32::from_le_bytes(b[off + 8..off + 12].try_into().unwrap());
            crate::core::system_time_from(secs, nsec as i64)
        };
        let (mtime, atime, ctime) = if b.len() == INODE_ROW_LEN {
            (read_time(23), read_time(35), read_time(47))
        } else {
            let e = std::time::SystemTime::UNIX_EPOCH;
            (e, e, e)
        };
        Ok(Self {
            kind: b[0],
            size: u64::from_le_bytes(b[1..9].try_into().unwrap()),
            perm: u16::from_le_bytes(b[9..11].try_into().unwrap()),
            uid: u32::from_le_bytes(b[11..15].try_into().unwrap()),
            gid: u32::from_le_bytes(b[15..19].try_into().unwrap()),
            chunk_size: u32::from_le_bytes(b[19..23].try_into().unwrap()),
            mtime,
            atime,
            ctime,
        })
    }

    fn kind_to_filetype(&self) -> fuser::FileType {
        if self.kind == 1 {
            fuser::FileType::Directory
        } else {
            fuser::FileType::RegularFile
        }
    }
}

fn filetype_to_kind(ft: fuser::FileType) -> u8 {
    if ft == fuser::FileType::Directory {
        1
    } else {
        2
    }
}

fn dirent_key(parent: u64, name: &str) -> String {
    format!("{parent}/{name}")
}

/// 把 io 错误统一构造。
fn db_err<E: std::fmt::Display>(ctx: &str, e: E) -> io::Error {
    io::Error::other(format!("redb {ctx}：{e}"))
}

/// 一次写会话的挂起块（key=(ino,idx) → 压缩块 + 该写后 size）。在 fsync 时合并一个事务提交。
#[derive(Default)]
struct Pending {
    /// 挂起块：(ino, idx) → StoredBlock。
    blocks: HashMap<(u64, u64), StoredBlock>,
    /// 挂起的 size 更新：ino → 最新逻辑大小。
    sizes: HashMap<u64, u64>,
    /// 挂起的截断：ino → keep_from（删除 >= keep_from 的块）。
    truncations: HashMap<u64, u64>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.sizes.is_empty() && self.truncations.is_empty()
    }

    /// commit 失败回滚：把 `flushing` 的条目并回（self=active）。**active 已有的键不覆盖**——
    /// active 是 swap 之后更新的写，优先级高于回滚的旧 flushing 内容（D1 lost-update 修复）。
    fn merge_from_flushing(&mut self, flushing: Pending) {
        for (k @ (ino, idx), v) in flushing.blocks {
            // active 的较新 truncate 是操作序列中的后继，范围外旧块不得在失败回滚时复活。
            if self
                .truncations
                .get(&ino)
                .is_none_or(|&keep_from| idx < keep_from)
            {
                self.blocks.entry(k).or_insert(v);
            }
        }
        for (k, v) in flushing.sizes {
            self.sizes.entry(k).or_insert(v);
        }
        for (k, v) in flushing.truncations {
            // truncation 取「保留更多」语义（min keep_from）；但 active 若已有该 ino 的写意图，
            // 以 active 为准（更新）。active 无则采纳 flushing 的旧 truncation。
            self.truncations.entry(k).or_insert(v);
        }
    }
}

/// 读路径单层查询结果：命中块 / 被截断越界 / 本层无信息（继续查下一层）。
enum BlockLookup {
    Hit(StoredBlock),
    Truncated,
    Miss,
}

/// 在单个 `Pending` 缓冲内查 (ino, idx)：先看块命中，再看 truncation 拦截。与原 read-through 一致。
fn lookup_block_in(p: &Pending, ino: u64, idx: u64) -> BlockLookup {
    if let Some(blk) = p.blocks.get(&(ino, idx)) {
        return BlockLookup::Hit(blk.clone());
    }
    if let Some(&keep_from) = p.truncations.get(&ino) {
        if idx >= keep_from {
            return BlockLookup::Truncated;
        }
    }
    BlockLookup::Miss
}

/// 写批处理的双缓冲暂存（D1 torn-read + lost-update 根治）。
/// - `active`：接收新写（put/truncate）。
/// - `flushing`：commit_pending swap 出来、正落 redb 的那一代（IO 期间不持 inner 锁，故读路径须查它）。
///   稳态为空；commit 成功后清空，失败则合并回 active。
#[derive(Default)]
struct Inner {
    active: Pending,
    flushing: Pending,
}

/// 容器后端（布局 V）。
pub struct ContainerStore {
    db: Database,
    next_ino: Mutex<u64>,
    /// 写批处理的双缓冲暂存（§6.1 + D1）。fsync/flush/sync_all 才落 redb 事务。
    inner: Mutex<Inner>,
    /// 串行化 commit_pending，与 `inner` 锁分离：IO 期间不阻塞读写并发（D1）。
    commit_lock: Mutex<()>,
    default_chunk_size: u32,
    /// 统一指标注册表（全 crate 共享 `Arc`）。默认自建一个私有实例；`with_metrics` 注入共享实例，
    /// 使 `commit_pending` 的成功/失败/落盘块数/flushing 峰值可经统一 Prometheus 出口观测。
    metrics: Arc<Metrics>,
    /// 故障注入（仅测试）：置位时下一次 commit_pending 的 redb commit 返回 EIO，用于
    /// 确定性复现 lost-update。仿 shadow.rs `fault_commit_sync` 模式。
    #[cfg(test)]
    fault_commit: std::sync::atomic::AtomicBool,
}

impl ContainerStore {
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_chunk_size(path, crate::core::DEFAULT_CHUNK_SIZE as u32)
    }

    pub fn open_with_chunk_size(path: &Path, default_chunk_size: u32) -> io::Result<Self> {
        if default_chunk_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "default_chunk_size 不能为 0",
            ));
        }
        let db = Database::create(path).map_err(|e| db_err("create", e))?;

        // 初始化根 inode + 建表（幂等：已存在则保留）。
        let mut max_ino = ROOT_INO;
        {
            let txn = db.begin_write().map_err(|e| db_err("begin_write", e))?;
            {
                let mut inodes = txn
                    .open_table(INODES)
                    .map_err(|e| db_err("open inodes", e))?;
                txn.open_table(DIRENTS)
                    .map_err(|e| db_err("open dirents", e))?;
                txn.open_table(BLOCKS)
                    .map_err(|e| db_err("open blocks", e))?;
                if inodes
                    .get(ROOT_INO)
                    .map_err(|e| db_err("get root", e))?
                    .is_none()
                {
                    let now = std::time::SystemTime::now();
                    let root = InodeRow {
                        kind: 1,
                        size: 0,
                        perm: 0o755,
                        uid: 0,
                        gid: 0,
                        chunk_size: default_chunk_size,
                        mtime: now,
                        atime: now,
                        ctime: now,
                    };
                    inodes
                        .insert(ROOT_INO, &root.encode()[..])
                        .map_err(|e| db_err("insert root", e))?;
                } else {
                    // 扫描已有 inode 求最大 ino，使 next_ino 不撞已存在项。
                    let iter = inodes.iter().map_err(|e| db_err("iter inodes", e))?;
                    for row in iter {
                        let (k, _) = row.map_err(|e| db_err("iter row", e))?;
                        max_ino = max_ino.max(k.value());
                    }
                }
            }
            txn.commit().map_err(|e| db_err("commit init", e))?;
        }

        Ok(Self {
            db,
            next_ino: Mutex::new(max_ino + 1),
            inner: Mutex::new(Inner::default()),
            commit_lock: Mutex::new(()),
            default_chunk_size,
            metrics: Metrics::new(),
            #[cfg(test)]
            fault_commit: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// 链式注入共享指标注册表（全 crate 单一 `Arc<Metrics>`）。默认自建私有实例，
    /// `run_mount` 用本方法把 container 埋点接进统一 `.prom` 出口。
    pub fn with_metrics(mut self, m: Arc<Metrics>) -> Self {
        self.metrics = m;
        self
    }

    fn alloc_ino(&self) -> u64 {
        let mut g = self.next_ino.lock();
        let ino = *g;
        *g += 1;
        ino
    }

    /// 离线压实容器（回收 redb MVCC 未引用页 + 碎片，FIRST-RUN §4「BV 无 compact」修复）。
    ///
    /// redb 写事务用 MVCC：旧版本页在无活跃读事务引用前不回收，稳态下容器文件膨胀
    /// （首轮实测 0.54x，物理比逻辑还大）。`Database::compact` 重排页面、释放尾部空间，
    /// 把文件收缩到接近真实占用（设计 §6.1 推算 64KiB 块 compact 后约 1.34x 膨胀）。
    ///
    /// 须在**无任何活跃读/写事务**时调用（独占 `&mut self`），故仅作为离线 `zipfs compact`
    /// 子命令入口，不在挂载运行期触发。返回 `Ok(true)` 表示确实压实了数据。
    pub fn compact(&mut self) -> io::Result<bool> {
        self.db.compact().map_err(|e| db_err("compact", e))
    }

    /// 读某 inode 行（不经挂起暂存——size 的挂起更新单独 read-through）。
    fn read_inode(&self, ino: u64) -> io::Result<Option<InodeRow>> {
        let txn = self.db.begin_read().map_err(|e| db_err("begin_read", e))?;
        let inodes = txn
            .open_table(INODES)
            .map_err(|e| db_err("open inodes", e))?;
        match inodes.get(ino).map_err(|e| db_err("get inode", e))? {
            Some(v) => Ok(Some(InodeRow::decode(v.value())?)),
            None => Ok(None),
        }
    }

    fn row_to_attr(&self, ino: u64, mut row: InodeRow) -> Attr {
        // size read-through 暂存（写后读一致）：active 优先于 flushing（active 是更新的写）。
        {
            let inner = self.inner.lock();
            if let Some(&sz) = inner.active.sizes.get(&ino) {
                row.size = sz;
            } else if let Some(&sz) = inner.flushing.sizes.get(&ino) {
                row.size = sz;
            }
        }
        Attr {
            ino,
            size: row.size,
            kind: row.kind_to_filetype(),
            perm: row.perm,
            uid: row.uid,
            gid: row.gid,
            mtime: row.mtime,
            atime: row.atime,
            ctime: row.ctime,
            chunk_size: row.chunk_size,
        }
    }

    /// 编码块 blob：flags(1B) + 压缩字节。
    fn encode_block(blk: &StoredBlock) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + blk.bytes.len());
        out.push(if blk.stored_verbatim { 1 } else { 0 });
        out.extend_from_slice(&blk.bytes);
        out
    }

    fn decode_block(raw: &[u8]) -> StoredBlock {
        let verbatim = raw.first().copied().unwrap_or(0) == 1;
        StoredBlock {
            bytes: raw.get(1..).unwrap_or(&[]).to_vec(),
            stored_verbatim: verbatim,
        }
    }
}

impl Store for ContainerStore {
    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
        let txn = self.db.begin_read().ok()?;
        let dirents = txn.open_table(DIRENTS).ok()?;
        let key = dirent_key(parent, name);
        let child = dirents.get(key.as_str()).ok()??.value();
        let row = self.read_inode(child).ok()??;
        Some(self.row_to_attr(child, row))
    }

    fn create(&self, parent: Ino, name: &str, attr: Attr) -> io::Result<Ino> {
        super::validate_name(name)?;
        let ino = self.alloc_ino();
        let chunk_size = if attr.chunk_size == 0 {
            self.default_chunk_size
        } else {
            attr.chunk_size
        };
        let row = InodeRow {
            kind: filetype_to_kind(attr.kind),
            size: 0,
            perm: attr.perm,
            uid: attr.uid,
            gid: attr.gid,
            chunk_size,
            mtime: attr.mtime,
            atime: attr.atime,
            ctime: attr.ctime,
        };
        let txn = self
            .db
            .begin_write()
            .map_err(|e| db_err("begin_write", e))?;
        {
            let mut inodes = txn
                .open_table(INODES)
                .map_err(|e| db_err("open inodes", e))?;
            let mut dirents = txn
                .open_table(DIRENTS)
                .map_err(|e| db_err("open dirents", e))?;
            let key = dirent_key(parent, name);
            if dirents
                .get(key.as_str())
                .map_err(|e| db_err("get dirent", e))?
                .is_some()
            {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "已存在"));
            }
            inodes
                .insert(ino, &row.encode()[..])
                .map_err(|e| db_err("insert inode", e))?;
            dirents
                .insert(key.as_str(), ino)
                .map_err(|e| db_err("insert dirent", e))?;
        }
        txn.commit().map_err(|e| db_err("commit create", e))?;
        Ok(ino)
    }

    fn mkdir(&self, parent: Ino, name: &str, attr: Attr) -> io::Result<Ino> {
        let mut a = attr;
        a.kind = fuser::FileType::Directory;
        self.create(parent, name, a)
    }

    fn unlink(&self, parent: Ino, name: &str) -> io::Result<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| db_err("begin_write", e))?;
        let removed_child;
        {
            let mut dirents = txn
                .open_table(DIRENTS)
                .map_err(|e| db_err("open dirents", e))?;
            let mut inodes = txn
                .open_table(INODES)
                .map_err(|e| db_err("open inodes", e))?;
            let mut blocks = txn
                .open_table(BLOCKS)
                .map_err(|e| db_err("open blocks", e))?;
            let key = dirent_key(parent, name);
            let child = dirents
                .remove(key.as_str())
                .map_err(|e| db_err("remove dirent", e))?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "不存在"))?
                .value();
            // D2 孤儿块修复：在删 redb inode 之前先持 pending 锁清掉该 child 的
            // blocks/sizes/truncations。配合 commit_pending 的存在性检查，使任何
            // 并发 flush 都不能复活该 ino 的块——窗口被双向堵死：
            //   1) 清理在 inode 删除之前发生，flush 若在此之后跑，pending 里已无该块；
            //   2) 即便 flush 抢在清理与本事务 commit 之间跑，commit_pending 也会
            //      因 INODES 表里该 inode 仍在/已删而分别得到一致结果（删后跳过）。
            {
                let mut inner = self.inner.lock();
                let Inner { active, flushing } = &mut *inner;
                for p in [active, flushing] {
                    p.blocks.retain(|&(i, _), _| i != child);
                    p.sizes.remove(&child);
                    p.truncations.remove(&child);
                }
            }
            inodes
                .remove(child)
                .map_err(|e| db_err("remove inode", e))?;
            // 删除该 inode 的所有数据块。评审 D2：用 RangeInclusive 到 (child, u64::MAX)，
            // 避免 child==u64::MAX 时 `child+1` 溢出（release 回绕成空范围 → 块漏删、资源泄漏）。
            let to_del: Vec<(u64, u64)> = blocks
                .range((child, 0)..=(child, u64::MAX))
                .map_err(|e| db_err("range blocks", e))?
                .map(|r| r.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()
                .map_err(|e| db_err("collect blocks", e))?;
            for k in to_del {
                blocks.remove(k).map_err(|e| db_err("remove block", e))?;
            }
            removed_child = child;
        }
        txn.commit().map_err(|e| db_err("commit unlink", e))?;
        // 兜底：本事务期间（pending 清理之后、commit 之前）仍可能有并发线程把该
        // child 的块重新入 active（例如尚持旧 ino 句柄的写）。再清一次确保不留残块。
        {
            let mut inner = self.inner.lock();
            let Inner { active, flushing } = &mut *inner;
            for p in [active, flushing] {
                p.blocks.retain(|&(i, _), _| i != removed_child);
                p.sizes.remove(&removed_child);
                p.truncations.remove(&removed_child);
            }
        }
        Ok(())
    }

    fn rmdir(&self, parent: Ino, name: &str) -> io::Result<()> {
        // 评审 D3：rmdir 必须拒绝非空目录（ENOTEMPTY），否则 unlink 删掉目录 dirent + inode 却
        // 留下子项的 dirent/inode 成孤儿（既无法 lookup 又永占空间，compact 也回收不掉）。
        let child = self
            .lookup(parent, name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "不存在"))?;
        if child.kind != fuser::FileType::Directory {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "rmdir 目标非目录",
            ));
        }
        if !self.readdir(child.ino).is_empty() {
            return Err(io::Error::from_raw_os_error(libc::ENOTEMPTY));
        }
        self.unlink(parent, name)
    }

    fn rename(&self, old: (Ino, &str), new: (Ino, &str)) -> io::Result<()> {
        super::validate_name(old.1)?;
        super::validate_name(new.1)?;
        let txn = self
            .db
            .begin_write()
            .map_err(|e| db_err("begin_write", e))?;
        {
            let mut dirents = txn
                .open_table(DIRENTS)
                .map_err(|e| db_err("open dirents", e))?;
            let mut inodes = txn
                .open_table(INODES)
                .map_err(|e| db_err("open inodes", e))?;
            let mut blocks = txn
                .open_table(BLOCKS)
                .map_err(|e| db_err("open blocks", e))?;
            let old_key = dirent_key(old.0, old.1);
            let child = dirents
                .remove(old_key.as_str())
                .map_err(|e| db_err("remove old dirent", e))?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "源不存在"))?
                .value();
            let new_key = dirent_key(new.0, new.1);
            // 覆盖目标若存在：删其 inode + 块。
            if let Some(prev) = dirents
                .remove(new_key.as_str())
                .map_err(|e| db_err("remove new dirent", e))?
            {
                let prev = prev.value();
                inodes
                    .remove(prev)
                    .map_err(|e| db_err("remove prev inode", e))?;
                let to_del: Vec<(u64, u64)> = blocks
                    .range((prev, 0)..=(prev, u64::MAX)) // 评审 D2：避免 prev+1 溢出
                    .map_err(|e| db_err("range blocks", e))?
                    .map(|r| r.map(|(k, _)| k.value()))
                    .collect::<Result<_, _>>()
                    .map_err(|e| db_err("collect blocks", e))?;
                for k in to_del {
                    blocks.remove(k).map_err(|e| db_err("remove block", e))?;
                }
            }
            dirents
                .insert(new_key.as_str(), child)
                .map_err(|e| db_err("insert new dirent", e))?;
        }
        txn.commit().map_err(|e| db_err("commit rename", e))?;
        Ok(())
    }

    fn readdir(&self, dir: Ino) -> Vec<DirEntry> {
        let Ok(txn) = self.db.begin_read() else {
            return Vec::new();
        };
        let Ok(dirents) = txn.open_table(DIRENTS) else {
            return Vec::new();
        };
        // C-8：在同一读事务内打开 INODES，使 dirent 与 child inode 取自同一快照，消除
        // 「dirent 在但 child 在两次 begin_read 间被并发 unlink → inode None → unwrap_or
        // 伪造 RegularFile」的跨事务类型错报。同快照下 dirent 存在则其 inode 必存在（create/
        // unlink 原子提交），若仍缺失说明该 dirent 是真孤儿，跳过而非伪造类型。
        let Ok(inodes) = txn.open_table(INODES) else {
            return Vec::new();
        };
        let prefix = format!("{dir}/");
        let mut out = Vec::new();
        let Ok(iter) = dirents.iter() else {
            return Vec::new();
        };
        for row in iter.flatten() {
            let key = row.0.value();
            let Some(name) = key.strip_prefix(&prefix) else {
                continue;
            };
            // 仅直接子项（名字里不含 '/'，否则是更深路径——但本编码 name 不含 '/'）。
            if name.contains('/') {
                continue;
            }
            let child = row.1.value();
            // 同快照读 child inode 行。缺失 → 真孤儿 dirent，跳过（不伪造类型）。
            let kind = match inodes.get(child) {
                Ok(Some(v)) => match InodeRow::decode(v.value()) {
                    Ok(r) => r.kind_to_filetype(),
                    Err(_) => continue,
                },
                Ok(None) => continue,
                Err(_) => continue,
            };
            out.push(DirEntry {
                ino: child,
                name: name.to_string(),
                kind,
            });
        }
        out
    }

    fn setattr(&self, ino: Ino, attr: Attr) -> io::Result<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| db_err("begin_write", e))?;
        {
            let mut inodes = txn
                .open_table(INODES)
                .map_err(|e| db_err("open inodes", e))?;
            let mut row = {
                let existing = inodes
                    .get(ino)
                    .map_err(|e| db_err("get inode", e))?
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "不存在"))?;
                InodeRow::decode(existing.value())?
            };
            row.perm = attr.perm;
            row.uid = attr.uid;
            row.gid = attr.gid;
            row.size = attr.size;
            // 时间写回（V 布局自持久化，无底层文件可问）。前端已解析 TimeOrNow 后填入 attr。
            row.mtime = attr.mtime;
            row.atime = attr.atime;
            row.ctime = attr.ctime;
            inodes
                .insert(ino, &row.encode()[..])
                .map_err(|e| db_err("insert inode", e))?;
        }
        txn.commit().map_err(|e| db_err("commit setattr", e))?;
        Ok(())
    }

    fn getattr_ino(&self, ino: Ino) -> Option<Attr> {
        let row = self.read_inode(ino).ok()??;
        Some(self.row_to_attr(ino, row))
    }

    fn get_block(&self, ino: Ino, idx: u64) -> io::Result<Option<StoredBlock>> {
        // 1) read-through 双缓冲：active 先于 flushing（active 更新）。条目恒在
        //    active∪flushing∪redb → 消灭 commit 中间窗口的 torn read（D1）。
        {
            let inner = self.inner.lock();
            match lookup_block_in(&inner.active, ino, idx) {
                BlockLookup::Hit(blk) => return Ok(Some(blk)),
                BlockLookup::Truncated => return Ok(None),
                BlockLookup::Miss => {}
            }
            match lookup_block_in(&inner.flushing, ino, idx) {
                BlockLookup::Hit(blk) => return Ok(Some(blk)),
                BlockLookup::Truncated => return Ok(None),
                BlockLookup::Miss => {}
            }
        }
        // 2) 落 redb。
        let txn = self.db.begin_read().map_err(|e| db_err("begin_read", e))?;
        let blocks = txn
            .open_table(BLOCKS)
            .map_err(|e| db_err("open blocks", e))?;
        match blocks.get((ino, idx)).map_err(|e| db_err("get block", e))? {
            Some(v) => Ok(Some(Self::decode_block(v.value()))),
            None => Ok(None),
        }
    }

    fn block_geometry(&self, ino: Ino) -> Option<(u64, u32)> {
        let row = self.read_inode(ino).ok()??;
        if row.kind != 2 {
            return None;
        }
        // size read-through 暂存：active 优先于 flushing。
        let size = {
            let inner = self.inner.lock();
            inner
                .active
                .sizes
                .get(&ino)
                .or_else(|| inner.flushing.sizes.get(&ino))
                .copied()
                .unwrap_or(row.size)
        };
        Some((size, row.chunk_size))
    }

    fn put_block(&self, ino: Ino, idx: u64, blk: StoredBlock, new_size: u64) -> io::Result<()> {
        let mut inner = self.inner.lock();
        let p = &mut inner.active;
        p.blocks.insert((ino, idx), blk);
        p.sizes.insert(ino, new_size);
        Ok(())
    }

    fn truncate_blocks(&self, ino: Ino, keep_from: u64, new_size: u64) -> io::Result<()> {
        let mut inner = self.inner.lock();
        let p = &mut inner.active;
        // 丢弃 active 块中 >= keep_from 的。flushing 的旧块由 truncation 拦截（读 active-first）。
        p.blocks
            .retain(|&(i, blk_idx), _| i != ino || blk_idx < keep_from);
        let entry = p.truncations.entry(ino).or_insert(keep_from);
        *entry = (*entry).min(keep_from);
        p.sizes.insert(ino, new_size);
        Ok(())
    }

    fn fsync(&self, _ino: Ino) -> io::Result<()> {
        // 简化：fsync 提交全部挂起（redb 是单库事务，无法只 commit 一个 inode 的子集而不
        // 提交其他挂起块；按全局 barrier 处理仍满足 POSIX「该 fd 数据已落盘」）。
        self.commit_pending()
    }

    fn sync_all(&self) -> io::Result<()> {
        self.commit_pending()
    }
}

impl ContainerStore {
    /// 把挂起暂存合并到一个 redb 写事务并 commit（写批处理核心，§6.1 + D1）。
    ///
    /// **双缓冲协议**（消灭 torn-read 与 lost-update）：
    /// ① 持 `commit_lock` 串行化 commit；② 持 inner 锁 swap active↔flushing（active 清空，
    /// 新写继续进 active），释放 inner 锁；③ 用 flushing 落 redb（IO 期间不持 inner 锁，
    /// 读写并发不阻塞——读路径仍能查 flushing）；④ 成功清空 flushing；失败把 flushing
    /// 合并回 active（active 已有键不覆盖，那是更新的写）并返回 Err。
    fn commit_pending(&self) -> io::Result<()> {
        // 串行化 commit：保证同一时刻只有一代 flushing 在落盘，swap/merge 不交错。
        let _commit_guard = self.commit_lock.lock();

        // swap：active → flushing，active 清空（新写继续进 active）。
        {
            let mut inner = self.inner.lock();
            if inner.active.is_empty() {
                // 无新挂起。flushing 此刻必为空（上一次 commit 已清/合并），直接返回。
                return Ok(());
            }
            let Inner { active, flushing } = &mut *inner;
            std::mem::swap(active, flushing);
        }

        // IO 期间不持 inner 锁。读路径查 active∪flushing∪redb，flushing 仍可见 → 无 torn read。
        let flushing = {
            let inner = self.inner.lock();
            // 克隆出 flushing 内容用于落盘（保留缓冲本体在 inner 内供读路径查询）。
            Pending {
                blocks: inner.flushing.blocks.clone(),
                sizes: inner.flushing.sizes.clone(),
                truncations: inner.flushing.truncations.clone(),
            }
        };

        // 埋点：观测本代 flushing 的缓冲字节峰值（Σ 块字节）与块数（成功后计）。仅自增，
        // 不改双缓冲 swap/merge/存在性检查等控制流。
        let flushing_bytes: u64 = flushing.blocks.values().map(|b| b.bytes.len() as u64).sum();
        let flushing_blocks = flushing.blocks.len() as u64;
        self.metrics.observe_flushing_bytes(flushing_bytes);

        match self.flush_to_redb(&flushing) {
            Ok(()) => {
                // 成功：清空 flushing（其内容已 durable 进 redb）。
                self.inner.lock().flushing = Pending::default();
                self.metrics.record_commit_ok(flushing_blocks);
                Ok(())
            }
            Err(e) => {
                // 失败：把 flushing 合并回 active（active 已有键不覆盖）→ 数据不丢，下次 fsync 重试。
                let mut inner = self.inner.lock();
                let flushing = std::mem::take(&mut inner.flushing);
                inner.active.merge_from_flushing(flushing);
                drop(inner);
                self.metrics.record_commit_failed();
                Err(e)
            }
        }
    }

    /// 把一代 `Pending` 落进一个 redb 写事务并 commit。失败返回 Err（调用方负责回滚）。
    fn flush_to_redb(&self, pending: &Pending) -> io::Result<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| db_err("begin_write", e))?;
        {
            let mut blocks = txn
                .open_table(BLOCKS)
                .map_err(|e| db_err("open blocks", e))?;
            let mut inodes = txn
                .open_table(INODES)
                .map_err(|e| db_err("open inodes", e))?;

            // D2 孤儿块修复：本事务内按 ino 缓存「INODES 表是否仍存在该 inode」，
            // 供 truncations / blocks 两个循环复用，避免每块查一次表。已删 inode
            // （并发 unlink 已 commit）的挂起块/截断一律跳过，与下方 sizes 循环对称，
            // 否则会把无 inode 引用的脏块写进 BLOCKS → 孤儿块（lookup 不可达、compact 回收不掉）。
            let mut ino_exists: HashMap<u64, bool> = HashMap::new();
            let mut inode_present = |ino: u64| -> io::Result<bool> {
                if let Some(&e) = ino_exists.get(&ino) {
                    return Ok(e);
                }
                let exists = inodes
                    .get(ino)
                    .map_err(|e| db_err("get inode", e))?
                    .is_some();
                ino_exists.insert(ino, exists);
                Ok(exists)
            };

            // 截断：删除 >= keep_from 的块。仅对仍存在的 inode 执行（删已删 inode 的块
            // 本是无害幂等，但与 sizes/blocks 同标准处理，保持一致性）。
            for (&ino, &keep_from) in &pending.truncations {
                if !inode_present(ino)? {
                    continue;
                }
                let to_del: Vec<(u64, u64)> = blocks
                    .range((ino, keep_from)..=(ino, u64::MAX)) // 评审 D2：避免 ino+1 溢出
                    .map_err(|e| db_err("range blocks", e))?
                    .map(|r| r.map(|(k, _)| k.value()))
                    .collect::<Result<_, _>>()
                    .map_err(|e| db_err("collect blocks", e))?;
                for k in to_del {
                    blocks.remove(k).map_err(|e| db_err("remove block", e))?;
                }
            }
            // 写挂起块。inode 不存在则跳过（孤儿块防护）。
            for (&(ino, idx), blk) in &pending.blocks {
                if !inode_present(ino)? {
                    continue;
                }
                let encoded = Self::encode_block(blk);
                blocks
                    .insert((ino, idx), &encoded[..])
                    .map_err(|e| db_err("insert block", e))?;
            }
            // 写挂起 size。
            let now = std::time::SystemTime::now();
            for (&ino, &size) in &pending.sizes {
                let row = {
                    let Some(existing) = inodes.get(ino).map_err(|e| db_err("get inode", e))?
                    else {
                        continue;
                    };
                    InodeRow::decode(existing.value())?
                };
                let mut row = row;
                row.size = size;
                // 内容已变 → 更新 mtime/ctime（shadow 由底层 fs 自动获得，V 布局须显式维护）。
                row.mtime = now;
                row.ctime = now;
                inodes
                    .insert(ino, &row.encode()[..])
                    .map_err(|e| db_err("insert inode", e))?;
            }
        }
        // 故障注入（仅测试）：在 commit 前置位检查，确定性复现 commit 失败 → lost-update 路径。
        #[cfg(test)]
        if self
            .fault_commit
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(io::Error::other("注入的 commit 故障"));
        }
        txn.commit().map_err(|e| db_err("commit pending", e))?;
        Ok(())
    }

    /// 故障注入（仅测试）：令下一次 `commit_pending` 的 redb commit 返回 EIO。
    #[cfg(test)]
    fn fault_next_commit(&self) {
        self.fault_commit
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// 测试钩子：把 active swap 进 flushing 但不 commit redb，构造「flushing 有块、
    /// active 无、redb 无」的中间态，验证读路径查三层（torn-read 自洽）。
    #[cfg(test)]
    fn test_swap_active_into_flushing(&self) {
        let mut inner = self.inner.lock();
        let Inner { active, flushing } = &mut *inner;
        std::mem::swap(active, flushing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::codec::{compress, decompress, Algo};

    fn mk_block(plain: &[u8]) -> StoredBlock {
        let (bytes, verbatim) = compress(plain, Algo::Zstd, 3).unwrap();
        StoredBlock {
            bytes,
            stored_verbatim: verbatim,
        }
    }

    fn new_file(store: &ContainerStore, name: &str, cs: u32) -> u64 {
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
            chunk_size: cs,
        };
        store.create(ROOT_INO, name, attr).unwrap()
    }

    fn dir_attr_t(cs: u32) -> Attr {
        Attr {
            ino: 0,
            size: 0,
            kind: fuser::FileType::Directory,
            perm: 0o755,
            uid: 0,
            gid: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            atime: std::time::SystemTime::UNIX_EPOCH,
            ctime: std::time::SystemTime::UNIX_EPOCH,
            chunk_size: cs,
        }
    }

    #[test]
    fn rmdir_rejects_non_empty_dir() {
        // 评审 D3：rmdir 非空目录旧码直接 unlink → 子项成孤儿。须返回 ENOTEMPTY 且不动子项。
        let cs = 4096u32;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.redb");
        let store = ContainerStore::open_with_chunk_size(&path, cs).unwrap();
        let sub = store.mkdir(ROOT_INO, "sub", dir_attr_t(cs)).unwrap();
        let child_attr = Attr {
            chunk_size: cs,
            ..dir_attr_t(cs)
        };
        let mut fattr = child_attr;
        fattr.kind = fuser::FileType::RegularFile;
        store.create(sub, "inner.txt", fattr).unwrap();

        let res = store.rmdir(ROOT_INO, "sub");
        assert_eq!(
            res.as_ref().map_err(|e| e.raw_os_error()),
            Err(Some(libc::ENOTEMPTY)),
            "rmdir 非空目录应 ENOTEMPTY，实际：{res:?}"
        );
        // 子项与目录都还在（未被破坏）。
        assert!(store.lookup(ROOT_INO, "sub").is_some(), "目录应保留");
        assert!(store.lookup(sub, "inner.txt").is_some(), "子项应保留");
        // 删空后可成功 rmdir。
        store.unlink(sub, "inner.txt").unwrap();
        store.rmdir(ROOT_INO, "sub").unwrap();
        assert!(store.lookup(ROOT_INO, "sub").is_none(), "空目录应删除");
    }

    /// C-8：readdir 须在单个读事务内一并读 dirents 与每个 child 的 inode 行（同快照），
    /// 返回自洽的条目类型——文件报 RegularFile、子目录报 Directory，不靠 unwrap_or 伪造。
    #[test]
    fn readdir_reports_consistent_kinds_in_single_txn() {
        let cs = 4096u32;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.redb");
        let store = ContainerStore::open_with_chunk_size(&path, cs).unwrap();

        let file_ino = new_file(&store, "file.txt", cs);
        let subdir_ino = store.mkdir(ROOT_INO, "subdir", dir_attr_t(cs)).unwrap();

        let entries = store.readdir(ROOT_INO);
        let file_entry = entries
            .iter()
            .find(|e| e.name == "file.txt")
            .expect("file.txt 应在 readdir 结果中");
        let dir_entry = entries
            .iter()
            .find(|e| e.name == "subdir")
            .expect("subdir 应在 readdir 结果中");

        assert_eq!(file_entry.ino, file_ino);
        assert_eq!(
            file_entry.kind,
            fuser::FileType::RegularFile,
            "文件项应报 RegularFile"
        );
        assert_eq!(dir_entry.ino, subdir_ino);
        assert_eq!(
            dir_entry.kind,
            fuser::FileType::Directory,
            "子目录项应报 Directory，不得被 unwrap_or(RegularFile) 伪造"
        );
    }

    /// compact 后数据仍可读，且物理文件不大于 compact 前（通常显著收缩）。
    #[test]
    fn data_readable_and_size_not_grown_after_compact() {
        let cs = 4096u32;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.redb");

        // 写入大量可压缩块并反复覆盖 + 删除，制造 MVCC 未引用页（膨胀来源）。
        let keep_ino;
        {
            let store = ContainerStore::open_with_chunk_size(&path, cs).unwrap();
            keep_ino = new_file(&store, "keep.bin", cs);
            let plain = vec![b'Z'; cs as usize];
            // 反复覆盖同一批块多轮，每轮 fsync 一个事务 → 旧版本页堆积。
            for _round in 0..40 {
                for idx in 0..16u64 {
                    store
                        .put_block(keep_ino, idx, mk_block(&plain), (idx + 1) * cs as u64)
                        .unwrap();
                }
                store.fsync(keep_ino).unwrap();
            }
            // 再建一个文件写入后删除，制造更多垃圾页。
            let tmp_ino = new_file(&store, "tmp.bin", cs);
            for idx in 0..32u64 {
                store
                    .put_block(tmp_ino, idx, mk_block(&plain), (idx + 1) * cs as u64)
                    .unwrap();
            }
            store.fsync(tmp_ino).unwrap();
            store.unlink(ROOT_INO, "tmp.bin").unwrap();
            store.sync_all().unwrap();
        }
        let size_before = std::fs::metadata(&path).unwrap().len();

        // compact。
        let compacted;
        {
            let mut store = ContainerStore::open(&path).unwrap();
            compacted = store.compact().unwrap();
        }
        let size_after = std::fs::metadata(&path).unwrap().len();

        assert!(
            size_after <= size_before,
            "compact 后不应增大：before={size_before} after={size_after}"
        );

        // compact 后数据完整可读。
        {
            let store = ContainerStore::open(&path).unwrap();
            let blk = store.get_block(keep_ino, 0).unwrap().expect("块0仍在");
            let plain = decompress(&blk.bytes, Algo::Zstd, blk.stored_verbatim).unwrap();
            assert_eq!(plain, vec![b'Z'; cs as usize], "compact 后数据须一致");
            assert!(
                store.lookup(ROOT_INO, "keep.bin").is_some(),
                "keep.bin 仍在"
            );
            assert!(store.lookup(ROOT_INO, "tmp.bin").is_none(), "tmp.bin 已删");
        }
        eprintln!("[OK] compact: {size_before} -> {size_after} bytes (compacted={compacted})");
    }

    /// 空/新建容器 compact 不报错（幂等边界）。
    #[test]
    fn compact_creates_new_container_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.redb");
        {
            let _ = ContainerStore::open(&path).unwrap();
        }
        let mut store = ContainerStore::open(&path).unwrap();
        // 不 panic 即可（返回 true/false 取决于是否有可回收页）。
        let _ = store.compact().unwrap();
    }

    // ----- 时间戳：InodeRow 编解码往返 + 旧档兼容 + setattr 跨 reopen 持久 -----

    #[test]
    fn inode_row_encode_decode_round_trips_times() {
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(1_750_740_420, 999);
        let row = InodeRow {
            kind: 2,
            size: 4096,
            perm: 0o644,
            uid: 7,
            gid: 9,
            chunk_size: 65536,
            mtime: t,
            atime: t,
            ctime: t,
        };
        let bytes = row.encode();
        assert_eq!(bytes.len(), INODE_ROW_LEN);
        let back = InodeRow::decode(&bytes).unwrap();
        assert_eq!(back.size, 4096);
        assert_eq!(back.mtime, t);
        assert_eq!(back.atime, t);
        assert_eq!(back.ctime, t);
    }

    #[test]
    fn decode_legacy_v1_row_yields_epoch_times() {
        // 模拟旧版本写出的 23 字节行（无时间字段）：decode 应成功且时间退化为 epoch。
        let mut v1 = [0u8; INODE_ROW_LEN_V1];
        v1[0] = 2; // 普通文件
        v1[1..9].copy_from_slice(&123u64.to_le_bytes());
        v1[9..11].copy_from_slice(&0o600u16.to_le_bytes());
        v1[19..23].copy_from_slice(&65536u32.to_le_bytes());
        let back = InodeRow::decode(&v1).unwrap();
        assert_eq!(back.size, 123);
        assert_eq!(back.mtime, std::time::SystemTime::UNIX_EPOCH);
        assert_eq!(back.ctime, std::time::SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn setattr_mtime_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.zipfs");
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(1_750_740_420, 0);
        let ino = {
            let store = ContainerStore::open(&path).unwrap();
            let ino = new_file(&store, "f.bin", 65536);
            let mut a = store.getattr_ino(ino).unwrap();
            a.mtime = t;
            store.setattr(ino, a).unwrap();
            store.sync_all().unwrap();
            ino
        };
        // 重新打开数据库，mtime 应仍是写入值（持久化到 redb）。
        let store2 = ContainerStore::open(&path).unwrap();
        let a2 = store2.getattr_ino(ino).unwrap();
        assert_eq!(a2.mtime, t, "container setattr 的 mtime 应跨 reopen 持久");
    }

    // ----- D2：unlink 与并发 flush 的孤儿块缺陷 -----

    /// redb BLOCKS 表里某 ino 的块数（绕过挂起暂存，直查持久层）。
    fn redb_block_count(store: &ContainerStore, ino: u64) -> usize {
        let txn = store.db.begin_read().unwrap();
        let blocks = txn.open_table(BLOCKS).unwrap();
        blocks.range((ino, 0)..=(ino, u64::MAX)).unwrap().count()
    }

    /// 复现 HIGH C-2 孤儿块：put_block 入 pending → unlink 删 redb inode →
    /// 并发 flush（commit_pending）若把 pending 里已删 ino 的脏块照插 redb，
    /// 就产生无 inode 引用、lookup 不可达、compact 回收不掉的孤儿块。
    ///
    /// 旧码缺陷不对称：commit_pending 的 sizes 循环对不存在 inode 已 continue，
    /// 但 blocks 循环无存在性检查 → 照插。本测试断言 commit_pending 后 redb 无该 ino 的块。
    #[test]
    fn commit_pending_does_not_resurrect_unlinked_inode_blocks() {
        let cs = 4096u32;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.redb");
        let store = ContainerStore::open_with_chunk_size(&path, cs).unwrap();

        let ino = new_file(&store, "f.bin", cs);
        let plain = vec![b'X'; cs as usize];
        // 写一块进 pending（未 fsync，仍在内存暂存）。
        store
            .put_block(ino, 0, mk_block(&plain), cs as u64)
            .unwrap();

        // unlink 删 redb inode/dirent。旧码会清 pending，但若并发 flush 抢在
        // 「redb inode 已删」与「pending 清理」之间运行，块就被复活。
        // 这里直接构造该时序的等价末态：put_block 后立刻 commit_pending，
        // 模拟另一线程在 unlink 清理 pending 之前 flush。
        store.commit_pending().unwrap();

        // unlink 删除 inode。
        store.unlink(ROOT_INO, "f.bin").unwrap();

        // 再来一次 flush（模拟 unlink 删 inode 之后、仍可能有该 ino 残留块被并发
        // 写入的窗口）。无论 pending 是否已清，commit_pending 都不得把不存在
        // inode 的块写进 redb。
        store
            .put_block(ino, 1, mk_block(&plain), 2 * cs as u64)
            .unwrap();
        store.commit_pending().unwrap();

        assert_eq!(
            redb_block_count(&store, ino),
            0,
            "已删 inode 的块不得被 commit_pending 复活进 redb（孤儿块）"
        );
        assert!(
            store.getattr_ino(ino).is_none(),
            "inode 已删，getattr 应为 None"
        );
    }

    // ----- D1：torn-read + lost-update（双缓冲根治）-----

    /// lost-update：put 多块未 fsync → 注入 commit 失败 → commit_pending 返 Err →
    /// 后续 get_block 仍须读到全部块（旧码 mem::take 在事务成败已知前清空 pending，
    /// commit 早返回即 drop 暂存 → 数据永久丢失，下次 fsync 因 pending 空假成功掩盖）。
    #[test]
    fn commit_failure_does_not_lose_pending_blocks() {
        let cs = 4096u32;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.redb");
        let store = ContainerStore::open_with_chunk_size(&path, cs).unwrap();

        let ino = new_file(&store, "f.bin", cs);
        let p0 = vec![b'A'; cs as usize];
        let p1 = vec![b'B'; cs as usize];
        store.put_block(ino, 0, mk_block(&p0), cs as u64).unwrap();
        store
            .put_block(ino, 1, mk_block(&p1), 2 * cs as u64)
            .unwrap();

        // 注入：下一次 commit_pending 的 redb commit 返回 EIO。
        store.fault_next_commit();
        let res = store.fsync(ino);
        assert!(
            res.is_err(),
            "注入故障后 commit_pending 应返回 Err，实际 {res:?}"
        );

        // 关键断言：暂存未丢，写后读仍可见两块（合并回 active）。
        let b0 = store.get_block(ino, 0).unwrap().expect("块0 不得丢失");
        let plain0 = decompress(&b0.bytes, Algo::Zstd, b0.stored_verbatim).unwrap();
        assert_eq!(plain0, p0, "块0 内容须保留");
        let b1 = store.get_block(ino, 1).unwrap().expect("块1 不得丢失");
        let plain1 = decompress(&b1.bytes, Algo::Zstd, b1.stored_verbatim).unwrap();
        assert_eq!(plain1, p1, "块1 内容须保留");
        // size read-through 仍反映最新逻辑大小。
        assert_eq!(
            store.block_geometry(ino).map(|(s, _)| s),
            Some(2 * cs as u64),
            "size 不得回退"
        );

        // 故障已清，再 fsync 应成功落盘（pending 仍含两块，非空）。
        store.fsync(ino).unwrap();
        assert_eq!(redb_block_count(&store, ino), 2, "重试后两块应落 redb");
    }

    /// torn-read 自洽：构造「flushing 有块、active 无、redb 无」的中间态（swap 后未 commit），
    /// 断言 get_block 仍返回该块、block_geometry 返回正确 size（验证读路径查三层 active∪flushing∪redb）。
    #[test]
    fn failed_commit_merge_does_not_revive_blocks_removed_by_newer_truncate() {
        let mut flushing = Pending::default();
        flushing.blocks.insert((7, 3), mk_block(b"OLD-HIGH-BLOCK"));
        flushing.sizes.insert(7, 32);

        let mut active = Pending::default();
        active.truncations.insert(7, 2);
        active.sizes.insert(7, 16);
        active.merge_from_flushing(flushing);

        assert!(!active.blocks.contains_key(&(7, 3)));
        assert_eq!(active.sizes.get(&7), Some(&16));
        assert_eq!(active.truncations.get(&7), Some(&2));
    }

    #[test]
    fn get_block_reads_from_flushing_buffer_mid_commit() {
        let cs = 4096u32;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.redb");
        let store = ContainerStore::open_with_chunk_size(&path, cs).unwrap();

        let ino = new_file(&store, "f.bin", cs);
        let plain = vec![b'Q'; cs as usize];
        store
            .put_block(ino, 0, mk_block(&plain), cs as u64)
            .unwrap();

        // 模拟 commit 进行中：把 active swap 进 flushing，但尚未把块落 redb。
        // 此刻 active 空、redb 无块、块只在 flushing。旧单缓冲读路径只查 active+redb → 撕裂为 None。
        store.test_swap_active_into_flushing();

        let blk = store
            .get_block(ino, 0)
            .unwrap()
            .expect("中间态 get_block 须从 flushing 读到块（消灭 torn read）");
        let got = decompress(&blk.bytes, Algo::Zstd, blk.stored_verbatim).unwrap();
        assert_eq!(got, plain, "flushing 中的块内容须正确");
        assert_eq!(
            store.block_geometry(ino).map(|(s, _)| s),
            Some(cs as u64),
            "size 须从 flushing read-through，不撕裂为旧值"
        );
        // truncation 跨缓冲：flushing 有 ino 块、active 无 → idx>=1 应越界 None（无残留）。
        assert!(
            store.get_block(ino, 1).unwrap().is_none(),
            "未写过的块仍 None"
        );
    }

    // ----- 指标埋点：commit 成功/失败计数经注入的 Metrics 注册表可观测 -----

    /// 注入 `Arc<Metrics>`：put 两块 → fsync（记 commit_ok + 2 块）→ 注入 commit 失败再
    /// fsync（记 commit_failed）→ 断言注册表计数与序列化输出一致。复用 `fault_next_commit`。
    #[test]
    fn commit_metrics_count_ok_and_failed() {
        use crate::core::metrics::Metrics;

        let cs = 4096u32;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.redb");
        let metrics = Metrics::new();
        let store = ContainerStore::open_with_chunk_size(&path, cs)
            .unwrap()
            .with_metrics(metrics.clone());

        let ino = new_file(&store, "f.bin", cs);
        let plain = vec![b'M'; cs as usize];
        store
            .put_block(ino, 0, mk_block(&plain), cs as u64)
            .unwrap();
        store
            .put_block(ino, 1, mk_block(&plain), 2 * cs as u64)
            .unwrap();

        // fsync → commit 成功，记 1 次 commit_ok + 2 块 flushed。
        store.fsync(ino).unwrap();

        // 再写一块，注入 commit 失败 → 记 1 次 commit_failed（内容合并回 active，不丢）。
        store
            .put_block(ino, 2, mk_block(&plain), 3 * cs as u64)
            .unwrap();
        store.fault_next_commit();
        assert!(store.fsync(ino).is_err(), "注入故障后 fsync 应返回 Err");

        let mut out = String::new();
        metrics.write_prometheus(&mut out);
        assert!(
            out.contains("scrollz_commit_ok_total 1"),
            "应记 1 次成功提交：\n{out}"
        );
        assert!(
            out.contains("scrollz_blocks_flushed_total 2"),
            "应累计 2 块落盘（失败那次不计）：\n{out}"
        );
        assert!(
            out.contains("scrollz_commit_failed_total 1"),
            "应记 1 次失败提交：\n{out}"
        );
        assert!(
            out.contains("scrollz_flushing_bytes_peak "),
            "峰值 gauge 应存在：\n{out}"
        );
        // 峰值应反映曾观测到的最大 flushing 字节（>0，两块编码后字节和）。
        let peak_line = out
            .lines()
            .find(|l| l.starts_with("scrollz_flushing_bytes_peak "))
            .expect("峰值行存在");
        let peak: u64 = peak_line
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .expect("峰值可解析");
        assert!(peak > 0, "flushing 峰值应 > 0，实际 {peak}");
    }
}
