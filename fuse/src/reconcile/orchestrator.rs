//! reconcile 编排：前置门禁（shadow-only / 活跃 / underlay 非空 / 串行锁）+ underlay 快照。
//!
//! reconcile 把「停用期回落写」（挂载点 underlay 里 Claude 直接写进去的 jsonl 等）安全地重
//! 合并回 backing。落地前必须先过门禁并对 underlay 拍一份**不可变快照**（stash）——快照是后续
//! 合并输入与「删前复核」的唯一基准（评审 I-7/C-a）：活跃门禁只是时间点检查，jsonl fd 可能在
//! 轮次间关闭，故真正删除 live underlay 文件前还要用 `live_entry_unchanged` 复核其未变，杜绝
//! 「快照后又被追加 → 删除时丢掉新数据」的零丢失破口。

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::enable::discovery::{self, detect_activity};
use crate::enable::model::{validate_name, ApplyOptions, Backend, Paths};
use crate::reconcile::guard::{is_harmless, underlay_has_fallthrough};
use crate::store::lock::acquire_exclusive;

/// 单文件合并读入上限（spec §5.1）。超限条目不整体读进内存，降级 KeepBoth（Task 7 消费）。
pub const MAX_MERGE_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// underlay 里单个 fall-through 文件在快照时刻的完整证据：内容 + 身份三元组（mtime/size/ino）。
///
/// `rel` 是相对挂载点的路径（可含子目录）。`bytes` 是快照时读到的完整内容（≤ `MAX_MERGE_FILE_BYTES`）。
/// mtime/size/ino 一起构成「删前复核」的身份指纹：三者全等才认为 live 文件自快照后未被改动。
#[derive(Debug, Clone)]
pub struct EntrySnapshot {
    pub rel: String,
    pub bytes: Vec<u8>,
    pub mtime: SystemTime,
    pub size: u64,
    pub ino: u64,
}

/// underlay 在某一时刻的整体快照：所有 fall-through 文件的 `EntrySnapshot` + 时间戳。
///
/// 这是合并输入与删除比对的**唯一基准**（评审 I-7）：门禁通过后一切以此快照为准，不再重扫 live。
#[derive(Debug, Clone)]
pub struct UnderlaySnapshot {
    pub ts: String,
    pub entries: Vec<EntrySnapshot>,
}

/// 门禁通过后的产物：**持锁句柄**（`_lock` drop 即释放 reconcile 串行锁）+ underlay 快照。
///
/// `_lock` 必须与 `snapshot` 同生命周期——只要还在用快照做合并/删除，就必须持锁，防并发 reconcile
/// 交错。字段带前导下划线表示「持有以维持副作用（锁），不直接读」。
#[derive(Debug)]
pub struct Preconditions {
    pub _lock: File,
    pub snapshot: UnderlaySnapshot,
}

/// 前置门禁 + underlay 快照。顺序即优先级（越靠前越是硬性前提）：
/// 1. `validate_name`：拒路径穿越（no-unconscious 红线，name 下游喂 join）。
/// 2. backend 必须 `Shadow`（container 无 fall-through 语义，reconcile 不适用）。
/// 3. underlay 必须含 fall-through 条目（无回落写则无事可做）。
/// 4. `!force` 时活跃门禁：有活跃写者则拒（除非人工 `--force` 确认）。
/// 5. 取 `reconcile_lock` flock：串行化并发 reconcile 彼此（与 backing `.zipfs.lock` 是两把锁）。
/// 6. 快照：递归读每个 fall-through 文件（单 fd read+fstat）落 stash 并 fsync（文件 + 目录链），返回持锁 `Preconditions`。
pub fn check_preconditions(
    paths: &Paths,
    name: &str,
    backend: Backend,
    force: bool,
) -> io::Result<Preconditions> {
    validate_name(name)?;

    if backend != Backend::Shadow {
        return Err(io::Error::other(format!(
            "reconcile 仅支持 shadow 后端；{name:?} 为 {}，不适用（container 无 fall-through 语义）",
            backend.flag()
        )));
    }

    let mp = paths.mountpoint(name);
    if !underlay_has_fallthrough(&mp)? {
        return Err(io::Error::other(format!(
            "{} 挂载点 underlay 无回落写（fall-through 为空），无需 reconcile",
            mp.display()
        )));
    }

    if !force {
        let activity = detect_activity(&mp);
        if let Some(reason) = activity.reason() {
            return Err(io::Error::other(format!(
                "{} 疑似活跃会话（{reason}），拒绝 reconcile；确认空闲后用 --force 重试",
                mp.display()
            )));
        }
    }

    let ts = now_unix_secs();
    let lock_path = paths.reconcile_lock(name);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = acquire_exclusive(&lock_path)?;
    let snapshot = snapshot_underlay(&mp, &paths.reconcile_stash(name, &ts), ts)?;

    Ok(Preconditions {
        _lock: lock,
        snapshot,
    })
}

