//! 单文件原子重灌 backing archive + reconciling 进行中标记。
use std::io;

use super::*;

/// 单文件原子重灌（评审 I-1/C2）：把 `orig/<rel>` 重新灌成 backing archive，**原子替换**已存在
/// 的 `<backing>/<rel>`——绝不就地 O_TRUNC 覆盖。
///
/// 流程：`ingest_file(orig/<rel> → <backing>/<rel>.reconcile-tmp)`（`verify=true` 逐字节校验）→
/// `rename(tmp, <backing>/<rel>)` 原子覆盖 → fsync 父目录持久化 dirent。任一步崩溃时 backing
/// 该条目要么是旧 archive、要么是完整新 archive，绝不半写。仅 shadow 后端（reconcile 前提）。
///
/// chunk_size/level 取自提交标记 sidecar（无则回落 `ApplyOptions::default`），与 apply/reingest
/// 一致，保证重灌 archive 参数不漂移。
pub fn reingest_one_file(paths: &Paths, name: &str, rel: &str) -> io::Result<()> {
    validate_name(name)?;
    let orig_file = paths.orig(name).join(rel);
    let backing = paths.backing(name, Backend::Shadow);
    let backing_file = backing.join(rel);
    // 评审 R-lock：取 backing 排他锁（与 compact/seal/守护同一把 `.scrollz.lock`）保护本次 temp+rename
    // 覆盖 `backing/<rel>`——否则并发 compact/seal 与 reconcile 交错写同一 archive 致损坏（A3 类）。
    // reconcile 前提是未挂载（守护未持锁），故可取；有界重试兜住释放→重取瞬态。函数内短持、
    // 不跨 rebuild 的 remount（那由 lifecycle::reingest 自管），无自死锁。
    let _backing_lock = crate::store::lock::acquire_backing_retry(&backing)?;
    let opts: ApplyOptions = discovery::read_meta(&paths.meta_path(name))
        .ok()
        .flatten()
        .map(|m| m.options())
        .unwrap_or_default();

    if let Some(parent) = backing_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut tmp_os = backing_file.as_os_str().to_owned();
    tmp_os.push(".reconcile-tmp");
    let tmp = PathBuf::from(tmp_os);
    // 清理上次崩溃可能残留的临时 archive（ingest_file 的 O_TRUNC create 本身也会截断，但显式清
    // 更直白且避免误判残留为有效 archive）。
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }

    // verify=true：灌后逐字节 read-back 校验，确认 archive 与 orig 一致再原子就位（零丢失）。
    crate::ingest::ingest_file(&orig_file, &tmp, opts.chunk_size, opts.level, true)?;
    std::fs::rename(&tmp, &backing_file)?;
    if let Some(parent) = backing_file.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// 落/删 **独立的 reconcile 进行中标记**（评审 I-4，**绝不改 `Meta.committed`**）。
///
/// `on=true`：创建 `back_root/<name>.reconciling` 空标记文件并 fsync（文件内容 + 父目录 dirent），
/// 使「reconcile 进行中」崩溃可见——半改写 orig 期间任何生命周期维护据此让路。
/// `on=false`：删除标记（已不存在视为成功，幂等）并 fsync 父目录，reconcile 收尾复位。
pub fn set_reconciling(paths: &Paths, name: &str, on: bool) -> io::Result<()> {
    validate_name(name)?;
    let marker = paths.reconciling_marker(name);
    if on {
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)?;
        }
        File::create(&marker)?.sync_all()?;
        if let Some(parent) = marker.parent() {
            fsync_dir(parent)?;
        }
    } else {
        match std::fs::remove_file(&marker) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(parent) = marker.parent() {
            fsync_dir(parent)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::orchestrator::testsupport::*;

    #[test]
    fn reingest_one_file_atomically_replaces_backing_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");

        // orig/<rel>：合并后的权威明文（含子目录，验证 create_dir_all 链）。
        let rel = "sub/s.jsonl";
        let orig_file = paths.orig("demo").join(rel);
        std::fs::create_dir_all(orig_file.parent().unwrap()).unwrap();
        let content = b"{\"uuid\":\"u1\"}\n{\"uuid\":\"u2\"}\n".repeat(100);
        std::fs::write(&orig_file, &content).unwrap();

        // 预置一个陈旧 backing archive（内容不同），reingest 须原子替换为 orig 的新内容。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        std::fs::create_dir_all(backing_file.parent().unwrap()).unwrap();
        crate::ingest::ingest_file(
            &{
                let p = tmp.path().join("stale.src");
                std::fs::write(&p, b"STALE\n").unwrap();
                p
            },
            &backing_file,
            65536,
            3,
            true,
        )
        .unwrap();
        assert_eq!(read_archive(&backing_file), b"STALE\n");

        reingest_one_file(&paths, "demo", rel).unwrap();

        // backing archive 现读回 orig 的新内容；临时文件已 rename 消失。
        assert_eq!(read_archive(&backing_file), content);
        let mut tmp_os = backing_file.as_os_str().to_owned();
        tmp_os.push(".reconcile-tmp");
        assert!(
            !PathBuf::from(tmp_os).exists(),
            "reconcile-tmp 应已 rename 消失"
        );
    }

    #[test]
    fn reingest_one_file_creates_new_backing_entry() {
        // New 条目：backing 尚无该 rel（连父目录都缺）→ reingest 须建目录链并落 archive。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let rel = "fresh.jsonl";
        let orig_file = paths.orig("demo").join(rel);
        std::fs::create_dir_all(orig_file.parent().unwrap()).unwrap();
        std::fs::write(&orig_file, b"{\"new\":true}\n").unwrap();

        reingest_one_file(&paths, "demo", rel).unwrap();
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), b"{\"new\":true}\n");
    }

    #[test]
    fn set_reconciling_toggles_marker_without_touching_committed() {
        // 评审 I-4：reconciling 标记独立于 committed。落标记后 committed 必须原样为真；删标记复位。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let marker = paths.reconciling_marker("demo");
        assert!(!marker.exists(), "初始无标记");

        set_reconciling(&paths, "demo", true).unwrap();
        assert!(marker.exists(), "on 应落标记文件");
        // committed 不受影响（仍为真）。
        assert!(
            discovery::read_meta(&paths.meta_path("demo"))
                .unwrap()
                .is_some_and(|m| m.committed),
            "set_reconciling 绝不改 committed"
        );

        // 幂等：重复 off（含标记不存在）不报错。
        set_reconciling(&paths, "demo", false).unwrap();
        assert!(!marker.exists(), "off 应删标记");
        set_reconciling(&paths, "demo", false).unwrap();
    }

    #[test]
    fn set_reconciling_rejects_traversal_name() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let e = set_reconciling(&paths, "../escape", true).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn reingest_one_file_blocked_while_backing_locked() {
        // 评审 R-lock：reconcile 改写 backing 须与活守护/compact/seal 互斥（同一把 .scrollz.lock）。
        // 持 backing 锁时 reingest_one_file 应 WouldBlock（有界重试耗尽后），杜绝交错写损坏。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let rel = "f.jsonl";
        write_orig(&paths, "demo", rel, b"{\"uuid\":\"a\"}\n");
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        // 模拟活守护/compact 持有同一把 backing 锁。
        let _held = crate::store::lock::acquire_backing(&backing).unwrap();
        let res = reingest_one_file(&paths, "demo", rel);
        assert_eq!(
            res.as_ref().map_err(|e| e.kind()),
            Err(io::ErrorKind::WouldBlock),
            "backing 被持锁时 reingest 应 WouldBlock，实际：{res:?}"
        );
    }

}
