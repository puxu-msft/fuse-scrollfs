//! memory 外链透传恢复：物化回落写送回 canonical target，绝不落 orig。
use std::io;

use crate::reconcile::orchestrator::*;

/// memory 透传恢复（**例外规则**，不走 `delete_permitted`——靠「先安置进 target + 再把 underlay 整目录
/// relocate 到 stash」双重保全达成零丢失）。
///
/// 背景：Claude 的 `memory` 是指向外部共享目录的 symlink。停用期 FS 不服务 → Claude 把内容**物化**成
/// underlay 里的真实目录。恢复 = 把这些文件送回 symlink 真正指向的 target，再复原 symlink。
///
/// 路径安全（写 target 前，任一不满足即**不动 underlay**、返回 notes 待人工）：
/// - `symlink_target` 含 `..` 组件 → 拒（`../` 穿越）。
/// - `canonicalize(symlink_target)` 失败 → 目标悬空/不存在。
/// - 解析后非目录 → 疑被物化成真实文件等异常。
/// - 目标不可写（写探针失败）→ 待人工。
///
/// 安置规则（canonical 原版**绝不覆盖**、冲突**绝不静默丢**）：
/// - target 无同名 → 复制进 target（fsync + readback 校验）。
/// - target 同名**同内容** → 幂等 no-op。
/// - target 同名**异内容** → 不合并；underlay 版以 `<name>.underlay-<crc32>` 存在 target 旁（幂等：同内容
///   → 同名不重复；crc32 碰撞异内容 → 序号消歧不覆盖），canonical 原版原样不动。
///
/// 全部安置且校验后：把 underlay 整目录 relocate 到 `stash_dir`（rename，跨卷回落递归拷+删；保全审计/回滚
/// 底本）并 fsync 目录链，令 underlay 侧 **无任何 memory 残留**（**绝不复原 symlink**——`memory` symlink 已
/// 存于 backing、挂载时透明服务；underlay 若留目录或 symlink 即成 fall-through 残留，永久 wedge 重挂）。underlay
/// 目录不存在/已是 symlink（已恢复/无回落）→ 幂等返回。相对 `symlink_target` 按 symlink 所在目录（而非进程
/// CWD）解析。返回逐步骤 notes（审计）。
pub fn passthrough_restore_memory(
    underlay_dir: &Path,
    symlink_target: &Path,
    stash_dir: &Path,
) -> io::Result<Vec<String>> {
    let mut notes: Vec<String> = Vec::new();

    // 路径安全 1：拒 `..` 穿越（symlink 被改写指向树外的注入向量）。
    if symlink_target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        notes.push(format!(
            "symlink 目标 {} 含 `..` 穿越 → 拒绝、underlay memory 不删，待人工",
            symlink_target.display()
        ));
        return Ok(notes);
    }

    // 相对目标按 symlink 所在目录（= underlay_dir 父目录，symlink 将在此复原）解析，而非进程 CWD（评审 M1）。
    let target_to_resolve: PathBuf = if symlink_target.is_absolute() {
        symlink_target.to_path_buf()
    } else {
        match underlay_dir.parent() {
            Some(base) => base.join(symlink_target),
            None => symlink_target.to_path_buf(),
        }
    };

    // 路径安全 2：canonicalize（悬空/不存在即失败）。
    let canon_target = match std::fs::canonicalize(&target_to_resolve) {
        Ok(p) => p,
        Err(e) => {
            notes.push(format!(
                "symlink 目标 {} 悬空/不可解析（{e}）→ underlay memory 不删，待人工",
                symlink_target.display()
            ));
            return Ok(notes);
        }
    };

    // 路径安全 3：解析后必须是目录（非目录 = 疑被物化成真实文件等异常）。
    let md = std::fs::metadata(&canon_target)?;
    if !md.is_dir() {
        notes.push(format!(
            "symlink 目标 {} 解析后非目录（疑被物化）→ underlay memory 不删，待人工",
            canon_target.display()
        ));
        return Ok(notes);
    }

    // 路径安全 4：可写探针（不可写即待人工，避免半写）。
    if let Err(e) = probe_writable(&canon_target) {
        notes.push(format!(
            "symlink 目标 {} 不可写（{e}）→ underlay memory 不删，待人工",
            canon_target.display()
        ));
        return Ok(notes);
    }

    // underlay memory 现状分诊（`symlink_metadata` 不跟随，避免把已复原的 symlink 当目录再处理）：
    // - 已是 symlink → 上一条目已恢复，幂等跳过（同一 reconcile 多条 memory/* 条目会重复触达）。
    // - 非目录（异常物化）→ 不动、待人工。
    // - 不存在 → 已恢复/无回落，幂等返回。
    match std::fs::symlink_metadata(underlay_dir) {
        Ok(m) if m.file_type().is_symlink() => {
            notes.push(format!(
                "underlay memory {} 已是 symlink（上一条目已恢复），幂等跳过",
                underlay_dir.display()
            ));
            return Ok(notes);
        }
        Ok(m) if !m.is_dir() => {
            notes.push(format!(
                "underlay memory {} 非目录（异常）→ 不动、待人工",
                underlay_dir.display()
            ));
            return Ok(notes);
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            notes.push(format!(
                "underlay memory 目录 {} 不存在（已恢复或无回落）",
                underlay_dir.display()
            ));
            return Ok(notes);
        }
        Err(e) => return Err(e),
    }

    // 安置每个 underlay 文件到 canonical target（新增/冲突/幂等）。
    place_memory_files(underlay_dir, underlay_dir, &canon_target, &mut notes)?;

    // 全部安置后：underlay 整目录 relocate 到 stash（保全底本），underlay memory 目录随之整体消失。
    // **绝不在 underlay 复原任何 symlink**（评审 final BREACH 2）：`memory` symlink 已存于 backing、挂载
    // 时透明服务；underlay 侧必须以**无 memory 条目**收场——否则顶层 `memory`（目录或复原的 symlink）
    // 都是非白名单条目，令 `underlay_has_fallthrough` 永真、`ensure_underlay_empty` 永久拒挂（wedge）。
    // 崩溃持久化纪律（评审 M3）：relocate 后 fsync stash 父目录记 rename dirent，再 fsync underlay 父目录
    // 记 underlay memory 目录移除后的 dirent（均传播错误，与本文件其余落盘链一致）。
    relocate_dir(underlay_dir, stash_dir)?;
    if let Some(parent) = stash_dir.parent() {
        fsync_dir(parent)?;
    }
    if let Some(parent) = underlay_dir.parent() {
        fsync_dir(parent)?;
    }
    notes.push(format!(
        "underlay memory relocate 到 stash 并从 underlay 移除（不复原 symlink，挂载由 backing/memory 服务）：{}",
        stash_dir.display()
    ));
    Ok(notes)
}

