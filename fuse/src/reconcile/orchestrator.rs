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

use crate::enable::daemon::Mounter;
use crate::enable::discovery::{self, detect_activity};
use crate::enable::model::{validate_name, ApplyOptions, Backend, Paths};
use crate::reconcile::advisor::{recommend, Action, Confidence, Recommendation};
use crate::reconcile::guard::{is_harmless, underlay_has_fallthrough};
use crate::reconcile::merge::session_merge;
use crate::store::lock::acquire_exclusive_retry;

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
        // 评审 W1：underlay 空但 `.reconciling` marker 在 → 上次 reconcile 的收尾（清标记那步）
        // 被崩溃打断（underlay 已抽干、manifest 未落）。orig/backing 此刻已是权威提交态，清陈旧
        // marker 收敛即可——否则 marker 永久滞留，`bail_if_reconciling` 把 remount/compact/seal/
        // 自启全拦死，项目 wedge，只能人工 rm。这也是「重跑 reconcile 自恢复」承诺的兑现点。
        if paths.reconciling_marker(name).exists() {
            set_reconciling(paths, name, false)?;
            return Err(io::Error::other(format!(
                "{} underlay 已空但残留 reconciling 标记（上次收尾被中断）→ 已清标记收敛，项目恢复可维护；无需 reconcile",
                mp.display()
            )));
        }
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
    let lock = acquire_exclusive_retry(&lock_path)?;
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
    let backing = paths.backing(name, Backend::Shadow);
    let backing_file = backing.join(rel);
    // 评审 R-lock：取 backing 排他锁（与 compact/seal/守护同一把 `.zipfs.lock`）保护本次 temp+rename
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

/// 逆转类：一次 reconcile 对某条目所做落盘变更的**反做类别**（undo 依赖，§10.1）。写入 per-generation
/// manifest（`rel\tclass` 行格式），供 Task 4 `reconcile_undo` 逐条目反向还原。
///
/// - `RestoreOrig`：改过 orig（union/new-with-preimage），有前镜像 → 从 `stash/<ts>/orig/<rel>` 原子还原。
/// - `RemoveOrig`：新增了 orig（New，无前镜像）→ 删 orig + backing。
/// - `RemoveQuarantine`：把 underlay 副本隔离进 quarantine（KeepSeparate）→ byte-check 后删 quarantine 副本。
/// - `ReportMemory`：memory 透传实际 relocate 了 → undo 只报告待人工 git 回退（绝不触碰外部 target）。
/// - `Noop`：无需反做（identical/skip/deferred/透传路径安全闸拦截等，underlay 快照全局还原即可）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReversalClass {
    RestoreOrig,
    RemoveOrig,
    RemoveQuarantine,
    ReportMemory,
    Noop,
}

impl ReversalClass {
    /// 稳定的 manifest 序列化标签（`as_str`/`parse` 互逆）。
    fn as_str(self) -> &'static str {
        match self {
            ReversalClass::RestoreOrig => "RestoreOrig",
            ReversalClass::RemoveOrig => "RemoveOrig",
            ReversalClass::RemoveQuarantine => "RemoveQuarantine",
            ReversalClass::ReportMemory => "ReportMemory",
            ReversalClass::Noop => "Noop",
        }
    }

    /// 解析 manifest 标签（未知标签 → `None`，由调用方决定容错策略）。`reconcile_undo` 经
    /// `read_manifest` 消费。
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "RestoreOrig" => ReversalClass::RestoreOrig,
            "RemoveOrig" => ReversalClass::RemoveOrig,
            "RemoveQuarantine" => ReversalClass::RemoveQuarantine,
            "ReportMemory" => ReversalClass::ReportMemory,
            "Noop" => ReversalClass::Noop,
            _ => return None,
        })
    }
}

/// has_preimage 布尔 → 逆转类：改 orig 前有前镜像（union/merge）→ `RestoreOrig`（可原子还原）；无前镜像
/// （New，orig 是新增出来的）→ `RemoveOrig`（undo 删 orig + backing，防孤儿）。判别子就是 has_preimage。
fn reversal_for_preimage(has_preimage: bool) -> ReversalClass {
    if has_preimage {
        ReversalClass::RestoreOrig
    } else {
        ReversalClass::RemoveOrig
    }
}

/// 合成审计条目（非真实 rel）判定：`<prune>`/`<meta>`/`<rebuild>`/`<prune-symlinks>` 等以 `<` 开头的占位
/// 名，仅供人类审计、无对应磁盘 rel，一律不写入 manifest（评审 I-plan1）。
fn is_synthetic_rel(rel: &str) -> bool {
    rel.starts_with('<')
}

/// 单条目落盘报告（人类可读审计）。`decision`/`action` 是短标签，`notes` 记 stash 路径、
/// delete_permitted 未通过原因等细节。`reversal` 记该条目的逆转类（undo 依赖）。
#[derive(Debug, Clone)]
pub struct EntryReport {
    pub name: String,
    pub decision: String,
    pub action: String,
    pub notes: Vec<String>,
    pub reversal: ReversalClass,
}

/// 一次 reconcile 的整体报告：逐条目报告 + 快照 stash 目录（供审计/回滚定位）。
#[derive(Debug, Clone)]
pub struct ReconcileReport {
    pub entries: Vec<EntryReport>,
    pub stash_dir: PathBuf,
}

/// 逐条目规划（**从快照读 incoming**，非 live underlay，评审 I-7；base 取 orig；**不动盘**）。
///
/// **优先路由**（先于 size-cap/base 分类，与 `apply_entry` 同序，令 dry-run 报告如实反映 apply）：
/// - `is_subagents_entry` → `Union`（子会话强制无损并集，apply 绕过 advisor 隔离）。
/// - `is_passthrough_entry`（backing 顶层段是外链 symlink）→ `Passthrough`（写 canonical target，绝不落 orig）。
///
/// 否则对每个快照条目：base = `orig/<rel>`（存在则读，不存在 = New），incoming = 快照 `bytes`。
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
        // 优先路由（与 apply_entry 同序）：subagents/透传绕过 plan 的 size-cap/base 分类，否则报告
        // 会显示 New/KeepSeparate 而 apply 实际走并集/透传（写 canonical target，绝不落 orig）。
        if is_subagents_entry(&e.rel) {
            out.push((e.rel.clone(), EntryPlan::Union, subagents_rec()));
            continue;
        }
        if is_passthrough_entry(paths, name, &e.rel) {
            out.push((e.rel.clone(), EntryPlan::Passthrough, passthrough_rec()));
            continue;
        }
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

fn subagents_rec() -> Recommendation {
    Recommendation {
        action: Action::UnionIntoBase,
        confidence: Confidence::High,
        rationale:
            "subagents 子会话无损并集（apply 时并入 orig 对应路径 + reingest，绝不按 mtime 取舍）"
                .into(),
    }
}

fn passthrough_rec() -> Recommendation {
    Recommendation {
        action: Action::PassthroughRestore,
        confidence: Confidence::Medium,
        rationale: "memory 外链透传恢复：新文件复制进 canonical 目标、冲突改名保两份，绝不落 orig"
            .into(),
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

/// 判定快照条目是否属于 subagents 子会话目录（`<uuid>/subagents/*.jsonl`）。
///
/// 判据：`rel` 含名为 `subagents` 的普通路径段 **且** 以 `.jsonl` 结尾。此类条目在 `apply_entry`
/// 里被**优先路由**到 `reconcile_subagents_dir` 强制无损并集，绕过 advisor 的 SuspectReuse→隔离——
/// 子代理 transcript 天然按子代理分文件、uuid 各自独立，并集安全且不可按 mtime 取舍。
fn is_subagents_entry(rel: &str) -> bool {
    rel.ends_with(".jsonl")
        && Path::new(rel).components().any(
            |c| matches!(c, std::path::Component::Normal(s) if s.eq_ignore_ascii_case("subagents")),
        )
}

/// `rel` 顶层路径段（第一个 Normal 组件）。用于把 memory 物化条目归到其顶层目录（如 `memory`）。
fn top_component(rel: &str) -> Option<String> {
    Path::new(rel).components().find_map(|c| match c {
        std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
        _ => None,
    })
}

/// 判定条目是否属 memory 透传：其顶层段在 backing 里是 **symlink**（apply 期照 Claude 外链重建的
/// `memory` 软链）。停用期 Claude 把外链内容物化进 underlay 真实目录，此类条目应走透传恢复而非并入历史。
fn is_passthrough_entry(paths: &Paths, name: &str, rel: &str) -> bool {
    passthrough_top_symlink(paths, name, rel)
        .ok()
        .flatten()
        .is_some()
}

/// 若条目顶层段在 backing 是 symlink，返回 `(顶层段, symlink 目标)`；否则 `None`。
/// name 已在 `apply_entry` 入口 `validate_name`，此处只读 backing 元数据。
fn passthrough_top_symlink(
    paths: &Paths,
    name: &str,
    rel: &str,
) -> io::Result<Option<(String, PathBuf)>> {
    let Some(top) = top_component(rel) else {
        return Ok(None);
    };
    let link = paths.backing(name, Backend::Shadow).join(&top);
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(&link)?;
            Ok(Some((top, target)))
        }
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// subagents 子会话 jsonl 合并（**与主 jsonl 同一 `session_merge` 规则**，但强制无损并集）：
///
/// orig 有对应文件 → `session_merge` 并集写回；orig 无 → 全新落盘（New）。随后 `reingest_one_file`
/// 原子重灌 backing，最后经 `finish_delete`（LinesSuperset）删 underlay。**绝不按 mtime 删较旧者**、
/// 同名异内容一律并集保两侧——子代理 transcript disjoint uuid 是常态（各子代理独立），并入无丢失；
/// 隔离/取舍反而会丢子会话历史，故 subagents 一律并集。改 orig 前照旧 stash 前镜像（可回滚）。
pub fn reconcile_subagents_dir(
    paths: &Paths,
    name: &str,
    snap_entry: &EntrySnapshot,
    mp: &Path,
    ts: &str,
) -> io::Result<EntryReport> {
    validate_name(name)?;
    let rel = snap_entry.rel.clone();
    let orig_file = paths.orig(name).join(&rel);
    let mut notes: Vec<String> = vec!["subagents：强制无损并集（绝不按 mtime 取舍）".into()];

    let has_preimage = stash_orig_preimage(paths, name, &rel, ts, &mut notes)?;
    if let Some(parent) = orig_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let merged_bytes = if orig_file.exists() {
        let base_bytes = std::fs::read(&orig_file)?;
        let base_str = String::from_utf8_lossy(&base_bytes);
        let inc_str = String::from_utf8_lossy(&snap_entry.bytes);
        let merged = session_merge(base_str.as_ref(), inc_str.as_ref());
        let merged_bytes = lines_to_bytes(&merged.merged_lines);
        // 评审 R-C1：base 侧超集铁律（同 apply_entry Union）。不覆盖则中止、保两份。
        if !crate::reconcile::merge::base_covered_by_merged(
            &base_str,
            &String::from_utf8_lossy(&merged_bytes),
        ) {
            notes.push(
                "subagents 合并未覆盖 base 全部记录（疑合并核缺陷）→ 中止：不改 orig、不删 underlay".into(),
            );
            return Ok(EntryReport {
                name: rel,
                decision: "subagents".into(),
                action: "aborted-base-not-covered".into(),
                notes,
                reversal: ReversalClass::Noop,
            });
        }
        merged_bytes
    } else {
        snap_entry.bytes.clone()
    };
    atomic_write(&orig_file, &merged_bytes)?;
    reingest_one_file(paths, name, &rel)?;
    finish_delete(
        snap_entry,
        &orig_file,
        SupersetMode::LinesSuperset,
        mp,
        "subagents-union",
        reversal_for_preimage(has_preimage),
        notes,
    )
}

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
fn passthrough_action(notes: &[String]) -> &'static str {
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
        ".zipfs-memory-write-probe.{}.{nanos}",
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

/// 逐条目落盘（**严格顺序**，评审 I-3/C-a）：
/// 1. **先 stash orig 前镜像**（改 orig 前留底，可回滚）。
/// 2. 计算合并明文（Union：`session_merge` 并集；New：incoming bytes）。
/// 3. `atomic_write(orig/<rel>)`（全有或全无）。
/// 4. `reingest_one_file`（原子替换 backing archive）。
/// 5. `delete_permitted` 通过**才**删 underlay 条目；不过则中止该条目、underlay 保留、notes 记原因。
///
/// **优先路由**（先于 plan 匹配）：subagents 目录条目 → `reconcile_subagents_dir`（强制并集）；
/// memory 透传（backing 顶层段是 symlink）→ `passthrough_restore_memory`（例外规则）。
/// Identical 无需改 orig/backing，直接过 `delete_permitted`（ByteEqual）删 underlay。
/// KeepSeparate → `quarantine_reuse` 隔离（base 不动，ByteEqual 删除门）。仅 KeepBoth 仍 deferred。
///
/// `ts` 是**贯穿整个 reconcile run 的单一时间戳**（Task7 Minor2）：orig 前镜像 stash、quarantine、
/// passthrough stash 全用同一 `ts`，令一次 reconcile 的所有落盘落同一代次目录（而非每条目各自
/// `now_unix_secs`）。由 `reconcile` 从 `UnderlaySnapshot::ts` 传入。
pub fn apply_entry(
    paths: &Paths,
    name: &str,
    snap_entry: &EntrySnapshot,
    plan: &EntryPlan,
    mp: &Path,
    ts: &str,
) -> io::Result<EntryReport> {
    validate_name(name)?;
    let rel = snap_entry.rel.clone();
    let orig_file = paths.orig(name).join(&rel);
    let mut notes: Vec<String> = Vec::new();

    // 优先路由：subagents 子会话一律无损并集，绕过 plan（防 SuspectReuse 误隔离子会话）。
    if is_subagents_entry(&rel) {
        return reconcile_subagents_dir(paths, name, snap_entry, mp, ts);
    }

    // 优先路由：memory 透传。backing 顶层段是 symlink → 该条目属外链 memory 的物化回落写。
    // （plan_entries 现也产 Passthrough plan；据 backing symlink 判定，两条路由等价、互为兜底。）
    if matches!(plan, EntryPlan::Passthrough) || is_passthrough_entry(paths, name, &rel) {
        if let Some((top, target)) = passthrough_top_symlink(paths, name, &rel)? {
            let underlay_dir = mp.join(&top);
            let stash_dir = paths.quarantine(name, ts).join(&top);
            let notes = passthrough_restore_memory(&underlay_dir, &target, &stash_dir)?;
            // 据结果如实报 action（评审 M4）：路径安全闸未过时 underlay 未动，不能谎报 restored。
            let action = passthrough_action(&notes);
            // 实际 relocate（memory-restored）→ ReportMemory（undo 只报告待人工 git 回退）；透传 noop 或
            // 路径安全闸拦截（underlay 未动）→ Noop。
            let reversal = if action == "memory-restored" {
                ReversalClass::ReportMemory
            } else {
                ReversalClass::Noop
            };
            return Ok(EntryReport {
                name: rel,
                decision: "passthrough".into(),
                action: action.into(),
                notes,
                reversal,
            });
        }
    }

    match plan {
        EntryPlan::Union => {
            let base_bytes = std::fs::read(&orig_file)?;
            let base_str = String::from_utf8_lossy(&base_bytes);
            let inc_str = String::from_utf8_lossy(&snap_entry.bytes);
            let merged = session_merge(base_str.as_ref(), inc_str.as_ref());
            let merged_bytes = lines_to_bytes(&merged.merged_lines);
            // 评审 R-C1（双向超集铁律 base 半边）：incoming ⊆ merged 由 finish_delete 删除门把关；
            // base ⊆ merged 在此 fail-fast 校验——merged 若丢了 base 任一记录（疑合并核缺陷），
            // **绝不覆盖金源 orig、绝不删 underlay**，保两份待人工，杜绝静默失真。
            if !crate::reconcile::merge::base_covered_by_merged(
                &base_str,
                &String::from_utf8_lossy(&merged_bytes),
            ) {
                notes.push(
                    "合并结果未覆盖 base 全部记录（疑合并核缺陷）→ 中止：不改 orig、不删 underlay，保两份".into(),
                );
                return Ok(EntryReport {
                    name: rel,
                    decision: "union".into(),
                    action: "aborted-base-not-covered".into(),
                    notes,
                    reversal: ReversalClass::Noop,
                });
            }
            let has_preimage = stash_orig_preimage(paths, name, &rel, ts, &mut notes)?;
            atomic_write(&orig_file, &merged_bytes)?;
            reingest_one_file(paths, name, &rel)?;
            finish_delete(
                snap_entry,
                &orig_file,
                SupersetMode::LinesSuperset,
                mp,
                "union",
                reversal_for_preimage(has_preimage),
                notes,
            )
        }
        EntryPlan::New => {
            let has_preimage = stash_orig_preimage(paths, name, &rel, ts, &mut notes)?;
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
                reversal_for_preimage(has_preimage),
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
            // orig 未改、backing 已有 incoming：undo 无需反做（underlay 快照全局还原即可）。
            finish_delete(
                snap_entry,
                &orig_file,
                SupersetMode::ByteEqual,
                mp,
                "identical",
                ReversalClass::Noop,
                notes,
            )
        }
        EntryPlan::KeepSeparate => {
            // 疑 reuse：隔离 underlay 那份到 quarantine（移出树、保 UUID），base 不动，ByteEqual 删除门。
            let q = quarantine_reuse(paths, name, ts, snap_entry, mp)?;
            notes.push(format!("quarantine={}", q.display()));
            finish_delete(
                snap_entry,
                &q,
                SupersetMode::ByteEqual,
                mp,
                "keep-separate",
                ReversalClass::RemoveQuarantine,
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
                reversal: ReversalClass::Noop,
            })
        }
    }
}

/// 把当前 `orig/<rel>` 拷进 `reconcile_stash(name,ts)/orig/<rel>` 并 fsync（评审 I-3，改 orig 前留底）。
/// **返回是否真拷了前镜像**（= orig 预存）：orig 不存在（New 条目）→ 无前镜像可 stash，记 note 返回
/// `false`；实际拷贝 → 返回 `true`。该布尔是 union/subagents 伞下精确区分 merge（`RestoreOrig`）与
/// new（`RemoveOrig`）的判别子（防 undo 孤儿）。stash 路径记入 `notes`（回滚定位）。
///
/// `ts` 是**贯穿整个 reconcile run 的单一时间戳**（= `UnderlaySnapshot::ts`，Task7 Minor2）：一次
/// reconcile 内所有条目的前镜像与快照落同一 `reconcile_stash(name,ts)` 代次，便于审计/回滚定位，
/// 不再每条目各自 `now_unix_secs`（会散落到多个代次目录）。
fn stash_orig_preimage(
    paths: &Paths,
    name: &str,
    rel: &str,
    ts: &str,
    notes: &mut Vec<String>,
) -> io::Result<bool> {
    let orig_file = paths.orig(name).join(rel);
    if !orig_file.exists() {
        notes.push(format!("orig/{rel} 不存在，无前镜像可 stash（New 条目）"));
        return Ok(false);
    }
    let stash_root = paths.reconcile_stash(name, ts);
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
    Ok(true)
}

/// 落盘尾闸：`delete_permitted` 通过才删 underlay 条目（唯一删除入口），否则保留并记原因。
/// 删除后 fsync 父目录持久化 dirent。返回带 action 的 `EntryReport`。
fn finish_delete(
    snap_entry: &EntrySnapshot,
    receiver: &Path,
    mode: SupersetMode,
    mp: &Path,
    kind: &str,
    reversal: ReversalClass,
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
        reversal,
    })
}

