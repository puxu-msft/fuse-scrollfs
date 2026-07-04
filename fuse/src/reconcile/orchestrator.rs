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
use crate::reconcile::advisor::{recommend, Action, Confidence, Recommendation};
use crate::reconcile::guard::{is_harmless, underlay_has_fallthrough};
use crate::reconcile::merge::session_merge;
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

// ── 逐条目规划 + 落盘（据快照，非 live） ──────────────────────────────────────

/// 单条目的处置计划（§5.3 分类结果）。本任务只实现 `Union`/`New`/`Identical` 的落盘；
/// `KeepSeparate`/`Passthrough`/`KeepBoth` 由 `apply_entry` 记为 deferred，留 Task 8。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPlan {
    /// jsonl 无损并集并入 orig base。
    Union,
    /// 疑 session-id 重用等：另存不并入（Task 8）。
    KeepSeparate,
    /// orig 无此条目：全新 fall-through 文件直接落盘。
    New,
    /// 透传还原（Task 8）。
    Passthrough,
    /// 超限/冲突：两份都留，供人工核查（Task 8）。
    KeepBoth,
    /// incoming 与 orig 逐字节相同且已在 backing：直接删 underlay。
    Identical,
}

/// 单条目落盘报告（人类可读审计）。`decision`/`action` 是短标签，`notes` 记 stash 路径、
/// delete_permitted 未通过原因等细节。
#[derive(Debug, Clone)]
pub struct EntryReport {
    pub name: String,
    pub decision: String,
    pub action: String,
    pub notes: Vec<String>,
}

/// 一次 reconcile 的整体报告：逐条目报告 + 快照 stash 目录（供审计/回滚定位）。
#[derive(Debug, Clone)]
pub struct ReconcileReport {
    pub entries: Vec<EntryReport>,
    pub stash_dir: PathBuf,
}

/// 逐条目规划（**从快照读 incoming**，非 live underlay，评审 I-7；base 取 orig；**不动盘**）。
///
/// 对每个快照条目：base = `orig/<rel>`（存在则读，不存在 = New），incoming = 快照 `bytes`。
/// 超 `MAX_MERGE_FILE_BYTES`（快照未整体读入、`bytes` 为空）→ 降级 `KeepBoth`；否则：
/// orig 缺 → `New`；base 逐字节 == incoming → `Identical`；`.jsonl` → `session_merge` + `recommend`
/// 按 advisor 动作映射 EntryPlan；非 jsonl 且不同 → 保守 `KeepSeparate`（不做行合并，留 Task 8）。
pub fn plan_entries(
    paths: &Paths,
    name: &str,
    snap: &UnderlaySnapshot,
) -> io::Result<Vec<(String, EntryPlan, Recommendation)>> {
    validate_name(name)?;
    let orig_root = paths.orig(name);
    let mut out = Vec::with_capacity(snap.entries.len());
    for e in &snap.entries {
        if e.size > MAX_MERGE_FILE_BYTES {
            out.push((e.rel.clone(), EntryPlan::KeepBoth, oversize_rec()));
            continue;
        }
        let base = match std::fs::read(orig_root.join(&e.rel)) {
            Ok(b) => Some(b),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        let (plan, rec) = match base {
            None => (EntryPlan::New, new_entry_rec()),
            Some(base_bytes) if base_bytes == e.bytes => (EntryPlan::Identical, identical_rec()),
            Some(base_bytes) if e.rel.ends_with(".jsonl") => {
                let base_str = String::from_utf8_lossy(&base_bytes);
                let inc_str = String::from_utf8_lossy(&e.bytes);
                let merged = session_merge(base_str.as_ref(), inc_str.as_ref());
                let rec = recommend(&merged);
                (plan_from_action(&rec.action), rec)
            }
            Some(_) => (EntryPlan::KeepSeparate, non_jsonl_diff_rec()),
        };
        out.push((e.rel.clone(), plan, rec));
    }
    Ok(out)
}

/// advisor 动作 → EntryPlan 映射（jsonl 合并路径）。
fn plan_from_action(a: &Action) -> EntryPlan {
    match a {
        Action::UnionIntoBase => EntryPlan::Union,
        Action::KeepSeparate => EntryPlan::KeepSeparate,
        Action::PassthroughRestore => EntryPlan::Passthrough,
        Action::KeepBoth => EntryPlan::KeepBoth,
    }
}

fn oversize_rec() -> Recommendation {
    Recommendation {
        action: Action::KeepBoth,
        confidence: Confidence::Low,
        rationale: format!(
            "超单文件合并上限 {MAX_MERGE_FILE_BYTES}B，降级 KeepBoth 保两份（不有损合并）"
        ),
    }
}

fn new_entry_rec() -> Recommendation {
    Recommendation {
        action: Action::UnionIntoBase,
        confidence: Confidence::High,
        rationale: "orig 无此条目，全新 fall-through 文件直接落 orig（无 base 冲突）".into(),
    }
}

fn identical_rec() -> Recommendation {
    Recommendation {
        action: Action::UnionIntoBase,
        confidence: Confidence::High,
        rationale: "incoming 与 orig 逐字节相同，无需改写，直接删 underlay".into(),
    }
}

fn non_jsonl_diff_rec() -> Recommendation {
    Recommendation {
        action: Action::KeepSeparate,
        confidence: Confidence::Low,
        rationale: "非 .jsonl 且与 orig base 不同，不做行合并；留待人工/Task 8 处理".into(),
    }
}

/// merged_lines → 字节：以 `\n` 连接并补尾 `\n`（jsonl 行语义），使删除许可的行超集比对含尾
/// 空行 token 也自洽（incoming 尾 `\n` split 出的空串在 merged 中同样出现）。空则空字节。
fn lines_to_bytes(lines: &[String]) -> Vec<u8> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut s = lines.join("\n");
    s.push('\n');
    s.into_bytes()
}

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

