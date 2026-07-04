# 会话感知回落写重合并 —— 实施计划（Session Reconcile）

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实施。步骤用 `- [ ]` 复选框跟踪。

**Goal:** 给 zipfs 加一个会话感知、无损的重合并能力，把影子挂载停用期落进裸挂载点的回落写安全并回 archive，并在真挂载入口加失败即拒守卫。

**Architecture:** 纯合并核（record 解析 → 无损并集 merge → advisor 推荐，全无 IO、全单测）+ orchestrator（活跃门禁 / 快照 / 锁 / 原子替换 / 通用超集删除许可）+ 真挂载入口守卫（`ensure_underlay_empty`）+ CLI/systemd 接线。真源 = `.zipfs-orig` 明文，backing 按文件派生。

**Tech Stack:** Rust，`serde_json`，现有 `fuse` crate（`enable/*`、`ingest.rs`、`archive.rs`、`store/shadow.rs`）。

**Spec:** [docs/09-session-reconcile.md](../09-session-reconcile.md)（@923a587，两轮 subagent 评审已闭合全部 Critical）。

## Global Constraints

- **零静默丢失铁律**：合并恒为双方超集；删 underlay 条目前，其接收方（merged/quarantine/new/memory 目标）必须先 durable（fsync+readback）且逐字节 ⊇/== 该条目；任一不满足即中止保两份。
- **仅 shadow 后端**：container 项目拒绝 reconcile。
- **transcript 记录 = 任何带 `uuid` 字段的记录**（含量最大的 `attachment`、`system`），**绝不按 type 白名单**。
- **无单例折叠**：`last-prompt`/`ai-title`/`custom-title`/`mode` 等无 uuid 记录整行去重并集、保全 distinct。
- **compaction 桥 = `isCompactSummary:true`**，不是 `type:"summary"`。
- **原子替换**：orig / backing archive 一律 `tmp→fsync→rename→fsync_dir`，绝不就地 `O_TRUNC` 写金源。
- Rust：`cargo fmt` + `cargo clippy -- -D warnings`；`unwrap()` 仅测试；`unsafe` 附 `// SAFETY:`。
- 提交遵循 conventional commits；不加 `Co-authored-by`。
- 测试 backing 用 tempdir **子目录**（避免 `.zipfs.lock` 落共享 temp 根 flaky）。

## File Structure

新建模块 `fuse/src/reconcile/`：

| 文件 | 职责 |
|---|---|
| `reconcile/mod.rs` | 模块导出 + 公共类型 re-export |
| `reconcile/record.rs` | Claude jsonl 记录解析、切分（transcript/无uuid/坏行）、截断探测、compaction 标记 |
| `reconcile/merge.rs` | `session_merge` 纯核：无损并集、`Decision`/`Evidence`、同 uuid 冲突、稳定全序、幂等 |
| `reconcile/advisor.rs` | `Recommendation`/`Confidence`（复用 merge 的 `Evidence`，不另采证） |
| `reconcile/guard.rs` | `ensure_underlay_empty` + 无害隐藏项白名单谓词 |
| `reconcile/orchestrator.rs` | IO 编排：前置门禁、stash/快照、逐条目 handler、超集删除许可、报告 |
| `reconcile/paths.rs` | quarantine/stash/reconcile.lock/sentinel 路径（或并入 `enable/model.rs::Paths`） |

修改：`fuse/src/lib.rs`（挂 `mod reconcile`）、`enable/systemd.rs`（`resolve_managed_spec` 调 guard）、`enable/daemon.rs`（`spawn` 前调 guard）、`enable/lifecycle.rs`（`remount` 调 guard）、`enable/autostart.rs`（ExecStartPre sentinel）、`enable/discovery.rs`（NEEDS-RECONCILE）、`enable/mod.rs` + `main.rs`（`enable reconcile` 子命令）。

---

## 阶段 A —— 纯合并核（无 IO，全单测）

### Task 1: 记录模型与切分 `record.rs`

**Files:**
- Create: `fuse/src/reconcile/record.rs`
- Create: `fuse/src/reconcile/mod.rs`
- Modify: `fuse/src/lib.rs`（加 `pub mod reconcile;`）

**Interfaces:**
- Produces:
  - `struct RawRecord { line: String, json: Option<serde_json::Value> }`
  - `enum RecordKind { Transcript { uuid: String }, NoUuid, Unparsable { truncated: bool } }`
  - `fn classify_record(line: &str) -> (RawRecord, RecordKind)`
  - `fn parse_lines(content: &str) -> Vec<(RawRecord, RecordKind)>`
  - `fn is_compact_summary(v: &serde_json::Value) -> bool`
  - `fn record_uuid(v: &serde_json::Value) -> Option<&str>`（取 `uuid` 字段字符串）
  - `fn record_timestamp(v: &serde_json::Value) -> Option<&str>`

- [ ] **Step 1: 建模块骨架**

`fuse/src/reconcile/mod.rs`:
```rust
//! 会话感知回落写重合并。见 docs/09-session-reconcile.md。
pub mod record;
```
`fuse/src/lib.rs` 加一行（放在其它 `pub mod` 附近）：
```rust
pub mod reconcile;
```

- [ ] **Step 2: 写失败测试**