// ── per-generation manifest（undo 依赖，§10.1） ───────────────────────────────

/// 落盘一次 reconcile run 的 per-generation manifest 到 `reconcile_manifest(name,ts)`：首行 `ts`，其后
/// 每行真实 `rel\tclass`（逆转类）。**过滤合成审计条目**（`<prune>`/`<meta>` 等非真实 rel）与 `Noop`
/// 条目（identical/skip 等无需反做，underlay 快照全局还原即可覆盖），只写 undo 真正需要逐条反做的条目。
///
/// 原子写 + fsync（`atomic_write`）：manifest 存在即代表该代次可 undo；不完整写入绝不半落盘。best-effort
/// 由调用方兜底（写失败该 run 不可 undo，但不阻断收尾）。
fn write_manifest(paths: &Paths, name: &str, ts: &str, entries: &[EntryReport]) -> io::Result<()> {
    validate_name(name)?;
    let mut body = String::new();
    body.push_str(ts);
    body.push('\n');
    for e in entries {
        if is_synthetic_rel(&e.name) || e.reversal == ReversalClass::Noop {
            continue;
        }
        body.push_str(&e.name);
        body.push('\t');
        body.push_str(e.reversal.as_str());
        body.push('\n');
    }
    let dst = paths.reconcile_manifest(name, ts);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&dst, body.as_bytes())
}

/// 读回 `reconcile_manifest(name,ts)`：manifest 不存在 → `Ok(None)`（该代次无 undo 依据）；存在则解析
/// 首行 `ts` 之后的每行 `rel\tclass` 为 `(rel, ReversalClass)`。空行跳过；无法解析的行（缺 tab / 未知
/// class）→ `Err`（fail-closed：宁可拒绝 undo 也不静默漏条）。`reconcile_undo` 消费。
/// 校验 manifest 读回的相对路径 `rel`（Task2 Minor，纵深防御）：`rel` 是**多段相对路径**（如
/// `<uuid>/subagents/x.jsonl`），须每个组件均为 `Normal`——即非 `..`、非绝对根、非 `.`、非空。rel 实源自
/// 真实目录 walk（无 `..`）、stash 本地同信任域，风险低，但反做入口直接 `orig/backing/mp.join(rel)`，故作
/// 纵深防御拒绝穿越/绝对/空。命中（返回 `false`）由 `read_manifest` 跳过该条 + 记 warn，不中止整个 undo。
fn is_safe_rel(rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let mut saw_normal = false;
    for comp in Path::new(rel).components() {
        match comp {
            std::path::Component::Normal(_) => saw_normal = true,
            // RootDir（绝对）/ ParentDir（..）/ CurDir（.）/ Prefix 均拒。
            _ => return false,
        }
    }
    saw_normal
}

