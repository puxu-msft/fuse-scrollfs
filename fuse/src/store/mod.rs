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
pub(crate) mod lock;
pub mod shadow;

#[cfg(test)]
pub mod tests_support;

use crate::core::inode::Ino;
use std::io;

/// 校验单个目录项名，作为 Store 写入口（create/mkdir/symlink/rename 新名）的不变量。
///
/// 评审 E1：把路径安全做成**后端契约**，不依赖调用方恰好喂干净数据。挂载期 FUSE 内核会过滤
/// `/`、`.`、`..`，但 `ingest_dir_into_store` 直接把 `read_dir` 名喂给 `Store::create/mkdir`
/// **绕过内核**——一旦源含病态名（`/` 污染 container 键空间、`..` 让 shadow `join` 逃出 backing），
/// 无此防线即可越界。拒空名 / 含 NUL / `.` / `..` / 含 `/`。
pub(crate) fn validate_name(name: &str) -> io::Result<()> {
    let bad = |msg: &str| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("非法目录项名 {name:?}：{msg}"),
        )
    };
    if name.is_empty() {
        return Err(bad("不能为空"));
    }
    if name.contains('\0') {
        return Err(bad("含 NUL"));
    }
    if name == "." || name == ".." {
        return Err(bad("不能为 . 或 .."));
    }
    if name.contains('/') {
        return Err(bad("不能含 /"));
    }
    Ok(())
}

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
    /// 修改时间 / 访问时间 / 状态变更时间。getattr 经 `to_file_attr` 直接呈现给内核——
    /// shadow 后端由底层文件 `meta` 取真值，container 由 InodeRow 存取。缺失退化为 UNIX_EPOCH。
    /// 历史 bug：本结构曾无时间字段，前端把四时间写死 1970。
    pub mtime: std::time::SystemTime,
    pub atime: std::time::SystemTime,
    pub ctime: std::time::SystemTime,
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

    /// 读取符号链接目标。默认不支持（ENOSYS）；shadow 路径镜像后端实现（serve Claude `memory` 外链）。
    fn readlink(&self, _ino: Ino) -> io::Result<std::path::PathBuf> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }

    /// 创建符号链接，返回新条目属性。默认不支持（ENOSYS）；shadow 后端实现。
    fn symlink(&self, _parent: Ino, _name: &str, _target: &std::path::Path) -> io::Result<Attr> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }

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

    // ---- in-archive 尾日志（写放大根治，docs/04 §8.4）----
    /// 该后端是否支持 in-archive 尾日志（fsync 只追加未封尾块的原始增量，不重压整块）。
    /// 默认 false——容器布局（redb 自带 WAL）走旧 put_block 路径。ShadowStore 重写为 true。
    fn supports_tail_journal(&self) -> bool {
        false
    }

    /// fsync 路径：把未封尾块自上次以来的**原始字节增量**追加进 archive 尾日志并 durable
    /// （O(delta)，不压缩、不重写整块）。`new_size` 是该 fsync 后文件应有的逻辑大小（含未封尾）。
    /// 默认 `Unsupported`——仅 `supports_tail_journal()` 为 true 的后端实现。
    fn append_tail(&self, _ino: Ino, _delta: &[u8], _new_size: u64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "该后端不支持尾日志（append_tail）",
        ))
    }

    /// 封块：把累积的尾块作为压缩块 `idx` 落盘并**重置尾日志**（idx 转为不可变封块）。
    /// 默认 = `put_block`（无尾日志的后端，封块即普通写块）。ShadowStore 重置 journal。
    fn seal_tail_block(
        &self,
        ino: Ino,
        idx: u64,
        blk: StoredBlock,
        new_size: u64,
    ) -> io::Result<()> {
        self.put_block(ino, idx, blk, new_size)
    }

    // ---- head 缓存（发现读快路径，docs/02）----
    /// 设置 head 缓存：Core 在块 0 封为满的不可变正文块时，把首 `rawlen` 字节的**已压缩**字节
    /// 交来（压缩在 Core 完成，§2/M2：明文此刻在手，免事后解压回捞）。Store 累积进脏会话，
    /// 随 fsync 落盘。默认 no-op——不支持快路径的后端（ContainerStore 首版）忽略。
    fn set_head_cache(
        &self,
        _ino: Ino,
        _stored_bytes: Vec<u8>,
        _verbatim: bool,
        _rawlen: u64,
    ) -> io::Result<()> {
        Ok(())
    }

    /// 读 head 缓存覆盖区内 `[off, off+len)` 所需的**压缩字节 + verbatim**（解压 + 切片交 Core）。
    /// 仅当 `[off,off+len)` 完全落在缓存覆盖前缀内、且无挂起写会话（脏块 0 可能与盘上缓存不一致）
    /// 时返回 `Some`；否则 `None`，调用方回退逐块路径。默认 None——不支持快路径的后端。
    fn read_head_cache(
        &self,
        _ino: Ino,
        _off: u64,
        _len: u64,
    ) -> io::Result<Option<(Vec<u8>, bool)>> {
        Ok(None)
    }

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

    /// 可观测：返回 `(物理字节, 逻辑字节)`——statfs 据此让 `df` 显压缩比。默认 None（不支持）。
    fn compression_stats(&self) -> Option<(u64, u64)> {
        None
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;
    use crate::store::shadow::ShadowStore;

    #[test]
    fn validate_name_rejects_traversal_and_separators() {
        for bad in ["", ".", "..", "a/b", "/abs", "../escape", "x\0y"] {
            assert!(validate_name(bad).is_err(), "应拒绝非法名: {bad:?}");
        }
        for ok in ["a.jsonl", "-home-xp-proj", "f_1", "session.log"] {
            assert!(validate_name(ok).is_ok(), "应接受合法名: {ok:?}");
        }
    }

    #[test]
    fn shadow_create_mkdir_reject_path_traversal() {
        // 评审 E1：绕过内核（ingest 直喂 read_dir 名）时，后端须自挡 `..`/`/`，否则 join 逃出 backing。
        let dir = tempfile::tempdir().unwrap();
        let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), 4096).unwrap();
        let mk = |kind| Attr {
            ino: 0,
            size: 0,
            kind,
            perm: 0o644,
            uid: 0,
            gid: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            atime: std::time::SystemTime::UNIX_EPOCH,
            ctime: std::time::SystemTime::UNIX_EPOCH,
            chunk_size: 4096,
        };
        assert!(
            store
                .create(1, "..", mk(fuser::FileType::RegularFile))
                .is_err(),
            "create('..') 须拒绝"
        );
        assert!(
            store
                .create(1, "a/b", mk(fuser::FileType::RegularFile))
                .is_err(),
            "create 含 / 须拒绝"
        );
        assert!(
            store
                .mkdir(1, "../x", mk(fuser::FileType::Directory))
                .is_err(),
            "mkdir('../x') 须拒绝"
        );
        // backing 父目录下不应出现逃逸文件。
        assert!(!dir.path().parent().unwrap().join("b").exists());
    }
}
