//! reconcile 编排：前置门禁（shadow-only / 活跃 / underlay 非空 / 串行锁）+ underlay 快照。
//!
//! reconcile 把「停用期回落写」（挂载点 underlay 里 Claude 直接写进去的 jsonl 等）安全地重
//! 合并回 backing。落地前必须先过门禁并对 underlay 拍一份**不可变快照**（stash）——快照是后续
//! 合并输入与「删前复核」的唯一基准（评审 I-7/C-a）：活跃门禁只是时间点检查，jsonl fd 可能在
//! 轮次间关闭，故真正删除 live underlay 文件前还要用 `live_entry_unchanged` 复核其未变，杜绝
//! 「快照后又被追加 → 删除时丢掉新数据」的零丢失破口。
//!
//! ## 子模块划分
//!
//! 本文件（`mod.rs`）只保留**跨子模块共享的类型**（EntrySnapshot/UnderlaySnapshot/Preconditions/
//! SupersetMode/EntryPlan/ReversalClass/EntryReport/ReconcileReport/Confirm/ConfirmFn/
//! ReconcileOptions/UndoReport）+ 集中的 `use` 再导出（令各子模块 `use super::*` 即可取到通用依赖
//! 与彼此的 helper）。落盘管线按角色拆成子模块：
//! - [`preconditions`]：门禁 + underlay 快照。
//! - [`io`]：fsync/atomic_write 落盘原语。
//! - [`delete_gate`]：删除许可门（durable 超集 + live 未变）。
//! - [`reingest`]：单文件重灌 backing + reconciling 标记。
//! - [`plan`]：逐条目分类规划。
//! - [`quarantine`]：keep-separate 隔离。
//! - [`routes::subagents`] / [`routes::memory_passthrough`]：subagents 无损并集 / memory 外链透传。
//! - [`apply`]：逐条目落盘。
//! - [`manifest`]：per-generation manifest（undo 依赖）。
//! - [`prune`]：抽干后剪空目录 / 冗余软链。
//! - [`driver`]：顶层 `reconcile` 主循环。
//! - [`undo`]：`reconcile_undo` 回退最近一代。

mod apply;
mod delete_gate;
mod driver;
mod io;
mod manifest;
mod plan;
mod preconditions;
mod prune;
mod quarantine;
mod reingest;
mod routes;
mod undo;

// ── 通用依赖再导出（`use super::*` 令各子模块共享，glob 导入不触发 unused 告警） ──
pub(crate) use std::collections::HashSet;
pub(crate) use std::fs::File;
pub(crate) use std::io::{Read, Write};
pub(crate) use std::os::unix::fs::MetadataExt;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) use crate::enable::daemon::Mounter;
pub(crate) use crate::enable::discovery::{self, detect_activity};
pub(crate) use crate::enable::model::{validate_name, ApplyOptions, Backend, Paths};
pub(crate) use crate::reconcile::advisor::{recommend, Action, Confidence, Recommendation};
pub(crate) use crate::reconcile::guard::{is_harmless, underlay_has_fallthrough};
pub(crate) use crate::reconcile::merge::session_merge;
pub(crate) use crate::store::lock::acquire_exclusive_retry;

// ── 公开 API 再导出（保持 `orchestrator::X` 路径不变） ──
pub use apply::apply_entry;
pub use delete_gate::{delete_permitted, durable_superset_ok, readback_eq};
pub use driver::reconcile;
pub use io::atomic_write;
pub use plan::plan_entries;
pub use preconditions::{check_preconditions, live_entry_unchanged};
pub use quarantine::quarantine_reuse;
pub use reingest::{reingest_one_file, set_reconciling};
pub use routes::memory_passthrough::passthrough_restore_memory;
pub use routes::subagents::reconcile_subagents_dir;
pub use undo::reconcile_undo;

