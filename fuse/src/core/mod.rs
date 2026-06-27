//! Core 压缩内核占位（P1+ 填充）。
//!
//! 设计见 docs/01-zipfs-design.md §3「共享内核」与 §11「模块布局」。
//! 分块数学 / RMW / codec / inode 属性都在此层，两种磁盘布局（V 容器、S 影子树）共享。
//! P0 透传阶段不使用本层，仅保留可编译的签名骨架，避免 P1 起步时重新设计接缝。
//!
//! P0 阶段大部分尚未 wire-in，故 allow(dead_code)；P1 起逐步去除。
#![allow(dead_code)]

pub mod chunk;
pub mod codec;
pub mod inode;

/// 默认逻辑块大小（64KiB）。基准变量取 16 / 64 / 256 KiB，见 §3。
pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;
