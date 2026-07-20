//! Tier 1 故障注入集成测试（docs/05 §4）：`FaultIo` 确定性崩溃模拟器 × `ArchiveUpdater` 双
//! superblock 提交协议。**格式层不变量的穷举证明**，非端到端崩溃恢复的等价替身（§2 边界）：
//! 不覆盖 WriteSession 脏块层 / temp+rename / 目录项 durability（归 crash-test.sh + Tier 2）。
//!
//! 文件级 `#![cfg(feature = "fault-injection")]`：不带 feature 的那趟 `cargo test` 整文件消失，
//! 免 `use scrollz::blockio::FaultIo` 找不到符号而红。运行：`cargo test --features fault-injection`。
#![cfg(feature = "fault-injection")]

use std::io::{Cursor, Write};
use scrollz::archive::{
    ArchiveReader, ArchiveUpdater, ArchiveWriter, DATA_START, SB_A_OFFSET, SB_B_OFFSET, SB_LEN,
};
use scrollz::blockio::FaultIo;

/// 建一个 1 块基线 archive（seq0 两槽，块内容 V0），返回字节。
fn base_archive() -> Vec<u8> {
    let cur = Cursor::new(Vec::new());
    let mut w = ArchiveWriter::new(cur, 64).unwrap();
    w.append_block(b"V0V0V0V0", false, 8).unwrap();
    w.finish().unwrap().into_inner()
}

/// 把崩溃镜像字节写临时文件，经**现有** `ArchiveReader::open` 打开（独立 oracle：复用生产 reader，
/// 不靠被测 commit 返回值或 reader 自洽自证，docs/05 §4）。
fn open_mirror(bytes: &[u8]) -> std::io::Result<ArchiveReader> {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(bytes).unwrap();
    tmp.flush().unwrap();
    ArchiveReader::open(tmp.path())
}

/// commit 的注入点（按语义 offset 区间 / sync 次序调度，非裸写序号，docs/05 §4 防脆弱）。
#[derive(Clone, Copy, Debug)]
enum Inject {
    /// index 写（数据区）EIO —— barrier 1 区域写（弱：非活跃槽天然没污染）。
    IndexWrite,
    /// barrier 1 sync EIO。
    Barrier1Sync,
    /// 非活跃 SB 槽写 EIO —— barrier 2 区域写（强：index 已 durable、SB 未落，须回落上一版）。
    SbWrite,
    /// barrier 2 sync EIO（强）。
    Barrier2Sync,
}

/// 对每个注入点：先成功提交 v1（建立「上一已提交版」），第二次提交武装注入 → 断言 ① commit 返
/// Err（非静默吞）② 最悲观崩溃镜像（durable）开成 v1（半截 SB 被 sb_crc 拒、活跃槽回落）。
fn run_eio_point(point: Inject) {
    let io = FaultIo::from_bytes(base_archive());
    let mut up = ArchiveUpdater::from_io(io.clone()).unwrap();

    // 成功提交 v1（seq1，内容 V1），建立崩溃回落基准。
    up.set_block(0, b"V1V1V1V1", false, 8).unwrap();
    up.commit().expect("首次 commit 应成功（无注入）");
    assert_eq!(
        read_block0(&io.crash_with_mask(0)),
        b"V1V1V1V1",
        "首次提交后 durable 应为 v1"
    );

    // 第二次提交：写入 v2，提交前武装注入。
    up.set_block(0, b"V2V2V2V2", false, 8).unwrap();
    match point {
        Inject::IndexWrite => io.fail_write_in(DATA_START, u64::MAX),
        Inject::Barrier1Sync => io.fail_sync_in(1),
        Inject::SbWrite => io.fail_write_in(SB_A_OFFSET, SB_B_OFFSET + SB_LEN),
        Inject::Barrier2Sync => io.fail_sync_in(2),
    }
    let res = up.commit();
    assert!(
        res.is_err(),
        "{point:?}：注入应使 commit 返回 Err（非静默吞）"
    );

    // 崩溃后最悲观镜像（mask=0 = durable）：开成上一已提交版 v1，绝不半截 v2、绝不报损坏。
    let mirror = io.crash_with_mask(0);
    let r = open_mirror(&mirror).expect("崩溃镜像应回落为一致版本，不报损坏");
    assert_eq!(
        r.read_block(0).unwrap().unwrap().0,
        b"V1V1V1V1",
        "{point:?}：失败提交后应回落 v1（双 SB 防『SB 先于其数据生效』）"
    );
}

/// 读镜像块 0 的存储字节（断言用）。
fn read_block0(bytes: &[u8]) -> Vec<u8> {
    open_mirror(bytes)
        .expect("镜像应可开为某一致版本")
        .read_block(0)
        .unwrap()
        .unwrap()
        .0
}

