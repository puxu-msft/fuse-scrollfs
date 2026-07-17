//! 内存 `Store` 实现：测试支撑件（rmw 单测的后端 + 差分测试的「被测 Store」之一）。
//!
//! 公开但仅供测试用——它把已压缩块存内存，行为对齐真实后端（codec 在 Core，本层只搬运
//! 不透明字节）。不写盘，故无崩溃一致性语义；用于隔离 Core 写编排逻辑的正确性。

use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

use super::{Attr, DirEntry, Store, StoredBlock};
use crate::core::inode::Ino;

const ROOT_INO: u64 = 1;

#[derive(Default)]
struct Node {
    /// 普通文件的块（idx → 压缩块）；目录为空。
    blocks: HashMap<u64, StoredBlock>,
    attr: Option<Attr>,
    /// 目录子项：name → ino。
    children: HashMap<String, u64>,
}

struct Inner {
    nodes: HashMap<u64, Node>,
    next_ino: u64,
}

/// 内存 Store。线程安全由一把 Mutex 提供（测试场景够用）。
pub struct MemStore {
    inner: Mutex<Inner>,
    default_chunk_size: u32,
}

impl MemStore {
    pub fn new(chunk_size: u32) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_INO,
            Node {
                attr: Some(Attr {
                    ino: ROOT_INO,
                    size: 0,
                    kind: fuser::FileType::Directory,
                    perm: 0o755,
                    uid: 0,
                    gid: 0,
                    mtime: std::time::SystemTime::UNIX_EPOCH,
                    atime: std::time::SystemTime::UNIX_EPOCH,
                    ctime: std::time::SystemTime::UNIX_EPOCH,
                    chunk_size,
                }),
                ..Default::default()
            },
        );
        Self {
            inner: Mutex::new(Inner { nodes, next_ino: 2 }),
            default_chunk_size: chunk_size,
        }
    }

    /// 直接在根下建一个匿名普通文件（rmw 单测便捷入口），返回 ino。
    pub fn new_file(&self) -> Ino {
        let mut g = self.inner.lock().unwrap();
        let ino = g.next_ino;
        g.next_ino += 1;
        let cs = self.default_chunk_size;
        g.nodes.insert(
            ino,
            Node {
                attr: Some(Attr {
                    ino,
                    size: 0,
                    kind: fuser::FileType::RegularFile,
                    perm: 0o644,
                    uid: 0,
                    gid: 0,
                    mtime: std::time::SystemTime::UNIX_EPOCH,
                    atime: std::time::SystemTime::UNIX_EPOCH,
                    ctime: std::time::SystemTime::UNIX_EPOCH,
                    chunk_size: cs,
                }),
                ..Default::default()
            },
        );
        ino
    }
}

impl Store for MemStore {
    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
        let g = self.inner.lock().unwrap();
        let child = *g.nodes.get(&parent)?.children.get(name)?;
        g.nodes.get(&child)?.attr.clone()
    }

    fn create(&self, parent: Ino, name: &str, mut attr: Attr) -> io::Result<Ino> {
        let mut g = self.inner.lock().unwrap();
        if !g.nodes.contains_key(&parent) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "父目录不存在"));
        }
        let ino = g.next_ino;
        {
            let children = &mut g.nodes.get_mut(&parent).unwrap().children;
            if children.contains_key(name) {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "已存在"));
            }
            children.insert(name.to_string(), ino);
        }
        g.next_ino += 1;
        attr.ino = ino;
        g.nodes.insert(
            ino,
            Node {
                attr: Some(attr),
                ..Default::default()
            },
        );
        Ok(ino)
    }

    fn mkdir(&self, parent: Ino, name: &str, attr: Attr) -> io::Result<Ino> {
        self.create(parent, name, attr)
    }

    fn unlink(&self, parent: Ino, name: &str) -> io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let child = g
            .nodes
            .get_mut(&parent)
            .and_then(|p| p.children.remove(name))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "不存在"))?;
        g.nodes.remove(&child);
        Ok(())
    }

    fn rmdir(&self, parent: Ino, name: &str) -> io::Result<()> {
        self.unlink(parent, name)
    }

    fn rename(&self, old: (Ino, &str), new: (Ino, &str)) -> io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let child = g
            .nodes
            .get_mut(&old.0)
            .and_then(|p| p.children.remove(old.1))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "源不存在"))?;
        // 覆盖目标若存在。
        if let Some(prev) = g
            .nodes
            .get_mut(&new.0)
            .and_then(|p| p.children.remove(new.1))
        {
            g.nodes.remove(&prev);
        }
        g.nodes
            .get_mut(&new.0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "目标父目录不存在"))?
            .children
            .insert(new.1.to_string(), child);
        Ok(())
    }

    fn readdir(&self, dir: Ino) -> Vec<DirEntry> {
        let g = self.inner.lock().unwrap();
        let Some(node) = g.nodes.get(&dir) else {
            return Vec::new();
        };
        node.children
            .iter()
            .filter_map(|(name, &ino)| {
                let attr = g.nodes.get(&ino)?.attr.as_ref()?;
                Some(DirEntry {
                    ino,
                    name: name.clone(),
                    kind: attr.kind,
                })
            })
            .collect()
    }

    fn setattr(&self, ino: Ino, attr: Attr) -> io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let node = g
            .nodes
            .get_mut(&ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "不存在"))?;
        if let Some(existing) = node.attr.as_mut() {
            existing.size = attr.size;
            existing.perm = attr.perm;
            existing.uid = attr.uid;
            existing.gid = attr.gid;
        }
        Ok(())
    }

    fn getattr_ino(&self, ino: Ino) -> Option<Attr> {
        self.inner.lock().unwrap().nodes.get(&ino)?.attr.clone()
    }

    fn get_block(&self, ino: Ino, idx: u64) -> io::Result<Option<StoredBlock>> {
        let g = self.inner.lock().unwrap();
        Ok(g.nodes.get(&ino).and_then(|n| n.blocks.get(&idx).cloned()))
    }

    fn block_geometry(&self, ino: Ino) -> Option<(u64, u32)> {
        let g = self.inner.lock().unwrap();
        let attr = g.nodes.get(&ino)?.attr.as_ref()?;
        if attr.kind != fuser::FileType::RegularFile {
            return None;
        }
        Some((attr.size, attr.chunk_size))
    }

    fn put_block(&self, ino: Ino, idx: u64, blk: StoredBlock, new_size: u64) -> io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let node = g
            .nodes
            .get_mut(&ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "不存在"))?;
        node.blocks.insert(idx, blk);
        if let Some(attr) = node.attr.as_mut() {
            attr.size = new_size;
        }
        Ok(())
    }

    fn truncate_blocks(&self, ino: Ino, keep_from: u64, new_size: u64) -> io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let node = g
            .nodes
            .get_mut(&ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "不存在"))?;
        node.blocks.retain(|&idx, _| idx < keep_from);
        if let Some(attr) = node.attr.as_mut() {
            attr.size = new_size;
        }
        Ok(())
    }

    fn fsync(&self, _ino: Ino) -> io::Result<()> {
        Ok(())
    }

    fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }
}
