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
pub mod rmw;
pub mod wsession;

/// 默认逻辑块大小（**1MiB**）。
///
/// 实测裁决（`bench/results/dict-chunk-ratio/` + `bench/results/20260628-algo-compare/`）：
/// 64KiB 是错的——boilerplate 复现间距 p90≈154KiB，64KiB 只逮到 p50 以下，砍掉绝大部分长程冗余
/// （真实路径 Shadow 仅 5.43x、Container 病态 1.89x）。**1MiB 是实时随机访问块的甜点**
/// （Shadow zstd-3=13.7x / zstd-19=15.9x；Container 8.8x），更高比值上 2–4MiB。块越大压缩越好、
/// 写调用越少（写更快）；append 负载下尾块缓冲让 append 成本与块大小无关，故可纯按比值选。
/// 代价：随机中间写 RMW 需读改写整块（本负载罕见）、`max_write`(128KiB) 拆分（尾缓冲已缓解）。
/// 仍可 `--chunk-size` 覆盖。原 64KiB 默认（§6.1「不默认 256KiB」）按真实数据退役。
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// 默认 zstd 压缩等级（3）。`--level` 可覆盖。
///
/// 保持 3 而非 19：活跃写负载下 19 的 CPU 代价 25–100x，比值仅 +13~16%（1MiB/L3=13.7x→L19=15.9x），
/// 写延迟优先。要更高比值用 `--level 19` 或冷封存（zstd-19 --long，35x，见 algo-compare 结论 #4）。
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
