//! 无损并集合并核。带 uuid 记录按 uuid 并集（同 uuid 取完整更长者）；无 uuid 记录整行去重并集；
//! 坏行 verbatim。稳定全序（timestamp→原始行号）。分类只驱动 advisor 推荐，永不有损。

use crate::reconcile::record::{
    classify_record, is_compact_summary, record_parent_uuid, record_timestamp, RawRecord,
    RecordKind,
};
use std::collections::{BTreeMap, HashSet};

/// **base 侧超集铁律门（评审 R-C1，§5.3 步4 的 base 半边）**：`merged` 是否覆盖 `base` 的全部内容。
///
/// 覆盖判据**按记录语义而非裸行**（关键）：
/// - base 的每个 **transcript uuid** 必 ∈ merged 的 uuid 集（uuid 级）——同 uuid「取更长者」是
///   §4.1 许可的无损收敛（短者为崩溃截断），uuid 仍在即视为覆盖，故裸行比对会**误判**过严。
/// - base 的每条 **no-uuid 行 / 坏行**必 ∈ merged 的 no-uuid 行集（行级去重语义）。
///
/// 返回 false = merged 丢了 base 的某记录（疑合并核缺陷）。删除门只证 incoming ⊆ merged，
/// 本函数补 base ⊆ merged，二者合起来才是设计要求的**双向超集**——不过则调用方中止、不覆盖 orig。
pub fn base_covered_by_merged(base: &str, merged: &str) -> bool {
    let mut merged_uuids: HashSet<String> = HashSet::new();
    let mut merged_lines: HashSet<&str> = HashSet::new();
    for line in merged.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match classify_record(line).1 {
            RecordKind::Transcript { uuid } => {
                merged_uuids.insert(uuid);
            }
            _ => {
                merged_lines.insert(line);
            }
        }
    }
    for line in base.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match classify_record(line).1 {
            RecordKind::Transcript { uuid } => {
                if !merged_uuids.contains(&uuid) {
                    return false;
                }
            }
            _ => {
                if !merged_lines.contains(line) {
                    return false;
                }
            }
        }
    }
    true
}

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
        if lo.map(|x| t < x).unwrap_or(true) {
            lo = Some(t);
        }
        if hi.map(|x| t > x).unwrap_or(true) {
            hi = Some(t);
        }
    }
    Some((lo?.to_string(), hi?.to_string()))
}

