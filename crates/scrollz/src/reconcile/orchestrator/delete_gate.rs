//! 删除许可门（唯一删除入口）：durable 超集 + live 自快照未变。
use std::io;

use super::*;

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

#[cfg(test)]
mod tests {
    use super::*;

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

}