`fuse/src/reconcile/record.rs` 末尾：
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_record_keyed_by_uuid_any_type() {
        // attachment/system 也带 uuid → 必须判 Transcript（不按 type 白名单）。
        for ty in ["assistant", "user", "attachment", "system"] {
            let line = format!(r#"{{"type":"{ty}","uuid":"u1","timestamp":"2026-06-30T10:00:00.000Z"}}"#);
            let (_, kind) = classify_record(&line);
            assert!(matches!(kind, RecordKind::Transcript { .. }), "type={ty} 应判 Transcript");
        }
    }

    #[test]
    fn no_uuid_metadata_is_nouuid_not_transcript() {
        for line in [
            r#"{"type":"ai-title","aiTitle":"x","sessionId":"s"}"#,
            r#"{"type":"last-prompt","lastPrompt":"y","leafUuid":"l","sessionId":"s"}"#,
            r#"{"type":"mode","mode":"normal","sessionId":"s"}"#,
        ] {
            let (_, kind) = classify_record(line);
            assert!(matches!(kind, RecordKind::NoUuid), "应判 NoUuid: {line}");
        }
    }

    #[test]
    fn last_line_truncated_json_flagged() {
        let (_, kind) = classify_record(r#"{"type":"assistant","uuid":"u1","mes"#);
        assert!(matches!(kind, RecordKind::Unparsable { truncated: true }));
    }

    #[test]
    fn compact_summary_detected() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"type":"user","isCompactSummary":true,"uuid":"u","parentUuid":"p"}"#).unwrap();
        assert!(is_compact_summary(&v));
        let v2: serde_json::Value = serde_json::from_str(r#"{"type":"summary"}"#).unwrap();
        assert!(!is_compact_summary(&v2), "type:summary 不是 compaction 标记");
    }
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cd fuse && cargo test -p zipfs reconcile::record 2>&1 | tail -20`
Expected: 编译失败（`classify_record` 等未定义）。

- [ ] **Step 4: 最小实现**

`fuse/src/reconcile/record.rs` 顶部：
```rust
//! Claude 会话 jsonl 记录切分。transcript = 带 uuid（任意 type）；其余无 uuid；坏行含截断探测。

#[derive(Debug, Clone)]
pub struct RawRecord {
    pub line: String,
    pub json: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordKind {
    Transcript { uuid: String },
    NoUuid,
    Unparsable { truncated: bool },
}

pub fn record_uuid(v: &serde_json::Value) -> Option<&str> {
    v.get("uuid").and_then(|u| u.as_str())
}

pub fn record_timestamp(v: &serde_json::Value) -> Option<&str> {
    v.get("timestamp").and_then(|t| t.as_str())
}

pub fn is_compact_summary(v: &serde_json::Value) -> bool {
    v.get("isCompactSummary").and_then(|b| b.as_bool()).unwrap_or(false)
}

pub fn classify_record(line: &str) -> (RawRecord, RecordKind) {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => {
            let kind = match record_uuid(&v) {
                Some(u) => RecordKind::Transcript { uuid: u.to_string() },
                None => RecordKind::NoUuid,
            };
            (RawRecord { line: line.to_string(), json: Some(v) }, kind)
        }
        Err(_) => {
            // 截断探测：非空、以 `{` 起头、未闭合 → 视为截断半行。
            let t = line.trim();
            let truncated = t.starts_with('{') && !t.ends_with('}');
            (RawRecord { line: line.to_string(), json: None },
             RecordKind::Unparsable { truncated })
        }
    }
}

pub fn parse_lines(content: &str) -> Vec<(RawRecord, RecordKind)> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(classify_record)
        .collect()
}
```
在 `mod.rs` 之外无需改动；确保 `serde_json` 已是依赖（`ingest.rs` 已用，故已在 `Cargo.toml`）。

- [ ] **Step 5: 运行确认通过 + fmt/clippy**

Run: `cd fuse && cargo test -p zipfs reconcile::record 2>&1 | tail -20 && cargo fmt && cargo clippy -p zipfs -- -D warnings 2>&1 | tail -5`
Expected: 测试 PASS，clippy 无警告。

- [ ] **Step 6: 提交**

```bash
git add fuse/src/reconcile/mod.rs fuse/src/reconcile/record.rs fuse/src/lib.rs
git commit -m "feat(reconcile): jsonl 记录切分 record.rs（transcript=有uuid/无uuid/截断探测/compaction 标记）"
```

---

### Task 2: 无损并集合并核 `merge.rs`

**Files:**
- Create: `fuse/src/reconcile/merge.rs`
- Modify: `fuse/src/reconcile/mod.rs`（加 `pub mod merge;`）

**Interfaces:**
- Consumes: `record::{RawRecord, RecordKind, classify_record, parse_lines, record_uuid, record_timestamp, is_compact_summary}`
- Produces:
  - `enum Decision { LogOnly, Incremental, CompactionBridge, SuspectReuse }`
  - `struct Evidence { base_uuids: usize, incoming_uuids: usize, uuid_overlap: usize, incoming_only_transcript: usize, has_compaction_bridge: bool, base_ts_range: Option<(String,String)>, incoming_ts_range: Option<(String,String)>, truncated_lines: usize }`
  - `struct MergeResult { decision: Decision, evidence: Evidence, merged_lines: Vec<String>, conflicts: Vec<String> }`
  - `fn session_merge(base: &str, incoming: &str) -> MergeResult`

- [ ] **Step 1: 写失败测试**（真实事故三例 + 反折叠 + 幂等）

`fuse/src/reconcile/merge.rs` 末尾：
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn ts(u: &str, t: &str) -> String {
        format!(r#"{{"type":"assistant","uuid":"{u}","parentUuid":null,"timestamp":"{t}"}}"#)
    }

    #[test]
    fn log_only_keeps_base_transcript_and_unions_logs() {
        // 373e2835 类：base 有正文，incoming 只有日志记录 → 正文全留 + 日志并入。
        let base = format!("{}\n{}\n", ts("u1", "2026-06-27T12:00:00.000Z"),
            r#"{"type":"ai-title","aiTitle":"旧标题","sessionId":"s"}"#);
        let incoming = format!("{}\n{}\n",
            r#"{"type":"ai-title","aiTitle":"新标题","sessionId":"s"}"#,
            r#"{"type":"mode","mode":"normal","sessionId":"s"}"#);
        let r = session_merge(&base, &incoming);
        assert_eq!(r.decision, Decision::LogOnly);
        // 反折叠：新旧两个 ai-title 都在（不是只留最新一条）。
        assert!(r.merged_lines.iter().any(|l| l.contains("旧标题")));
        assert!(r.merged_lines.iter().any(|l| l.contains("新标题")));
        assert!(r.merged_lines.iter().any(|l| l.contains("u1")), "base 正文保留");
    }

    #[test]
    fn many_last_prompt_all_preserved_not_folded() {
        // 反 newest-wins：3 条不同 last-prompt 必须全留。
        let base = format!("{}\n{}\n",
            r#"{"type":"last-prompt","lastPrompt":"p1","leafUuid":"a"}"#,
            r#"{"type":"last-prompt","lastPrompt":"p2","leafUuid":"b"}"#);
        let incoming = format!("{}\n",
            r#"{"type":"last-prompt","lastPrompt":"p3","leafUuid":"c"}"#);
        let r = session_merge(&base, &incoming);
        let n = r.merged_lines.iter().filter(|l| l.contains("last-prompt")).count();
        assert_eq!(n, 3, "三条 distinct last-prompt 全保留，不折叠");
    }

    #[test]
    fn suspect_reuse_when_disjoint_uuids_no_bridge() {
        // 925fc3a1 类：uuid 全disjoint、无 compaction 桥 → SuspectReuse。
        let base = format!("{}\n", ts("a1", "2026-06-24T16:55:00.000Z"));
        let incoming = format!("{}\n", ts("b1", "2026-06-30T10:42:00.000Z"));
        let r = session_merge(&base, &incoming);
        assert_eq!(r.decision, Decision::SuspectReuse);
        assert_eq!(r.evidence.uuid_overlap, 0);
    }

    #[test]
    fn compaction_bridge_is_incremental_not_reuse() {
        // incoming 含 isCompactSummary → 判 CompactionBridge（合法续写），非 reuse。
        let base = format!("{}\n", ts("a1", "2026-06-24T16:55:00.000Z"));
        let incoming = format!("{}\n{}\n",
            r#"{"type":"user","isCompactSummary":true,"uuid":"b1","parentUuid":"a1","timestamp":"2026-06-30T10:00:00.000Z"}"#,
            ts("b2", "2026-06-30T10:01:00.000Z"));
        let r = session_merge(&base, &incoming);
        assert_eq!(r.decision, Decision::CompactionBridge);
    }

    #[test]
    fn incremental_unions_by_uuid_no_dup() {
        let base = format!("{}\n{}\n", ts("u1","2026-06-30T10:00:00.000Z"), ts("u2","2026-06-30T10:01:00.000Z"));
        let incoming = format!("{}\n{}\n", ts("u2","2026-06-30T10:01:00.000Z"), ts("u3","2026-06-30T10:02:00.000Z"));
        let r = session_merge(&base, &incoming);
        assert_eq!(r.decision, Decision::Incremental);
        let uu: Vec<_> = r.merged_lines.iter().filter(|l| l.contains(r#""uuid""#)).collect();
        assert_eq!(uu.len(), 3, "u1/u2/u3 各一次，u2 不重复");
    }

    #[test]
    fn merge_is_idempotent() {
        let base = format!("{}\n", ts("u1","2026-06-30T10:00:00.000Z"));
        let incoming = format!("{}\n", ts("u2","2026-06-30T10:01:00.000Z"));
        let once = session_merge(&base, &incoming).merged_lines.join("\n");
        let twice = session_merge(&once, &incoming).merged_lines.join("\n");
        assert_eq!(once, twice, "merge(merge(b,i),i) == merge(b,i)");
    }

    #[test]
    fn same_uuid_conflict_takes_longer_complete() {
        // 崩溃截断：同 uuid，一份完整一份短 → 取完整更长者 + 记 conflict。
        let full = r#"{"type":"assistant","uuid":"u1","timestamp":"2026-06-30T10:00:00.000Z","message":"complete"}"#;
        let short = r#"{"type":"assistant","uuid":"u1","timestamp":"2026-06-30T10:00:00.000Z"}"#;
        let r = session_merge(&format!("{short}\n"), &format!("{full}\n"));
        assert!(r.merged_lines.iter().any(|l| l.contains("complete")));
        assert_eq!(r.merged_lines.iter().filter(|l| l.contains(r#""u1""#)).count(), 1);
        assert!(!r.conflicts.is_empty(), "同 uuid 冲突记入报告");
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd fuse && cargo test -p zipfs reconcile::merge 2>&1 | tail -20`
Expected: 编译失败（`session_merge` 未定义）。

