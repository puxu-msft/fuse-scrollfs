//! inode 属性 / 句柄表占位（P1+ 填充压缩相关元数据）。
//!
//! 设计见 docs/01-scrollz-design.md §4「FUSE 层」。inode 分配、内存 attr 缓存、打开句柄表、
//! lookup-count / forget 延迟回收、每-inode 锁都属此层职责。
//! P0 透传阶段在 `passthrough.rs` 内自带一份精简实现；这里保留压缩文件系统专用的
//! 「逻辑大小 + chunk_size + chunk 索引」属性骨架，供 P1+ 的 V/S 两布局复用。

/// inode 编号类型别名，对齐 §5 Store trait 的 `Ino`。
pub type Ino = u64;

/// 压缩文件系统的逻辑 inode 属性（区别于底层 FS 的物理 stat）。
///
/// P0 透传直接复用底层 stat，不用本结构；P1+ 容器布局需要它来记录逻辑大小与分块参数。
#[derive(Debug, Clone)]
pub struct LogicalAttr {
    pub ino: Ino,
    /// 逻辑（解压后）文件大小，与底层物理占用解耦。
    pub uncompressed_size: u64,
    /// 该文件采用的块大小（允许逐文件不同，便于基准扫描）。
    pub chunk_size: u32,
}

impl LogicalAttr {
    #[allow(unused_variables)]
    pub fn new(ino: Ino, chunk_size: u32) -> Self {
        todo!("P1+：构造逻辑属性，见 docs/01-scrollz-design.md §4")
    }
}
