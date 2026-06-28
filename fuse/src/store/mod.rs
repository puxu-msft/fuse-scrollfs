//! `Store` 接缝：可插拔的唯一差异面（P2/P3 写路径已落地）。
//!
//! 设计见 docs/01-zipfs-design.md §5。`Store` 只管「不透明已压缩块 + 命名空间 + 属性」，
//! 不碰压缩——压缩在 Core 完成。两布局（V 容器 / S 影子树）各实现一份。
//!
//! ## 写批处理契约（§6.1：必备项，非优化）
//!
//! 一次 FUSE `write` 回调内 Core 可能产出多块 `put_block`/`truncate_blocks`。Store **不应**
//! 每块各自落盘——而是把这些变更累积在内存（per-inode 脏块缓冲 / 一个挂起的 redb 写事务），
//! 仅在 `fsync(ino)` / `sync_all()` 时真正持久化提交。`get_block` 必须 **read-through** 脏缓冲，
//! 让同一会话内「写后读」可见。这对齐 §6.1 microbench 结论（批量事务比每块一事务快 8–18x），
//! 也避免重蹈 `sqlitefs`「每写 COW sync」覆辙。

pub mod container;
pub mod shadow;

#[cfg(test)]
pub mod tests_support;

use crate::core::inode::Ino;
use std::io;

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
    /// 该普通文件的逻辑块大小（目录可填默认值，不参与分块）。create 时由前端给定，
    /// 后续 get/put_block 与 Core 分块数学都以它为准。
    pub chunk_size: u32,
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
///
/// 写方法返回 `io::Result`：底层 syscall / 事务可能失败，必须显式向上传递映射成 errno，
/// 不得静默吞掉（用户规则「错误显式处理」+ §10 一致性）。
pub trait Store: Send + Sync {
    // ---- 命名空间 / 元数据 ----
    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr>;
    fn create(&self, parent: Ino, name: &str, attr: Attr) -> io::Result<Ino>;
    fn mkdir(&self, parent: Ino, name: &str, attr: Attr) -> io::Result<Ino>;
    fn unlink(&self, parent: Ino, name: &str) -> io::Result<()>;
    fn rmdir(&self, parent: Ino, name: &str) -> io::Result<()>;
    fn rename(&self, old: (Ino, &str), new: (Ino, &str)) -> io::Result<()>;
    fn readdir(&self, dir: Ino) -> Vec<DirEntry>;
    /// 更新属性。`size`/`perm`/`uid`/`gid` 取自 `attr`；`chunk_size` 不可变更（建文件时定）。
    fn setattr(&self, ino: Ino, attr: Attr) -> io::Result<()>;

    /// 按 ino 取属性（getattr 用，根 inode 亦走这里）。返回 None 表示该 ino 不存在。
    fn getattr_ino(&self, ino: Ino) -> Option<Attr>;

    // ---- 数据：StoredBlock = 已压缩字节 + flags ----
    /// 取第 `idx` 块（须 read-through 脏缓冲，见写批处理契约）。越界返回 Ok(None)。
    fn get_block(&self, ino: Ino, idx: u64) -> io::Result<Option<StoredBlock>>;

    /// 取某 inode 的 `(uncompressed_size, chunk_size)`，供 Core 算块范围与末块长度。
    /// 返回 None 表示不是可分块的普通文件（目录 / 不存在）。
    fn block_geometry(&self, ino: Ino) -> Option<(u64, u32)>;

    /// 写第 `idx` 块（累积进脏缓冲，fsync/flush 才落盘）。`new_size` 是该写之后文件应有的
    /// 逻辑大小（Core 据 off+len 与原大小取 max 算好；append/越 EOF 时增大）。
    fn put_block(&self, ino: Ino, idx: u64, blk: StoredBlock, new_size: u64) -> io::Result<()>;

    /// 截断到 `new_size`：丢弃 `keep_from` 及之后的块，并把逻辑大小改为 `new_size`。
    /// `keep_from` = ceil(new_size / chunk_size)（末块若被部分截断由 Core 先 RMW 再调本方法）。
    fn truncate_blocks(&self, ino: Ino, keep_from: u64, new_size: u64) -> io::Result<()>;

    // ---- 持久化 ----
    /// 单文件持久化（POSIX fsync 语义）：把该 inode 的脏缓冲落盘 + 后端 fsync。
    fn fsync(&self, ino: Ino) -> io::Result<()>;
    /// flush（FUSE flush 回调）：默认与 fsync 同义提交该 inode 的挂起写，保证 close 前可见。
    fn flush(&self, ino: Ino) -> io::Result<()> {
        self.fsync(ino)
    }
    /// 全局 barrier：提交所有挂起写。
    fn sync_all(&self) -> io::Result<()>;

    /// FUSE `release`（最后一个 fd 关闭）通知：后端可借此释放 per-inode 缓存资源
    /// （如布局 S 的已打开 `ArchiveReader` 缓存）。默认空实现——不缓存的后端无需关心。
    /// 注意：仅作为「释放缓存」的提示，不承担持久化职责（落盘由 flush/fsync 负责）。
    fn release(&self, _ino: Ino) {}
}