- [ ] **Step 3: 实现 merge 核**

`fuse/src/reconcile/merge.rs` 顶部：
```rust
//! 无损并集合并核。带 uuid 记录按 uuid 并集（同 uuid 取完整更长者）；无 uuid 记录整行去重并集；
//! 坏行 verbatim。稳定全序（timestamp→原始行号）。分类只驱动 advisor 推荐，永不有损。

use std::collections::BTreeMap;
use crate::reconcile::record::{
    classify_record, is_compact_summary, record_timestamp, RawRecord, RecordKind,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    LogOnly,
    Incremental,
    CompactionBridge,
    SuspectReuse,
}

#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub base_uuids: usize,
    pub incoming_uuids: usize,
    pub uuid_overlap: usize,
    pub incoming_only_transcript: usize,
    pub has_compaction_bridge: bool,
    pub base_ts_range: Option<(String, String)>,
    pub incoming_ts_range: Option<(String, String)>,
    pub truncated_lines: usize,
}

#[derive(Debug, Clone)]
pub struct MergeResult {
    pub decision: Decision,
    pub evidence: Evidence,
    pub merged_lines: Vec<String>,
    pub conflicts: Vec<String>,
}

// 内部：一条待排序的输出项，携带排序键（timestamp, 全局序号）。
struct Item {
    ts: Option<String>,
    ord: usize,
    line: String,
}

fn ts_range<'a>(it: impl Iterator<Item = &'a str>) -> Option<(String, String)> {
    let mut lo: Option<&str> = None;
    let mut hi: Option<&str> = None;
    for t in it {
        if lo.map(|x| t < x).unwrap_or(true) { lo = Some(t); }
        if hi.map(|x| t > x).unwrap_or(true) { hi = Some(t); }
    }
    Some((lo?.to_string(), hi?.to_string()))
}

pub fn session_merge(base: &str, incoming: &str) -> MergeResult {
    let base_recs: Vec<_> = base.lines().filter(|l| !l.trim().is_empty()).map(classify_record).collect();
    let inc_recs: Vec<_> = incoming.lines().filter(|l| !l.trim().is_empty()).map(classify_record).collect();

    let uuid_of = |k: &RecordKind| match k { RecordKind::Transcript { uuid } => Some(uuid.clone()), _ => None };
    let base_uuids: std::collections::BTreeSet<String> =
        base_recs.iter().filter_map(|(_, k)| uuid_of(k)).collect();
    let inc_uuids: std::collections::BTreeSet<String> =
        inc_recs.iter().filter_map(|(_, k)| uuid_of(k)).collect();
    let overlap = base_uuids.intersection(&inc_uuids).count();

    let has_bridge = inc_recs.iter().any(|(r, _)| r.json.as_ref().map(is_compact_summary).unwrap_or(false));
    let inc_only_transcript = inc_uuids.difference(&base_uuids).count();

    // ── 无损并集：uuid 去重（同 uuid 取完整更长者），无 uuid 整行去重，坏行 verbatim ──
    let mut by_uuid: BTreeMap<String, Item> = BTreeMap::new();
    let mut seen_lines: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut extras: Vec<Item> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut truncated = 0usize;
    let mut ord = 0usize;

    let mut ingest = |recs: &[(RawRecord, RecordKind)]| {
        for (r, k) in recs {
            let ts = r.json.as_ref().and_then(|v| record_timestamp(v).map(|s| s.to_string()));
            match k {
                RecordKind::Transcript { uuid } => {
                    match by_uuid.get(uuid) {
                        Some(prev) if prev.line.len() >= r.line.len() => {
                            if prev.line != r.line { conflicts.push(uuid.clone()); }
                        }
                        Some(_) => {
                            conflicts.push(uuid.clone());
                            by_uuid.insert(uuid.clone(), Item { ts, ord, line: r.line.clone() });
                        }
                        None => { by_uuid.insert(uuid.clone(), Item { ts, ord, line: r.line.clone() }); }
                    }
                }
                RecordKind::NoUuid => {
                    if seen_lines.insert(r.line.clone()) {
                        extras.push(Item { ts, ord, line: r.line.clone() });
                    }
                }
                RecordKind::Unparsable { truncated: tr } => {
                    if *tr { truncated += 1; }
                    if seen_lines.insert(r.line.clone()) {
                        extras.push(Item { ts, ord, line: r.line.clone() });
                    }
                }
            }
            ord += 1;
        }
    };
    ingest(&base_recs);
    ingest(&inc_recs);

    let mut items: Vec<Item> = by_uuid.into_values().chain(extras).collect();
    items.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.ord.cmp(&b.ord)));
    let merged_lines: Vec<String> = items.into_iter().map(|i| i.line).collect();

    let decision = if has_bridge {
        Decision::CompactionBridge
    } else if inc_uuids.is_empty() {
        Decision::LogOnly
    } else if overlap > 0 {
        Decision::Incremental
    } else if !base_uuids.is_empty() {
        Decision::SuspectReuse
    } else {
        Decision::Incremental // base 也无 transcript：纯日志两侧，安全并入
    };

    let evidence = Evidence {
        base_uuids: base_uuids.len(),
        incoming_uuids: inc_uuids.len(),
        uuid_overlap: overlap,
        incoming_only_transcript: inc_only_transcript,
        has_compaction_bridge: has_bridge,
        base_ts_range: ts_range(base_recs.iter().filter_map(|(r, _)| r.json.as_ref().and_then(record_timestamp))),
        incoming_ts_range: ts_range(inc_recs.iter().filter_map(|(r, _)| r.json.as_ref().and_then(record_timestamp))),
        truncated_lines: truncated,
    };

    MergeResult { decision, evidence, merged_lines, conflicts }
}
```
`mod.rs` 加 `pub mod merge;`。

