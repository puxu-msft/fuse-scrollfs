//! scrollz 库 crate：FUSE 透明压缩文件系统的可复用核心。
//!
//! P1 起把核心模块（archive 格式、core 分块/codec、store 后端、两个 FUSE 前端）放进
//! lib，使 `scrollz` 主二进制、`mkfixture` 工具二进制与集成测试都能共享同一套实现，
//! 避免跨 bin 复制粘贴。见 docs/01-scrollz-design.md §11 模块布局。

pub mod archive;
pub mod blockio;
pub mod compact;
pub mod core;
pub mod enable;
pub mod fixture;
pub mod ingest;
pub mod passthrough;
pub mod reconcile;
pub mod rwfs;
pub mod seal;
pub mod store;