fn read_manifest(
    paths: &Paths,
    name: &str,
    ts: &str,
) -> io::Result<Option<Vec<(String, ReversalClass)>>> {
    validate_name(name)?;
    let path = paths.reconcile_manifest(name, ts);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    // 首行是 ts 头，跳过。
    for line in content.lines().skip(1) {
        if line.is_empty() {
            continue;
        }
        let (rel, class) = line.split_once('\t').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest 行缺 tab 分隔：{line:?}"),
            )
        })?;
        let reversal = ReversalClass::parse(class).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest 未知逆转类：{class:?}"),
            )
        })?;
        // Task2 Minor：纵深防御——拒绝含 `..`/绝对/空组件的 rel（跳过该条 + warn，不中止整个 undo）。
        if !is_safe_rel(rel) {
            log::warn!(
                "{name} 代次 {ts} manifest rel {rel:?} 含非法组件（穿越/绝对/空），跳过该条 undo"
            );
            continue;
        }
        out.push((rel.to_owned(), reversal));
    }
    Ok(Some(out))
}

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
fn prune_empty_underlay_dirs(mp: &Path) -> io::Result<()> {
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
fn prune_redundant_symlinks(paths: &Paths, name: &str, mp: &Path) -> io::Result<Vec<String>> {
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

/// 逐条目的人工确认决定（`ReconcileOptions::confirm` 回调返回）。策略 B：本 driver 只按此裁决，
/// **不自动执行**——交互式提示留 CLI（Task 10），非交互驱动由调用方给恒定策略实现（如全 Accept）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// 采纳建议：按 plan 落盘（Union/New/Identical/KeepSeparate/subagents/passthrough）。
    Accept,
    /// 两份都留：不删 base、underlay 保留（据现有 KeepBoth handler；快照 stash 已留副本）。
    KeepBoth,
    /// 跳过此条：underlay 原样保留、orig 不动。
    Skip,
}

/// 逐条目裁决回调类型（rel + 建议 → `Confirm`）。策略 B：driver 只据此裁决，交互实现留调用方。
pub type ConfirmFn = dyn Fn(&str, &Recommendation) -> Confirm;

/// reconcile 运行选项。`confirm` 是逐条目裁决回调（rel + 建议 → `Confirm`）。
pub struct ReconcileOptions {
    /// 只出建议单、**零改动**（不 set_reconciling、不 apply）。
    pub dry_run: bool,
    /// 跳过活跃门禁（人工确认空闲后）。
    pub force: bool,
    /// 全量重建：逐条落盘后清 reconciling 标记，委托 `lifecycle::reingest` 从 orig 全量重建 backing。
    pub rebuild: bool,
    /// 逐条目裁决回调。
    pub confirm: Box<ConfirmFn>,
}

/// 顶层 reconcile 主循环（把 Task 1-8 的 handler 串成端到端 driver）：
///
/// 1. 读 meta 取 backend（无 meta / 非 shadow → 拒）；`check_preconditions` 取串行锁 + underlay 快照。
/// 2. `dry_run` → 只 `plan_entries` 出建议单、构建报告返回，**零改动**（不 set_reconciling、不 apply）。
/// 3. 否则 `set_reconciling(true)` → 对每条目 `plan_entries` 给 (rel,plan,rec) → `confirm` 裁决：
///    `Accept`→`apply_entry`；`KeepBoth`→按 KeepBoth handler（不删 base、underlay 保留）；`Skip`→跳过。
/// 4. 逐条处置后：underlay 已清空且非 rebuild → meta 字节数收尾（重扫 backing/orig，committed 不变）；
///    随后 `set_reconciling(false)` 关闭半改写窗口。
/// 5. `rebuild` → 委托 `lifecycle::reingest`（从 orig 全量重建 backing、重挂、自写 meta）。
///
/// **run ts 单一**（Task7 Minor2）：`snapshot.ts` 贯穿整个 run，所有 stash（快照 + 各条目 orig 前
/// 镜像 + quarantine）落同一 `reconcile_stash(name,ts)` 代次。
///
/// **零丢失**：dry_run 绝不动盘；`apply_entry` 的唯一删除门（durable 超集 + live 未变）逐条把关；
/// 崩溃续跑幂等（reconciling 标记在→重跑安全，合并是并集不动点、已删条目不在新快照里故不复现）。
///
/// **rebuild 崩溃恢复**：若在清标记后、`reingest` 中途崩溃，此时 underlay 已清空 → 再跑 reconcile
/// 会被前置门禁拒（无 fall-through）；但 orig 是已 fsync 的权威源、`reingest` 会回滚 backing 并留
/// `.reingest-bak`，故无数据丢失，恢复走 `enable remount` / 手动 `enable reingest` 而非重跑 reconcile。
pub fn reconcile(
    paths: &Paths,
    name: &str,
    opts: ReconcileOptions,
    mounter: &dyn Mounter,
) -> io::Result<ReconcileReport> {
    validate_name(name)?;

    // backend 从 meta 读（无 meta 拒——未 apply）。非 shadow 由 check_preconditions 拒。
    let meta = discovery::read_meta(&paths.meta_path(name))?.ok_or_else(|| {
        io::Error::other(format!(
            "{name} 无提交标记 meta，无法 reconcile（未 apply？）"
        ))
    })?;
    let backend = meta.backend;

    // 1. 门禁 + 快照（取串行锁；backend 非 shadow 在此拒）。锁随 `pre` 存活到函数末。
    let pre = check_preconditions(paths, name, backend, opts.force)?;
    let ts = pre.snapshot.ts.clone();
    let stash_dir = paths.reconcile_stash(name, &ts);
    let mp = paths.mountpoint(name);

    // 2. dry_run：只 plan、零改动。
    if opts.dry_run {
        let plans = plan_entries(paths, name, &pre.snapshot)?;
        let entries = plans
            .into_iter()
            .map(|(rel, plan, rec)| EntryReport {
                name: rel,
                decision: format!("{plan:?}"),
                action: "dry-run".into(),
                notes: vec![rec.rationale],
                reversal: ReversalClass::Noop,
            })
            .collect();
        return Ok(ReconcileReport { entries, stash_dir });
    }

    // 3. 进行中标记（半改写 orig 窗口开）→ 逐条裁决落盘。
    set_reconciling(paths, name, true)?;
    let plans = plan_entries(paths, name, &pre.snapshot)?;
    let mut entries = Vec::with_capacity(plans.len());
    for (rel, plan, rec) in plans {
        // plan 的 rel 恒来自快照；找回对应 EntrySnapshot 供 apply（快照是合并/删除唯一基准）。
        let Some(snap_entry) = pre.snapshot.entries.iter().find(|e| e.rel == rel) else {
            // 理论不可达（plan 源自快照）；防御地记一条审计条目而非静默跳过，绝不动盘。
            entries.push(EntryReport {
                name: rel,
                decision: "skip".into(),
                action: "unmatched-snapshot".into(),
                notes: vec!["plan 条目在快照中无对应项（不可达），防御跳过、underlay 不动".into()],
                reversal: ReversalClass::Noop,
            });
            continue;
        };
        let report = match (opts.confirm)(&rel, &rec) {
            Confirm::Accept => apply_entry(paths, name, snap_entry, &plan, &mp, &ts)?,
            // KeepBoth：按现有 KeepBoth handler（不删 base、underlay 保留；快照 stash 已存副本）。
            Confirm::KeepBoth => {
                apply_entry(paths, name, snap_entry, &EntryPlan::KeepBoth, &mp, &ts)?
            }
            Confirm::Skip => EntryReport {
                name: rel,
                decision: "skip".into(),
                action: "skipped+underlay-kept".into(),
                notes: vec!["用户跳过此条：underlay 原样保留、orig 不动".into()],
                reversal: ReversalClass::Noop,
            },
        };
        entries.push(report);
    }

    // 逐条 apply 后剪除已抽干的空 underlay 子目录（评审 final BREACH 1）：finish_delete 只删文件不
    // rmdir，`<uuid>/subagents/*.jsonl` 抽干后空 `<uuid>/` 仍是顶层非白名单条目，令下面的 drained 复扫
    // 永假、`ensure_underlay_empty` 永久拒挂。自底向上、只删「仅含白名单/全空」目录，仍存 Skip/KeepBoth/
    // 未删条目的目录保留。best-effort：剪枝报错绝不阻断收尾（非数据安全，数据已抽干），与下面 meta
    // finalize 同为「失败仅记 warn、不 wedge」——否则清标记被跳过、reconciling 标记永久卡住把维护拦死。
    if let Err(e) = prune_empty_underlay_dirs(&mp) {
        entries.push(EntryReport {
            name: format!("<prune {name}>"),
            decision: "prune-empty-dirs".into(),
            action: "warn".into(),
            notes: vec![format!(
                "剪除空 underlay 子目录失败（仅影响重挂门禁，非数据安全）：{e}"
            )],
            reversal: ReversalClass::Noop,
        });
    }

    // 并列清除与 backing 同名同目标的顶层冗余 underlay 软链（§6 memory-symlink 短路）：memory 软链
    // 在、写已透传 canonical → walk_snapshot 跳过 symlink 不处理，却令 fall-through 永真拒挂。删这类
    // 冗余软链（backing 有同款、挂载时透传）解锁重挂；目标不一致/异常项保留并报告，绝不误删。best-effort：
    // 与空目录剪枝同为「失败仅记 warn、不 wedge」（非数据安全，软链无内容）。
    match prune_redundant_symlinks(paths, name, &mp) {
        Ok(sym_notes) if !sym_notes.is_empty() => {
            entries.push(EntryReport {
                name: format!("<prune-symlinks {name}>"),
                decision: "prune-redundant-symlinks".into(),
                action: "kept-anomaly".into(),
                notes: sym_notes,
                reversal: ReversalClass::Noop,
            });
        }
        Ok(_) => {}
        Err(e) => {
            entries.push(EntryReport {
                name: format!("<prune-symlinks {name}>"),
                decision: "prune-redundant-symlinks".into(),
                action: "warn".into(),
                notes: vec![format!(
                    "清除冗余 underlay 软链失败（仅影响重挂门禁，非数据安全）：{e}"
                )],
                reversal: ReversalClass::Noop,
            });
        }
    }

    // 4. underlay 清空且非 rebuild → meta 字节数收尾（rebuild 由 reingest 自写 meta，不重复）。
    //    收尾是**纯 list 显示**（非数据安全），失败绝不能阻断下面的 set_reconciling(false)——否则
    //    underlay 已清空、下轮 reconcile 会在前置门禁因「无 fall-through」被拒（永远走不到清标记），
    //    reconciling 标记就永久卡住、把所有生命周期维护经 bail_if_reconciling 拦死。与 reingest 的
    //    「meta 写失败 warn-not-fail」一致：best-effort，失败只记 warn 条目。
    //    同理 underlay 复扫用 unwrap_or(true)（探测出错→保守视为未清空、跳过收尾），绝不因复扫报错
    //    而阻断清标记。
    let drained = !underlay_has_fallthrough(&mp).unwrap_or(true);
    if !opts.rebuild && drained {
        if let Err(e) = finalize_meta_bytes(paths, name, &meta) {
            entries.push(EntryReport {
                name: format!("<meta {name}>"),
                decision: "meta-finalize".into(),
                action: "warn".into(),
                notes: vec![format!(
                    "meta 字节数收尾失败（仅影响 list 显示，非数据安全）：{e}"
                )],
                reversal: ReversalClass::Noop,
            });
        }
    }
    // per-generation manifest（undo 依赖，§10.1）：在条目循环后、清标记前落盘（评审 M3），记每条真实
    // 条目的逆转类供 Task 4 `reconcile_undo` 消费。best-effort：写失败仅 warn（该 run 不可 undo，但绝不
    // 阻断清标记——否则 reconciling 标记永久卡住把维护拦死，与 meta finalize 同策）。合成条目由
    // write_manifest 内部过滤，不入 manifest。
    if let Err(e) = write_manifest(paths, name, &ts, &entries) {
        log::warn!("{name} reconcile manifest 落盘失败（该 run 不可 undo，非数据安全）：{e}");
    }
    // 关闭半改写窗口：逐条 apply 已各自原子完成，orig 处于一致态。崩溃续跑靠「标记在→重跑幂等」，
    // 故仅在正常收尾时清标记（中途崩溃则标记留存，让生命周期维护让路、下次 reconcile 续做）。
    set_reconciling(paths, name, false)?;

    // 5. rebuild：清标记后委托 reingest 从 orig 全量重建 backing + 重挂（committed 全程不变满足其前提）。
    if opts.rebuild {
        let msg = crate::enable::lifecycle::reingest(paths, name, opts.force, mounter)?;
        entries.push(EntryReport {
            name: format!("<rebuild {name}>"),
            decision: "rebuild".into(),
            action: "reingest-delegated".into(),
            notes: vec![msg],
            reversal: ReversalClass::Noop,
        });
    }

    Ok(ReconcileReport { entries, stash_dir })
}

/// meta 字节数收尾（**非数据安全，仅 list 显示**）：重扫 backing 求 `bytes_archive`、扫 orig 求
/// `bytes_src`，据原 meta 选项重写 committed meta（committed 保持 true，仅字节数/applied_at 更新）。
fn finalize_meta_bytes(paths: &Paths, name: &str, meta: &discovery::Meta) -> io::Result<()> {
    let bytes_src = dir_file_bytes(&paths.orig(name))?;
    let bytes_archive = dir_file_bytes(&paths.backing(name, Backend::Shadow))?;
    let new_meta = discovery::Meta::from_apply(
        &meta.options(),
        bytes_src,
        bytes_archive,
        discovery::now_unix(),
    );
    discovery::write_meta(&paths.meta_path(name), &new_meta)
}

/// 递归求目录下所有常规文件字节数之和（meta 字节收尾用）。目录不存在 → 0；symlink/特殊文件不计。
fn dir_file_bytes(dir: &Path) -> io::Result<u64> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut total = 0u64;
    for dent in rd {
        let dent = dent?;
        let ft = dent.file_type()?;
        if ft.is_dir() {
            total = total.saturating_add(dir_file_bytes(&dent.path())?);
        } else if ft.is_file() {
            total = total.saturating_add(dent.metadata()?.len());
        }
    }
    Ok(total)
}

// ── reconcile-undo（回退最近一次重合并，供重选，§10） ─────────────────────────

/// 一次 `reconcile_undo` 的报告（§10.5 CLI 打印用）。
///
/// - `ts`：实际选中并回退的代次时间戳（`.undone` 二次 undo 的 no-op 也回填，`reversed` 为空）。
/// - `reversed`：逐条目实际反做记录 `(rel, 逆转类标签)`（RestoreOrig / RemoveOrig / RemoveQuarantine）。
/// - `skipped_live_changed`：还原 underlay 时因 live 已与快照不同而**保留 live、未覆盖**的条目
///   （reconcile 后又有新写；陈旧门与此逐条守卫双保险，绝不用旧快照盖新数据）。
/// - `memory_manual`：ReportMemory 条目（本代次往外部 memory 目标写过的文件），仅报告待用户 git 回退——
///   undo **绝不触碰外部 memory 目标**（§10.4）。
#[derive(Debug, Clone, Default)]
pub struct UndoReport {
    pub ts: String,
    pub reversed: Vec<(String, String)>,
    pub skipped_live_changed: Vec<String>,
    pub memory_manual: Vec<String>,
}

/// 回退**最近一代** reconcile（§10）：把项目还原到该 run **之前**的状态（underlay + orig + backing），
/// 随后可换选项重跑 `reconcile`。**无 mounter 参数**（评审 M4：undo 不重挂，未挂载判定走
/// `discovery::is_mounted`）。全程与 reconcile 对称：reconcile 锁 + `reconciling` marker + 陈旧门 +
/// 逐条守卫，满足零丢失铁律。
///
/// **前置门禁（缺一即拒，§10.2）：**
/// 1. `validate_name`；项目**未挂载**（`is_mounted` 为真即拒）；**shadow** 后端（读 meta 判，非 shadow 拒）。
/// 2. 取 **reconcile 锁**（`reconcile_lock`，持锁到结束，与 reconcile / 其他 undo 互斥）。
/// 3. 选**目标代次 = 全体 ts 最大**的一代：无任何代次→Err；**最新代次无 manifest**（崩溃未完成的 run）
///    →Err 且**绝不清 marker**（marker 归崩溃 run）。该代次已 `.undone`→no-op（幂等，返回空 reversed）。
/// 4. **陈旧门（§10.2 C1）**：`detect_activity` 空闲否则拒；且对 `stash/<ts>/underlay/**` 每个快照文件，
///    比对 live `mp/<rel>`：**live 缺失或与快照逐字节相等**才算未变（mtime/size/ino 未随进程存活，用
///    byte-equal，比 mtime 更强）。任一 live 已变（存在且字节不同）→ **拒绝整个 undo**、报告该 rel、零改动。
///
/// **逆转（§10.3）：** `set_reconciling(true)`（半改写窗口保护）→ 逐条目按 manifest `ReversalClass` 反做
/// → 统一还原 underlay（逐条守卫）→ `set_reconciling(false)` **先于** 落 `.undone`（闭合崩溃 wedge 窗口，
/// Task4 Important）→ 剪空目录。逆转/还原任一步
/// 出错即传播 `Err` 而**不清 marker**（marker 留存 → 生命周期维护让路、可修复后重跑，重跑幂等）。
/// 置 marker 后复检挂载态、命中即清 marker 并中止的可测小函数（Task1 Important）。抽出以便单测——真实
/// 挂载态复检（`discovery::is_mounted` 读 `/proc/self/mountinfo`）在集成环境验证。`mounted` 为真表示复检
/// 发现项目已在 undo 准备期间被挂载：先 `set_reconciling(false)` 清 marker（此刻尚未任何改写、清 marker
/// 安全），再返回 `Err` 中止（绝不留滞留 marker）；为假则放行（`Ok`）。
fn abort_if_mounted_clearing_marker(paths: &Paths, name: &str, mounted: bool) -> io::Result<()> {
    if mounted {
        set_reconciling(paths, name, false)?;
        return Err(io::Error::other(format!(
            "{name} 在 undo 准备期间被挂载，已中止；请卸载后重试"
        )));
    }
    Ok(())
}