> 注：`SuspectReuse` 的 `merged_lines` 仍计算（无损并集），但 orchestrator 对 SuspectReuse 默认不采用 merged、而走 keep-separate（§5.3）；merged 供用户选择「改并入」时复用。

- [ ] **Step 4: 运行确认通过 + fmt/clippy**

Run: `cd fuse && cargo test -p zipfs reconcile::merge 2>&1 | tail -20 && cargo fmt && cargo clippy -p zipfs -- -D warnings 2>&1 | tail -5`
Expected: 全 PASS，无 clippy 警告。

- [ ] **Step 5: 提交**

```bash
git add fuse/src/reconcile/merge.rs fuse/src/reconcile/mod.rs
git commit -m "feat(reconcile): 无损并集合并核 merge.rs（uuid 并集/日志保全 distinct/稳定全序/幂等/reuse 判据）"
```

---

### Task 3: 决策推荐 `advisor.rs`

**Files:**
- Create: `fuse/src/reconcile/advisor.rs`
- Modify: `fuse/src/reconcile/mod.rs`

**Interfaces:**
- Consumes: `merge::{Decision, Evidence, MergeResult}`
- Produces:
  - `enum Confidence { High, Medium, Low }`
  - `enum Action { UnionIntoBase, KeepSeparate, PassthroughRestore, KeepBoth }`
  - `struct Recommendation { action: Action, confidence: Confidence, rationale: String }`
  - `fn recommend(r: &MergeResult) -> Recommendation`

- [ ] **Step 1: 写失败测试**