/// 删前复核（评审 C-a）：对 `mp/snap.rel` 重新 `symlink_metadata`，比 mtime/size/ino 与快照是否
/// 全等。文件已不存在 → `Ok(false)`（视为已变，不删）。用于真正删除 live underlay 文件前确认其
/// 自快照后未被追加/替换，杜绝零丢失破口。
pub fn live_entry_unchanged(mp: &Path, snap: &EntrySnapshot) -> io::Result<bool> {
    let live = mp.join(&snap.rel);
    let meta = match std::fs::symlink_metadata(&live) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mtime_same = meta.modified().is_ok_and(|m| m == snap.mtime);
    Ok(mtime_same && meta.size() == snap.size && meta.ino() == snap.ino)
}

/// 当前 unix 秒作为 stash 时间戳字符串。时钟异常（早于 UNIX_EPOCH）退化为 "0"（不 panic）。
fn now_unix_secs() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// 递归快照 `mp` 下所有 fall-through 文件到 `stash/underlay/<rel>` 并 fsync（文件 + 目录链），返回内存快照。
///
/// 跳过 `is_harmless` 白名单项（与 guard 一致）。每个文件用**单一 fd 单次读取 + fstat** 捕获——
/// 未超 `MAX_MERGE_FILE_BYTES` 者 `read_to_end` 后 fstat，bytes/size/stash 三者同源自洽；超限文件不
/// 整体读入 `bytes`（留空，仅按 size/ino/mtime 身份捕获，供 Task 7 降级 KeepBoth），stash 侧仍原样拷贝留证。
fn snapshot_underlay(mp: &Path, stash: &Path, ts_secs: String) -> io::Result<UnderlaySnapshot> {
    let mut entries = Vec::new();
    let underlay_stash = stash.join("underlay");
    walk_snapshot(mp, mp, stash, &underlay_stash, &mut entries)?;
    Ok(UnderlaySnapshot {
        ts: ts_secs,
        entries,
    })
}

