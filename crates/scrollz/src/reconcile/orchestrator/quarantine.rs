//! keep-separate 隔离（疑 session-id 重用）：搬 underlay 副本出树保全。
use std::io;

use super::*;

/// keep-separate 隔离（疑 session-id 重用）：把 underlay 的 reuse `.jsonl`（**保留原 UUID 文件名**）
/// 搬到 `paths.quarantine(name, ts)/<rel>`（**移出 projects 树**，避免下次挂载又被当 fall-through 反复
/// 触发）并 fsync（文件 + 目录链）。**base（projects 树内 orig/backing）绝不改动**——隔离只把 underlay
/// 那一份可疑内容原样保全供人工核查，不并入历史（reuse 若误并会污染无关会话）。
///
/// 只负责「搬出 + durable」，**不删 underlay**：删除仍由 `apply_entry` 经 `finish_delete`（唯一删除入口，
/// receiver=隔离副本、`ByteEqual`）统一把关。返回隔离副本路径（供报告/人工定位）。
///
/// 隔离区跨目录、可能跨卷；`atomic_write` 以快照 `bytes` 原样写出（保原 UUID 名），配 `ByteEqual` readback
/// 校验副本逐字节等于源，杜绝隔离 copy 半写就误删 underlay。
pub fn quarantine_reuse(
    paths: &Paths,
    name: &str,
    ts: &str,
    snap_entry: &EntrySnapshot,
    _mp: &Path,
) -> io::Result<PathBuf> {
    validate_name(name)?;
    let quarantine_root = paths.quarantine(name, ts);
    let dst = quarantine_root.join(&snap_entry.rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 原样写快照内容（非再读 live），使隔离副本与快照 bytes 逐字节同源 → ByteEqual 删除门自洽。
    atomic_write(&dst, &snap_entry.bytes)?;
    // 补齐 <rel 子目录>/<ts>/<name> 目录链 fsync（atomic_write 只 fsync 了 dst 直接父目录）。
    if let Some(parent) = dst.parent() {
        fsync_dir_chain(parent, &quarantine_root)?;
    }
    Ok(dst)
}
