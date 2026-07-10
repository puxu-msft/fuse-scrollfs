//! 后端接缝：把「布局 V 容器」抽象成一个最小 KV 接口，让 redb 与 rusqlite
//! 共用同一套场景 harness，公平对照。
//!
//! 这不是 zipfs 的 `Store` trait（那个还管命名空间/元数据），而是只覆盖
//! microbench 关心的部分：变长 blob 的批量写、随机读改写、提交策略、空间度量。

use std::path::Path;

/// 提交策略：对照设计 §6.1 的「每写一事务」陷阱。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitPolicy {
    /// 每写一块就 commit 一次（最坏情况，事务开销 dominate）。
    PerBlock,
    /// 每 K 块攒成一个事务再 commit（设计推荐的写批处理）。
    Batched(usize),
}

/// 容器后端必须实现的最小接口。
pub trait Backend {
    /// 后端名（用于报告）。
    fn name(&self) -> &'static str;

    /// 批量插入：把 `blocks` 里的 (ino, idx, blob) 全部写入。
    /// 按 `policy` 决定事务边界。返回实际写入字节数。
    fn bulk_insert(&mut self, blocks: &[(u64, u64, Vec<u8>)], policy: CommitPolicy) -> u64;

    /// 读出一个 blob（RMW 的读半程）。返回长度（None 表示不存在）。
    /// 复用 `out` 缓冲避免分配。
    fn get_block(&self, ino: u64, idx: u64, out: &mut Vec<u8>) -> Option<usize>;

    /// 单块写回（RMW 的写半程），每次独立提交一个事务。
    /// 用于 PerBlock 策略下逐块测延迟。
    fn put_block_committed(&mut self, ino: u64, idx: u64, blob: &[u8]);

    /// 批量写回一组 RMW 结果，合到一个事务里提交（Batched 策略）。
    fn put_batch_committed(&mut self, items: &[(u64, u64, Vec<u8>)]);

    /// 当前容器文件大小（字节）。
    fn file_size(&self) -> u64;

    /// 压实/回收，返回压实后文件大小。不支持则返回 None。
    fn compact(&mut self) -> Option<u64>;

    /// 强制把数据落盘并刷新文件大小视图（关闭再看大小前调用）。
    fn sync(&mut self);
}

/// 后端选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Redb,
    Sqlite,
}

impl BackendKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "redb" => Ok(BackendKind::Redb),
            "sqlite" | "rusqlite" => Ok(BackendKind::Sqlite),
            other => Err(format!("未知后端: {other}（可选 redb|sqlite）")),
        }
    }
}

/// 取文件大小的小工具，显式处理错误（缺失视为 0）。
pub fn path_size(p: &Path) -> u64 {
    match std::fs::metadata(p) {
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}
