//! 抽干后剪除空 underlay 子目录 + 清除与 backing 同款的冗余顶层软链。
use std::io;

use super::*;

// ── 顶层 reconcile 编排（Task 9） ──────────────────────────────────────────────

/// 逐条目 apply 后**自底向上**剪除 underlay 里已抽干的空目录（评审 final BREACH 1）。
///
/// `finish_delete` 只 `remove_file`、从不 rmdir，故嵌套条目（如 `<uuid>/subagents/*.jsonl`）全抽干后
/// 空目录 `<uuid>/subagents/`、`<uuid>/` 仍留存 underlay；顶层 `<uuid>/` 令 `underlay_has_fallthrough`
/// 永真、`ensure_underlay_empty` 永久拒挂（wedge 重挂）。此函数自底向上遍历，凡「仅含 `is_harmless`
/// 白名单项（或全空）」的目录即 rmdir。
///
/// 保守规则（零丢失）：仍存留任一**非白名单条目**（用户 Skip/KeepBoth、`delete_permitted` 未过留下的
/// 文件，或 fifo/socket 等特殊文件）的目录**保留不删**——该项目正确地维持 NEEDS-RECONCILE，绝不强删非
/// 空目录。**绝不删 `mp` 本身**（FUSE 挂载点必须留存，只可能删其后代空目录）。`mp` 不存在视为无事可做。
pub(crate) fn prune_empty_underlay_dirs(mp: &Path) -> io::Result<()> {
    // mp 自身永不被删（只有父目录会对子目录调 remove_dir，而 mp 无父层参与此遍历）；返回值忽略。
    let _ = prune_dir_bottom_up(mp)?;
    Ok(())
}

/// `prune_empty_underlay_dirs` 的递归实现：先递归子目录（自底向上），再对「已抽干」的子目录 rmdir。
/// 返回 `dir` 剪枝后是否「无非白名单条目」（供父层判定是否可删 `dir`）。
fn prune_dir_bottom_up(dir: &Path) -> io::Result<bool> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e),
    };
    let mut has_kept = false;
    let mut removed_any = false;
    for dent in rd {
        let dent = dent?;
        if is_harmless(&dent.file_name()) {
            continue;
        }
        let ft = dent.file_type()?;
        if !ft.is_dir() {
            // 非目录、非白名单（常规文件 / fifo / socket 等）→ 该目录须保留（fail-closed）。
            has_kept = true;
            continue;
        }
        if prune_dir_bottom_up(&dent.path())? {
            match std::fs::remove_dir(dent.path()) {
                Ok(()) => removed_any = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                // 竞态/意外非空兜底：删不掉即视为保留，不传播（best-effort 剪枝，绝不误伤数据）。
                Err(_) => has_kept = true,
            }
        } else {
            has_kept = true;
        }
    }
    if removed_any {
        // 持久化本目录内子目录移除的 dirent（best-effort，与本文件其余落盘链一致，失败不阻断剪枝）。
        let _ = fsync_dir(dir);
    }
    Ok(!has_kept)
}