/// 据 `passthrough_restore_memory` 的 notes 归纳 `EntryReport.action`（评审 M4，如实反映结果）：
/// 含「从 underlay 移除」→ `memory-restored`（成功 relocate、underlay 无残留）；含「幂等跳过/已恢复/
/// 不存在」→ `memory-noop`；否则（路径安全闸拦截、underlay 未动）→ `memory-deferred`（待人工）。
pub(crate) fn passthrough_action(notes: &[String]) -> &'static str {
    if notes.iter().any(|n| n.contains("从 underlay 移除")) {
        "memory-restored"
    } else if notes
        .iter()
        .any(|n| n.contains("幂等跳过") || n.contains("已恢复") || n.contains("不存在"))
    {
        "memory-noop"
    } else {
        "memory-deferred"
    }
}

/// 在目录 `dir` 内建临时探针文件再删，判定可写。仅探测写权限，不留痕。
///
/// 探针名带 pid + 纳秒（评审 M2）：memory target 常是跨项目共享目录，`reconcile_lock` 只按项目名串行，
/// 两项目并发时固定探针名会互删致 `remove_file` 撞 `NotFound`。唯一名 + 容忍 `NotFound` 清理 → 不误判。
fn probe_writable(dir: &Path) -> io::Result<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(
        ".scrollz-memory-write-probe.{}.{nanos}",
        std::process::id()
    ));
    File::create(&probe)?;
    match std::fs::remove_file(&probe) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 递归把 `dir` 下的文件安置进 `canon_target`（`root` 是 underlay memory 根，算 rel 用）。