pub fn reconcile_undo(paths: &Paths, name: &str) -> io::Result<UndoReport> {
    validate_name(name)?;

    // 前置门禁 1a：项目必须未挂载（undo 半改写 orig/backing，不能作用在挂载态视图上）。
    let mp = paths.mountpoint(name);
    if discovery::is_mounted(&mp) {
        return Err(io::Error::other(format!(
            "{name} 已挂载，拒绝 reconcile-undo；请先卸载后重试"
        )));
    }

    // 前置门禁 1b：shadow 后端（container 无 fall-through / per-file 语义，undo 不适用）。无 meta = 未 apply。
    let meta = discovery::read_meta(&paths.meta_path(name))?.ok_or_else(|| {
        io::Error::other(format!(
            "{name} 无提交标记 meta，无法 reconcile-undo（未 apply？）"
        ))
    })?;
    if meta.backend != Backend::Shadow {
        return Err(io::Error::other(format!(
            "reconcile-undo 仅支持 shadow 后端；{name:?} 为 {}，不适用",
            meta.backend.flag()
        )));
    }

    // 前置门禁 2：取 reconcile 锁（与 reconcile / 其他 undo 互斥），持锁到函数末。
    let lock_path = paths.reconcile_lock(name);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = acquire_exclusive_retry(&lock_path)?;

    // 前置门禁 3：选目标代次 = 全体 ts 最大的一代（§10.2/评审 I2）。
    let Some(ts) = latest_generation(paths, name)? else {
        return Err(io::Error::other(format!(
            "{name} 无可回退的 reconcile 记录（无任何代次）"
        )));
    };
    let stash_root = paths.reconcile_stash(name, &ts);
    // 最新代次无 manifest = 崩溃未完成的 run → 拒绝，**绝不清属于崩溃 run 的 marker**（此处也从未动 marker）。
    let manifest = read_manifest(paths, name, &ts)?.ok_or_else(|| {
        io::Error::other(format!(
            "{name} 最新代次 {ts} 无 manifest（该 reconcile run 未完成、不可 undo）；请查 stash 手动恢复"
        ))
    })?;
    // 幂等：该代次已 `.undone` → no-op（返回回填 ts 的空报告，防二次误触）。
    let undone_marker = stash_root.join(".undone");
    if undone_marker.exists() {
        // 防御性清 marker（闭合崩溃 wedge 窗口，Task4）：若上一次 undo 在「.undone 已落、marker 未清」
        // 两次 fsync 之间崩溃，reconciling marker 会滞留、经 bail_if_reconciling 永久挡住 remount/维护
        // （数据已还原、无丢失，但项目卡死）。此处在短路 return 前顺手清——幂等：正常二次 undo 时 marker
        // 已不在、无副作用；若因旧崩溃滞留则顺手闭合窗口。no-op 报告不变。
        set_reconciling(paths, name, false)?;
        return Ok(UndoReport {
            ts,
            ..Default::default()
        });
    }

    // 前置门禁 4：陈旧门（§10.2 C1）——detect_activity 空闲 + 每条快照 underlay 文件 live 缺失或逐字节相等。
    if let Some(reason) = detect_activity(&mp).reason() {
        return Err(io::Error::other(format!(
            "{name} 挂载点疑似活跃（{reason}），拒绝 reconcile-undo；确认空闲后重试"
        )));
    }
    let stash_underlay = stash_root.join("underlay");
    let changed = live_underlay_changed_since_snapshot(&stash_underlay, &mp)?;
    if !changed.is_empty() {
        return Err(io::Error::other(format!(
            "{name} reconcile 后 live underlay 已有新写，拒绝整个 undo（零改动）：{changed:?}；\
             请先 `enable reconcile` 收编新写、或手动处理"
        )));
    }

    // ── 逆转（§10.3）：置 marker（半改写窗口）→ 逐条目反做 → 还原 underlay → .undone → 清 marker。
    set_reconciling(paths, name, true)?;

    // Task1 Important：置 marker **后、任何改写前**复检挂载态，闭合「未挂载判定（门禁 1a）→ 置 marker」
    // 间的自启挂载竞态窗口。此窗口内项目 = 未挂载 + underlay 已被上代 reconcile 抽干（空）+ marker 未置：
    // reconcile 靠「underlay 非空」挡自启，但 undo 的 underlay 是空的、该保护失效，仅 marker 能挡——而 marker
    // 到此刻才置。故此空档 systemd 自启（underlay 空 + 无 marker → 放行）可把项目挂上，undo 随后在活 FUSE
    // 挂载之上改写 backing/写回 mp → 不一致却「成功」返回。加此复检后：任何挂载要么早于置 marker（被本复检
    // 抓到）、要么晚于置 marker（被 marker 挡下自启入口）→ 窗口闭合。命中即先清 marker（此刻尚未改写、清
    // marker 安全）再返回 Err（与既有崩溃窗口修复同精神：中止路径绝不留滞留 marker）。
    abort_if_mounted_clearing_marker(paths, name, discovery::is_mounted(&mp))?;

    let mut report = UndoReport {
        ts: ts.clone(),
        ..Default::default()
    };
    for (rel, class) in &manifest {
        match class {
            ReversalClass::RestoreOrig => {
                undo_restore_orig(paths, name, &stash_root, rel)?;
                report.reversed.push((rel.clone(), "RestoreOrig".into()));
            }
            ReversalClass::RemoveOrig => {
                undo_remove_orig(paths, name, rel)?;
                report.reversed.push((rel.clone(), "RemoveOrig".into()));
            }
            ReversalClass::RemoveQuarantine => {
                undo_remove_quarantine(paths, name, &ts, &stash_root, rel)?;
                report
                    .reversed
                    .push((rel.clone(), "RemoveQuarantine".into()));
            }
            // ReportMemory：仅报告本代次往外部目标写过的文件，绝不触碰外部 memory 目标（§10.4）。
            ReversalClass::ReportMemory => report.memory_manual.push(rel.clone()),
            // manifest 已过滤 Noop（write_manifest），防御性忽略。
            ReversalClass::Noop => {}
        }
    }

    // 统一还原 underlay：stash/<ts>/underlay/** 逐文件拷回 mp/<rel>，逐条守卫记 skipped_live_changed。
    restore_underlay_from_snapshot(&stash_underlay, &mp, &mut report.skipped_live_changed)?;

    // 先清 marker 再落 .undone（Task4 Important：闭合崩溃 wedge 窗口）→ 剪除还原可能留的空目录。
    // 次序理由：先清 marker 后若两次 fsync 间崩溃，.undone 缺失 → 重跑重做幂等 undo 并再清 marker，
    // 收敛；且此空档 underlay 已还原为非空 → ensure_underlay_empty 仍挡自启挂载，不误挂。反之（旧序）
    // 先落 .undone 后崩溃 → .undone 在而 marker 滞留 → 短路直接 return 永不清 marker、永久 wedge
    //（上方 `.undone` 短路已补防御清 marker，双保险）。
    set_reconciling(paths, name, false)?;
    write_undone_marker(&undone_marker)?;
    prune_empty_underlay_dirs(&mp)?;

    Ok(report)
}

/// 枚举 `reconcile_stash(name)` 下所有 `<ts>` 代次目录，返回**数值 ts 最大**者。无代次/目录不存在 → `None`。
/// ts 是 unix 秒字符串，按 `u64` 解析比较（解析失败退化为字典序，容错）。
fn latest_generation(paths: &Paths, name: &str) -> io::Result<Option<String>> {
    // reconcile_stash(name, ts) 的父目录即 `<name>` 代次根，取一个占位 ts 后 parent。
    let probe = paths.reconcile_stash(name, "0");
    let Some(gen_root) = probe.parent() else {
        return Ok(None);
    };
    let rd = match std::fs::read_dir(gen_root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut best: Option<String> = None;
    for dent in rd {
        let dent = dent?;
        if !dent.file_type()?.is_dir() {
            continue;
        }
        let ts = dent.file_name().to_string_lossy().into_owned();
        best = Some(match best {
            Some(cur) if !ts_greater(&ts, &cur) => cur,
            _ => ts,
        });
    }
    Ok(best)
}

/// ts 数值比较（unix 秒），解析失败退化为字典序。返回 `a > b`。
fn ts_greater(a: &str, b: &str) -> bool {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x > y,
        _ => a > b,
    }
}

/// 陈旧门比对（§10.2 C1）：遍历 `stash_underlay`（快照）下每个文件，与 `mp/<rel>` 逐字节比对。
/// live **存在且内容不同** → 收进返回 Vec（reconcile 后有新写）；live 缺失或逐字节相等 → 视为未变。
///
/// 用 byte-equal（而非 mtime/size/ino）：快照落盘只存内容，身份三元组随进程退出已丢失，byte-equal
/// 是可从磁盘复算的、比 mtime 更强的「未变」判据。
fn live_underlay_changed_since_snapshot(
    stash_underlay: &Path,
    mp: &Path,
) -> io::Result<Vec<String>> {
    let mut changed = Vec::new();
    walk_compare_snapshot(stash_underlay, stash_underlay, mp, &mut changed)?;
    Ok(changed)
}

/// `live_underlay_changed_since_snapshot` 的递归实现。`root` 是快照 underlay 根（算 rel 用）。
fn walk_compare_snapshot(
    root: &Path,
    dir: &Path,
    mp: &Path,
    changed: &mut Vec<String>,
) -> io::Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for dent in rd {
        let dent = dent?;
        let path = dent.path();
        let ft = dent.file_type()?;
        if ft.is_dir() {
            walk_compare_snapshot(root, &path, mp, changed)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("stash underlay 条目逃出根"))?;
        let live = mp.join(rel);
        match std::fs::read(&live) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {} // live 缺失 → 未变
            Err(e) => return Err(e),
            Ok(live_bytes) => {
                if live_bytes != std::fs::read(&path)? {
                    changed.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    Ok(())
}

/// RestoreOrig 反做：**先 fail-closed 校验** `stash/<ts>/orig/<rel>` 前镜像存在（缺→Err 中止，绝不
/// 静默半还原，评审 I-plan2）→ 读前镜像 → `atomic_write(orig/<rel>)` 原子还原 → `reingest_one_file`
/// 原子重建 `backing/<rel>`（与 reconcile 同原语）。
fn undo_restore_orig(paths: &Paths, name: &str, stash_root: &Path, rel: &str) -> io::Result<()> {
    let preimage = stash_root.join("orig").join(rel);
    let bytes = match std::fs::read(&preimage) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::other(format!(
                "RestoreOrig 反做中止：前镜像 {} 缺失（绝不静默半还原）；reconciling 标记保留，修复后可重跑",
                preimage.display()
            )));
        }
        Err(e) => return Err(e),
    };
    let orig_file = paths.orig(name).join(rel);
    if let Some(parent) = orig_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&orig_file, &bytes)?;
    reingest_one_file(paths, name, rel)
}

/// RemoveOrig 反做：删 `orig/<rel>` + `backing/<rel>`（NotFound 容忍，幂等重跑安全）。防 undo 后残留
/// new 增出的孤儿。
fn undo_remove_orig(paths: &Paths, name: &str, rel: &str) -> io::Result<()> {
    remove_file_if_exists(&paths.orig(name).join(rel))?;
    // 评审 R-lock：删 backing/<rel> 也取 backing 锁，与 compact/seal/守护互斥（同 reingest_one_file）。
    let backing = paths.backing(name, Backend::Shadow);
    let _backing_lock = crate::store::lock::acquire_backing_retry(&backing)?;
    remove_file_if_exists(&backing.join(rel))
}

/// RemoveQuarantine 反做：`quarantine(name,ts)/<rel>` 副本先逐字节校验 == `stash/<ts>/underlay/<rel>`
/// **快照**（校验基准是快照、非 live，评审 I1）后删除。quarantine 副本缺失 → 幂等跳过（已删）；校验
/// 不符 → Err（绝不误删与快照不符的隔离副本，可能被人工改过）。orig/backing **绝不触碰**（keep-separate
/// 当初就没改 base）。
fn undo_remove_quarantine(
    paths: &Paths,
    name: &str,
    ts: &str,
    stash_root: &Path,
    rel: &str,
) -> io::Result<()> {
    let quarantine_file = paths.quarantine(name, ts).join(rel);
    let q_bytes = match std::fs::read(&quarantine_file) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()), // 已删，幂等
        Err(e) => return Err(e),
        Ok(b) => b,
    };
    let snapshot_file = stash_root.join("underlay").join(rel);
    if q_bytes != std::fs::read(&snapshot_file)? {
        return Err(io::Error::other(format!(
            "RemoveQuarantine 反做中止：隔离副本 {} 与快照 {} 不符（绝不误删）",
            quarantine_file.display(),
            snapshot_file.display()
        )));
    }
    remove_file_if_exists(&quarantine_file)
}

/// 删单文件（NotFound 容忍，幂等）并 best-effort fsync 父目录持久化 dirent。
fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        let _ = fsync_dir(parent);
    }
    Ok(())
}

/// 统一还原 underlay（§10.3 步3）：把 `stash_underlay`（快照）下每个文件拷回 `mp/<rel>`（重建目录结构）。
///
/// **逐条守卫（承 §10.2 C1）**：仅当 live **缺失**或与快照**逐字节一致**才覆盖还原；live 已存在且不同
/// （reconcile 后新写）→ **保留 live、记入 `skipped`、绝不覆盖**（陈旧门 + 此守卫双保险，绝不用旧快照
/// 盖新数据）。原子写还原（`atomic_write`）。
fn restore_underlay_from_snapshot(
    stash_underlay: &Path,
    mp: &Path,
    skipped: &mut Vec<String>,
) -> io::Result<()> {
    walk_restore_snapshot(stash_underlay, stash_underlay, mp, skipped)
}