`fuse/src/reconcile/advisor.rs` 末尾：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::merge::session_merge;

    fn rec(base: &str, inc: &str) -> Recommendation { recommend(&session_merge(base, inc)) }

    #[test]
    fn log_only_high_union() {
        let r = rec(
            "{\"type\":\"assistant\",\"uuid\":\"u1\",\"timestamp\":\"2026-06-27T12:00:00.000Z\"}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\"t\"}\n",
        );
        assert!(matches!(r.action, Action::UnionIntoBase));
        assert!(matches!(r.confidence, Confidence::High));
    }

    #[test]
    fn suspect_reuse_low_keep_separate() {
        let r = rec(
            "{\"type\":\"assistant\",\"uuid\":\"a\",\"timestamp\":\"2026-06-24T00:00:00.000Z\"}\n",
            "{\"type\":\"assistant\",\"uuid\":\"b\",\"timestamp\":\"2026-06-30T00:00:00.000Z\"}\n",
        );
        assert!(matches!(r.action, Action::KeepSeparate));
        assert!(matches!(r.confidence, Confidence::Low));
    }
}
```

- [ ] **Step 2: 确认失败** — Run: `cd fuse && cargo test -p zipfs reconcile::advisor 2>&1 | tail -20` → 编译失败。

- [ ] **Step 3: 实现**

`fuse/src/reconcile/advisor.rs`:
```rust
//! 决策推荐：复用 merge 的 Evidence，只叠置信度 + 措辞，不另采证（评审 I2）。
use crate::reconcile::merge::{Decision, MergeResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confidence { High, Medium, Low }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action { UnionIntoBase, KeepSeparate, PassthroughRestore, KeepBoth }

#[derive(Debug, Clone)]
pub struct Recommendation {
    pub action: Action,
    pub confidence: Confidence,
    pub rationale: String,
}

pub fn recommend(r: &MergeResult) -> Recommendation {
    match r.decision {
        Decision::LogOnly => Recommendation {
            action: Action::UnionIntoBase,
            confidence: Confidence::High,
            rationale: "incoming 仅日志记录、无 transcript；base 正文全保留 + 日志并入，无损".into(),
        },
        Decision::CompactionBridge => Recommendation {
            action: Action::UnionIntoBase,
            confidence: Confidence::High,
            rationale: "检出 isCompactSummary 桥，属合法 compaction 续写 → 并入".into(),
        },
        Decision::Incremental => Recommendation {
            action: Action::UnionIntoBase,
            confidence: if r.conflicts.is_empty() { Confidence::High } else { Confidence::Medium },
            rationale: format!("transcript uuid 有交集（overlap={}），uuid 并集无丢失", r.evidence.uuid_overlap),
        },
        Decision::SuspectReuse => Recommendation {
            action: Action::KeepSeparate,
            confidence: Confidence::Low,
            rationale: "transcript uuid 全disjoint、无 compaction 桥、时间窗不相交 → 疑 session-id 重用；默认隔离保两份，可经确认改并入".into(),
        },
    }
}
```
`mod.rs` 加 `pub mod advisor;`。

- [ ] **Step 4: 确认通过 + fmt/clippy** — Run: `cd fuse && cargo test -p zipfs reconcile 2>&1 | tail && cargo fmt && cargo clippy -p zipfs -- -D warnings 2>&1 | tail -5`

- [ ] **Step 5: 提交**
```bash
git add fuse/src/reconcile/advisor.rs fuse/src/reconcile/mod.rs
git commit -m "feat(reconcile): advisor.rs 决策推荐（复用 Evidence，置信度+推荐动作）"
```

---

## 阶段 B —— 真挂载入口守卫

### Task 4: `guard.rs` + 接入所有挂载入口

**Files:**
- Create: `fuse/src/reconcile/guard.rs`
- Modify: `fuse/src/reconcile/mod.rs`
- Modify: `fuse/src/enable/lifecycle.rs`（`remount` spawn 前调用）
- Modify: `fuse/src/enable/systemd.rs`（`resolve_managed_spec` 内调用）

**Interfaces:**
- Produces:
  - `fn is_harmless(name: &std::ffi::OsStr) -> bool`（`.fuse_hidden*`/`.DS_Store`/`.*.swp` 等白名单）
  - `fn underlay_has_fallthrough(mp: &std::path::Path) -> std::io::Result<bool>`
  - `fn ensure_underlay_empty(mp: &std::path::Path) -> std::io::Result<()>`（非空 fall-through → `Err`，错误信息指向 `enable reconcile`）

- [ ] **Step 1: 写失败测试**

`fuse/src/reconcile/guard.rs` 末尾：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_dir_ok() {
        let d = tempfile::tempdir().unwrap();
        assert!(ensure_underlay_empty(d.path()).is_ok());
    }
    #[test]
    fn harmless_hidden_ignored() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".fuse_hidden0001"), b"").unwrap();
        std::fs::write(d.path().join(".DS_Store"), b"").unwrap();
        assert!(ensure_underlay_empty(d.path()).is_ok(), "无害隐藏项应放行");
    }
    #[test]
    fn fallthrough_jsonl_blocks() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("s.jsonl"), b"{}").unwrap();
        let e = ensure_underlay_empty(d.path()).unwrap_err();
        assert!(e.to_string().contains("reconcile"), "错误应指向 reconcile");
    }
}
```

- [ ] **Step 2: 确认失败** — Run: `cd fuse && cargo test -p zipfs reconcile::guard 2>&1 | tail -20`

- [ ] **Step 3: 实现 guard**

`fuse/src/reconcile/guard.rs`:
```rust
//! 挂载前 underlay 守卫：真正挂载前的最后一道，非空 fall-through 即拒（评审 C1）。
use std::ffi::OsStr;
use std::io;
use std::path::Path;

pub fn is_harmless(name: &OsStr) -> bool {
    let n = name.to_string_lossy();
    n.starts_with(".fuse_hidden")
        || n == ".DS_Store"
        || (n.starts_with('.') && (n.ends_with(".swp") || n.ends_with(".swx") || n.ends_with('~')))
}

pub fn underlay_has_fallthrough(mp: &Path) -> io::Result<bool> {
    let rd = match std::fs::read_dir(mp) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    for dent in rd {
        let dent = dent?;
        if !is_harmless(&dent.file_name()) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn ensure_underlay_empty(mp: &Path) -> io::Result<()> {
    if underlay_has_fallthrough(mp)? {
        return Err(io::Error::other(format!(
            "{} 挂载点 underlay 含停用期回落写，拒绝挂载（防静默盖住）；先 `zipfs enable reconcile` 重合并",
            mp.display()
        )));
    }
    Ok(())
}
```
`mod.rs` 加 `pub mod guard;`。

- [ ] **Step 4: 接入 remount**

`fuse/src/enable/lifecycle.rs` 的 `remount`：在 `mount_spec` 构造后、`mounter.spawn(&spec)` 前插入：
```rust
    crate::reconcile::guard::ensure_underlay_empty(&mp)?;
```
（`remount` 里 `mp` 已定义；此处 mp 是挂载点，非空 fall-through 即拒。）

- [ ] **Step 5: 接入 systemd 自启入口**

`fuse/src/enable/systemd.rs` 的 `resolve_managed_spec`：在返回 spec **之前**、确定挂载点后加：
```rust
    crate::reconcile::guard::ensure_underlay_empty(&spec.mountpoint)?;
```
（确认 `resolve_managed_spec` 返回类型为 `io::Result<MountSpec>`；若签名不返回 Result，改为在其唯一调用点 `mount-managed` handler 挂载前调用。实施时 grep `resolve_managed_spec` 确认签名。）

- [ ] **Step 6: 运行 + 全量测试 + fmt/clippy**

Run: `cd fuse && cargo test -p zipfs reconcile 2>&1 | tail && cargo test -p zipfs enable 2>&1 | tail && cargo fmt && cargo clippy -p zipfs -- -D warnings 2>&1 | tail -5`
Expected: 新测试 PASS；既有 enable 测试不回归。

- [ ] **Step 7: 提交**
```bash
git add fuse/src/reconcile/guard.rs fuse/src/reconcile/mod.rs fuse/src/enable/lifecycle.rs fuse/src/enable/systemd.rs
git commit -m "feat(reconcile): 挂载前 underlay 守卫接入 remount + systemd 自启入口（C1）"
```

---

## 阶段 C —— orchestrator（IO 编排）

> 本阶段前先精读 `enable/lifecycle.rs`（`reingest`/`fsync_parent`/`wait_daemon_exit`/`detect_activity` 调用）、`ingest.rs`（`ingest_tree` 逐文件、verify）、`enable/model.rs::Paths`，复用现有原语，勿另起炉灶。

### Task 5: reconcile 路径 + 前置门禁（活跃/后端/锁/快照）

**Files:**
- Modify: `fuse/src/enable/model.rs`（`Paths` 加 `reconcile_stash`/`quarantine`/`reconcile_lock`/`needs_reconcile_sentinel`）
- Create: `fuse/src/reconcile/orchestrator.rs`
- Modify: `fuse/src/reconcile/mod.rs`

**Interfaces:**
- Consumes: `enable::model::{Paths, Backend}`, `enable::discovery`
- Produces:
  - `Paths::reconcile_stash(&self, name, ts) -> PathBuf`（`zipfs_home/reconcile-stash/<name>/<ts>`）
  - `Paths::quarantine(&self, name, ts) -> PathBuf`（`zipfs_home/reconcile-quarantine/<name>/<ts>`）
  - `Paths::reconcile_lock(&self, name) -> PathBuf`（`back_root/<name>.reconcile.lock`）
  - `struct Preconditions { /* 已校验的句柄：flock guard、snapshot 清单 */ }`
  - `fn check_preconditions(paths:&Paths, name:&str, backend:Backend, force:bool) -> io::Result<Preconditions>`

- [ ] **Step 1: 写失败测试**（路径 + 门禁：container 拒绝 / 活跃拒绝 / 空 underlay 拒绝）

`fuse/src/reconcile/orchestrator.rs` 末尾（参照 `lifecycle.rs` tests 的 `paths_in` 构造隔离 Paths）：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::enable::model::{Backend, Paths};
    use std::path::Path;

    fn paths_in(root: &Path) -> Paths {
        Paths { projects_root: root.join("projects"), zipfs_home: root.join("zip") }
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
}
```

- [ ] **Step 2: 确认失败** — Run: `cd fuse && cargo test -p zipfs reconcile::orchestrator 2>&1 | tail -20`

- [ ] **Step 3: 实现路径 + 门禁**

`enable/model.rs` 的 `impl Paths` 加：
```rust
    pub fn reconcile_stash(&self, name: &str, ts: &str) -> PathBuf {
        self.zipfs_home.join("reconcile-stash").join(name).join(ts)
    }
    pub fn quarantine(&self, name: &str, ts: &str) -> PathBuf {
        self.zipfs_home.join("reconcile-quarantine").join(name).join(ts)
    }
    pub fn reconcile_lock(&self, name: &str) -> PathBuf {
        self.back_root().join(format!("{name}.reconcile.lock"))
    }
```
`orchestrator.rs` 顶部实现 `check_preconditions`：校验 name（`validate_name`）、backend==Shadow 否则 Err、underlay 有 fall-through（复用 `guard::underlay_has_fallthrough`）否则 Err、`!force` 时 `discovery::detect_activity` 空闲否则 Err、取 `reconcile_lock` flock（复用 store 里现有 flock 原语或 `fs2`/`libc::flock`，与 `ingest`/backing 锁一致）。返回持锁 `Preconditions`。

- [ ] **Step 4: 确认通过 + fmt/clippy**

- [ ] **Step 5: 提交**
```bash
git add fuse/src/enable/model.rs fuse/src/reconcile/orchestrator.rs fuse/src/reconcile/mod.rs
git commit -m "feat(reconcile): orchestrator 前置门禁（shadow-only/活跃/underlay/锁）+ reconcile 路径"
```

---

### Task 6: 原子写原语 + 通用超集删除许可

**Files:**
- Modify: `fuse/src/reconcile/orchestrator.rs`

**Interfaces:**
- Produces:
  - `fn atomic_write(dst:&Path, bytes:&[u8]) -> io::Result<()>`（`<dst>.tmp`→fsync→rename→fsync_dir）
  - `fn durable_superset_ok(receiver:&Path, source_bytes:&[u8], mode: SupersetMode) -> io::Result<bool>`
  - `enum SupersetMode { ByteEqual, LinesSuperset }`
  - `fn readback_eq(path:&Path, bytes:&[u8]) -> io::Result<bool>`

- [ ] **Step 1: 写失败测试**
```rust
    #[test]
    fn atomic_write_then_readback() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.jsonl");
        atomic_write(&p, b"line1\nline2\n").unwrap();
        assert!(readback_eq(&p, b"line1\nline2\n").unwrap());
        assert!(!p.with_extension("jsonl.tmp").exists(), "tmp 应已 rename 消失");
    }
    #[test]
    fn lines_superset_detects_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let recv = tmp.path().join("m.jsonl");
        atomic_write(&recv, b"a\nb\n").unwrap();          // 缺 c
        let ok = durable_superset_ok(&recv, b"a\nb\nc\n", SupersetMode::LinesSuperset).unwrap();
        assert!(!ok, "接收方缺行 → 不许删源");
    }