#[test]
fn eio_index_write_failure_falls_back_to_previous_version() {
    run_eio_point(Inject::IndexWrite);
}

#[test]
fn eio_barrier1_sync_failure_falls_back_to_previous_version() {
    run_eio_point(Inject::Barrier1Sync);
}

#[test]
fn eio_sb_slot_write_failure_falls_back_to_previous_version() {
    run_eio_point(Inject::SbWrite);
}

#[test]
fn eio_barrier2_sync_failure_falls_back_to_previous_version() {
    run_eio_point(Inject::Barrier2Sync);
}

// ----- 撕裂写（512B 对齐部分写，docs/05 §4 / 任务 2.3）-----

/// 建一个 `n` 块基线 archive（每块 1 字节 verbatim "x"，使 index = n*24 字节可超 512B）。
fn base_archive_n(n: usize) -> Vec<u8> {
    let cur = Cursor::new(Vec::new());
    let mut w = ArchiveWriter::new(cur, 64).unwrap();
    for _ in 0..n {
        w.append_block(b"x", true, 1).unwrap();
    }
    w.finish().unwrap().into_inner()
}

#[test]
fn torn_index_write_512_aligned_failure_falls_back_or_reports_corruption_no_silent_misread() {
    // 25 块 → index = 600B（> 512B 扇区）。撕裂 index 写（只落前 512B），其余 commit 步骤正常。
    // 崩溃后：新 SB（seq2）指向的 index 区被截断（尾部未达盘）→ append-only 下 index 尾部越界，reader
    // 经 bounds 拒该槽（index_crc 为更内层兜底）→ 回落上一已提交版 v1（或 fail-closed 报损），**绝不**
    // 把半截 v2 当合法读出。
    let io = FaultIo::from_bytes(base_archive_n(25));
    let mut up = ArchiveUpdater::from_io(io.clone()).unwrap();

    // 成功提交 v1（块0 = "V1"）。
    up.set_block(0, b"V1", true, 25).unwrap();
    up.commit().expect("首次 commit 应成功");
    let v1_block0 = read_block0(&io.crash_with_mask(0));
    assert_eq!(&v1_block0, b"V1");

    // 第二次提交：写块0 = "V2"，撕裂 index 写（数据区，第一条 commit 内数据区写 = index）。
    up.set_block(0, b"V2", true, 25).unwrap();
    io.tear_write_in(DATA_START, u64::MAX, 512); // 只落前 512B（< 600B index）
    let _ = up.commit(); // 撕裂写本身静默成功（部分落盘），不必返 Err。

    // 崩溃镜像（durable）：撕裂的 v2 index 不完整 → reader 必回落 v1 或 fail-closed，绝不静默错读。
    let mirror = io.crash_with_mask(0);
    match open_mirror(&mirror) {
        Ok(r) => assert_eq!(
            r.read_block(0).unwrap().unwrap().0,
            v1_block0,
            "撕裂 index 的 v2 槽应被拒，回落 v1（绝不静默错读半截 v2）"
        ),
        Err(e) => assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
            ),
            "若不回落则须 fail-closed（InvalidData/UnexpectedEof，spec §10 不钉具体 kind）：{e}"
        ),
    }
}

// ----- 穷举崩溃点 durability + fail-closed（带外 oracle，docs/05 §4 / 任务 2.4）-----

/// 建一个 0 块 archive（seq0 两槽，无 journal），返回字节。
fn empty_archive() -> Vec<u8> {
    let cur = Cursor::new(Vec::new());
    let w = ArchiveWriter::new(cur, 64).unwrap();
    w.finish().unwrap().into_inner()
}

/// 测试侧独立解析镜像的活跃 seq（带外 oracle：经 pub `parse_superblock`，**不**经 commit 返回值
/// 或 reader 自洽，docs/05 §4）。两槽取较大有效 seq。
fn active_seq_of(bytes: &[u8]) -> u64 {
    let a = scrollz::archive::parse_superblock(&bytes[SB_A_OFFSET as usize..]);
    let b = scrollz::archive::parse_superblock(&bytes[SB_B_OFFSET as usize..]);
    match (a, b) {
        (Some(x), Some(y)) => x.seq.max(y.seq),
        (Some(x), None) => x.seq,
        (None, Some(y)) => y.seq,
        (None, None) => panic!("两槽皆不可解析"),
    }
}