/// `restore_underlay_from_snapshot` 的递归实现。`root` 是快照 underlay 根（算 rel 用）。
fn walk_restore_snapshot(
    root: &Path,
    dir: &Path,
    mp: &Path,
    skipped: &mut Vec<String>,
) -> io::Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for dent in rd {
        let dent = dent?;
        let path = dent.path();
        let ft = dent.file_type()?;
        if ft.is_dir() {
            walk_restore_snapshot(root, &path, mp, skipped)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io::Error::other("stash underlay 条目逃出根"))?;
        let live = mp.join(rel);
        let snap_bytes = std::fs::read(&path)?;
        match std::fs::read(&live) {
            // live 缺失 → 还原快照。
            Err(e) if e.kind() == io::ErrorKind::NotFound => restore_one(&live, &snap_bytes)?,
            Err(e) => return Err(e),
            // 逐字节一致 → 已是快照内容，幂等 no-op（不重写）。
            Ok(live_bytes) if live_bytes == snap_bytes => {}
            // live 已存在且不同（reconcile 后新写）→ 保留 live、记 skipped、绝不覆盖。
            Ok(_) => skipped.push(rel.to_string_lossy().into_owned()),
        }
    }
    Ok(())
}

/// 原子还原单个 underlay 文件（重建父目录链）。
fn restore_one(live: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = live.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(live, bytes)
}