/// 逐条目落盘（**严格顺序**，评审 I-3/C-a）：
/// 1. **先 stash orig 前镜像**（改 orig 前留底，可回滚）。
/// 2. 计算合并明文（Union：`session_merge` 并集；New：incoming bytes）。
/// 3. `atomic_write(orig/<rel>)`（全有或全无）。
/// 4. `reingest_one_file`（原子替换 backing archive）。
/// 5. `delete_permitted` 通过**才**删 underlay 条目；不过则中止该条目、underlay 保留、notes 记原因。
///
/// Identical 无需改 orig/backing，直接过 `delete_permitted`（ByteEqual）删 underlay。
/// KeepSeparate → `quarantine_reuse` 隔离（base 不动，ByteEqual 删除门）。仅 KeepBoth 仍 deferred。
pub fn apply_entry(
    paths: &Paths,
    name: &str,
    snap_entry: &EntrySnapshot,
    plan: &EntryPlan,
    mp: &Path,
) -> io::Result<EntryReport> {
    validate_name(name)?;
    let rel = snap_entry.rel.clone();
    let orig_file = paths.orig(name).join(&rel);
    let mut notes: Vec<String> = Vec::new();

    match plan {
        EntryPlan::Union => {
            stash_orig_preimage(paths, name, &rel, &mut notes)?;
            let base_bytes = std::fs::read(&orig_file)?;
            let base_str = String::from_utf8_lossy(&base_bytes);
            let inc_str = String::from_utf8_lossy(&snap_entry.bytes);
            let merged = session_merge(base_str.as_ref(), inc_str.as_ref());
            let merged_bytes = lines_to_bytes(&merged.merged_lines);
            atomic_write(&orig_file, &merged_bytes)?;
            reingest_one_file(paths, name, &rel)?;
            finish_delete(
                snap_entry,
                &orig_file,
                SupersetMode::LinesSuperset,
                mp,
                "union",
                notes,
            )
        }
        EntryPlan::New => {
            stash_orig_preimage(paths, name, &rel, &mut notes)?;
            if let Some(parent) = orig_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            atomic_write(&orig_file, &snap_entry.bytes)?;
            reingest_one_file(paths, name, &rel)?;
            finish_delete(
                snap_entry,
                &orig_file,
                SupersetMode::LinesSuperset,
                mp,
                "new",
                notes,
            )
        }
        EntryPlan::Identical => {
            // Minor1：Identical 前提是 incoming 已在 backing。但「orig 有、backing 缺」时直接删
            // underlay 会致挂载视图缺该文件（backing 才是被 serve 的一侧）。缺则先从 orig 重灌补齐
            // backing，再走删除门；orig 不动（已与 incoming 逐字节相同）。
            let backing_file = paths.backing(name, Backend::Shadow).join(&rel);
            if !backing_file.exists() {
                notes.push(format!(
                    "backing/{rel} 缺失（orig 有 backing 缺）→ 降级 reingest 补齐后再删"
                ));
                reingest_one_file(paths, name, &rel)?;
            }
            finish_delete(
                snap_entry,
                &orig_file,
                SupersetMode::ByteEqual,
                mp,
                "identical",
                notes,
            )
        }
        EntryPlan::KeepSeparate => {
            // 疑 reuse：隔离 underlay 那份到 quarantine（移出树、保 UUID），base 不动，ByteEqual 删除门。
            let ts = now_unix_secs();
            let q = quarantine_reuse(paths, name, &ts, snap_entry, mp)?;
            notes.push(format!("quarantine={}", q.display()));
            finish_delete(
                snap_entry,
                &q,
                SupersetMode::ByteEqual,
                mp,
                "keep-separate",
                notes,
            )
        }
        other => {
            notes.push(format!(
                "{other:?} 未在本任务落盘（KeepBoth 待人工/后续），underlay 保留待处理"
            ));
            Ok(EntryReport {
                name: rel,
                decision: format!("{other:?}"),
                action: "deferred".into(),
                notes,
            })
        }
    }
}