/// memory-symlink 短路：清除 `mp` 顶层「与 backing 同名同目标」的冗余 underlay 软链（§6）。
///
/// 背景：Claude 的 `memory` 常是指向 canonical 目标的 symlink。停用期软链仍在、写已透传到
/// canonical → 无 split-brain、无内容要合并；但 `walk_snapshot` 跳过 symlink，该顶层软链既不
/// 进快照被处理、又令 `underlay_has_fallthrough` 判非空 → 永久 wedge 重挂。此步遍历 `mp` 顶层：
/// 某条目是 symlink 且 backing 同名条目也是 symlink 且二者 `read_link` 目标相等 → `remove_file`
/// 删 underlay 那个（backing 有同款、挂载时透传服务）。目标不一致或 backing 无同名 symlink（异常）
/// → **保留** + push 一条报告串，绝不误删。
///
/// **零丢失**：只删「与 backing 同名同目标的 symlink」；真实目录 `memory`（split-brain）不是
/// symlink，天然不命中此步（`is_symlink` 为假即跳过），仍走 `passthrough_restore_memory`。返回
/// 异常保留项的报告 Vec（并入 `ReconcileReport`）。
pub(crate) fn prune_redundant_symlinks(paths: &Paths, name: &str, mp: &Path) -> io::Result<Vec<String>> {
    let mut notes: Vec<String> = Vec::new();
    let backing_root = paths.backing(name, Backend::Shadow);
    let rd = match std::fs::read_dir(mp) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(notes),
        Err(e) => return Err(e),
    };
    for dent in rd {
        let dent = dent?;
        if !dent.file_type()?.is_symlink() {
            // 真实目录/文件（含 split-brain memory 目录）天然不命中——绝不误删。
            continue;
        }
        let name_os = dent.file_name();
        let under_link = dent.path();
        let backing_link = backing_root.join(&name_os);
        let top = name_os.to_string_lossy();

        // backing 同名条目须也是 symlink，否则保留（异常：underlay 有软链 backing 无对应）。
        let backing_is_symlink = match std::fs::symlink_metadata(&backing_link) {
            Ok(m) => m.file_type().is_symlink(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        if !backing_is_symlink {
            notes.push(format!(
                "underlay 顶层 symlink {top} 在 backing 无同名 symlink → 保留、待人工"
            ));
            continue;
        }

        let under_target = std::fs::read_link(&under_link)?;
        let backing_target = std::fs::read_link(&backing_link)?;
        if under_target != backing_target {
            notes.push(format!(
                "underlay 顶层 symlink {top} 目标 {} 与 backing 同名目标 {} 不一致 → 保留、待人工",
                under_target.display(),
                backing_target.display()
            ));
            continue;
        }

        // 同名同目标：backing 有同款、挂载时透传服务，删 underlay 冗余软链。
        match std::fs::remove_file(&under_link) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(parent) = under_link.parent() {
            // 持久化软链移除的 dirent（best-effort，与本文件其余落盘链一致）。
            let _ = fsync_dir(parent);
        }
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::orchestrator::testsupport::*;

    #[test]
    fn prune_redundant_symlink_removes_matching_underlay_link() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();

        let target = tmp.path().join("canonical");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, mp.join("memory")).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();

        let notes = prune_redundant_symlinks(&paths, "demo", &mp).unwrap();
        assert!(
            notes.is_empty(),
            "同目标冗余软链应静默删除、无异常报告：{notes:?}"
        );
        assert!(
            std::fs::symlink_metadata(mp.join("memory")).is_err(),
            "underlay memory 软链应被删除"
        );
        assert!(
            !underlay_has_fallthrough(&mp).unwrap(),
            "删除冗余软链后 underlay 不再判非空（不 wedge 重挂）"
        );
    }

    #[test]
    fn prune_redundant_symlink_keeps_mismatched_target() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();

        // read_link 不解析目标，target 无需真实存在。
        std::os::unix::fs::symlink(tmp.path().join("a"), mp.join("memory")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("b"), backing.join("memory")).unwrap();

        let notes = prune_redundant_symlinks(&paths, "demo", &mp).unwrap();
        assert!(!notes.is_empty(), "目标不一致应保留并报告");
        assert!(
            std::fs::symlink_metadata(mp.join("memory"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "目标不一致的 underlay 软链绝不被误删"
        );
    }

    #[test]
    fn prune_redundant_symlink_ignores_real_dir_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();

        // underlay memory 是真实目录（含文件）；backing memory 是软链。
        let memdir = mp.join("memory");
        std::fs::create_dir_all(&memdir).unwrap();
        std::fs::write(memdir.join("f.md"), b"x").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("canon"), backing.join("memory")).unwrap();

        let notes = prune_redundant_symlinks(&paths, "demo", &mp).unwrap();
        assert!(
            memdir.join("f.md").exists(),
            "真实目录 memory（split-brain）绝不被此步误删"
        );
        assert!(notes.is_empty(), "非 symlink 条目不产生报告：{notes:?}");
    }

}
