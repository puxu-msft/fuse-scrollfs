//! 布局 S —— 影子树 / 每文件压缩包（P1：只读 + 顺序读路径）。
//!
//! 设计见 docs/01-zipfs-design.md §7。底层目录树**镜像**逻辑树：逻辑 `/a/b.txt`
//! → 后端 `BACKING/a/b.txt`，该后端文件是一个**分块压缩包**（archive.rs 的 footer 布局，
//! 不是单 zstd 流——那正是 fuse-zstd 随机写弱的根因）。
//!
//! P1 范围（本文件）：
//! - **读侧**：lookup / readdir / getattr 走底层镜像目录的真实 stat（mode/uid/gid/mtime
//!   复用底层 inode）；普通文件的逻辑大小 `uncompressed_size` 取自 archive footer（§7）。
//! - `get_block(ino, idx)` 打开对应后端 archive、读块，返回 `StoredBlock`（压缩字节 + flags），
//!   **解压交给 Core**（§2「压缩在 Core」），Store 只搬运不透明字节。
//! - 写侧方法（create/put_block/...）P1 不实现，留 `unimplemented!()`；只读挂载不会触达，
//!   P2 再补 temp+rename 的 RMW（§7）。
//!
//! inode 映射：ShadowStore 自维护一份 `ino ↔ 相对路径` 表（类似 passthrough，但只读语义）。

use super::{Attr, DirEntry, Store, StoredBlock};
use crate::archive::ArchiveReader;
use crate::core::inode::Ino;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 根 inode 编号，对齐 FUSE 约定。
const ROOT_INO: u64 = 1;

/// ShadowStore 内部的 inode 映射：ino ↔ 相对 backing 根的路径。
///
/// 只读场景下不需要 lookup-count 延迟回收（无 unlink-while-open 写语义），故比
/// passthrough 的表更精简：只保证「同一路径同一 ino」稳定即可。
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
}

/// 影子树后端（布局 S）。`backing` 为底层目录根（archive 树）。
pub struct ShadowStore {
    backing: PathBuf,
    inodes: Mutex<InodeMap>,
}

impl ShadowStore {
    /// 用底层 archive 树根构造。`backing` 必须存在且为目录。
    pub fn open(backing: PathBuf) -> std::io::Result<Self> {
        let meta = fs::metadata(&backing)?;
        if !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("backing 不是目录：{}", backing.display()),
            ));
        }
        // 注：§7 提到逻辑大小可放 xattr 以免「为读属性而 open 每个 header」；P1 先用
        // 「读 footer 取 uncompressed_size」求正确，xattr 优化留作后续元数据阶段。
        Ok(Self {
            backing,
            inodes: Mutex::new(InodeMap::new()),
        })
    }

    /// 把相对路径解析为 backing 下的绝对路径。
    fn abs_of(&self, rel: &Path) -> PathBuf {
        self.backing.join(rel)
    }

    /// 取某 ino 的相对路径。
    fn rel_of(&self, ino: Ino) -> Option<PathBuf> {
        self.inodes.lock().unwrap().path_of(ino)
    }

    /// 由底层 metadata + 相对路径构造 Store 层 `Attr`。
    ///
    /// 普通文件的 `size` 取**逻辑大小**（archive footer 的 uncompressed_size），
    /// 而非底层物理文件大小——否则上层会按压缩后字节数去读，读不全（§7）。
    /// 目录与非 archive 文件直接用底层 size。
    fn attr_from_meta(&self, ino: Ino, meta: &fs::Metadata, abs: &Path) -> Attr {
        let kind = filetype_from_meta(meta);
        let size = if kind == fuser::FileType::RegularFile {
            logical_size(abs).unwrap_or_else(|| meta.size())
        } else {
            meta.size()
        };
        Attr {
            ino,
            size,
            kind,
            perm: (meta.mode() & 0o7777) as u16,
            uid: meta.uid(),
            gid: meta.gid(),
        }
    }
}

/// 读 archive footer 取逻辑大小；非 archive / 打不开则返回 None（回退底层 size）。
fn logical_size(abs: &Path) -> Option<u64> {
    ArchiveReader::open(abs)
        .ok()
        .map(|r| r.footer().uncompressed_size)
}

/// 由底层 metadata 推 FUSE FileType。
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
                // 非 UTF-8 名字：P1 跳过（§7 提及超长/非法名可逆编码留作后续）。
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

    /// 按 ino 取属性（getattr）。根 inode 也走这里。
    fn getattr_ino(&self, ino: Ino) -> Option<Attr> {
        let rel = self.rel_of(ino)?;
        let abs = self.abs_of(&rel);
        let meta = fs::symlink_metadata(&abs).ok()?;
        Some(self.attr_from_meta(ino, &meta, &abs))
    }

    fn get_block(&self, ino: Ino, idx: u64) -> Option<StoredBlock> {
        let rel = self.rel_of(ino)?;
        let abs = self.abs_of(&rel);
        let reader = ArchiveReader::open(&abs).ok()?;
        let (bytes, entry) = reader.read_block(idx).ok()??;
        Some(StoredBlock {
            bytes,
            stored_verbatim: entry.is_verbatim(),
        })
    }

    /// 取某 inode 的逻辑大小 + chunk_size（供 Core 计算块范围 / 末块长度）。
    fn block_geometry(&self, ino: Ino) -> Option<(u64, u32)> {
        let rel = self.rel_of(ino)?;
        let abs = self.abs_of(&rel);
        let reader = ArchiveReader::open(&abs).ok()?;
        let f = reader.footer();
        Some((f.uncompressed_size, f.chunk_size))
    }

    // ----- 写侧：P1 不实现（只读挂载不触达），P2 补 temp+rename RMW（§7） -----

    fn create(&self, _parent: Ino, _name: &str, _attr: Attr) -> Ino {
        unimplemented!("P2 ShadowStore::create（写路径）")
    }
    fn mkdir(&self, _parent: Ino, _name: &str, _attr: Attr) -> Ino {
        unimplemented!("P2 ShadowStore::mkdir（写路径）")
    }
    fn unlink(&self, _parent: Ino, _name: &str) {
        unimplemented!("P2 ShadowStore::unlink（写路径）")
    }
    fn rmdir(&self, _parent: Ino, _name: &str) {
        unimplemented!("P2 ShadowStore::rmdir（写路径）")
    }
    fn rename(&self, _old: (Ino, &str), _new: (Ino, &str)) {
        unimplemented!("P2 ShadowStore::rename（写路径）")
    }
    fn setattr(&self, _ino: Ino, _attr: Attr) {
        unimplemented!("P2 ShadowStore::setattr（写路径）")
    }
    fn put_block(&self, _ino: Ino, _idx: u64, _blk: StoredBlock) {
        unimplemented!("P2 ShadowStore::put_block（写路径）")
    }
    fn truncate_blocks(&self, _ino: Ino, _keep_from: u64) {
        unimplemented!("P2 ShadowStore::truncate_blocks（写路径）")
    }
    fn fsync(&self, _ino: Ino) {
        unimplemented!("P2 ShadowStore::fsync（写路径）")
    }
    fn sync_all(&self) {
        unimplemented!("P2 ShadowStore::sync_all（写路径）")
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
}