pub fn session_merge(base: &str, incoming: &str) -> MergeResult {
    let base_recs: Vec<_> = base
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(classify_record)
        .collect();
    let inc_recs: Vec<_> = incoming
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(classify_record)
        .collect();

    let uuid_of = |k: &RecordKind| match k {
        RecordKind::Transcript { uuid } => Some(uuid.clone()),
        _ => None,
    };
    let base_uuids: std::collections::BTreeSet<String> =
        base_recs.iter().filter_map(|(_, k)| uuid_of(k)).collect();
    let inc_uuids: std::collections::BTreeSet<String> =
        inc_recs.iter().filter_map(|(_, k)| uuid_of(k)).collect();
    let overlap = base_uuids.intersection(&inc_uuids).count();

    let has_bridge = inc_recs.iter().any(|(r, _)| {
        r.json.as_ref().is_some_and(|v| {
            // 桥 = isCompactSummary 且 parentUuid 指向 base 的某记录（评审 I-5：仅布尔会误并无关重用会话）。
            is_compact_summary(v) && record_parent_uuid(v).is_some_and(|p| base_uuids.contains(p))
        })
    });
    let inc_only_transcript = inc_uuids.difference(&base_uuids).count();

    // ── 无损并集：uuid 去重（同 uuid 取完整更长者），无 uuid 整行去重，坏行 verbatim ──
    let mut by_uuid: BTreeMap<String, Item> = BTreeMap::new();
    let mut seen_lines: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut extras: Vec<Item> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut truncated = 0usize;
    let mut ord = 0usize;

    // 评审 I-1：last_ts 必须跨 base→incoming 两次 ingest 保持，否则 incoming 前导无 ts 记录
    // 会重置为 None、被天然序 hoist 到文件头、越过 base 真实 transcript。声明在闭包外 =
    // incoming 前导无 ts 记录继承 base 末尾 ts（append 续写语义），排在其后。
    let mut last_ts: Option<String> = None;
    let mut ingest = |recs: &[(RawRecord, RecordKind)]| {
        // 评审 I-2：无 timestamp 的记录（日志/元数据/坏行）继承**前一条**记录的 ts，避免 Option::None
        // 天然序把它们整体 hoist 到文件头、破坏交织。稳定全序 = (继承后的 ts, 全局序号)。
        for (r, k) in recs {
            let own_ts = r
                .json
                .as_ref()
                .and_then(|v| record_timestamp(v).map(|s| s.to_string()));
            if own_ts.is_some() {
                last_ts = own_ts.clone();
            }
            let ts = own_ts.or_else(|| last_ts.clone());
            match k {
                RecordKind::Transcript { uuid } => match by_uuid.get(uuid) {
                    Some(prev) if prev.line.len() >= r.line.len() => {
                        if prev.line != r.line {
                            // 评审 R-I2：同 uuid 内容分叉，保留更长者（§4.1，短者疑崩溃截断）。
                            // conflicts 携**被丢的落败整行**（非仅 uuid），使其可从报告复原（零丢失）。
                            conflicts.push(format!("uuid={uuid} dropped={}", r.line));
                        }
                    }
                    Some(prev) => {
                        conflicts.push(format!("uuid={uuid} dropped={}", prev.line));
                        by_uuid.insert(
                            uuid.clone(),
                            Item {
                                ts,
                                ord,
                                line: r.line.clone(),
                            },
                        );
                    }
                    None => {
                        by_uuid.insert(
                            uuid.clone(),
                            Item {
                                ts,
                                ord,
                                line: r.line.clone(),
                            },
                        );
                    }
                },
                RecordKind::NoUuid => {
                    if seen_lines.insert(r.line.clone()) {
                        extras.push(Item {
                            ts,
                            ord,
                            line: r.line.clone(),
                        });
                    }
                }
                RecordKind::Unparsable { truncated: tr } => {
                    if *tr {
                        truncated += 1;
                    }
                    if seen_lines.insert(r.line.clone()) {
                        extras.push(Item {
                            ts,
                            ord,
                            line: r.line.clone(),
                        });
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
        base_ts_range: ts_range(
            base_recs
                .iter()
                .filter_map(|(r, _)| r.json.as_ref().and_then(record_timestamp)),
        ),
        incoming_ts_range: ts_range(
            inc_recs
                .iter()
                .filter_map(|(r, _)| r.json.as_ref().and_then(record_timestamp)),
        ),
        truncated_lines: truncated,
    };

    MergeResult {
        decision,
        evidence,
        merged_lines,
        conflicts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(u: &str, t: &str) -> String {
        format!(r#"{{"type":"assistant","uuid":"{u}","parentUuid":null,"timestamp":"{t}"}}"#)
    }

    #[test]
    fn base_covered_gate_uuid_aware() {
        // 评审 R-C1：base 覆盖门按记录语义（uuid 级 + no-uuid 行级）。
        // 1) base uuid 丢失 → false。
        let base = format!("{}\n", ts("u1", "2026-06-27T00:00:00.000Z"));
        let merged_missing = format!("{}\n", ts("u2", "2026-06-27T00:00:00.000Z"));
        assert!(
            !base_covered_by_merged(&base, &merged_missing),
            "base 的 uuid u1 不在 merged → 未覆盖"
        );
        // 2) base no-uuid 行丢失 → false。
        let base_log = "{\"type\":\"ai-title\",\"aiTitle\":\"X\"}\n";
        assert!(
            !base_covered_by_merged(base_log, ""),
            "base 日志行不在 merged → 未覆盖"
        );
        // 3) 同 uuid「取更长者」不应误判过严：base 短变体、merged 保 uuid 的长变体 → 覆盖成立。
        let base_short = "{\"uuid\":\"u1\",\"x\":\"AA\"}\n";
        let merged_long = "{\"uuid\":\"u1\",\"x\":\"BBBBBB\"}\n";
        assert!(
            base_covered_by_merged(base_short, merged_long),
            "同 uuid 取更长者是 §4.1 许可收敛，uuid 仍在即视为覆盖（不得误判过严）"
        );
        // 4) 正常超集 → 覆盖。
        let m = format!("{}\n{}\n", ts("u1", "t"), base_log.trim());
        assert!(base_covered_by_merged(&base, &m) && base_covered_by_merged(base_log, &m));
    }

    #[test]
    fn empty_string_uuid_records_are_not_folded() {
        // 评审 R-I1：`"uuid":""`（空串）曾被当 Transcript 键 `""`，所有空串 uuid 记录共键、
        // 只留最长者，其余 distinct 静默丢。空串/非字符串 uuid 应降级 NoUuid（整行去重全保）。
        let base = format!(
            "{}\n{}\n",
            r#"{"type":"x","uuid":"","a":"AAAA"}"#, // 空串 uuid，distinct 内容
            r#"{"type":"x","uuid":"","a":"BBBBBBBB"}"#
        );
        let incoming = String::new();
        let r = session_merge(&base, &incoming);
        assert!(
            r.merged_lines.iter().any(|l| l.contains("AAAA")),
            "较短的空 uuid 记录必须保留（不折叠）"
        );
        assert!(
            r.merged_lines.iter().any(|l| l.contains("BBBBBBBB")),
            "较长的空 uuid 记录也保留"
        );
    }

    #[test]
    fn log_only_keeps_base_transcript_and_unions_logs() {
        // 373e2835 类：base 有正文，incoming 只有日志记录 → 正文全留 + 日志并入。
        let base = format!(
            "{}\n{}\n",
            ts("u1", "2026-06-27T12:00:00.000Z"),
            r#"{"type":"ai-title","aiTitle":"旧标题","sessionId":"s"}"#
        );
        let incoming = format!(
            "{}\n{}\n",
            r#"{"type":"ai-title","aiTitle":"新标题","sessionId":"s"}"#,
            r#"{"type":"mode","mode":"normal","sessionId":"s"}"#
        );
        let r = session_merge(&base, &incoming);
        assert_eq!(r.decision, Decision::LogOnly);
        // 反折叠：新旧两个 ai-title 都在（不是只留最新一条）。
        assert!(r.merged_lines.iter().any(|l| l.contains("旧标题")));
        assert!(r.merged_lines.iter().any(|l| l.contains("新标题")));
        assert!(
            r.merged_lines.iter().any(|l| l.contains("u1")),
            "base 正文保留"
        );
    }

    #[test]
    fn many_last_prompt_all_preserved_not_folded() {
        // 反 newest-wins：3 条不同 last-prompt 必须全留。
        let base = format!(
            "{}\n{}\n",
            r#"{"type":"last-prompt","lastPrompt":"p1","leafUuid":"a"}"#,
            r#"{"type":"last-prompt","lastPrompt":"p2","leafUuid":"b"}"#
        );
        let incoming = format!(
            "{}\n",
            r#"{"type":"last-prompt","lastPrompt":"p3","leafUuid":"c"}"#
        );
        let r = session_merge(&base, &incoming);
        let n = r
            .merged_lines
            .iter()
            .filter(|l| l.contains("last-prompt"))
            .count();
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
        let incoming = format!(
            "{}\n{}\n",
            r#"{"type":"user","isCompactSummary":true,"uuid":"b1","parentUuid":"a1","timestamp":"2026-06-30T10:00:00.000Z"}"#,
            ts("b2", "2026-06-30T10:01:00.000Z")
        );
        let r = session_merge(&base, &incoming);
        assert_eq!(r.decision, Decision::CompactionBridge);
    }

    #[test]
    fn incremental_unions_by_uuid_no_dup() {
        let base = format!(
            "{}\n{}\n",
            ts("u1", "2026-06-30T10:00:00.000Z"),
            ts("u2", "2026-06-30T10:01:00.000Z")
        );
        let incoming = format!(
            "{}\n{}\n",
            ts("u2", "2026-06-30T10:01:00.000Z"),
            ts("u3", "2026-06-30T10:02:00.000Z")
        );
        let r = session_merge(&base, &incoming);
        assert_eq!(r.decision, Decision::Incremental);
        let uu: Vec<_> = r
            .merged_lines
            .iter()
            .filter(|l| l.contains(r#""uuid""#))
            .collect();
        assert_eq!(uu.len(), 3, "u1/u2/u3 各一次，u2 不重复");
    }

    #[test]
    fn merge_is_idempotent() {
        let base = format!("{}\n", ts("u1", "2026-06-30T10:00:00.000Z"));
        let incoming = format!("{}\n", ts("u2", "2026-06-30T10:01:00.000Z"));
        let once = session_merge(&base, &incoming).merged_lines.join("\n");
        let twice = session_merge(&once, &incoming).merged_lines.join("\n");
        assert_eq!(once, twice, "merge(merge(b,i),i) == merge(b,i)");
    }

    #[test]
    fn no_uuid_record_not_hoisted_to_front() {
        // 评审 I-2：无 ts 的日志记录继承前一条 transcript 的 ts，不被 Option::None 天然序提到文件头。
        let base = format!(
            "{}\n{}\n{}\n",
            ts("u1", "2026-06-30T10:00:00.000Z"),
            r#"{"type":"mode","mode":"normal"}"#,
            ts("u2", "2026-06-30T10:05:00.000Z")
        );
        let r = session_merge(&base, "");
        let i_u1 = r
            .merged_lines
            .iter()
            .position(|l| l.contains("u1"))
            .unwrap();
        let i_mode = r
            .merged_lines
            .iter()
            .position(|l| l.contains(r#""mode""#))
            .unwrap();
        assert!(i_mode > i_u1, "mode 应留在 u1 之后，而非被 hoist 到头部");
    }

    #[test]
    fn incoming_leading_no_ts_record_not_hoisted_across_seam() {
        // 评审 I-1：incoming 前导无 ts 记录必须继承 base **末尾** ts（append 续写语义），
        // 排在 base 真实 transcript 之后，而非被 Option::None < Some hoist 到文件头。
        let base = format!(
            "{}\n{}\n",
            ts("u1", "2026-06-30T10:00:00.000Z"),
            ts("u2", "2026-06-30T10:05:00.000Z")
        );
        let incoming = format!("{}\n", r#"{"type":"mode","mode":"normal"}"#); // 无 ts
        let r = session_merge(&base, &incoming);
        let i_u2 = r
            .merged_lines
            .iter()
            .position(|l| l.contains("u2"))
            .unwrap();
        let i_mode = r
            .merged_lines
            .iter()
            .position(|l| l.contains(r#""mode""#))
            .unwrap();
        assert!(
            i_mode > i_u2,
            "incoming 前导无 ts 记录应继承 base 末尾 ts、排在其后，而非 hoist 到头部"
        );
    }

    #[test]
    fn same_uuid_conflict_takes_longer_complete() {
        // 崩溃截断：同 uuid，一份完整一份短 → 取完整更长者 + 记 conflict。
        let full = r#"{"type":"assistant","uuid":"u1","timestamp":"2026-06-30T10:00:00.000Z","message":"complete"}"#;
        let short = r#"{"type":"assistant","uuid":"u1","timestamp":"2026-06-30T10:00:00.000Z"}"#;
        let r = session_merge(&format!("{short}\n"), &format!("{full}\n"));
        assert!(r.merged_lines.iter().any(|l| l.contains("complete")));
        assert_eq!(
            r.merged_lines
                .iter()
                .filter(|l| l.contains(r#""u1""#))
                .count(),
            1
        );
        assert!(!r.conflicts.is_empty(), "同 uuid 冲突记入报告");
        // 评审 R-I2：conflicts 须携被丢的落败整行，使其可从报告复原（零丢失）。
        assert!(
            r.conflicts
                .iter()
                .any(|c| c.contains("dropped=") && c.contains("u1")),
            "冲突报告应含被丢落败行内容：{:?}",
            r.conflicts
        );
    }
}
