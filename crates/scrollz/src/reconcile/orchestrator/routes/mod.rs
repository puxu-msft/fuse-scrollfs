//! 特殊路由：绕过通用 plan/apply 分类的两类条目。
//!
//! - [`subagents`]：`<uuid>/subagents/*.jsonl` 子会话，强制无损并集。
//! - [`memory_passthrough`]：外链 `memory` 目录的物化回落写，透传恢复到 canonical target。

pub(crate) mod memory_passthrough;
pub(crate) mod subagents;
