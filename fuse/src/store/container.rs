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

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Mutex;

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
}

/// 容器后端（布局 V）。
pub struct ContainerStore {
    db: Database,
    next_ino: Mutex<u64>,
    /// 写批处理的挂起暂存（§6.1）。fsync/flush/sync_all 才落 redb 事务。
    pending: Mutex<Pending>,
    default_chunk_size: u32,
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
            pending: Mutex::new(Pending::default()),
            default_chunk_size,
        })
    }

    fn alloc_ino(&self) -> u64 {
        let mut g = self.next_ino.lock().unwrap();
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
        // size read-through 挂起暂存（写后读一致）。
        if let Some(&sz) = self.pending.lock().unwrap().sizes.get(&ino) {
            row.size = sz;
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
            inodes
                .remove(child)
                .map_err(|e| db_err("remove inode", e))?;
            // 删除该 inode 的所有数据块。
            let to_del: Vec<(u64, u64)> = blocks
                .range((child, 0)..(child + 1, 0))
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
        // 清挂起暂存里该 inode 的残留。
        {
            let mut p = self.pending.lock().unwrap();
            p.blocks.retain(|&(i, _), _| i != removed_child);
            p.sizes.remove(&removed_child);
            p.truncations.remove(&removed_child);
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
                    .range((prev, 0)..(prev + 1, 0))
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
            let kind = self
                .read_inode(child)
                .ok()
                .flatten()
                .map(|r| r.kind_to_filetype())
                .unwrap_or(fuser::FileType::RegularFile);
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
        // 1) read-through 挂起暂存。
        {
            let p = self.pending.lock().unwrap();
            if let Some(blk) = p.blocks.get(&(ino, idx)) {
                return Ok(Some(blk.clone()));
            }
            if let Some(&keep_from) = p.truncations.get(&ino) {
                if idx >= keep_from {
                    return Ok(None);
                }
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
        // size read-through 挂起。
        let size = self
            .pending
            .lock()
            .unwrap()
            .sizes
            .get(&ino)
            .copied()
            .unwrap_or(row.size);
        Some((size, row.chunk_size))
    }

    fn put_block(&self, ino: Ino, idx: u64, blk: StoredBlock, new_size: u64) -> io::Result<()> {
        let mut p = self.pending.lock().unwrap();
        p.blocks.insert((ino, idx), blk);
        p.sizes.insert(ino, new_size);
        Ok(())
    }

    fn truncate_blocks(&self, ino: Ino, keep_from: u64, new_size: u64) -> io::Result<()> {
        let mut p = self.pending.lock().unwrap();
        // 丢弃挂起块中 >= keep_from 的。
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
    /// 把挂起暂存合并到一个 redb 写事务并 commit（写批处理核心，§6.1）。
    fn commit_pending(&self) -> io::Result<()> {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        if pending.is_empty() {
            return Ok(());
        }
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

            // 截断：删除 >= keep_from 的块。
            for (&ino, &keep_from) in &pending.truncations {
                let to_del: Vec<(u64, u64)> = blocks
                    .range((ino, keep_from)..(ino + 1, 0))
                    .map_err(|e| db_err("range blocks", e))?
                    .map(|r| r.map(|(k, _)| k.value()))
                    .collect::<Result<_, _>>()
                    .map_err(|e| db_err("collect blocks", e))?;
                for k in to_del {
                    blocks.remove(k).map_err(|e| db_err("remove block", e))?;
                }
            }
            // 写挂起块。
            for (&(ino, idx), blk) in &pending.blocks {
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
        txn.commit().map_err(|e| db_err("commit pending", e))?;
        Ok(())
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

    /// compact 后数据仍可读，且物理文件不大于 compact 前（通常显著收缩）。
    #[test]
    fn compact_后数据可读且体积不增() {
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
    fn compact_新建容器不报错() {
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
}