/// `snapshot_underlay` 的递归实现。`root` 是挂载点根（算 `rel` 用），`dir` 是当前遍历目录，
/// `stash_root` 是 `reconcile_stash(name,ts)` 根（fsync 目录链的上界），`stash_underlay` 是其 `underlay` 子目录。
fn walk_snapshot(
    root: &Path,
    dir: &Path,
    stash_root: &Path,
    stash_underlay: &Path,
    entries: &mut Vec<EntrySnapshot>,
) -> io::Result<()> {
    for dent in std::fs::read_dir(dir)? {
        let dent = dent?;
        let name = dent.file_name();
        if is_harmless(&name) {
            continue;
        }
        let path = dent.path();
        let file_type = dent.file_type()?;
        if file_type.is_dir() {
            walk_snapshot(root, &path, stash_root, stash_underlay, entries)?;
            continue;
        }
        if !file_type.is_file() {
            // symlink / fifo / socket 等非普通文件：不纳入合并快照（reconcile 只处理常规内容文件）。
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("underlay 条目逃出挂载点根"))?
            .to_string_lossy()
            .into_owned();

        // 快照落盘目标：先建 <ts>/underlay/<rel 子目录> 各级。
        let dst = stash_underlay.join(&rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // 单句柄捕获：开一次 fd，先 fstat 取 size 判 cap。未超限 → 同 fd `read_to_end` 后再 fstat，
        // 且 stash 由已读的 `bytes` 写出（非再次读源）——bytes/size/stash 三者同源自洽（snap.size ==
        // bytes.len()）；超限 → 不整体读入内存，仅按 size/mtime/ino 身份捕获，stash 侧原样 copy 留证
        //（copy-without-loading），`bytes` 留空。
        let file = File::open(&path)?;
        let size = file.metadata()?.size();
        let (bytes, meta) = if size > MAX_MERGE_FILE_BYTES {
            std::fs::copy(&path, &dst)?;
            (Vec::new(), file.metadata()?)
        } else {
            let mut file = file;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let meta = file.metadata()?;
            std::fs::write(&dst, &bytes)?;
            (bytes, meta)
        };

        // 持久化：fsync stash 文件，再补齐从其父目录到 stash 根的目录链（<rel>/underlay/<ts>）。
        fsync_path(&dst)?;
        if let Some(parent) = dst.parent() {
            fsync_dir_chain(parent, stash_root)?;
        }

        entries.push(EntrySnapshot {
            rel,
            mtime: meta.modified()?,
            size: meta.size(),
            ino: meta.ino(),
            bytes,
        });
    }
    Ok(())
}

/// fsync 单个文件（确保 stash 内容落盘，崩溃不丢快照）。
fn fsync_path(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// fsync 目录项（确保新建条目在父目录中可见落盘）。
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// fsync 从 stash 文件父目录 `from` 逐级向上到 `stash_root`（含）的每层目录。
///
/// 补齐 `create_dir_all` 新建的 `<rel>`/`underlay`/`<ts>` 各级 dirent，使 stash 拷贝的整条目录链
/// 崩溃可恢复、不被孤儿化（本项目对崩溃持久化敏感）。`from` 恒为 `stash_root` 的后代，向上必达上界。
fn fsync_dir_chain(from: &Path, stash_root: &Path) -> io::Result<()> {
    let mut dir = from;
    loop {
        fsync_dir(dir)?;
        if dir == stash_root {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    Ok(())
}

/// 原子写：`bytes` → `<dst>.tmp`（`sync_all`）→ `rename(tmp, dst)` → fsync 父目录。
///
/// 崩溃安全的「全有或全无」落盘（复用 `lifecycle::fsync_parent` 思路）：先把内容写进同目录临时
/// 文件并 fsync 其内容，再原子 rename 覆盖 `dst`，最后 fsync 父目录持久化这次 rename 的 dirent。
/// 任一步崩溃时 `dst` 要么是旧内容要么是完整新内容，绝不出现半截写入。临时文件名恒为 `<dst>.tmp`
/// （同目录 → rename 同文件系统内原子），与 reconcile 删除许可链的 readback 基准配套。
pub fn atomic_write(dst: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp_os = dst.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);

    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dst)?;
    if let Some(parent) = dst.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// 删除许可的超集比对模式。
///
/// - `ByteEqual`：接收方内容逐字节 == 源（严格镜像，如整文件覆盖式合并）。
/// - `LinesSuperset`：源的每一行都 ∈ 接收方的行集合（接收方 ⊇ 源，允许接收方含额外行，
///   如 jsonl 追加式合并——已把源全部行并入接收方，接收方可能还有别处来的更多行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupersetMode {
    ByteEqual,
    LinesSuperset,
}

/// 读回 `path` 全部内容，逐字节比对是否 == `bytes`。文件缺失/读失败 → `Err`（上层视为不许删）。
///
/// 「readback」语义：删源前必须从磁盘重新读接收方（而非信任内存），确认写确实落地且内容符合预期。
pub fn readback_eq(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    Ok(buf == bytes)
}

/// durable 超集校验：从磁盘**读回** `receiver`，按 `mode` 判定其是否已 durable 覆盖 `source_bytes`。
///
/// - `ByteEqual` = receiver 逐字节 == `source_bytes`。
/// - `LinesSuperset` = `source_bytes` 的每一行都出现在 receiver 的行集合中（receiver ⊇ source 的行）。
///
/// 读回而非信任内存 → 兼具「已落盘」与「内容正确」双重保证，是删源前的接收方侧闸门。
pub fn durable_superset_ok(
    receiver: &Path,
    source_bytes: &[u8],
    mode: SupersetMode,
) -> io::Result<bool> {
    let mut recv = Vec::new();
    File::open(receiver)?.read_to_end(&mut recv)?;
    match mode {
        SupersetMode::ByteEqual => Ok(recv == source_bytes),
        SupersetMode::LinesSuperset => {
            let recv_lines: HashSet<&[u8]> = recv.split(|&b| b == b'\n').collect();
            Ok(source_bytes
                .split(|&b| b == b'\n')
                .all(|line| recv_lines.contains(line)))
        }
    }
}

/// **通用删除许可门（唯一删除入口）**：接收方 durable 且超集/相等 **且** live underlay 自快照未变，
/// 二者同真才返 `true`（评审 C-a 零丢失核心闸）。
///
/// 两个条件缺一不可：
/// 1. `durable_superset_ok(receiver, &src.bytes, mode)`——源内容已 durable 并入接收方（接收方侧安全）。
/// 2. `live_entry_unchanged(mp, src)`——live underlay 文件自快照以来未被追加/替换（源侧无新增数据丢失）。
///
/// Task 7/8 一切删 underlay 的路径都必须经此门，任一条件为假即不许删。
pub fn delete_permitted(
    receiver: &Path,
    src: &EntrySnapshot,
    mode: SupersetMode,
    mp: &Path,
) -> io::Result<bool> {
    Ok(durable_superset_ok(receiver, &src.bytes, mode)? && live_entry_unchanged(mp, src)?)
}

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
    let backing_file = paths.backing(name, Backend::Shadow).join(rel);
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
    use crate::enable::model::{Backend, Paths};
    use std::path::Path;

    fn paths_in(root: &Path) -> Paths {
        Paths {
            projects_root: root.join("projects"),
            zipfs_home: root.join("zip"),
        }
    }

    #[test]
    fn container_backend_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();
        std::fs::write(paths.mountpoint("demo").join("s.jsonl"), b"{}").unwrap();
        let e = check_preconditions(&paths, "demo", Backend::Container, false).unwrap_err();
        assert!(e.to_string().contains("shadow"));
    }

    #[test]
    fn empty_underlay_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();
        let e = check_preconditions(&paths, "demo", Backend::Shadow, false).unwrap_err();
        assert!(e.to_string().contains("underlay") || e.to_string().contains("无回落"));
    }

    #[test]
    fn snapshot_taken_and_live_unchanged_true() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        std::fs::write(mp.join("s.jsonl"), b"{\"a\":1}\n").unwrap();

        let pre = check_preconditions(&paths, "demo", Backend::Shadow, true).unwrap();
        assert_eq!(pre.snapshot.entries.len(), 1);
        let snap = &pre.snapshot.entries[0];
        assert_eq!(snap.rel, "s.jsonl");
        assert_eq!(snap.bytes, b"{\"a\":1}\n");
        // 单句柄 read+fstat 自洽：size 与实际读入 bytes 长度全等（未超 cap 条目）。
        assert_eq!(snap.size, snap.bytes.len() as u64);
        // 未改动 live 文件 → 复核为真。
        assert!(live_entry_unchanged(&mp, snap).unwrap());
        // stash 落盘存在。
        let stashed = paths
            .reconcile_stash("demo", &pre.snapshot.ts)
            .join("underlay")
            .join("s.jsonl");
        assert!(stashed.exists(), "快照应落到 stash");
    }

    #[test]
    fn live_unchanged_false_after_append() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        std::fs::write(mp.join("s.jsonl"), b"{\"a\":1}\n").unwrap();

        let pre = check_preconditions(&paths, "demo", Backend::Shadow, true).unwrap();
        let snap = pre.snapshot.entries[0].clone();

        // 快照后追加 → size/mtime 变 → 复核必须为假（零丢失基石）。
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mp.join("s.jsonl"))
            .unwrap();
        f.write_all(b"{\"b\":2}\n").unwrap();
        f.sync_all().unwrap();
        drop(f);

        assert!(!live_entry_unchanged(&mp, &snap).unwrap());
    }

    #[test]
    fn missing_live_entry_is_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let snap = EntrySnapshot {
            rel: "gone.jsonl".to_string(),
            bytes: Vec::new(),
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            ino: 0,
        };
        assert!(!live_entry_unchanged(&mp, &snap).unwrap());
    }

    #[test]
    fn active_session_rejected_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        // 刚写的 jsonl → recent_log_write 命中活跃；force=false 必须被拒。
        std::fs::write(mp.join("s.jsonl"), b"{}").unwrap();
        let e = check_preconditions(&paths, "demo", Backend::Shadow, false).unwrap_err();
        assert!(e.to_string().contains("活跃"));
    }

    #[test]
    fn traversal_name_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let e = check_preconditions(&paths, "../escape", Backend::Shadow, false).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn atomic_write_then_readback() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.jsonl");
        atomic_write(&p, b"line1\nline2\n").unwrap();
        assert!(readback_eq(&p, b"line1\nline2\n").unwrap());
        assert!(
            !p.with_extension("jsonl.tmp").exists(),
            "tmp 应已 rename 消失"
        );
    }

    #[test]
    fn readback_eq_detects_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.jsonl");
        atomic_write(&p, b"line1\n").unwrap();
        assert!(!readback_eq(&p, b"line2\n").unwrap(), "内容不符应为假");
    }

    #[test]
    fn byte_equal_superset_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let recv = tmp.path().join("m.jsonl");
        atomic_write(&recv, b"a\nb\n").unwrap();
        assert!(durable_superset_ok(&recv, b"a\nb\n", SupersetMode::ByteEqual).unwrap());
        assert!(
            !durable_superset_ok(&recv, b"a\nb\nc\n", SupersetMode::ByteEqual).unwrap(),
            "ByteEqual 要求逐字节相等"
        );
    }

    #[test]
    fn lines_superset_accepts_when_receiver_covers() {
        let tmp = tempfile::tempdir().unwrap();
        let recv = tmp.path().join("m.jsonl");
        atomic_write(&recv, b"a\nb\nc\n").unwrap();
        // 接收方是源的超集（含额外行 c）→ 许可。
        let ok = durable_superset_ok(&recv, b"a\nb\n", SupersetMode::LinesSuperset).unwrap();
        assert!(ok, "接收方覆盖源全部行 → 许可");
    }

    #[test]
    fn lines_superset_detects_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let recv = tmp.path().join("m.jsonl");
        atomic_write(&recv, b"a\nb\n").unwrap(); // 缺 c
        let ok = durable_superset_ok(&recv, b"a\nb\nc\n", SupersetMode::LinesSuperset).unwrap();
        assert!(!ok, "接收方缺行 → 不许删源");
    }

    #[test]
    fn delete_permitted_when_superset_and_live_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let mp = tmp.path().join("mp");
        std::fs::create_dir_all(&mp).unwrap();
        let live = mp.join("s.jsonl");
        std::fs::write(&live, b"a\nb\n").unwrap();
        let md = std::fs::metadata(&live).unwrap();
        let snap = EntrySnapshot {
            rel: "s.jsonl".into(),
            bytes: b"a\nb\n".to_vec(),
            mtime: md.modified().unwrap(),
            size: md.len(),
            ino: md.ino(),
        };
        let recv = tmp.path().join("m.jsonl");
        atomic_write(&recv, b"a\nb\nc\n").unwrap(); // 接收方是超集
        let ok = delete_permitted(&recv, &snap, SupersetMode::LinesSuperset, &mp).unwrap();
        assert!(ok, "超集 + live 未变 → 许可删");
    }

    #[test]
    fn delete_blocked_when_live_underlay_changed() {
        // 评审 C-a：接收方即便超集，若 live underlay 自快照后被追加（mtime/size 变）→ 不许删。
        let tmp = tempfile::tempdir().unwrap();
        let mp = tmp.path().join("mp");
        std::fs::create_dir_all(&mp).unwrap();
        let live = mp.join("s.jsonl");
        std::fs::write(&live, b"a\nb\n").unwrap();
        let md = std::fs::metadata(&live).unwrap();
        let snap = EntrySnapshot {
            rel: "s.jsonl".into(),
            bytes: b"a\nb\n".to_vec(),
            mtime: md.modified().unwrap(),
            size: md.len(),
            ino: md.ino(),
        };
        let recv = tmp.path().join("m.jsonl");
        atomic_write(&recv, b"a\nb\nc\n").unwrap(); // 接收方是超集
                                                    // Claude 追加 → live 变。
        std::fs::write(&live, b"a\nb\nEXTRA\n").unwrap();
        let ok = delete_permitted(&recv, &snap, SupersetMode::LinesSuperset, &mp).unwrap();
        assert!(!ok, "live underlay 已变 → 即便接收方超集也不许删（防丢尾）");
    }

    #[test]
    fn delete_blocked_when_receiver_not_superset() {
        let tmp = tempfile::tempdir().unwrap();
        let mp = tmp.path().join("mp");
        std::fs::create_dir_all(&mp).unwrap();
        let live = mp.join("s.jsonl");
        std::fs::write(&live, b"a\nb\nc\n").unwrap();
        let md = std::fs::metadata(&live).unwrap();
        let snap = EntrySnapshot {
            rel: "s.jsonl".into(),
            bytes: b"a\nb\nc\n".to_vec(),
            mtime: md.modified().unwrap(),
            size: md.len(),
            ino: md.ino(),
        };
        let recv = tmp.path().join("m.jsonl");
        atomic_write(&recv, b"a\nb\n").unwrap(); // 接收方缺 c → 非超集
        let ok = delete_permitted(&recv, &snap, SupersetMode::LinesSuperset, &mp).unwrap();
        assert!(!ok, "接收方非超集 → 即便 live 未变也不许删");
    }

    /// 写一份 committed 提交标记（供 reingest_one_file 取 chunk_size/level）。
    fn write_committed_meta(paths: &Paths, name: &str) {
        let meta = discovery::Meta::from_apply(&ApplyOptions::default(), 0, 0, 0);
        std::fs::create_dir_all(paths.back_root()).unwrap();
        discovery::write_meta(&paths.meta_path(name), &meta).unwrap();
    }

    /// 读回一个 backing archive 文件的完整明文（逐块解压）。
    fn read_archive(path: &Path) -> Vec<u8> {
        use crate::core::codec::{decompress_block, Algo};
        let r = crate::archive::ArchiveReader::open(path).unwrap();
        let mut got = Vec::new();
        for i in 0..r.chunk_count() {
            let (b, e) = r.read_block(i).unwrap().unwrap();
            got.extend_from_slice(
                &decompress_block(&b, Algo::Zstd, e.is_verbatim(), None).unwrap(),
            );
        }
        got
    }

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
}