/// 落 `.undone` 标记（空文件 + fsync 文件与父目录）到目标代次 stash：防二次误触（再敲 undo 认出已消费）。
fn write_undone_marker(marker: &Path) -> io::Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    File::create(marker)?.sync_all()?;
    if let Some(parent) = marker.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enable::daemon::fake::FakeMounter;
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
    fn check_preconditions_clears_stale_marker_when_underlay_empty() {
        // 评审 W1：underlay 空 + 残留 reconciling marker（上次收尾被崩溃打断）→ check_preconditions
        // 必须清陈旧 marker 收敛（解 wedge），而非拒绝后留 marker 卡死。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap(); // 空挂载点（underlay 无 fall-through）
        set_reconciling(&paths, "demo", true).unwrap();
        assert!(paths.reconciling_marker("demo").exists());

        let res = check_preconditions(&paths, "demo", Backend::Shadow, false);
        assert!(res.is_err(), "underlay 空 → 无需 reconcile（返回 Err）");
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "陈旧 marker 必须被清（解 wedge），否则 bail_if_reconciling 永久拦死维护"
        );
    }

    #[test]
    fn set_reconciling_rejects_traversal_name() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let e = set_reconciling(&paths, "../escape", true).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn abort_if_mounted_clears_marker_and_errs_when_mounted() {
        // Task1 Important 复检路径：置 marker 后若复检发现已挂载 → 中止 + marker 已清（此刻尚未改写，
        // 清 marker 安全）。真实挂载态复检（is_mounted 读 /proc/self/mountinfo）靠集成环境；此处直接
        // 以 mounted=true 驱动抽出的可测函数。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        // 模拟逆转前刚置 marker 的状态。
        set_reconciling(&paths, "demo", true).unwrap();
        assert!(
            paths.reconciling_marker("demo").exists(),
            "前置：marker 已置"
        );

        let e = abort_if_mounted_clearing_marker(&paths, "demo", true).unwrap_err();
        assert!(e.to_string().contains("被挂载"), "应报被挂载中止");
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "中止路径必须已清 marker，绝不留滞留 marker"
        );
    }

    #[test]
    fn abort_if_mounted_is_noop_when_not_mounted() {
        // mounted=false → 放行（Ok），marker 原样保留供后续逆转使用。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        set_reconciling(&paths, "demo", true).unwrap();

        abort_if_mounted_clearing_marker(&paths, "demo", false).unwrap();
        assert!(
            paths.reconciling_marker("demo").exists(),
            "未挂载：marker 应原样保留"
        );
    }

    #[test]
    fn read_manifest_skips_traversal_and_absolute_rel() {
        // Task2 Minor：manifest 含 `../evil` / 绝对 / 空 rel → 跳过该条（不 join 到树外），合法条保留。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let ts = "7";
        let manifest = paths.reconcile_manifest("demo", ts);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        // 首行 ts 头，随后混入穿越/绝对/空 rel 与一条合法 rel。
        std::fs::write(
            &manifest,
            "7\n\
             ../evil.jsonl\tRestoreOrig\n\
             /etc/passwd\tRemoveOrig\n\
             a/../../b.jsonl\tRestoreOrig\n\
             good/s.jsonl\tRemoveOrig\n",
        )
        .unwrap();

        let out = read_manifest(&paths, "demo", ts).unwrap().unwrap();
        // 仅合法条保留；三条非法 rel 全被跳过。
        assert_eq!(
            out,
            vec![("good/s.jsonl".to_string(), ReversalClass::RemoveOrig)],
            "穿越/绝对/含 .. 的 rel 必须被跳过，仅保留合法条"
        );
    }

    #[test]
    fn is_safe_rel_accepts_multi_segment_rejects_traversal() {
        // 多段相对路径合法；`..`/绝对/`.`/空 均拒。
        assert!(is_safe_rel("uuid/subagents/x.jsonl"));
        assert!(is_safe_rel("s.jsonl"));
        assert!(!is_safe_rel(""));
        assert!(!is_safe_rel("../evil"));
        assert!(!is_safe_rel("a/../b"));
        assert!(!is_safe_rel("/etc/passwd"));
        assert!(!is_safe_rel("."));
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

        let report = apply_entry(
            &paths,
            "demo",
            &snap.entries[0],
            &EntryPlan::Union,
            &mp,
            "0",
        )
        .unwrap();

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

        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::Union, &mp, "0").unwrap();

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

        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::Union, &mp, "0").unwrap();
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

        let report =
            apply_entry(&paths, "demo", &snap.entries[0], &EntryPlan::New, &mp, "0").unwrap();

        let orig_file = paths.orig("demo").join(rel);
        assert_eq!(std::fs::read(&orig_file).unwrap(), incoming);
        assert!(!mp.join(rel).exists(), "New 落盘后 underlay 应删");
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), incoming);
        assert!(report.action.contains("underlay-removed"));
    }

    #[test]
    fn reingest_one_file_blocked_while_backing_locked() {
        // 评审 R-lock：reconcile 改写 backing 须与活守护/compact/seal 互斥（同一把 .zipfs.lock）。
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

        let report = apply_entry(
            &paths,
            "demo",
            &snap.entries[0],
            &EntryPlan::Identical,
            &mp,
            "0",
        )
        .unwrap();
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
        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::Identical, &mp, "0").unwrap();

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
    fn subagents_dir_unions_disjoint_uuids_without_mtime_delete() {
        // subagents 同名两侧 disjoint uuid（主 jsonl 规则会判 SuspectReuse→隔离），但 subagents
        // 强制无损并集：两侧 uuid 都保留、无一方被删。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "sess-uuid/subagents/agent-1.jsonl";
        let base = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"sa1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
        );
        let incoming = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"sb1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
        );
        let orig_file = write_orig(&paths, "demo", rel, base.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, incoming.as_bytes());

        let report = reconcile_subagents_dir(&paths, "demo", &snap_e, &mp, "0").unwrap();

        // orig 现含两侧 uuid（并集，无一方被丢）。
        let merged = std::fs::read_to_string(&orig_file).unwrap();
        assert!(merged.contains("sa1"), "base uuid 保留：{merged}");
        assert!(merged.contains("sb1"), "incoming uuid 并入：{merged}");
        // backing 重灌为并集。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), merged.as_bytes());
        // underlay 经 LinesSuperset 校验后删。
        assert!(!mp.join(rel).exists(), "并集且校验后应删 underlay");
        assert!(report.action.contains("underlay-removed"));
        assert!(report.decision.contains("subagents"));
    }

    #[test]
    fn apply_entry_routes_subagents_to_union_not_quarantine() {
        // 即便 plan 判 KeepSeparate（disjoint uuid），subagents 路径必须优先路由到并集而非隔离。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let rel = "s/subagents/a.jsonl";
        let base = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"x1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
        );
        let incoming = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"y1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
        );
        write_orig(&paths, "demo", rel, base.as_bytes());
        let snap_e = snap_entry_of(&mp, rel, incoming.as_bytes());

        // 传 KeepSeparate plan，但路由据 subagents 路径改走并集。
        let report =
            apply_entry(&paths, "demo", &snap_e, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert!(
            report.decision.contains("subagents"),
            "应优先走 subagents 并集：{}",
            report.decision
        );
        let orig_file = paths.orig("demo").join(rel);
        let merged = std::fs::read_to_string(&orig_file).unwrap();
        assert!(
            merged.contains("x1") && merged.contains("y1"),
            "两侧 uuid 并集：{merged}"
        );
        // 未落隔离区（quarantine 未记录）。
        assert!(
            !report.notes.iter().any(|n| n.starts_with("quarantine=")),
            "subagents 不应走隔离"
        );
    }

    #[test]
    fn subagents_new_entry_falls_to_new_when_orig_missing() {
        // orig 无对应 subagents 文件 → New 落盘（不崩），reingest + 删 underlay。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let rel = "u/subagents/fresh.jsonl";
        let incoming = b"{\"type\":\"summary\",\"summary\":\"s\"}\n";
        let snap_e = snap_entry_of(&mp, rel, incoming);
        let report = reconcile_subagents_dir(&paths, "demo", &snap_e, &mp, "0").unwrap();
        let orig_file = paths.orig("demo").join(rel);
        assert_eq!(std::fs::read(&orig_file).unwrap(), incoming);
        assert!(!mp.join(rel).exists());
        assert!(report.action.contains("underlay-removed"));
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
            "0",
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
    fn plan_entries_routes_subagents_to_union_matching_apply() {
        // 报告准确性：subagents 条目即便 orig 无对应文件（朴素分类会判 New），plan_entries 必须
        // 与 apply_entry 同序优先路由到 Union（无损并集），否则 dry-run 报告与实际 apply 不符。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let rel = "sess-uuid/subagents/agent.jsonl";
        let body = b"{\"type\":\"assistant\",\"uuid\":\"sa1\",\"parentUuid\":null}\n";
        let snap_e = snap_entry_of(&mp, rel, body);
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };

        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(
            plans[0].1,
            EntryPlan::Union,
            "subagents 应判 Union 而非 New"
        );
        assert!(
            plans[0].2.rationale.contains("subagents"),
            "rationale 应说明 subagents 并集：{}",
            plans[0].2.rationale
        );

        // plan↔apply 一致性：apply_entry 对同条目实际路由到 subagents 并集。
        let report = apply_entry(&paths, "demo", &snap.entries[0], &plans[0].1, &mp, "0").unwrap();
        assert!(
            report.decision.contains("subagents"),
            "apply 实际应走 subagents，plan 须匹配：{}",
            report.decision
        );
    }

    #[test]
    fn plan_entries_routes_memory_passthrough_matching_apply() {
        // 报告准确性：backing/memory 是外链 symlink 时，memory/* 条目必须判 Passthrough 而非
        // New/KeepSeparate——apply_entry 会走透传恢复（写 canonical target，绝不落 orig）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // backing/memory = 指向树外 target 的 symlink（apply 期照 Claude 外链重建）。
        let target = tmp.path().join("external-memory");
        std::fs::create_dir_all(&target).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();

        // 非 jsonl memory 文件：朴素分类（orig 无）会判 New。
        let rel = "memory/foo.md";
        let snap_e = snap_entry_of(&mp, rel, b"body\n");
        let snap = UnderlaySnapshot {
            ts: "0".into(),
            entries: vec![snap_e],
        };

        let plans = plan_entries(&paths, "demo", &snap).unwrap();
        assert_eq!(
            plans[0].1,
            EntryPlan::Passthrough,
            "memory 外链条目应判 Passthrough 而非 New/KeepSeparate"
        );
        assert!(
            matches!(plans[0].2.action, Action::PassthroughRestore),
            "透传建议 action 应为 PassthroughRestore：{:?}",
            plans[0].2.action
        );
        assert!(
            plans[0].2.rationale.contains("透传") || plans[0].2.rationale.contains("memory"),
            "rationale 应说明 memory 透传：{}",
            plans[0].2.rationale
        );

        // plan↔apply 一致性：apply_entry 对同条目实际路由到透传恢复。
        let report = apply_entry(&paths, "demo", &snap.entries[0], &plans[0].1, &mp, "0").unwrap();
        assert_eq!(
            report.decision, "passthrough",
            "apply 实际应走透传，plan 须匹配"
        );
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
        let report = apply_entry(&paths, "demo", &snap_e, &EntryPlan::KeepBoth, &mp, "0").unwrap();
        assert_eq!(report.action, "deferred");
        assert!(mp.join(rel).exists(), "deferred 计划不得删 underlay");
    }

    // ── memory 透传恢复（例外规则） ─────────────────────────────────────────

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
    fn apply_entry_routes_memory_passthrough_via_backing_symlink() {
        // backing/memory 是 symlink → apply_entry 应据此路由到透传恢复。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // 外部 memory 目标（无 `..`，存在可写）。
        let target = tmp.path().join("external-memory");
        std::fs::create_dir_all(&target).unwrap();
        // backing/memory = 指向 target 的 symlink（apply 期照 Claude 外链重建）。
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();

        // underlay：memory 被物化，Claude 写了新文件。
        let rel = "memory/NOTES.md";
        let snap_e = snap_entry_of(&mp, rel, b"note-body\n");

        // 传 KeepSeparate（模拟 plan_entries 对非 jsonl 的保守判定），路由应改走透传。
        let report =
            apply_entry(&paths, "demo", &snap_e, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert_eq!(report.decision, "passthrough", "应路由到透传");
        // 文件送进 target。
        assert_eq!(
            std::fs::read(target.join("NOTES.md")).unwrap(),
            b"note-body\n"
        );
        // underlay memory 侧无残留（**不复原 symlink**；挂载由 backing/memory 服务）。
        assert!(
            mp.join("memory").symlink_metadata().is_err(),
            "underlay memory 应无残留（无目录、无复原 symlink）"
        );
    }

    #[test]
    fn apply_entry_second_memory_entry_is_idempotent_noop() {
        // 同一 reconcile 内多条 memory/* 条目：首条复原 symlink 后，次条应幂等跳过（不再 relocate）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let target = tmp.path().join("external-memory");
        std::fs::create_dir_all(&target).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();

        // underlay 物化两个文件。
        let e1 = snap_entry_of(&mp, "memory/A.md", b"aaa\n");
        let e2 = snap_entry_of(&mp, "memory/B.md", b"bbb\n");

        let r1 = apply_entry(&paths, "demo", &e1, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert_eq!(r1.action, "memory-restored");
        // 首条已把整目录 relocate 并复原 symlink；A、B 都进了 target。
        assert_eq!(std::fs::read(target.join("A.md")).unwrap(), b"aaa\n");
        assert_eq!(std::fs::read(target.join("B.md")).unwrap(), b"bbb\n");

        let r2 = apply_entry(&paths, "demo", &e2, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert_eq!(r2.action, "memory-noop", "次条应如实报 noop");
        assert!(
            r2.notes.iter().any(|n| n.contains("不存在")),
            "次条应因 underlay memory 已整目录 relocate 移除而 noop：{:?}",
            r2.notes
        );
        // memory 侧无残留（首条已 relocate 整目录、未复原 symlink，次条不再触碰）。
        assert!(
            mp.join("memory").symlink_metadata().is_err(),
            "underlay memory 应无残留"
        );
    }

    #[test]
    fn apply_entry_memory_deferred_action_when_target_dangling() {
        // 评审 M4：路径安全闸拦截（悬空目标）时 underlay 未动，action 必须如实为 memory-deferred。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        // backing/memory 指向不存在的目标（悬空）。
        std::os::unix::fs::symlink(tmp.path().join("gone-mem"), backing.join("memory")).unwrap();
        let e = snap_entry_of(&mp, "memory/N.md", b"n\n");

        let report = apply_entry(&paths, "demo", &e, &EntryPlan::KeepSeparate, &mp, "0").unwrap();
        assert_eq!(report.decision, "passthrough");
        assert_eq!(report.action, "memory-deferred", "悬空 → 不能谎报 restored");
        // underlay 文件保留（未动）。
        assert!(mp.join("memory/N.md").exists(), "悬空目标 → underlay 保留");
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

    // ── 顶层 reconcile 编排（Task 9） ─────────────────────────────────────────

    /// 把 `bytes` 写成 `mp/rel` 的 live underlay 回落写文件（含子目录建链）。
    fn write_underlay(mp: &Path, rel: &str, bytes: &[u8]) {
        let p = mp.join(rel);
        if let Some(par) = p.parent() {
            std::fs::create_dir_all(par).unwrap();
        }
        std::fs::write(&p, bytes).unwrap();
    }

    /// 构造一个「已 apply」态可 reconcile 项目：committed meta + orig/<rel>=base + backing 灌好。
    fn setup_committed(paths: &Paths, name: &str, rel: &str, base: &[u8]) -> PathBuf {
        write_committed_meta(paths, name);
        std::fs::create_dir_all(paths.mountpoint(name)).unwrap();
        let orig_file = write_orig(paths, name, rel, base);
        reingest_one_file(paths, name, rel).unwrap();
        orig_file
    }

    fn accept_all() -> Box<ConfirmFn> {
        Box::new(|_, _| Confirm::Accept)
    }

    #[test]
    fn reconcile_dry_run_reports_without_mutating() {
        // dry_run：只出建议单，orig/underlay/backing/marker 全不变。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        let backing_before = read_archive(&backing_file);
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let underlay_before = std::fs::read(mp.join(rel)).unwrap();
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: true,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        assert!(!report.entries.is_empty(), "dry_run 应出建议单");
        assert!(
            report.entries.iter().all(|e| e.action == "dry-run"),
            "dry_run 条目动作应标 dry-run：{:?}",
            report.entries
        );
        // 零改动。
        assert_eq!(
            std::fs::read(&orig_file).unwrap(),
            BASE_LOG.as_bytes(),
            "orig 不变"
        );
        assert_eq!(
            std::fs::read(mp.join(rel)).unwrap(),
            underlay_before,
            "underlay 不变"
        );
        assert_eq!(read_archive(&backing_file), backing_before, "backing 不变");
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "dry_run 不落 reconciling 标记"
        );
    }

    #[test]
    fn reconcile_full_flow_accept_drains_underlay_and_updates_meta() {
        // 全流程 Accept：门禁→set_reconciling(true)→逐条 apply→underlay 清空→meta 更新→清标记。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // underlay 清空（结束态可挂：ensure_underlay_empty 放行）。
        assert!(
            !underlay_has_fallthrough(&mp).unwrap(),
            "Accept 全流程后 underlay 应清空"
        );
        assert!(!mp.join(rel).exists());
        crate::reconcile::guard::ensure_underlay_empty(&mp).unwrap();
        // orig 合并 incoming。
        let merged = std::fs::read_to_string(&orig_file).unwrap();
        for needle in ["u1", "old", "new", "\"mode\""] {
            assert!(
                merged.contains(needle),
                "orig 应含合并结果 {needle}：{merged}"
            );
        }
        // backing 重灌为合并结果。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(read_archive(&backing_file), merged.as_bytes());
        // reconciling 标记已清。
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾应清 reconciling 标记"
        );
        // meta 字节数收尾，committed 全程不变。
        let meta = discovery::read_meta(&paths.meta_path("demo"))
            .unwrap()
            .unwrap();
        assert!(meta.committed, "committed 全程不变");
        assert_eq!(
            meta.bytes_src,
            dir_file_bytes(&paths.orig("demo")).unwrap(),
            "bytes_src 应重扫 orig"
        );
        assert_eq!(
            meta.bytes_archive,
            dir_file_bytes(&paths.backing("demo", Backend::Shadow)).unwrap(),
            "bytes_archive 应重扫 backing"
        );
        assert!(meta.bytes_src > 0 && meta.bytes_archive > 0);
        assert!(
            report
                .entries
                .iter()
                .any(|e| e.action.contains("underlay-removed")),
            "应有条目报 underlay-removed：{:?}",
            report.entries
        );
    }

    #[test]
    fn reconcile_skip_keeps_that_underlay_entry_and_clears_marker() {
        // 中途 Skip 某条：该 underlay 保留、orig 不落该条；其余 Accept 落盘。收尾清 reconciling 标记。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        // 两条全新 fall-through（orig 无 → New）。
        write_underlay(
            &mp,
            "a.jsonl",
            b"{\"type\":\"summary\",\"summary\":\"a\"}\n",
        );
        write_underlay(
            &mp,
            "b.jsonl",
            b"{\"type\":\"summary\",\"summary\":\"b\"}\n",
        );
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: Box::new(|rel, _| {
                if rel == "a.jsonl" {
                    Confirm::Skip
                } else {
                    Confirm::Accept
                }
            }),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // Skip 的 a：underlay 保留、orig 未落。
        assert!(mp.join("a.jsonl").exists(), "Skip 的条目 underlay 应保留");
        assert!(
            !paths.orig("demo").join("a.jsonl").exists(),
            "Skip 的条目不应落 orig"
        );
        // Accept 的 b：underlay 删除、orig 落盘。
        assert!(
            !mp.join("b.jsonl").exists(),
            "Accept 的条目 underlay 应删除"
        );
        assert!(
            paths.orig("demo").join("b.jsonl").exists(),
            "Accept 的条目应落 orig"
        );
        // reconciling 标记已清（半改写窗口正常关闭）。
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾应清 reconciling 标记"
        );
        assert!(
            report.entries.iter().any(|e| e.decision == "skip"),
            "报告应含 skip 条目：{:?}",
            report.entries
        );
    }

    #[test]
    fn reconcile_crash_resume_is_idempotent() {
        // 崩溃续跑幂等：同一 incoming 重现在 underlay（上次崩溃在删 underlay 前）→ 重跑收敛，
        // orig 不放大（并集不动点）、underlay 再次清空、不重复删。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();

        let opts1 = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        reconcile(&paths, "demo", opts1, &m).unwrap();
        let merged1 = std::fs::read(&orig_file).unwrap();
        assert!(!mp.join(rel).exists(), "首轮后 underlay 清空");

        // 模拟崩溃续跑：同一 incoming 再次出现。
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let opts2 = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        reconcile(&paths, "demo", opts2, &m).unwrap();
        let merged2 = std::fs::read(&orig_file).unwrap();

        assert_eq!(merged1, merged2, "重跑不放大 orig（并集不动点）");
        assert!(!mp.join(rel).exists(), "重跑收敛：underlay 再次清空");
    }

    #[test]
    fn reconcile_rebuild_delegates_to_reingest_and_remounts() {
        // rebuild：逐条 apply 后清标记，委托 reingest 从 orig 全量重建 backing + 重挂；旧 backing 留底。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: true,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        assert!(
            !mp.join(rel).exists(),
            "rebuild 前逐条 apply 应先清 underlay"
        );
        // reingest 重挂 → FakeMounter 记挂载。
        assert!(m.is_mounted(&mp), "rebuild 委托 reingest 后应重挂");
        // reingest 特征：旧 backing 留底 .reingest-bak。
        let bak = {
            let mut s = paths.backing("demo", Backend::Shadow).into_os_string();
            s.push(".reingest-bak");
            PathBuf::from(s)
        };
        assert!(bak.is_dir(), "reingest 应留旧 backing 底本");
        // 报告含 rebuild 委托项，标记已清。
        assert!(
            report.entries.iter().any(|e| e.decision == "rebuild"),
            "报告应含 rebuild 委托项：{:?}",
            report.entries
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "rebuild 前须清 reconciling 标记"
        );
        // backing 读回合并结果（reingest 从 orig 重建）。
        let backing_file = paths.backing("demo", Backend::Shadow).join(rel);
        assert_eq!(
            read_archive(&backing_file),
            std::fs::read(paths.orig("demo").join(rel)).unwrap()
        );
    }

    #[test]
    fn reconcile_single_run_ts_all_stash_in_one_generation() {
        // Task7 Minor2：一次 reconcile 所有 stash（快照 underlay + 各条目 orig 前镜像）落同一 ts 代次。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        // 两条 Union（orig 有 base、underlay 有 incoming）。
        for rel in ["a.jsonl", "b.jsonl"] {
            write_orig(&paths, "demo", rel, BASE_LOG.as_bytes());
            reingest_one_file(&paths, "demo", rel).unwrap();
            write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        }
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // 只有一个 ts 代次目录。
        let gen_root = paths.zipfs_home.join("reconcile-stash").join("demo");
        let ts_dirs: Vec<PathBuf> = std::fs::read_dir(&gen_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(
            ts_dirs.len(),
            1,
            "一次 reconcile 所有 stash 应落同一 ts 代次：{ts_dirs:?}"
        );
        // report.stash_dir 即该唯一代次，且快照 + 两条前镜像全在其下。
        assert_eq!(report.stash_dir, ts_dirs[0]);
        assert!(report.stash_dir.join("underlay/a.jsonl").exists(), "a 快照");
        assert!(report.stash_dir.join("underlay/b.jsonl").exists(), "b 快照");
        assert!(
            report.stash_dir.join("orig/a.jsonl").exists(),
            "a orig 前镜像"
        );
        assert!(
            report.stash_dir.join("orig/b.jsonl").exists(),
            "b orig 前镜像"
        );
    }

    #[test]
    fn reconcile_rejects_without_committed_meta() {
        // 无 meta（未 apply）→ 拒绝。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        write_underlay(&mp, "s.jsonl", b"{}\n");
        let m = FakeMounter::default();
        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let e = reconcile(&paths, "demo", opts, &m).unwrap_err();
        assert!(e.to_string().contains("meta"), "无 meta 应拒绝：{e}");
    }

    #[test]
    fn reconcile_rejects_container_backend() {
        // meta 记 container 后端 → check_preconditions 拒（无 fall-through 语义）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();
        write_underlay(&mp, "s.jsonl", b"{}\n");
        std::fs::create_dir_all(paths.back_root()).unwrap();
        let meta = discovery::Meta::from_apply(
            &ApplyOptions {
                backend: Backend::Container,
                ..ApplyOptions::default()
            },
            0,
            0,
            0,
        );
        discovery::write_meta(&paths.meta_path("demo"), &meta).unwrap();
        let m = FakeMounter::default();
        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let e = reconcile(&paths, "demo", opts, &m).unwrap_err();
        assert!(e.to_string().contains("shadow"), "container 应拒绝：{e}");
    }

    #[test]
    fn reconcile_meta_finalize_failure_still_clears_marker_no_wedge() {
        // 评审 HIGH：meta 字节收尾（纯 list 显示）失败绝不能阻断 set_reconciling(false)——否则
        // underlay 已清空、下轮 reconcile 被前置门禁拒、标记永久卡住把维护全拦死。best-effort 验证。
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root 无视权限位，注入不成立 → 跳过。
        }
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        // 注入 finalize 失败：orig 下放一个不可读子目录 → dir_file_bytes 递归 read_dir 失败。
        // back_root 保持可写（set_reconciling(false) 仍能删标记，隔离出「收尾失败 ≠ 清标记失败」）。
        let blocked = paths.orig("demo").join("blocked-sub");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(blocked.join("x"), b"y").unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();
        // 恢复权限便于 tempdir 清理。
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        // underlay 已清空，但标记必须已清（不 wedge）。
        assert!(!mp.join(rel).exists(), "underlay 应已清空");
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾 meta 失败也须清 reconciling 标记（不 wedge）"
        );
        // 报告如实记 meta-finalize warn。
        assert!(
            report.entries.iter().any(|e| e.action == "warn"),
            "应记 meta 收尾 warn 条目：{:?}",
            report.entries
        );
    }

    #[test]
    fn reconcile_prunes_drained_subdirs_and_removes_memory_symlink_so_remount_unblocked() {
        // 整分支收尾评审两处集成缝 bug（BREACH 1 + BREACH 2）：reconcile 抽干 underlay 后必须让
        // `underlay_has_fallthrough` 归假（= `ensure_underlay_empty` 放行 → 重挂解锁），且零丢失。
        //   (a) 嵌套 `<uuid>/subagents/x.jsonl` 抽干后空目录 `<uuid>/subagents/`、`<uuid>/` 若不剪除，
        //       顶层 `<uuid>/` 令 fall-through 永真（BREACH 1）。
        //   (b) memory 分裂脑：backing/memory 是指向树外 target 的 symlink，underlay/memory 是含真实
        //       文件的目录。透传若在 underlay 复原 memory symlink，顶层 memory 条目令 fall-through 永真
        //       （BREACH 2）。underlay 侧必须无任何 memory 残留（挂载由 backing/memory 服务）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // (a) 嵌套子代理 fall-through 文件（orig 无 → New；subagents 路由强制并集）。
        let sub_rel = "sess-uuid/subagents/agent.jsonl";
        let sub_body = b"{\"type\":\"assistant\",\"uuid\":\"sa1\",\"parentUuid\":null}\n";
        write_underlay(&mp, sub_rel, sub_body);

        // (b) memory 分裂脑：树外 target + backing/memory symlink → target + underlay/memory 真实目录含文件。
        let target = tmp.path().join("external-memory"); // 移出 projects 树
        std::fs::create_dir_all(&target).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();
        let mem_body = b"# NOTES\nrelocated-body\n";
        write_underlay(&mp, "memory/NOTE.md", mem_body);

        // 挂载前顶层确有 fall-through（<uuid>/ 与 memory/ 两个顶层条目）。
        assert!(
            underlay_has_fallthrough(&mp).unwrap(),
            "前提：reconcile 前 underlay 顶层含 fall-through"
        );

        let m = FakeMounter::default();
        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // ── 核心断言：underlay 顶层归空 → ensure_underlay_empty 放行 → 重挂解锁（两 breach 均已修）。
        assert!(
            !crate::reconcile::guard::underlay_has_fallthrough(&mp).unwrap(),
            "抽干后 underlay 顶层必须无 fall-through（否则重挂永久 wedge）：{:?}",
            report.entries
        );
        crate::reconcile::guard::ensure_underlay_empty(&mp).unwrap();

        // BREACH 1：抽干的空子目录被剪除（顶层 <uuid>/ 无残留）。
        assert!(
            mp.join("sess-uuid").symlink_metadata().is_err(),
            "抽干的空 <uuid>/ 目录应被剪除"
        );
        // BREACH 2：underlay 侧 memory 无任何残留（既非目录也非复原的 symlink）。
        assert!(
            mp.join("memory").symlink_metadata().is_err(),
            "underlay memory 必须无残留（不复原 symlink）"
        );

        // ── 零丢失（a）：子代理会话内容落 orig 且已重灌 backing。
        let orig_sub = paths.orig("demo").join(sub_rel);
        assert_eq!(
            std::fs::read(&orig_sub).unwrap(),
            sub_body,
            "子代理会话内容应无损落 orig"
        );
        let backing_sub = backing.join(sub_rel);
        assert_eq!(
            read_archive(&backing_sub),
            sub_body,
            "子代理会话内容应重灌进 backing"
        );

        // ── 零丢失（b）：memory 文件被安置到 canonical target（挂载时 backing/memory symlink 服务）。
        assert_eq!(
            std::fs::read(target.join("NOTE.md")).unwrap(),
            mem_body,
            "memory 文件应安置到 canonical target"
        );

        // 无静默丢弃：报告含子代理 underlay-removed 与 memory-restored。
        assert!(
            report
                .entries
                .iter()
                .any(|e| e.action.contains("underlay-removed") && e.decision.contains("subagents")),
            "应有 subagents underlay-removed 条目：{:?}",
            report.entries
        );
        assert!(
            report.entries.iter().any(|e| e.action == "memory-restored"),
            "应有 memory-restored 条目：{:?}",
            report.entries
        );
        // reconciling 标记正常清（不 wedge）。
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾应清 reconciling 标记"
        );
    }

    // ── memory-symlink 短路：清与 backing 同目标的冗余 underlay 软链（§6） ──────────

    /// underlay 顶层 `memory` 软链与 backing 同名软链同目标 → 删 underlay 那个、
    /// `underlay_has_fallthrough` 转假（不再 wedge 重挂），无异常报告。
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

    /// underlay 与 backing 同名软链**目标不一致**（异常）→ underlay 软链保留 + 报告非空。
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

    /// 真实目录 `memory`（split-brain，非 symlink）天然不命中此步 → 绝不被误删。
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

    /// per-generation manifest：一次含 union（orig 预存）+ new（orig 缺）+ keep-separate（疑 reuse）
    /// 的 reconcile 后，`read_manifest` 逐条逆转类正确、合成条目（`<prune>`/`<meta>` 等）不入 manifest。
    #[test]
    fn reconcile_writes_manifest_with_per_entry_reversal_class() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // union：orig 预存 .jsonl + LogOnly incoming → Union（有前镜像）→ RestoreOrig。
        let rel_union = "s.jsonl";
        write_orig(&paths, "demo", rel_union, BASE_LOG.as_bytes());
        reingest_one_file(&paths, "demo", rel_union).unwrap();
        write_underlay(&mp, rel_union, INCOMING_LOG.as_bytes());

        // new：orig 缺 → New（无前镜像）→ RemoveOrig。
        let rel_new = "new.jsonl";
        write_underlay(&mp, rel_new, INCOMING_LOG.as_bytes());

        // keep-separate：orig 预存、disjoint uuid + 时间窗不交 → SuspectReuse → KeepSeparate →
        // RemoveQuarantine。
        let rel_keep = "3f2a-b1c2-uuid.jsonl";
        let keep_base = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
        );
        let keep_incoming = concat!(
            "{\"type\":\"assistant\",\"uuid\":\"b1\",\"parentUuid\":null,",
            "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
        );
        write_orig(&paths, "demo", rel_keep, keep_base.as_bytes());
        reingest_one_file(&paths, "demo", rel_keep).unwrap();
        write_underlay(&mp, rel_keep, keep_incoming.as_bytes());

        let m = FakeMounter::default();
        let opts = ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();

        // run ts = stash_dir 末段。
        let ts = report
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let manifest = read_manifest(&paths, "demo", &ts)
            .unwrap()
            .expect("reconcile 后 manifest 应存在");
        let map: std::collections::HashMap<String, ReversalClass> = manifest.into_iter().collect();

        assert_eq!(
            map.get(rel_union),
            Some(&ReversalClass::RestoreOrig),
            "union（orig 预存）→ RestoreOrig：{map:?}"
        );
        assert_eq!(
            map.get(rel_new),
            Some(&ReversalClass::RemoveOrig),
            "new（orig 缺）→ RemoveOrig：{map:?}"
        );
        assert_eq!(
            map.get(rel_keep),
            Some(&ReversalClass::RemoveQuarantine),
            "keep-separate → RemoveQuarantine：{map:?}"
        );
        // 合成条目（`<prune>`/`<meta>`/`<rebuild>`/`<prune-symlinks>`）绝不入 manifest。
        assert!(
            map.keys().all(|k| !k.starts_with('<')),
            "合成条目不应出现在 manifest：{map:?}"
        );
    }

    /// dry_run 不写 manifest（零改动）。
    #[test]
    fn reconcile_dry_run_writes_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();

        let opts = ReconcileOptions {
            dry_run: true,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        };
        let report = reconcile(&paths, "demo", opts, &m).unwrap();
        let ts = report
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            read_manifest(&paths, "demo", &ts).unwrap().is_none(),
            "dry_run 不应写 manifest"
        );
    }

    // ── reconcile-undo（回退最近一次重合并，§10） ─────────────────────────────

    /// 把文件 mtime/atime 回拨 `secs_ago` 秒（测试用，绕过 5min 活跃门以单独验证陈旧 byte 门）。
    fn backdate_mtime(path: &Path, secs_ago: u64) {
        use std::os::unix::ffi::OsStrExt;
        let t = SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        let d = t.duration_since(UNIX_EPOCH).unwrap();
        let tv = libc::timeval {
            tv_sec: d.as_secs() as libc::time_t,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::utimes(cpath.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes 回拨 mtime 失败");
    }

    const KEEP_BASE: &str = concat!(
        "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":null,",
        "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
    );
    const KEEP_INCOMING: &str = concat!(
        "{\"type\":\"assistant\",\"uuid\":\"b1\",\"parentUuid\":null,",
        "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
    );

    fn accept_opts() -> ReconcileOptions {
        ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        }
    }

    /// 跑一次 union（orig 预存）+ new（orig 缺）+ keep-separate（疑 reuse）的 reconcile，返回 run ts。
    fn reconcile_three_kinds(paths: &Paths, mp: &Path) -> String {
        write_orig(paths, "demo", "s.jsonl", BASE_LOG.as_bytes());
        reingest_one_file(paths, "demo", "s.jsonl").unwrap();
        write_underlay(mp, "s.jsonl", INCOMING_LOG.as_bytes());

        write_underlay(mp, "new.jsonl", INCOMING_LOG.as_bytes());

        write_orig(paths, "demo", "3f2a-b1c2-uuid.jsonl", KEEP_BASE.as_bytes());
        reingest_one_file(paths, "demo", "3f2a-b1c2-uuid.jsonl").unwrap();
        write_underlay(mp, "3f2a-b1c2-uuid.jsonl", KEEP_INCOMING.as_bytes());

        let m = FakeMounter::default();
        let rec = reconcile(paths, "demo", accept_opts(), &m).unwrap();
        rec.stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn reconcile_undo_full_flow_restores_orig_removes_new_and_quarantine() {
        // union+new+keep-separate reconcile 后 undo：RestoreOrig 还原前镜像、RemoveOrig 删新增、
        // RemoveQuarantine 删隔离副本、underlay 从快照还原、结束态可再 reconcile。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        let ts = reconcile_three_kinds(&paths, &mp);
        let orig_union = paths.orig("demo").join("s.jsonl");
        let orig_new = paths.orig("demo").join("new.jsonl");
        let orig_keep = paths.orig("demo").join("3f2a-b1c2-uuid.jsonl");
        let quarantine_keep = paths.quarantine("demo", &ts).join("3f2a-b1c2-uuid.jsonl");

        // reconcile 后态：underlay 清空、union orig 已合并、new orig 已建、keep 已隔离。
        assert!(!mp.join("s.jsonl").exists());
        assert!(orig_new.exists(), "new 应落 orig");
        assert!(quarantine_keep.exists(), "keep 应隔离");
        assert_ne!(
            std::fs::read(&orig_union).unwrap(),
            BASE_LOG.as_bytes(),
            "union orig 已合并（≠ base）"
        );

        // ── undo ──
        let report = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(report.ts, ts, "选中最近一代");

        // RestoreOrig：union orig 还原前镜像（== base）；backing 重建为 base。
        assert_eq!(
            std::fs::read(&orig_union).unwrap(),
            BASE_LOG.as_bytes(),
            "union orig 还原前镜像"
        );
        assert_eq!(
            read_archive(&paths.backing("demo", Backend::Shadow).join("s.jsonl")),
            BASE_LOG.as_bytes(),
            "union backing 重建为 base"
        );
        // RemoveOrig：new orig + backing 删除。
        assert!(!orig_new.exists(), "new orig 应删");
        assert!(
            !paths
                .backing("demo", Backend::Shadow)
                .join("new.jsonl")
                .exists(),
            "new backing 应删"
        );
        // RemoveQuarantine：隔离副本删除；keep orig base 不动。
        assert!(!quarantine_keep.exists(), "keep 隔离副本应删");
        assert_eq!(
            std::fs::read(&orig_keep).unwrap(),
            KEEP_BASE.as_bytes(),
            "keep orig base 绝不触碰"
        );

        // underlay 从快照还原：三条都回 mp。
        assert_eq!(
            std::fs::read(mp.join("s.jsonl")).unwrap(),
            INCOMING_LOG.as_bytes()
        );
        assert_eq!(
            std::fs::read(mp.join("new.jsonl")).unwrap(),
            INCOMING_LOG.as_bytes()
        );
        assert_eq!(
            std::fs::read(mp.join("3f2a-b1c2-uuid.jsonl")).unwrap(),
            KEEP_INCOMING.as_bytes()
        );

        // reversed 记三条逆转类。
        let rev: std::collections::HashMap<String, String> = report.reversed.into_iter().collect();
        assert_eq!(rev.get("s.jsonl").map(String::as_str), Some("RestoreOrig"));
        assert_eq!(rev.get("new.jsonl").map(String::as_str), Some("RemoveOrig"));
        assert_eq!(
            rev.get("3f2a-b1c2-uuid.jsonl").map(String::as_str),
            Some("RemoveQuarantine")
        );

        // .undone 落 + reconciling 清 + 结束态可再 reconcile（underlay 又有 fall-through）。
        assert!(
            paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "落 .undone 标记"
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "收尾清 reconciling 标记"
        );
        assert!(
            underlay_has_fallthrough(&mp).unwrap(),
            "还原后 underlay 又有 fall-through → 可再 reconcile"
        );
    }

    #[test]
    fn reconcile_undo_stale_gate_rejects_and_zero_change() {
        // 陈旧门：undo 前对某快照条目在 mp 写不同内容 → 拒绝整个 undo、零改动、报告该 rel。
        // 回拨 mtime 绕过 5min 活跃门，单独验证 byte 门（活跃门另有覆盖）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let orig_merged = std::fs::read(&orig_file).unwrap();

        // reconcile 后 Claude 又写不同内容（回拨 mtime → 非活跃，但与快照字节不同）。
        write_underlay(&mp, rel, b"{\"type\":\"NEW-AFTER-RECONCILE\"}\n");
        backdate_mtime(&mp.join(rel), 600);

        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("已有新写") && e.to_string().contains(rel),
            "陈旧门应拒绝并报告 rel：{e}"
        );
        // 零改动：orig 未还原、live 未动、marker 未落、.undone 未落。
        assert_eq!(
            std::fs::read(&orig_file).unwrap(),
            orig_merged,
            "拒绝→orig 未动"
        );
        assert_eq!(
            std::fs::read(mp.join(rel)).unwrap(),
            b"{\"type\":\"NEW-AFTER-RECONCILE\"}\n",
            "拒绝→live 未动"
        );
        assert!(
            !paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "拒绝→不落 .undone"
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "拒绝前从未置 reconciling 标记"
        );
    }

    #[test]
    fn stale_gate_helper_reports_only_changed_rels() {
        // 陈旧门核心比对：live 与快照逐字节不同 → 报告该 rel；相等或 live 缺失 → 不报。
        let tmp = tempfile::tempdir().unwrap();
        let stash_underlay = tmp.path().join("stash").join("underlay");
        std::fs::create_dir_all(&stash_underlay).unwrap();
        std::fs::write(stash_underlay.join("same.jsonl"), b"SNAP\n").unwrap();
        std::fs::write(stash_underlay.join("diff.jsonl"), b"SNAP\n").unwrap();
        std::fs::write(stash_underlay.join("gone.jsonl"), b"SNAP\n").unwrap();
        let mp = tmp.path().join("mp");
        std::fs::create_dir_all(&mp).unwrap();
        std::fs::write(mp.join("same.jsonl"), b"SNAP\n").unwrap(); // 相等
        std::fs::write(mp.join("diff.jsonl"), b"CHANGED\n").unwrap(); // 不同
                                                                      // gone.jsonl live 缺失

        let changed = live_underlay_changed_since_snapshot(&stash_underlay, &mp).unwrap();
        assert_eq!(changed, vec!["diff.jsonl".to_string()], "仅报字节不同者");
    }

    #[test]
    fn restore_underlay_guard_keeps_changed_live() {
        // 逐条守卫：还原步遇 live 缺失 → 还原快照；live 与快照不同 → 不覆盖、保留 live、记 skipped。
        let tmp = tempfile::tempdir().unwrap();
        let stash_underlay = tmp.path().join("stash").join("underlay");
        std::fs::create_dir_all(&stash_underlay).unwrap();
        std::fs::write(stash_underlay.join("a.jsonl"), b"SNAP-A\n").unwrap();
        std::fs::write(stash_underlay.join("b.jsonl"), b"SNAP-B\n").unwrap();
        let mp = tmp.path().join("mp");
        std::fs::create_dir_all(&mp).unwrap();
        // a 缺失 → 还原；b 已存在且不同 → 保留 live。
        std::fs::write(mp.join("b.jsonl"), b"LIVE-B-CHANGED\n").unwrap();

        let mut skipped = Vec::new();
        restore_underlay_from_snapshot(&stash_underlay, &mp, &mut skipped).unwrap();

        assert_eq!(
            std::fs::read(mp.join("a.jsonl")).unwrap(),
            b"SNAP-A\n",
            "缺失 → 还原快照"
        );
        assert_eq!(
            std::fs::read(mp.join("b.jsonl")).unwrap(),
            b"LIVE-B-CHANGED\n",
            "不同 → 保留 live、绝不覆盖"
        );
        assert_eq!(
            skipped,
            vec!["b.jsonl".to_string()],
            "记 skipped_live_changed"
        );
    }

    #[test]
    fn reconcile_undo_marker_stays_on_restore_orig_preimage_missing() {
        // marker 对称：中途注入失败（RestoreOrig 前镜像缺失）→ reconciling 标记仍在、无 .undone、可重跑。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        let orig_file = setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let preimage = paths.reconcile_stash("demo", &ts).join("orig").join(rel);
        assert!(preimage.exists(), "reconcile 应留 union 前镜像");
        let preimage_bytes = std::fs::read(&preimage).unwrap();

        // 注入失败：删前镜像 → RestoreOrig fail-closed 中止。
        std::fs::remove_file(&preimage).unwrap();
        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("前镜像") && e.to_string().contains("缺失"),
            "应因前镜像缺失 fail-closed 中止：{e}"
        );
        // marker 仍在（半改写窗口未收尾），无 .undone。
        assert!(
            paths.reconciling_marker("demo").exists(),
            "中途失败 → reconciling 标记保留，让维护让路"
        );
        assert!(
            !paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "失败 → 不落 .undone"
        );

        // 修复（复原前镜像）后重跑 → 幂等成功收尾。
        std::fs::write(&preimage, &preimage_bytes).unwrap();
        let report = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(report.ts, ts);
        assert_eq!(
            std::fs::read(&orig_file).unwrap(),
            BASE_LOG.as_bytes(),
            "重跑后 orig 还原前镜像"
        );
        assert!(
            paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "重跑成功落 .undone"
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "重跑成功清 reconciling 标记"
        );
    }

    #[test]
    fn reconcile_undo_rejects_crashed_run_without_manifest_and_keeps_marker() {
        // 最新代次无 manifest（崩溃未完成的 run）→ 拒绝，且绝不清崩溃 run 的 reconciling marker。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();

        // 崩溃代次：有 underlay 快照但无 manifest。
        let ts = "1000";
        let stash_underlay = paths.reconcile_stash("demo", ts).join("underlay");
        std::fs::create_dir_all(&stash_underlay).unwrap();
        std::fs::write(stash_underlay.join("s.jsonl"), b"{}\n").unwrap();
        // 崩溃 run 遗留的 reconciling 标记。
        set_reconciling(&paths, "demo", true).unwrap();

        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("manifest") && e.to_string().contains("未完成"),
            "无 manifest 的崩溃 run 应拒绝：{e}"
        );
        assert!(
            paths.reconciling_marker("demo").exists(),
            "绝不清除属于崩溃 run 的 reconciling 标记"
        );
    }

    #[test]
    fn reconcile_undo_rejects_when_no_generation() {
        // 无任何 reconcile 代次 → Err「无可回退」。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();
        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("无可回退") || e.to_string().contains("无任何代次"),
            "无代次应拒绝：{e}"
        );
    }

    #[test]
    fn reconcile_undo_second_time_is_noop() {
        // .undone 二次 undo → no-op（返回回填 ts 的空报告，零改动）。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        let r1 = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(r1.ts, ts);
        assert!(!r1.reversed.is_empty(), "首次 undo 有实际逆转");

        // 二次 undo：.undone 已在 → no-op（在活跃/陈旧门之前短路，即便 underlay 已还原）。
        let r2 = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(r2.ts, ts, "no-op 仍回填 ts");
        assert!(
            r2.reversed.is_empty(),
            ".undone 二次 undo → 空 reversed（no-op）"
        );
    }

    #[test]
    fn reconcile_undo_reports_memory_manual_without_touching_target() {
        // ReportMemory：memory 条目进 memory_manual、外部目标未被 undo 触碰、underlay 从快照还原。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_committed_meta(&paths, "demo");
        let mp = paths.mountpoint("demo");
        std::fs::create_dir_all(&mp).unwrap();

        // backing/memory = 指向树外 target 的 symlink；underlay memory 被物化含文件。
        let target = tmp.path().join("external-memory");
        std::fs::create_dir_all(&target).unwrap();
        let backing = paths.backing("demo", Backend::Shadow);
        std::fs::create_dir_all(&backing).unwrap();
        std::os::unix::fs::symlink(&target, backing.join("memory")).unwrap();
        let mem_body = b"# NOTES\nrelocated\n";
        write_underlay(&mp, "memory/NOTE.md", mem_body);

        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        // reconcile 后：memory 文件安置进 target。
        assert_eq!(std::fs::read(target.join("NOTE.md")).unwrap(), mem_body);

        let report = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(report.ts, ts);
        // memory 条目进 memory_manual（供用户 git 回退）。
        assert!(
            report.memory_manual.iter().any(|m| m == "memory/NOTE.md"),
            "memory 条目应进 memory_manual：{:?}",
            report.memory_manual
        );
        // undo 绝不触碰外部 memory 目标（target 仍有 reconcile 写入的文件）。
        assert_eq!(
            std::fs::read(target.join("NOTE.md")).unwrap(),
            mem_body,
            "undo 绝不触碰外部 memory 目标"
        );
        // underlay memory 从快照还原。
        assert_eq!(
            std::fs::read(mp.join("memory/NOTE.md")).unwrap(),
            mem_body,
            "underlay memory 从快照还原"
        );
    }

    #[test]
    fn latest_generation_picks_numeric_max_not_lexical() {
        // ts 按数值比较（"9" < "100"），非字典序。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        for ts in ["9", "100", "20"] {
            std::fs::create_dir_all(paths.reconcile_stash("demo", ts)).unwrap();
        }
        assert_eq!(
            latest_generation(&paths, "demo").unwrap().as_deref(),
            Some("100"),
            "数值最大而非字典序最大"
        );
    }

    // ── Task4：前置门禁 + 崩溃窗口 ─────────────────────────────────────────────

    #[test]
    fn reconcile_undo_rejects_non_shadow_backend() {
        // 前置门禁 1b：container 后端（无 fall-through / per-file 语义）→ 拒，错误含 "shadow"。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();
        std::fs::create_dir_all(paths.back_root()).unwrap();
        let meta = discovery::Meta::from_apply(
            &ApplyOptions {
                backend: Backend::Container,
                ..ApplyOptions::default()
            },
            0,
            0,
            0,
        );
        discovery::write_meta(&paths.meta_path("demo"), &meta).unwrap();

        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("shadow"),
            "container 后端应拒绝 reconcile-undo：{e}"
        );
    }

    #[test]
    fn reconcile_undo_rejects_without_meta() {
        // 前置门禁 1b：无 meta（未 apply / 无可回退记录）→ 拒。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.mountpoint("demo")).unwrap();

        let e = reconcile_undo(&paths, "demo").unwrap_err();
        assert!(
            e.to_string().contains("meta"),
            "无 meta 应拒绝 reconcile-undo：{e}"
        );
    }

    // 前置门禁 1a（已挂载 → 拒）依赖 `discovery::is_mounted` 读真实 `/proc/self/mountinfo`，
    // 无法在 tempdir 单测里注入一个真实 fuse 挂载态（tempdir 路径永非 fuse 挂载点），故不硬造。
    // 该门在「未挂载」方向由本文件所有 undo 测试隐式覆盖（均在未挂载态跑通、未被此门误拒）；
    // 「已挂载 → Err」需真实 fuse mount 的集成测试环境，此处退化说明、不写脆弱断言。

    #[test]
    fn reconcile_undo_short_circuit_clears_lingering_marker() {
        // 崩溃窗口回归（Task4 Important）：模拟上一次 undo 在「.undone 已落、marker 未清」两次 fsync
        // 之间崩溃 → marker 滞留。再调 reconcile_undo 命中 `.undone` 短路 → 短路防御必须顺手清 marker，
        // 闭合 wedge 窗口。RED-before（旧码短路直接 return、永不清 marker）/ GREEN-after。
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let rel = "s.jsonl";
        setup_committed(&paths, "demo", rel, BASE_LOG.as_bytes());
        let mp = paths.mountpoint("demo");
        write_underlay(&mp, rel, INCOMING_LOG.as_bytes());
        let m = FakeMounter::default();
        let rec = reconcile(&paths, "demo", accept_opts(), &m).unwrap();
        let ts = rec
            .stash_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        // 首次 undo：正常收尾（清 marker + 落 .undone）。
        reconcile_undo(&paths, "demo").unwrap();
        assert!(
            paths.reconcile_stash("demo", &ts).join(".undone").exists(),
            "首次 undo 应落 .undone"
        );
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "首次 undo 应已清 marker"
        );

        // 模拟崩溃窗口：.undone 已在，但 reconciling marker 被（旧序崩溃）重新滞留。
        set_reconciling(&paths, "demo", true).unwrap();
        assert!(
            paths.reconciling_marker("demo").exists(),
            "前提：marker 滞留（模拟崩溃窗口）"
        );

        // 再调 undo → 命中 .undone 短路，短路防御清 marker。
        let report = reconcile_undo(&paths, "demo").unwrap();
        assert_eq!(report.ts, ts, "短路仍回填 ts");
        assert!(report.reversed.is_empty(), "短路 no-op → 空 reversed");
        assert!(
            !paths.reconciling_marker("demo").exists(),
            "短路防御必须清滞留 marker（闭合崩溃 wedge 窗口）"
        );
    }
}
