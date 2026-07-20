//! 决策推荐：复用 merge 的 Evidence，只叠置信度 + 措辞，不另采证（评审 I2）。
use crate::reconcile::merge::{Decision, MergeResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    UnionIntoBase,
    KeepSeparate,
    PassthroughRestore,
    KeepBoth,
}

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
            confidence: if r.conflicts.is_empty() {
                Confidence::High
            } else {
                Confidence::Medium
            },
            rationale: format!(
                "transcript uuid 有交集（overlap={}），uuid 并集无丢失",
                r.evidence.uuid_overlap
            ),
        },
        Decision::SuspectReuse => Recommendation {
            action: Action::KeepSeparate,
            confidence: Confidence::Low,
            rationale: "transcript uuid 全disjoint、无 compaction 桥、时间窗不相交 → 疑 session-id 重用；默认隔离保两份，可经确认改并入".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::merge::session_merge;

    fn rec(base: &str, inc: &str) -> Recommendation {
        recommend(&session_merge(base, inc))
    }

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