/// 把当前 `orig/<rel>` 拷进 `reconcile_stash(name,ts)/orig/<rel>` 并 fsync（评审 I-3，改 orig 前留底）。
/// orig 不存在（New 条目）→ 无前镜像可 stash，记 note 后返回。stash 路径记入 `notes`（回滚定位）。
fn stash_orig_preimage(
    paths: &Paths,
    name: &str,
    rel: &str,
    notes: &mut Vec<String>,
) -> io::Result<()> {
    let orig_file = paths.orig(name).join(rel);
    if !orig_file.exists() {
        notes.push(format!("orig/{rel} 不存在，无前镜像可 stash（New 条目）"));
        return Ok(());
    }
    let ts = now_unix_secs();
    let stash_root = paths.reconcile_stash(name, &ts);
    let dst = stash_root.join("orig").join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&orig_file, &dst)?;
    fsync_path(&dst)?;
    if let Some(parent) = dst.parent() {
        fsync_dir_chain(parent, &stash_root)?;
    }
    notes.push(format!("stash-preimage={}", dst.display()));
    Ok(())
}

/// 落盘尾闸：`delete_permitted` 通过才删 underlay 条目（唯一删除入口），否则保留并记原因。
/// 删除后 fsync 父目录持久化 dirent。返回带 action 的 `EntryReport`。
fn finish_delete(
    snap_entry: &EntrySnapshot,
    receiver: &Path,
    mode: SupersetMode,
    mp: &Path,
    kind: &str,
    mut notes: Vec<String>,
) -> io::Result<EntryReport> {
    let rel = snap_entry.rel.clone();
    let action = if delete_permitted(receiver, snap_entry, mode, mp)? {
        let live = mp.join(&rel);
        match std::fs::remove_file(&live) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(parent) = live.parent() {
            let _ = fsync_dir(parent);
        }
        format!("{kind}-applied+underlay-removed")
    } else {
        notes.push(
            "delete_permitted 未通过（接收方非超集 或 live underlay 自快照已变）：underlay 保留"
                .into(),
        );
        format!("{kind}-applied+underlay-kept")
    };
    Ok(EntryReport {
        name: rel,
        decision: kind.into(),
        action,
        notes,
    })
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

    // ── 逐条目规划 + 落盘 ──────────────────────────────────────────────────

    /// 把 `bytes` 写成 `mp/rel` 的 live underlay 文件，并按其真实 metadata 造快照条目。
    fn snap_entry_of(mp: &Path, rel: &str, bytes: &[u8]) -> EntrySnapshot {
        let live = mp.join(rel);
        if let Some(p) = live.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&live, bytes).unwrap();
        let md = std::fs::metadata(&live).unwrap();
        EntrySnapshot {
            rel: rel.to_string(),
            bytes: bytes.to_vec(),
            mtime: md.modified().unwrap(),
            size: md.len(),
            ino: md.ino(),
        }
    }

    /// 建 orig/<rel>（含父目录）写入 base 内容。
    fn write_orig(paths: &Paths, name: &str, rel: &str, content: &[u8]) -> PathBuf {
        let f = paths.orig(name).join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, content).unwrap();
        f
    }

    const BASE_LOG: &str = concat!(
        "{\"type\":\"assistant\",\"uuid\":\"u1\",\"timestamp\":\"2026-06-27T12:00:00.000Z\"}\n",
        "{\"type\":\"ai-title\",\"aiTitle\":\"old\"}\n"
    );
    const INCOMING_LOG: &str = concat!(
        "{\"type\":\"ai-title\",\"aiTitle\":\"new\"}\n",
        "{\"type\":\"mode\",\"mode\":\"normal\"}\n"
    );

    #[test]
    fn apply_union_log_only_merges_orig_reingests_and_removes_underlay() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "s.jsonl";
        let orig_file = write_orig(&paths, "demo", rel, BASE_LOG.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, INCOMING_LOG.as_bytes());
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };

        // incoming 只有日志记录（无 uuid）→ LogOnly → Union。
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::Union);

        let report = apply_entry(&paths, "demo", &snap.entries[0], &EntryPlan::Union, &mp).unwrap();

        // orig 现含合并结果：base(u1/old) 全留 + incoming(new/mode) 并入。
        let merged = std::fs::read_to_string(&orig_file).unwrap();
        for needle in ["u1", "old", "new", "\"mode\""] {
            assert!(
                merged.contains(needle),
                "orig 合并结果应含 {needle}：{merged}"
            );
        }
        // underlay 条目已删（delete_permitted 通过）。
        assert!(
            !mp.join(rel).exists(),
            "delete_permitted 通过 → underlay 应删"
        );
        // backing 该文件已原子重灌为合并结果（read-back 逐字节等于 orig）。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), merged.as_bytes());
        assert!(
            report.action.contains("underlay-removed"),
            "report.action={}",
            report.action
        );
    }

    #[test]
    fn apply_union_stashes_orig_preimage_before_mutating_and_is_rollbackable() {
        // 评审 I-3：改 orig 前 stash 里必须已有旧版，中途放弃可从 stash 回滚 orig。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "s.jsonl";
        let orig_file = write_orig(&paths, "demo", rel, BASE_LOG.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, INCOMING_LOG.as_bytes());

        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::Union, &mp).unwrap();

        // orig 已被合并改写（≠ base）。
        assert_ne!(std::fs::read(&orig_file).unwrap(), BASE_LOG.as_bytes());
        // stash 前镜像 = 改 orig 前的 base，可回滚。
        let stashed = report
            .notes
            .iter()
            .find_map(|n| n.strip_prefix("stash-preimage="))
            .expect("应记录 stash-preimage 路径");
        let stashed = PathBuf::from(stashed);
        assert!(stashed.exists(), "stash 前镜像文件应存在");
        assert_eq!(
            std::fs::read(&stashed).unwrap(),
            BASE_LOG.as_bytes(),
            "stash 应是改 orig 前的镜像"
        );
        // 从 stash 回滚 orig → 复原 base。
        std::fs::copy(&stashed, &orig_file).unwrap();
        assert_eq!(std::fs::read_to_string(&orig_file).unwrap(), BASE_LOG);
    }

    #[test]
    fn apply_union_keeps_underlay_when_live_changed_after_snapshot() {
        // 评审 C-a：接收方即便超集，若 live underlay 自快照后被追加 → delete_permitted 不过、underlay 保留。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "s.jsonl";
        write_orig(&paths, "demo", rel, BASE_LOG.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, INCOMING_LOG.as_bytes());

        // 快照后 Claude 追加 live → size/mtime 变 → live_entry_unchanged 为假。
        let live = mp.join(rel);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .unwrap();
        f.write_all(b"{\"type\":\"extra\"}\n").unwrap();
        f.sync_all().unwrap();
        drop(f);

        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::Union, &mp).unwrap();
        assert!(
            mp.join(rel).exists(),
            "live 已变 → underlay 必须保留（防丢尾）"
        );
        assert!(
            report.action.contains("underlay-kept"),
            "report.action={}",
            report.action
        );
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("delete_permitted 未通过")),
            "notes 应记未通过原因：{:?}",
            report.notes
        );
    }

    #[test]
    fn apply_new_entry_writes_orig_backing_and_removes_underlay() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "fresh.jsonl";
        let incoming = b"{\"type\":\"summary\",\"summary\":\"s\"}\n";
        let snap_e = snap_entry_of(&mp, rel, incoming);
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };

        // orig 无此条目 → New。
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::New);

        let report = apply_entry(&paths, "demo", &snap.entries[0], &EntryPlan::New, &mp).unwrap();

        let orig_file = paths.orig("demo").join(rel);
        assert_eq!(std::fs::read(&orig_file).unwrap(), incoming);
        assert!(!mp.join(rel).exists(), "New 落盘后 underlay 应删");
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), incoming);
        assert!(report.action.contains("underlay-removed"));
    }

    #[test]
    fn apply_identical_removes_underlay_without_touching_orig_or_backing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "same.jsonl";
        let content = b"{\"type\":\"x\",\"uuid\":\"z\"}\n";
        let orig_file = write_orig(&paths, "demo", rel, content);
        // 预灌 backing（Identical 前提：incoming 已在 backing）。
        reingest_one_file(&paths, "demo", rel).unwrap();
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        let backing_before = read_archive(&backing_file);

        let snap_e = snap_entry_of(&mp, rel, content);
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::Identical);

        let report =
            apply_entry(&paths, "demo", &snap.entries[0], &EntryPlan::Identical, &mp).unwrap();
        assert!(!mp.join(rel).exists(), "Identical 应直接删 underlay");
        // orig / backing 均未改。
        assert_eq!(std::fs::read(&orig_file).unwrap(), content);
        assert_eq!(read_archive(&backing_file), backing_before);
        assert!(report.action.contains("underlay-removed"));
    }

    #[test]
    fn apply_identical_missing_backing_downgrades_to_reingest() {
        // Minor1（Task7 遗留）：orig 有、backing 缺时，Identical 直接删 underlay 会致挂载视图缺
        // 该文件。降级：先 reingest 从 orig 补齐 backing，再走删除门。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "same.jsonl";
        let content = b"{\"type\":\"x\",\"uuid\":\"z\"}\n";
        let orig_file = write_orig(&paths, "demo", rel, content);
        // 关键前提：orig 有、backing 无（不预灌）。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert!(!backing_file.exists(), "前提：backing 缺失");

        let snap_e = snap_entry_of(&mp, rel, content);
        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::Identical, &mp).unwrap();

        // 降级 reingest：backing 被补齐为 orig 内容。
        assert_eq!(
            read_archive(&backing_file),
            content,
            "backing 应补齐为 orig 内容"
        );
        assert!(!mp.join(rel).exists(), "补齐 backing 后应删 underlay");
        assert!(report.action.contains("underlay-removed"));
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("backing") && n.contains("reingest")),
            "notes 应记录降级 reingest：{:?}",
            report.notes
        );
        assert_eq!(std::fs::read(&orig_file).unwrap(), content, "orig 不变");
    }

    #[test]
    fn keep_separate_quarantines_reuse_preserving_uuid_and_leaves_base() {
        // SuspectReuse → KeepSeparate：隔离副本保原 <uuid>.jsonl 名、移出 projects 树；base 不动；
        // underlay 经 ByteEqual 超集校验后删。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // Claude 会话文件名即 <uuid>.jsonl。
        let rel = "3f2a-b1c2-uuid.jsonl";
        let base = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
        );
        let incoming = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"b1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
        );
        let orig_file = write_orig(&paths, "demo", rel, base.as_bytes());
        reingest_one_file(&paths, "demo", rel).unwrap();
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        let backing_before = read_archive(&backing_file);

        let snap_e = snap_entry_of(&mp, rel, incoming.as_bytes());
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };
        // disjoint uuid、无桥、时间窗不交 → SuspectReuse → KeepSeparate。
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::KeepSeparate);

        let report = apply_entry(
            &paths,
            "demo",
            &snap.entries[0],
            &EntryPlan::KeepSeparate,
            &mp,
        )
        .unwrap();

        // 隔离副本：quarantine 下出现原 <uuid>.jsonl，内容 == underlay incoming。
        let q = report
            .notes
            .iter()
            .find_map(|n| n.strip_prefix("quarantine="))
            .map(PathBuf::from)
            .expect("应记 quarantine 路径");
        assert_eq!(
            q.file_name().unwrap().to_str().unwrap(),
            rel,
            "保原 UUID 文件名"
        );
        assert_eq!(std::fs::read(&q).unwrap(), incoming.as_bytes());
        // 隔离区在 projects 树外（zipfs_home 下），不在 projects_root。
        assert!(
            q.starts_with(&paths.zipfs_home),
            "quarantine 应在 zipfs_home 下"
        );
        assert!(
            !q.starts_with(&paths.projects_root),
            "quarantine 应移出 projects 树"
        );
        // base（orig/backing）绝不改动。
        assert_eq!(
            std::fs::read(&orig_file).unwrap(),
            base.as_bytes(),
            "orig base 不变"
        );
        assert_eq!(
            read_archive(&backing_file),
            backing_before,
            "backing base 不变"
        );
        // underlay 经 ByteEqual 校验后删。
        assert!(!mp.join(rel).exists(), "隔离且校验后应删 underlay");
        assert!(report.action.contains("underlay-removed"));
    }

    #[test]
    fn plan_entries_downgrades_oversize_to_keep_both() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        // 超限条目：bytes 留空（快照未整体读入），size 记超限值。
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![EntrySnapshot {
                rel: "huge.jsonl".into(),
                bytes: Vec::new(),
                mtime: SystemTime::UNIX_EPOCH,
                size: MAX_MERGE_FILE_BYTES + 1,
                ino: 1,
            }],
        };
        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(plans[0].1, EntryPlan::KeepBoth, "超限应降级 KeepBoth");
    }

    #[test]
    fn apply_keep_both_still_deferred_keeps_underlay() {
        // KeepBoth 仍 deferred：underlay 保留、报告标 deferred（与已实现的 KeepSeparate 区分）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let rel = "kb.jsonl";
        let snap_e = snap_entry_of(&mp, rel, b"{\"a\":1}\n");
        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::KeepBoth, &mp).unwrap();
        assert_eq!(report.action, "deferred");
        assert!(mp.join(rel).exists(), "deferred 计划不得删 underlay");
    }
}