#[test]
fn exhaustive_crash_points_durability_and_fail_closed_out_of_band_oracle() {
    // 固定工作负载：0 块 archive 起，逐行 append_journal + commit_journal（各 fsync），仿 crash-test.sh。
    let io = FaultIo::from_bytes(empty_archive());
    let mut up = ArchiveUpdater::from_io(io.clone()).unwrap();

    // 逻辑内容 expected 历史快照（仿 append_tail_buffer）：expected[k] = 前 k 行拼接。
    let lines: Vec<Vec<u8>> = (0..20).map(|i| format!("line{i}\n").into_bytes()).collect();
    let mut expected: Vec<Vec<u8>> = vec![Vec::new()]; // 0 行 = 空内容
    let mut acc: Vec<u8> = Vec::new();
    for line in &lines {
        up.append_journal(line).unwrap();
        acc.extend_from_slice(line);
        up.set_size(acc.len() as u64);
        up.commit_journal().unwrap();
        expected.push(acc.clone());
    }

    // 带外台账：FaultIo 每次 sync 成功后记录的 durable 快照阶梯（= 穷举的悲观崩溃点）。
    let ladder = io.history();
    assert!(ladder.len() >= lines.len(), "每行至少推进一次 durable 状态");
    let mut acked = 0u64;
    for (k, snap) in ladder.iter().enumerate() {
        // ② 活跃 seq 单调不降（durability：已 fsync-acked 版本绝不丢失），独立 parse 得出、不靠 commit。
        let seq = active_seq_of(snap);
        assert!(
            seq >= acked,
            "崩溃点{k}：活跃 seq {seq} < 已 acked {acked}（durability 退化）"
        );
        acked = seq;
        // ① reader 读出内容 ∈ {历史 expected 前缀}（不只是 reader 自洽——自洽的坏数据照样自洽）。
        let r = open_mirror(snap).unwrap_or_else(|e| panic!("崩溃点{k} 镜像应可开为一致版本：{e}"));
        let content = r.read_tail().unwrap().unwrap_or_default();
        assert!(
            expected.contains(&content),
            "崩溃点{k}：reader 读出内容不在任何历史 expected 前缀中：{content:?}"
        );
    }

    // 终态 = 全部 20 行。
    let r = open_mirror(ladder.last().unwrap()).unwrap();
    assert_eq!(
        r.read_tail().unwrap().unwrap_or_default(),
        acc,
        "终态应重放出全部 20 行"
    );
}

// ----- 脏页乱序子集 × barrier 失败交叉（核心，docs/05 §4 / 任务 2.5）-----
//
// 单独「所有 barrier 成功 + 子集枚举」在 sync 屏障语义下**恰好排除**唯一危险 case（SB 落盘而其
// 依赖 index 没落）：barrier 1 一旦成功就强制 index durable，该组合自动出局。要真测到它，须把
// 第二次 commit 的两个 barrier sync **软化**（返 Ok 但不合并 → index/SB 全留 dirty、可乱序回写），
// 再枚举崩溃子集——子集 {只含 SB 写、不含 index 写} 即「SB 进 durable、index 未进」的危险镜像。
// 断言任一子集 reader 读出 ∈ {历史版本} 或 fail-closed，**绝不**让 SB 先于其依赖数据生效。

#[test]
fn dirty_page_reordered_subset_x_barrier_softened_double_barrier_truly_ordered() {
    let io = FaultIo::from_bytes(base_archive());
    let mut up = ArchiveUpdater::from_io(io.clone()).unwrap();

    // 成功提交 v1（块0 = "V1V1V1V1"）。
    up.set_block(0, b"V1V1V1V1", false, 8).unwrap();
    up.commit().expect("首次 commit 应成功");
    let v1 = read_block0(&io.crash_with_mask(0));
    assert_eq!(&v1, b"V1V1V1V1");

    // 第二次提交：写块0 = "V2V2V2V2"（改块集 → commit 重写 index），软化两个 barrier sync →
    // 提交后 [block, index, SB] 全留 dirty（barrier2↔barrier2 窗口），durable 仍 = v1。
    up.set_block(0, b"V2V2V2V2", false, 8).unwrap();
    io.soften_syncs(2);
    up.commit().expect("commit 仍推进（软 sync 返 Ok）");
    let n = io.dirty_count();
    assert!(
        (1..=12).contains(&n),
        "乱序窗口大小 {n} 应在剪枝上限内（N≤12）"
    );

    // 穷举乱序子集：任一崩溃镜像 reader 读出 ∈ {v1, v2} 或 fail-closed，绝不静默错读 Frankenstein。
    for mask in 0u64..(1u64 << n) {
        let mirror = io.crash_with_mask(mask);
        let Ok(r) = open_mirror(&mirror) else {
            continue; // open fail-closed 可接受（绝不静默错读）。
        };
        match r.read_block(0) {
            Ok(Some((bytes, _))) => assert!(
                bytes == v1 || bytes == b"V2V2V2V2",
                "mask {mask:#b}：读出非历史一致版本 {bytes:?}（SB 先于其数据生效？）"
            ),
            Ok(None) => {} // 块缺失（可接受，不静默错读）。
            Err(e) => assert!(
                matches!(
                    e.kind(),
                    std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
                ),
                "mask {mask:#b}：read_block 须 fail-closed（InvalidData/UnexpectedEof）"
            ),
        }
    }

    // 专项钉死 §8.3 核心：只持久化 SB 写（最后一条 dirty）、不持久化 index 写 → reader 必回落 v1
    // 或 fail-closed，绝不把指向未落盘 index 的 SB 当合法生效。
    let sb_only = 1u64 << (n - 1); // 窗口写序 [..., index, SB]，SB 恒最后一条。
    match open_mirror(&io.crash_with_mask(sb_only)) {
        Ok(r) => {
            // Ok(Some) 必为 v1（回落）；Ok(None)/Err = fail-closed，亦可接受。
            if let Ok(Some((bytes, _))) = r.read_block(0) {
                assert_eq!(
                    bytes, v1,
                    "SB 进 durable 而 index 未进 → 必回落 v1（双 SB 防『SB 先于其数据生效』）"
                );
            }
        }
        Err(e) => assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
            ),
            "或 fail-closed 报损（InvalidData/UnexpectedEof）：{e}"
        ),
    }
}