```

- [ ] **Step 2: 确认失败**

- [ ] **Step 3: 实现**（`atomic_write` 复用 `lifecycle::fsync_parent` 思路；`LinesSuperset` = source 每行 ∈ receiver 行集合；`ByteEqual` = 逐字节等）。

- [ ] **Step 4: 确认通过 + fmt/clippy**

- [ ] **Step 5: 提交**
```bash
git commit -am "feat(reconcile): 原子写 + 通用超集删除许可（durable+readback，评审 C2/数据 C2）"
```

---

### Task 7: 逐条目 handler + reconciling 标记 + 报告

**Files:**
- Modify: `fuse/src/reconcile/orchestrator.rs`
- Modify: `fuse/src/reconcile/mod.rs`

**Interfaces:**
- Consumes: `merge::session_merge`, `advisor::recommend`, `ingest::ingest_file`（或 `ingest_tree` 单文件路径）, Task 6 原语, `discovery::{read_meta, write_meta}`
- Produces:
  - `enum EntryPlan { Union, KeepSeparate, New, Passthrough, KeepBoth, Identical }`
  - `struct EntryReport { name:String, decision:String, action:String, notes:Vec<String> }`
  - `struct ReconcileReport { entries:Vec<EntryReport>, stash_dir:PathBuf }`
  - `fn plan_entries(paths:&Paths, name:&str) -> io::Result<Vec<(String, EntryPlan, Recommendation)>>`（dry-run 建议单）
  - `fn apply_entry(...) -> io::Result<EntryReport>`（单条：stash→原子写 orig→重灌 backing→超集校验→删 underlay 条目）
  - `fn set_reconciling(paths:&Paths, name:&str, on:bool) -> io::Result<()>`（切 committed 0/1）

- [ ] **Step 1..N（TDD）**：
  - 测试：log-only 条目 apply 后 orig 含合并结果、underlay 条目已删、backing 该文件重灌、报告记录动作；apply 前若接收方缺行则中止且 underlay 保留。
  - 测试：`set_reconciling(true)` 后 `discovery::probe` 判 Broken（committed=0），`false` 复位 Stopped/Active。
  - 实现 `plan_entries`（枚举 underlay 条目、按 §5.3 分类给 EntryPlan+Recommendation，不动盘）与 `apply_entry`（严格落盘顺序 + 超集许可 + reconciling 包裹）。
  - 每个语义单元一次提交。

- [ ] **末步提交**
```bash
git commit -am "feat(reconcile): 逐条目 handler（union/new）+ reconciling 标记 + 报告 + 超集删除门"
```

---

### Task 8: keep-separate 隔离 + subagents/其他非-jsonl + memory 透传

**Files:**
- Modify: `fuse/src/reconcile/orchestrator.rs`

**Interfaces:**
- Produces:
  - `fn quarantine_reuse(paths, name, ts, uuid_file) -> io::Result<PathBuf>`（保原 UUID 名、移出树、超集许可后删 underlay）
  - `fn reconcile_subagents_dir(...)`（子 jsonl 走 `session_merge`，绝不 mtime 删）
  - `fn passthrough_restore_memory(underlay_dir, symlink_target) -> io::Result<Vec<String>>`（canonicalize、拒 `../`、目标可写校验、内容哈希改名、悬空/不可写不删待人工）

- [ ] **TDD steps**：
  - 测试：SuspectReuse → quarantine 下出现原 `<uuid>.jsonl`、projects 树内 base 不变、underlay 条目在超集校验后删。
  - 测试：subagents 同名两侧异内容 → 结果并集、无一方被 mtime 删。
  - 测试：memory 透传——目标新文件被复制；`MEMORY.md` 冲突 → 目标旁出现 `MEMORY.md.underlay-<hash8>`；`../` 目标被拒；悬空目标时 underlay memory **不删**并入报告。
  - 实现三 handler，全部走 Task 6 超集删除许可。
  - 分语义单元提交。

- [ ] **末步提交**
```bash
git commit -am "feat(reconcile): reuse 隔离(保UUID/移出树) + subagents 合并 + memory 透传恢复(路径安全/内容哈希幂等)"
```

---

### Task 9: 顶层 `reconcile()` 编排 + 崩溃续跑 + meta 收尾

**Files:**
- Modify: `fuse/src/reconcile/orchestrator.rs`

**Interfaces:**
- Produces:
  - `struct ReconcileOptions { dry_run:bool, force:bool, rebuild:bool, confirm: Box<dyn Fn(&str,&Recommendation)->Confirm> }`
  - `enum Confirm { Accept, KeepBoth, Skip }`
  - `fn reconcile(paths:&Paths, name:&str, opts:ReconcileOptions, mounter:&dyn Mounter) -> io::Result<ReconcileReport>`

- [ ] **TDD steps**：
  - 测试：dry_run 只出报告、underlay/orig/backing 零改动。
  - 测试：全流程（FakeMounter）——门禁→快照→`set_reconciling(true)`→逐条 confirm→apply→underlay 清空→`set_reconciling(false)`→meta 字节数更新→结束态可挂。
  - 测试：中途注入 apply 失败 → orig 已改文件已原子替换、underlay 未删部分保留、committed 仍 0（判 Broken），重跑幂等收敛。
  - 测试：残留 stash 的发现/GC（超期 stash 可清）。
  - `--rebuild` 分支：委托 `lifecycle::reingest`（golden ⊕ 已确认合并树），复用现有原语。
  - 分语义单元提交。

- [ ] **末步提交**
```bash
git commit -am "feat(reconcile): 顶层 reconcile 编排（dry-run/confirm/rebuild）+ 崩溃续跑 + meta 收尾"
```

---

## 阶段 D —— CLI / systemd / list 接线

### Task 10: `enable reconcile` 子命令 + 交互确认

**Files:**
- Modify: `fuse/src/main.rs`（clap 子命令）
- Modify: `fuse/src/enable/mod.rs`（dispatch）

- [ ] **TDD/steps**：
  - 加 `EnableCmd::Reconcile { name, dry_run, force, rebuild }`；
  - 交互 `confirm` 闭包：dry-run 打印建议单；实跑逐条 `[a]ccept/[k]eep-both/[s]kip` 读 stdin；非交互（无 tty）且非 dry-run → 拒绝并提示（策略 B）。
  - 测试：`--dry-run` 走 orchestrator dry 分支输出建议单文本（可对 orchestrator 报告做快照断言）。
  - 提交。
```bash
git commit -am "feat(cli): enable reconcile 子命令 + 逐条交互确认（策略 B）"
```

### Task 11: discovery NEEDS-RECONCILE + list 展示

**Files:**
- Modify: `fuse/src/enable/discovery.rs`（probe/scan 检测 STOPPED+非空 underlay）
- Modify: `fuse/src/enable/mod.rs`（list 列渲染）

- [ ] **TDD/steps**：
  - `probe` 增字段/标志 `needs_reconcile`（Stopped 且 `guard::underlay_has_fallthrough`）；reconciling 中（committed=0 + reconcile_lock 持有）显示 `reconciling`。
  - 测试：构造 Stopped + 非空 underlay 的隔离 Paths → list 标 `NEEDS-RECONCILE`。
  - 提交。
```bash
git commit -am "feat(enable): list 标 NEEDS-RECONCILE / reconciling"
```

### Task 12: systemd ExecStartPre sentinel（防 crash-loop）

**Files:**
- Modify: `fuse/src/enable/autostart.rs`（单元模板加 `ExecStartPre`）
- Modify: `fuse/src/enable/systemd.rs`（sentinel 落盘/清理）

- [ ] **TDD/steps**：
  - 单元模板加 `ExecStartPre=zipfs enable guard-check %i`（非空 underlay → 落 `NEEDS-RECONCILE` sentinel + 非 0 退出，但**不进 Restart 循环**：`ExecStartPre` 失败使 unit 直接 failed 而非 watchdog 重启风暴）。
  - 加 `enable guard-check <name>` 隐藏子命令：调 `ensure_underlay_empty`，失败落 sentinel + 明确 stderr。
  - 测试 `mount_argv`/单元渲染含 `ExecStartPre`；`guard-check` 对非空 underlay 返回非 0 且落 sentinel。
  - 提交。
```bash
git commit -am "feat(autostart): ExecStartPre guard-check + NEEDS-RECONCILE sentinel（防 systemd crash-loop）"
```

---

## 阶段 E —— 落地本次事故（neighbors）

### Task 13: 在真实 neighbors 上重合并 + 重挂 + 验证

> 非代码任务；对真实用户数据操作，全程 dry-run 先行、逐条确认、字节校验。**先备份**。

- [ ] **Step 1: 冷备份真实数据**（防万一）
```bash
cp -a ~/.claude/projects/-home-xp-src-neighbors.zipfs-orig /tmp/neighbors-orig-backup-$(date +%s)
```
Expected: 备份完成，`du -sh` 与源相当。

- [ ] **Step 2: dry-run 出建议单**
```bash
cd /home/xp/src/zipfs/fuse && ./target/release/zipfs enable reconcile -home-xp-src-neighbors --dry-run
```
Expected: 373e2835/de756008 = LogOnly→UnionIntoBase(High)；925fc3a1 = SuspectReuse→KeepSeparate(Low)；memory = Passthrough。零改动。

- [ ] **Step 3: 逐条确认实跑**
```bash
./target/release/zipfs enable reconcile -home-xp-src-neighbors
```
按建议逐条确认（925fc3a1 采纳 keep-separate 隔离）。

- [ ] **Step 4: 字节/超集校验**
```bash
# orig 中 373e2835 应重新含完整正文 + 新标题；925fc3a1 隔离副本保原 UUID 名
ls ~/.claude-zip/reconcile-quarantine/-home-xp-src-neighbors/*/
python3 -c "import json,sys; [json.loads(l) for l in open(sys.argv[1])]" ~/.claude/projects/-home-xp-src-neighbors.zipfs-orig/373e2835-c5a4-4822-a8b9-23d9d3cbd667.jsonl
```
Expected: 隔离目录含 `925fc3a1-….jsonl`（原名）；orig 各文件合法 jsonl。

- [ ] **Step 5: 重挂 + 确认 ACTIVE**
```bash
./target/release/zipfs enable remount -home-xp-src-neighbors
./target/release/zipfs enable list | grep neighbors
```
Expected: 守卫放行（underlay 已清空）、状态 ZIPFS(Active)。

- [ ] **Step 6: 挂载视图字节一致**
```bash
diff <(cat ~/.claude/projects/-home-xp-src-neighbors/373e2835-*.jsonl) \
     ~/.claude/projects/-home-xp-src-neighbors.zipfs-orig/373e2835-*.jsonl && echo "BYTE-MATCH"
```
Expected: `BYTE-MATCH`（挂载点透明服务 == golden）。

---

## Self-Review 覆盖核对

- Spec §3 记录语义 → Task 1（record.rs 全覆盖 transcript=uuid/日志/截断/compaction）。
- Spec §4 无损并集 + 分类 + 幂等 → Task 2。
- Spec §4 置信度/推荐 → Task 3。
- Spec §5.4 守卫（真挂载入口/systemd）→ Task 4 + Task 12。
- Spec §5.3 前置门禁 → Task 5；原子替换 + 超集删除许可（通用门）→ Task 6/7/8。
- Spec §5.3 各条目 handler（union/reuse/new/subagents/other）→ Task 7/8。
- Spec §6 memory 透传（路径安全/幂等）→ Task 8。
- Spec §5.3 reconciling 标记 + §7 崩溃续跑/幂等 + meta 收尾 → Task 7/9。
- Spec §5.5 CLI/list/reconciling 显示 → Task 10/11。
- Spec §9 落地 neighbors → Task 13。
- Spec §2 shadow-only / 零丢失铁律 / Global Constraints → 贯穿门禁与超集删除许可。