///
/// 新增 → 复制 + fsync + readback；同名同内容 → 幂等跳过；同名异内容 → `<name>.underlay-<crc32>`
/// 存 target 旁（canonical 不动，crc32 碰撞序号消歧）。所有 rel 组件来自 `read_dir`（天然无 `..`），仍显式跳过非常规文件。
fn place_memory_files(
    root: &Path,
    dir: &Path,
    canon_target: &Path,
    notes: &mut Vec<String>,
) -> io::Result<()> {
    for dent in std::fs::read_dir(dir)? {
        let dent = dent?;
        let path = dent.path();
        let ft = dent.file_type()?;
        if ft.is_dir() {
            place_memory_files(root, &path, canon_target, notes)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("underlay memory 条目逃出根"))?;
        let dest = canon_target.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = std::fs::read(&path)?;
        let rel_disp = rel.to_string_lossy().into_owned();
        match std::fs::read(&dest) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                atomic_write(&dest, &content)?;
                if !readback_eq(&dest, &content)? {
                    return Err(io::Error::other(format!(
                        "memory 复制 {rel_disp} 后 readback 不符"
                    )));
                }
                notes.push(format!("memory 新增 → target/{rel_disp}"));
            }
            Err(e) => return Err(e),
            Ok(existing) if existing == content => {
                notes.push(format!("memory 已存在同内容，跳过：target/{rel_disp}"));
            }
            Ok(_) => {
                // 冲突：canonical 绝不覆盖。underlay 版以 `<name>.underlay-<crc32>` 存 target 旁。
                // crc32 碰撞（异内容同摘要）时用递增序号消歧，**绝不覆盖已保留的异内容副本**（评审 H2）。
                let hash8 = format!("{:08x}", crate::archive::crc32(&content));
                match resolve_variant_slot(&dest, &hash8, &content)? {
                    None => notes.push(format!(
                        "memory 冲突副本已存在同内容，幂等跳过：target/{rel_disp}"
                    )),
                    Some(variant) => {
                        atomic_write(&variant, &content)?;
                        if !readback_eq(&variant, &content)? {
                            return Err(io::Error::other(format!(
                                "memory 冲突副本 {rel_disp} 后 readback 不符"
                            )));
                        }
                        notes.push(format!(
                            "memory 冲突（canonical 保留）→ target 旁 {}",
                            variant
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// 为冲突 underlay 版求一个不覆盖任何**异内容**副本的落点（评审 H2 抗 crc32 碰撞）。
///
/// 先试 `<name>.underlay-<hash8>`：不存在 → 用它；已存在同内容 → `None`（幂等跳过）；已存在异内容
///（crc32 碰撞）→ 追加 `-1`/`-2`… 序号继续找，直到空槽或同内容槽。返回 `Some(空槽)` 或 `None`（已有同内容）。
fn resolve_variant_slot(dest: &Path, hash8: &str, content: &[u8]) -> io::Result<Option<PathBuf>> {
    let base = variant_path(dest, hash8);
    match std::fs::read(&base) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(base)),
        Err(e) => return Err(e),
        Ok(b) if b == content => return Ok(None),
        Ok(_) => {}
    }
    let mut n = 1u32;
    loop {
        let cand = variant_path(dest, &format!("{hash8}-{n}"));
        match std::fs::read(&cand) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(cand)),
            Err(e) => return Err(e),
            Ok(b) if b == content => return Ok(None),
            Ok(_) => n += 1,
        }
    }
}

/// 在 `dest` 同目录、同文件名后缀 `.underlay-<hash8>` 的冲突副本路径。
fn variant_path(dest: &Path, hash8: &str) -> PathBuf {
    let mut os = dest.as_os_str().to_owned();
    os.push(format!(".underlay-{hash8}"));
    PathBuf::from(os)
}

/// 把目录 `from` 整体搬到 `to`（rename 优先；跨卷回落递归拷贝 + 删源）。搬前建 `to` 的父目录。
fn relocate_dir(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_dir_recursive(from, to)?;
            std::fs::remove_dir_all(from)?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// 递归拷贝目录（跨卷 relocate 回落用）。常规文件 `copy`、目录递归、symlink 照原样重建；遇 FIFO/
/// socket/设备等**无法安全拷贝的特殊类型直接报错中止**——`relocate_dir` 会因此在 `remove_dir_all`
/// **之前**失败、保源不删，杜绝「跳过特殊文件 → 删源 → 底本缺该文件」的零丢失破口（评审 H1）。
fn copy_dir_recursive(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for dent in std::fs::read_dir(from)? {
        let dent = dent?;
        let ft = dent.file_type()?;
        let src = dent.path();
        let dst = to.join(dent.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else if ft.is_file() {
            std::fs::copy(&src, &dst)?;
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else {
            return Err(io::Error::other(format!(
                "跨卷 relocate 遇不可拷贝的特殊文件 {}，中止（保源不删，待人工）",
                src.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_restores_new_memory_file_into_target_and_removes_underlay() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("shared-memory");
        std::fs::create_dir_all(&target).unwrap();
        let underlay = tmp.path().join("mp").join("memory");
        std::fs::create_dir_all(&underlay).unwrap();
        std::fs::write(underlay.join("NEW.md"), b"fresh\n").unwrap();
        let stash = tmp.path().join("q").join("memory");

        let notes = passthrough_restore_memory(&underlay, &target, &stash).unwrap();

        // 新文件复制进 target。
        assert_eq!(std::fs::read(target.join("NEW.md")).unwrap(), b"fresh\n");
        // underlay memory relocate 到 stash（底本保全）。
        assert_eq!(std::fs::read(stash.join("NEW.md")).unwrap(), b"fresh\n");
        // underlay 侧 memory 条目彻底消失（**不复原 symlink**）——否则顶层 memory 残留会 wedge 重挂。
        assert!(
            underlay.symlink_metadata().is_err(),
            "underlay memory 必须无残留（无目录、无复原 symlink）"
        );
        assert!(notes.iter().any(|n| n.contains("从 underlay 移除")));
    }

    #[test]
    fn passthrough_conflict_keeps_underlay_variant_beside_canonical_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("mem");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("MEMORY.md"), b"CANONICAL\n").unwrap();
        let underlay = tmp.path().join("mp").join("memory");
        std::fs::create_dir_all(&underlay).unwrap();
        std::fs::write(underlay.join("MEMORY.md"), b"UNDERLAY-VERSION\n").unwrap();
        let stash = tmp.path().join("q").join("memory");

        passthrough_restore_memory(&underlay, &target, &stash).unwrap();

        // canonical 原版绝不覆盖。
        assert_eq!(
            std::fs::read(target.join("MEMORY.md")).unwrap(),
            b"CANONICAL\n"
        );
        // underlay 版以内容哈希后缀存 target 旁。
        let hash = format!("{:08x}", crate::archive::crc32(b"UNDERLAY-VERSION\n"));
        let variant = target.join(format!("MEMORY.md.underlay-{hash}"));
        assert!(
            variant.exists(),
            "应保留 underlay 版：{}",
            variant.display()
        );
        assert_eq!(std::fs::read(&variant).unwrap(), b"UNDERLAY-VERSION\n");

        // 幂等：重建同内容 underlay 再跑 → 同 hash 同名，不新增第二份。
        // （首轮已把 underlay memory 整目录 relocate 走、未复原 symlink，故此处直接重建目录即可。）
        std::fs::create_dir_all(&underlay).unwrap();
        std::fs::write(underlay.join("MEMORY.md"), b"UNDERLAY-VERSION\n").unwrap();
        let stash2 = tmp.path().join("q2").join("memory");
        passthrough_restore_memory(&underlay, &target, &stash2).unwrap();
        let variants = std::fs::read_dir(&target)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("MEMORY.md.underlay-")
            })
            .count();
        assert_eq!(variants, 1, "幂等：同内容同名不重复");
    }

    #[test]
    fn passthrough_rejects_traversal_target_keeps_underlay() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        // 含 `..` 穿越的目标路径。
        let traversal = tmp.path().join("real").join("..").join("real");
        let underlay = tmp.path().join("mp").join("memory");
        std::fs::create_dir_all(&underlay).unwrap();
        std::fs::write(underlay.join("x.md"), b"data\n").unwrap();
        let stash = tmp.path().join("q").join("memory");

        let notes = passthrough_restore_memory(&underlay, &traversal, &stash).unwrap();

        // 拒穿越 → underlay 不动（仍是真实目录、文件在）。
        assert!(
            underlay.symlink_metadata().unwrap().file_type().is_dir(),
            "拒穿越 → underlay 不 relocate"
        );
        assert!(underlay.join("x.md").exists(), "underlay 文件保留");
        assert!(!stash.exists(), "拒穿越 → 不搬 stash");
        assert!(
            notes.iter().any(|n| n.contains("穿越")),
            "notes 说明待人工：{notes:?}"
        );
    }

    #[test]
    fn passthrough_dangling_target_keeps_underlay_for_manual() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("missing-mem"); // 不存在（悬空）
        let underlay = tmp.path().join("mp").join("memory");
        std::fs::create_dir_all(&underlay).unwrap();
        std::fs::write(underlay.join("x.md"), b"data\n").unwrap();
        let stash = tmp.path().join("q").join("memory");

        let notes = passthrough_restore_memory(&underlay, &target, &stash).unwrap();

        assert!(
            underlay.symlink_metadata().unwrap().file_type().is_dir(),
            "悬空目标 → underlay 不 relocate"
        );
        assert!(underlay.join("x.md").exists(), "underlay 文件保留");
        assert!(
            notes.iter().any(|n| n.contains("悬空")),
            "notes 说明待人工：{notes:?}"
        );
    }

    #[test]
    fn passthrough_unwritable_target_keeps_underlay_for_manual() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("ro-mem");
        std::fs::create_dir_all(&target).unwrap();
        // 只读目标（去写权限）。root 下探针仍可写 → 跳过该断言。
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o500)).unwrap();
        let underlay = tmp.path().join("mp").join("memory");
        std::fs::create_dir_all(&underlay).unwrap();
        std::fs::write(underlay.join("x.md"), b"data\n").unwrap();
        let stash = tmp.path().join("q").join("memory");

        let notes = passthrough_restore_memory(&underlay, &target, &stash).unwrap();
        // 非 root 环境：不可写 → underlay 保留、待人工。
        if !notes.iter().any(|n| n.contains("不可写")) {
            // root 或特殊 fs：探针可写，本断言不适用，放行（避免 root CI flaky）。
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700));
            return;
        }
        assert!(
            underlay.symlink_metadata().unwrap().file_type().is_dir(),
            "不可写目标 → underlay 不 relocate"
        );
        assert!(underlay.join("x.md").exists());
        // 恢复权限便于 tempdir 清理。
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn passthrough_conflict_crc_collision_disambiguates_without_overwrite() {
        // 评审 H2：模拟同 crc32 摘要下异内容不覆盖——预置一个占位变体（异内容），跑冲突安置后
        // 两个异内容变体并存（占位版 + 新序号版），无一被覆盖。
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("mem");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("M.md"), b"CANON\n").unwrap();
        let underlay = tmp.path().join("mp").join("memory");
        std::fs::create_dir_all(&underlay).unwrap();
        std::fs::write(underlay.join("M.md"), b"UNDER-A\n").unwrap();
        // 预置：占据 <name>.underlay-<crc(UNDER-A)> 槽，但内容不同（模拟碰撞）。
        let hash = format!("{:08x}", crate::archive::crc32(b"UNDER-A\n"));
        let squatter = target.join(format!("M.md.underlay-{hash}"));
        std::fs::write(&squatter, b"COLLISION-OTHER\n").unwrap();
        let stash = tmp.path().join("q").join("memory");

        passthrough_restore_memory(&underlay, &target, &stash).unwrap();

        // 占位版（异内容）绝不被覆盖。
        assert_eq!(std::fs::read(&squatter).unwrap(), b"COLLISION-OTHER\n");
        // UNDER-A 落到序号消歧槽。
        let disambig = target.join(format!("M.md.underlay-{hash}-1"));
        assert!(
            disambig.exists(),
            "应序号消歧不覆盖：{}",
            disambig.display()
        );
        assert_eq!(std::fs::read(&disambig).unwrap(), b"UNDER-A\n");
        // canonical 不动。
        assert_eq!(std::fs::read(target.join("M.md")).unwrap(), b"CANON\n");
    }

}
