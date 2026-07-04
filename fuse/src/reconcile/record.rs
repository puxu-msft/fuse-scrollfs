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

pub fn record_parent_uuid(v: &serde_json::Value) -> Option<&str> {
    v.get("parentUuid").and_then(|u| u.as_str())
}

pub fn record_timestamp(v: &serde_json::Value) -> Option<&str> {
    v.get("timestamp").and_then(|t| t.as_str())
}

pub fn is_compact_summary(v: &serde_json::Value) -> bool {
    v.get("isCompactSummary")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
}

pub fn classify_record(line: &str) -> (RawRecord, RecordKind) {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => {
            let kind = match record_uuid(&v) {
                Some(u) => RecordKind::Transcript {
                    uuid: u.to_string(),
                },
                None => RecordKind::NoUuid,
            };
            (
                RawRecord {
                    line: line.to_string(),
                    json: Some(v),
                },
                kind,
            )
        }
        Err(_) => {
            // 截断探测：非空、以 `{` 起头、未闭合 → 视为截断半行。
            let t = line.trim();
            let truncated = t.starts_with('{') && !t.ends_with('}');
            (
                RawRecord {
                    line: line.to_string(),
                    json: None,
                },
                RecordKind::Unparsable { truncated },
            )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_record_keyed_by_uuid_any_type() {
        // attachment/system 也带 uuid → 必须判 Transcript（不按 type 白名单）。
        for ty in ["assistant", "user", "attachment", "system"] {
            let line =
                format!(r#"{{"type":"{ty}","uuid":"u1","timestamp":"2026-06-30T10:00:00.000Z"}}"#);
            let (_, kind) = classify_record(&line);
            assert!(
                matches!(kind, RecordKind::Transcript { .. }),
                "type={ty} 应判 Transcript"
            );
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
        let v: serde_json::Value = serde_json::from_str(
            r#"{"type":"user","isCompactSummary":true,"uuid":"u","parentUuid":"p"}"#,
        )
        .unwrap();
        assert!(is_compact_summary(&v));
        let v2: serde_json::Value = serde_json::from_str(r#"{"type":"summary"}"#).unwrap();
        assert!(
            !is_compact_summary(&v2),
            "type:summary 不是 compaction 标记"
        );
    }
}
