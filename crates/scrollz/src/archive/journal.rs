//! 尾日志记录：未封尾块的原始字节增量（docs/04 §2.2/§4.4，TDD §8.2）
//!
//! 每次 fsync 把「自上次 fsync 以来新追加的原始字节」作为一条记录 append 到尾日志区。
//! 记录格式：[rec_len(4) | rec_crc(4) | raw_bytes(rec_len)]。重放 = 顺序拼接全部完整记录的
//! payload（= 未封尾块的全量原始字节）。遇不完整/损坏记录即停并报告干净消费字节数；调用方据
//! `consumed == 输入长度` 区分完整的已提交 journal 与损坏前缀。rec_len 先与剩余字节 bounds 校验，
//! 防越界/OOM（H1）。

use super::format::crc32;

/// 尾日志记录头长度：rec_len(4) + rec_crc(4)。
pub const JOURNAL_REC_HEADER_LEN: usize = 8;

/// 序列化一条尾日志记录。`rec_crc = crc32(raw)`。
pub fn serialize_journal_record(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(JOURNAL_REC_HEADER_LEN + raw.len());
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32(raw).to_le_bytes());
    out.extend_from_slice(raw);
    out
}

/// 尾日志重放结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalReplay {
    /// 所有通过 bounds 与 CRC 校验的记录拼接出的原始字节。
    pub plain: Vec<u8>,
    /// 从输入起点连续、完整通过校验的 journal 字节数（含每条记录头）。
    pub consumed: usize,
}

/// 重放尾日志区，返回拼接的原始字节及干净消费字节数。遇不完整（半截头/半截 payload、rec_len
/// 越界）或 rec_crc 不符即停。已提交 SB 只可接受 `consumed == buf.len()`；SB 引用范围以外的 torn
/// tail 不应传入本函数，因而不会被误判为已提交数据损坏。
pub fn replay_journal(buf: &[u8]) -> JournalReplay {
    let mut plain = Vec::new();
    let mut consumed = 0usize;
    while consumed + JOURNAL_REC_HEADER_LEN <= buf.len() {
        let rec_len = u32::from_le_bytes(buf[consumed..consumed + 4].try_into().unwrap()) as usize;
        let rec_crc = u32::from_le_bytes(buf[consumed + 4..consumed + 8].try_into().unwrap());
        let data_start = consumed + JOURNAL_REC_HEADER_LEN;
        // bounds：payload 必须完整落在 buf 内（防越界读 / 巨值 rec_len 截断尾部）。
        let Some(data_end) = data_start.checked_add(rec_len) else {
            break;
        };
        if data_end > buf.len() {
            break;
        }
        let payload = &buf[data_start..data_end];
        if crc32(payload) != rec_crc {
            break;
        }
        plain.extend_from_slice(payload);
        consumed = data_end;
    }
    JournalReplay { plain, consumed }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 尾日志记录编解码 + 重放（docs/04 §8.2，TDD）----

    #[test]
    fn journal_single_record_round_trip() {
        let raw = b"hello world payload";
        let rec = serialize_journal_record(raw);
        assert_eq!(rec.len(), JOURNAL_REC_HEADER_LEN + raw.len());
        assert_eq!(replay_journal(&rec).plain, raw);
    }

    #[test]
    fn journal_multi_record_concat_order_preserved() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&serialize_journal_record(b"line1\n"));
        buf.extend_from_slice(&serialize_journal_record(b"line2\n"));
        buf.extend_from_slice(&serialize_journal_record(b"line3\n"));
        assert_eq!(replay_journal(&buf).plain, b"line1\nline2\nline3\n");
    }

    #[test]
    fn journal_empty_region_empty_records() {
        assert_eq!(replay_journal(&[]).plain, Vec::<u8>::new());
        // 空 payload 记录：合法，贡献 0 字节。
        let rec = serialize_journal_record(b"");
        assert_eq!(rec.len(), JOURNAL_REC_HEADER_LEN);
        assert_eq!(replay_journal(&rec).plain, Vec::<u8>::new());
    }

    #[test]
    fn journal_truncated_header_returns_previous_complete_prefix() {
        let mut buf = serialize_journal_record(b"complete");
        // 追加一段不足 8 字节的半截头（崩溃于写记录头中途）。
        buf.extend_from_slice(&[0u8; 5]);
        assert_eq!(replay_journal(&buf).plain, b"complete");
    }

    #[test]
    fn journal_truncated_payload_returns_previous_complete_prefix() {
        let mut buf = serialize_journal_record(b"first");
        // 第二条：头声明 100 字节但只写 3 字节 payload（半截写）。
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&crc32(&[0u8; 100]).to_le_bytes());
        buf.extend_from_slice(b"abc");
        assert_eq!(
            replay_journal(&buf).plain,
            b"first",
            "rec_len 越界 → 截到上一完整记录"
        );
    }

    #[test]
    fn journal_payload_corrupt_crc_detected_stops() {
        let mut buf = serialize_journal_record(b"good");
        buf.extend_from_slice(&serialize_journal_record(b"willcorrupt"));
        // 翻转第二条 payload 的一个字节（在 buf 末尾区）。
        let last = buf.len() - 1;
        buf[last] ^= 0xFF;
        assert_eq!(
            replay_journal(&buf).plain,
            b"good",
            "第二条 crc 不符 → 停在第一条"
        );
    }

    #[test]
    fn journal_rec_len_huge_value_no_oob_no_panic() {
        // rec_len = u32::MAX，bounds 校验应直接停，不 panic / 不分配。
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(b"x");
        assert_eq!(replay_journal(&buf).plain, Vec::<u8>::new());
    }

    #[test]
    fn journal_replay_reports_exact_clean_consumption() {
        let first = serialize_journal_record(b"first");
        let second = serialize_journal_record(b"second");
        let mut clean = first.clone();
        clean.extend_from_slice(&second);
        let complete = replay_journal(&clean);
        assert_eq!(complete.plain, b"firstsecond");
        assert_eq!(complete.consumed, clean.len());

        let mut corrupt = clean;
        *corrupt.last_mut().unwrap() ^= 0x80;
        let partial = replay_journal(&corrupt);
        assert_eq!(partial.plain, b"first");
        assert_eq!(partial.consumed, first.len());
        assert!(partial.consumed < corrupt.len());
    }
}
