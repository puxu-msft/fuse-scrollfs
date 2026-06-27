//! 布局 V —— 单容器 / 虚拟盘（P1+ 填充）。
//!
//! 设计见 docs/01-zipfs-design.md §6。整棵树（元数据 + 数据块）落进一个容器文件；
//! 首版默认 redb 全包（表 inodes / dirents / blocks，blob key=(ino,idx)）。
//! 真正的重头是「变长 blob 分配器 + 空闲管理 + 事务」，复用 redb 的 ACID B-tree 而非手搓。
//! P0 不引 redb，本文件仅占位。

use super::{Attr, DirEntry, Store, StoredBlock};
use crate::core::inode::Ino;

/// 容器后端（布局 V）。P1+ 接入 redb。
pub struct ContainerStore {
    // P1+：redb::Database 句柄、表定义、空闲管理状态。
}

impl ContainerStore {
    #[allow(dead_code)]
    pub fn open(_path: &std::path::Path) -> std::io::Result<Self> {
        todo!("P1+：打开/创建 redb 容器，见 docs/01-zipfs-design.md §6.1")
    }
}

impl Store for ContainerStore {
    fn lookup(&self, _parent: Ino, _name: &str) -> Option<Attr> {
        todo!("P1+ ContainerStore::lookup")
    }
    fn create(&self, _parent: Ino, _name: &str, _attr: Attr) -> Ino {
        todo!("P1+ ContainerStore::create")
    }
    fn mkdir(&self, _parent: Ino, _name: &str, _attr: Attr) -> Ino {
        todo!("P1+ ContainerStore::mkdir")
    }
    fn unlink(&self, _parent: Ino, _name: &str) {
        todo!("P1+ ContainerStore::unlink")
    }
    fn rmdir(&self, _parent: Ino, _name: &str) {
        todo!("P1+ ContainerStore::rmdir")
    }
    fn rename(&self, _old: (Ino, &str), _new: (Ino, &str)) {
        todo!("P1+ ContainerStore::rename")
    }
    fn readdir(&self, _dir: Ino) -> Vec<DirEntry> {
        todo!("P1+ ContainerStore::readdir")
    }
    fn setattr(&self, _ino: Ino, _attr: Attr) {
        todo!("P1+ ContainerStore::setattr")
    }
    fn getattr_ino(&self, _ino: Ino) -> Option<Attr> {
        todo!("P1+ ContainerStore::getattr_ino")
    }
    fn get_block(&self, _ino: Ino, _idx: u64) -> Option<StoredBlock> {
        todo!("P1+ ContainerStore::get_block")
    }
    fn block_geometry(&self, _ino: Ino) -> Option<(u64, u32)> {
        todo!("P1+ ContainerStore::block_geometry")
    }
    fn put_block(&self, _ino: Ino, _idx: u64, _blk: StoredBlock) {
        todo!("P1+ ContainerStore::put_block")
    }
    fn truncate_blocks(&self, _ino: Ino, _keep_from: u64) {
        todo!("P1+ ContainerStore::truncate_blocks")
    }
    fn fsync(&self, _ino: Ino) {
        todo!("P1+ ContainerStore::fsync")
    }
    fn sync_all(&self) {
        todo!("P1+ ContainerStore::sync_all")
    }
}
