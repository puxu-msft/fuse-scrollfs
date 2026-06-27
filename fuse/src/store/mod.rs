//! `Store` 接缝：可插拔的唯一差异面（P1+ 填充实现）。
//!
//! 设计见 docs/01-zipfs-design.md §5。`Store` 只管「不透明已压缩块 + 命名空间 + 属性」，
//! 不碰压缩——压缩在 Core 完成。两布局（V 容器 / S 影子树）各实现一份。
//! P0 透传不经过本层，仅保留 trait 签名与占位实现骨架，让 P1 起步即有稳定接缝。
//!
//! P0 阶段这些类型尚未被 wire-in，故整体 allow(dead_code)；P1 起逐步去除。
#![allow(dead_code)]

pub mod container;
pub mod shadow;

use crate::core::inode::Ino;

/// 目录项（readdir 返回）。
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: Ino,
    pub name: String,
    /// 文件类型（目录 / 普通文件等），对齐 libc 的 d_type 语义。
    pub kind: fuser::FileType,
}

/// Store 层的文件属性（与 Core 的 LogicalAttr 区分：这里是 Store 持久化的元数据视图）。
#[derive(Debug, Clone)]
pub struct Attr {
    pub ino: Ino,
    pub size: u64,
    pub kind: fuser::FileType,
    pub perm: u16,
    pub uid: u32,
    pub gid: u32,
}

/// 一个已压缩的存储块：不透明字节 + flags（压缩在 Core 完成，Store 不感知内容）。
#[derive(Debug, Clone)]
pub struct StoredBlock {
    pub bytes: Vec<u8>,
    /// 是否原样存储（不可压缩启发式置位），见 §3。
    pub stored_verbatim: bool,
}

/// 可插拔后端接缝。`--backend {container|shadow}` 切换实现，见 §11。
///
/// 注意 `fsync(ino)` 与 `sync_all()` 分开：POSIX `fsync(fd)` 只保证单文件落盘，
/// 若只有无参 `sync()`，容器布局会被迫全库 commit，单文件 fsync 跑分虚高、不可比（§5）。
pub trait Store: Send + Sync {
    // 命名空间 / 元数据
    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr>;
    fn create(&self, parent: Ino, name: &str, attr: Attr) -> Ino;
    fn mkdir(&self, parent: Ino, name: &str, attr: Attr) -> Ino;
    fn unlink(&self, parent: Ino, name: &str);
    fn rmdir(&self, parent: Ino, name: &str);
    fn rename(&self, old: (Ino, &str), new: (Ino, &str));
    fn readdir(&self, dir: Ino) -> Vec<DirEntry>;
    fn setattr(&self, ino: Ino, attr: Attr);

    /// 按 ino 取属性（getattr 用，根 inode 亦走这里）。返回 None 表示该 ino 不存在。
    fn getattr_ino(&self, ino: Ino) -> Option<Attr>;

    // 数据：StoredBlock = 已压缩字节 + flags
    fn get_block(&self, ino: Ino, idx: u64) -> Option<StoredBlock>;

    /// 取某 inode 的 `(uncompressed_size, chunk_size)`，供 Core 算块范围与末块长度。
    /// 返回 None 表示不是可分块的普通文件（目录 / 不存在）。
    fn block_geometry(&self, ino: Ino) -> Option<(u64, u32)>;
    fn put_block(&self, ino: Ino, idx: u64, blk: StoredBlock);
    /// 截断：丢弃 `keep_from` 及之后的块。
    fn truncate_blocks(&self, ino: Ino, keep_from: u64);

    // 持久化
    /// 单文件持久化（POSIX fsync 语义）。
    fn fsync(&self, ino: Ino);
    /// 全局 barrier。
    fn sync_all(&self);
}