// ── 跨子模块 helper 再导出（pub(crate)，令 `use super::*` 触达彼此私有 helper） ──
pub(crate) use apply::{finish_delete, stash_orig_preimage};
pub(crate) use io::{fsync_dir, fsync_dir_chain, fsync_path};
pub(crate) use manifest::{read_manifest, write_manifest};
pub(crate) use plan::{is_synthetic_rel, lines_to_bytes, reversal_for_preimage};
pub(crate) use prune::{prune_empty_underlay_dirs, prune_redundant_symlinks};
pub(crate) use routes::memory_passthrough::passthrough_action;
pub(crate) use routes::subagents::{
    is_passthrough_entry, is_subagents_entry, passthrough_top_symlink,
};

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
    pub(crate) fn as_str(self) -> &'static str {
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
    pub(crate) fn parse(s: &str) -> Option<Self> {
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

/// 跨子模块测试支撑：共享 fixture 构造 helper 与 jsonl 样本常量（仅 `#[cfg(test)]` 编译）。
/// 各子模块 `#[cfg(test)] mod tests` 经 `use crate::reconcile::orchestrator::testsupport::*` 复用。
#[cfg(test)]
pub(crate) mod testsupport {
    use super::*;
    use crate::enable::daemon::fake::FakeMounter;

    pub(crate) fn paths_in(root: &Path) -> Paths {
        Paths {
            projects_root: root.join("projects"),
            zipfs_home: root.join("zip"),
        }
    }

    pub(crate) fn write_committed_meta(paths: &Paths, name: &str) {
        let meta = discovery::Meta::from_apply(&ApplyOptions::default(), 0, 0, 0);
        std::fs::create_dir_all(paths.back_root()).unwrap();
        discovery::write_meta(&paths.meta_path(name), &meta).unwrap();
    }

    pub(crate) fn read_archive(path: &Path) -> Vec<u8> {
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

    pub(crate) fn snap_entry_of(mp: &Path, rel: &str, bytes: &[u8]) -> EntrySnapshot {
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
    pub(crate) fn write_orig(paths: &Paths, name: &str, rel: &str, content: &[u8]) -> PathBuf {
        let f = paths.orig(name).join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, content).unwrap();
        f
    }

    pub(crate) const BASE_LOG: &str = concat!(
        "{\"type\":\"assistant\",\"uuid\":\"u1\",\"timestamp\":\"2026-06-27T12:00:00.000Z\"}\n",
        "{\"type\":\"ai-title\",\"aiTitle\":\"old\"}\n"
    );
    pub(crate) const INCOMING_LOG: &str = concat!(
        "{\"type\":\"ai-title\",\"aiTitle\":\"new\"}\n",
        "{\"type\":\"mode\",\"mode\":\"normal\"}\n"
    );

    pub(crate) fn write_underlay(mp: &Path, rel: &str, bytes: &[u8]) {
        let p = mp.join(rel);
        if let Some(par) = p.parent() {
            std::fs::create_dir_all(par).unwrap();
        }
        std::fs::write(&p, bytes).unwrap();
    }

    /// 构造一个「已 apply」态可 reconcile 项目：committed meta + orig/<rel>=base + backing 灌好。
    pub(crate) fn setup_committed(paths: &Paths, name: &str, rel: &str, base: &[u8]) -> PathBuf {
        write_committed_meta(paths, name);
        std::fs::create_dir_all(paths.mountpoint(name)).unwrap();
        let orig_file = write_orig(paths, name, rel, base);
        reingest_one_file(paths, name, rel).unwrap();
        orig_file
    }

    pub(crate) fn accept_all() -> Box<ConfirmFn> {
        Box::new(|_, _| Confirm::Accept)
    }

    pub(crate) fn backdate_mtime(path: &Path, secs_ago: u64) {
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

    pub(crate) const KEEP_BASE: &str = concat!(
        "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":null,",
        "\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n"
    );
    pub(crate) const KEEP_INCOMING: &str = concat!(
        "{\"type\":\"assistant\",\"uuid\":\"b1\",\"parentUuid\":null,",
        "\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n"
    );

    pub(crate) fn accept_opts() -> ReconcileOptions {
        ReconcileOptions {
            dry_run: false,
            force: true,
            rebuild: false,
            confirm: accept_all(),
        }
    }

    pub(crate) fn reconcile_three_kinds(paths: &Paths, mp: &Path) -> String {
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
}