// ----- 双 SB 最根本不变量：commit 写非活跃槽 + sb_crc 拒损坏槽（评审 C1/C2 补强）-----

/// 活跃（seq 较大、能 parse）槽在镜像中的字节偏移（测试侧独立得出）。
fn active_slot_offset(bytes: &[u8]) -> usize {
    let a = scrollz::archive::parse_superblock(&bytes[SB_A_OFFSET as usize..]);
    let b = scrollz::archive::parse_superblock(&bytes[SB_B_OFFSET as usize..]);
    match (a, b) {
        (Some(x), Some(y)) if y.seq > x.seq => SB_B_OFFSET as usize,
        (Some(_), _) => SB_A_OFFSET as usize,
        (None, Some(_)) => SB_B_OFFSET as usize,
        (None, None) => panic!("两槽皆不可解析"),
    }
}

#[test]
fn commit_writes_inactive_slot_corrupt_active_slot_falls_back_to_last_committed_not_earlier() {
    // 双 superblock 存在的根本理由：commit 把新版本写**非活跃槽**，旧槽完好直到 barrier 2 原子翻转。
    // 连提交 v1、v2（均成功），再损坏活跃（seq2=v2）槽的 seq 字节 → reader 必经 sb_crc 拒该槽并回落
    // **上一已提交版 v1**（在另一槽保命），绝非更早的 base。
    //   - 若 commit 误写**活跃**槽：v1 会被 v2 覆盖在同一槽，另一槽仍是 base → 回落到 base ≠ v1 → 抓住。
    //   - 若 sb_crc 校验被关：损坏槽（seq 被异或成巨值）被当活跃 → 读出 v2 ≠ v1 → 抓住。
    // 一举钉死「写非活跃槽」(C1) 与「sb_crc 拒损坏槽」(C2) 两条 spec §4/§7 明文不变量。
    let io = FaultIo::from_bytes(base_archive()); // base 块 = "V0V0V0V0"
    let mut up = ArchiveUpdater::from_io(io.clone()).unwrap();
    up.set_block(0, b"V1V1V1V1", false, 8).unwrap();
    up.commit().expect("v1 提交应成功");
    up.set_block(0, b"V2V2V2V2", false, 8).unwrap();
    up.commit().expect("v2 提交应成功");

    // 完整 v2 durable 镜像：活跃槽 seq2=v2，另一槽 seq1=v1。损坏活跃槽 seq 低字节（异或 0xFF →
    // sb_crc 必失配；若 crc 被关则 seq 变巨值被选中读出 v2）。
    let mut mirror = io.crash_with_mask(0);
    let off = active_slot_offset(&mirror);
    mirror[off + 4] ^= 0xFF; // seq 字段首字节（magic 占 [0,4)，seq 起于 [4,12)）

    let r = open_mirror(&mirror).expect("活跃槽 sb_crc 损坏应回落另一槽，不报损坏");
    assert_eq!(
        r.read_block(0).unwrap().unwrap().0,
        b"V1V1V1V1",
        "回落必为上一已提交版 v1（commit 写非活跃槽，v1 在另一槽保命；sb_crc 拒损坏的 v2 槽）"
    );
}
