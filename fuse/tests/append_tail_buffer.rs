//! append 优化端到端：开放尾块缓冲在两后端（ShadowStore / ContainerStore）上的重压次数 +
//! 正确性（§1.1）。模拟目标负载「逐行 append 小记录 + 周期 fsync」，断言：
//! 1. 开启优化时，封块（重压）次数 ≈ 满块数，**远少于 append 次数**（旧路径 = 每次 append 重压）。
//! 2. 整文件内容、size 正确（读协调：未封尾块走缓冲）。
//! 3. fsync 后尾块已封、可经 Store 读出。
//! 4. `--no-tail-buffer`（enabled=false）退化为旧路径仍正确（差分覆盖在 model_based）。

use zipfs::core::codec::{decompress, Algo};
use zipfs::core::rmw::CodecParams;
use zipfs::core::wsession::WriteSession;
use zipfs::store::container::ContainerStore;
use zipfs::store::shadow::ShadowStore;
use zipfs::store::{Attr, Store};

const ROOT_INO: u64 = 1;
const CHUNK_SIZE: u32 = 4096;

fn params() -> CodecParams {
    CodecParams {
        algo: Algo::Zstd,
        level: 3,
        dict: None,
    }
}

fn new_attr() -> Attr {
    Attr {
        ino: 0,
        size: 0,
        kind: fuser::FileType::RegularFile,
        perm: 0o644,
        uid: 0,
        gid: 0,
        mtime: std::time::SystemTime::UNIX_EPOCH,
        atime: std::time::SystemTime::UNIX_EPOCH,
        ctime: std::time::SystemTime::UNIX_EPOCH,
        chunk_size: CHUNK_SIZE,
    }
}

/// 与 rwfs 读协调一致的整文件读回：尾块走缓冲，其余走 Store 解压，缺块零填充。
fn read_whole(ws: &WriteSession, store: &dyn Store, ino: u64) -> Vec<u8> {
    let (size, cs) = ws.geometry(store, ino).unwrap();
    let mut out = vec![0u8; size as usize];
    let cs = cs as u64;
    let nblocks = size.div_ceil(cs);
    for idx in 0..nblocks {
        let start = (idx * cs) as usize;
        let plain = if let Some(p) = ws.read_tail_block(idx) {
            p
        } else if let Some(b) = store.get_block(ino, idx).unwrap() {
            decompress(&b.bytes, Algo::Zstd, b.stored_verbatim).unwrap()
        } else {
            continue;
        };
        let end = (start + plain.len()).min(out.len());
        if start < end {
            out[start..end].copy_from_slice(&plain[..end - start]);
        }
    }
    out
}

/// 在某后端上跑「逐行 append + 周期 fsync」负载，返回 (期望内容, 封块次数)。
fn run_append_workload(
    ws: &mut WriteSession,
    store: &dyn Store,
    ino: u64,
    lines: usize,
) -> Vec<u8> {
    let mut expected = Vec::new();
    for i in 0..lines {
        // ~512 字节一行（贴近 transcript 小记录），内容半可压缩。
        let mut line = format!("line {i:06} ").into_bytes();
        line.resize(512, b'.');
        let off = ws.geometry(store, ino).unwrap().0;
        ws.write_at(store, ino, off, &line, &params()).unwrap();
        expected.extend_from_slice(&line);
        // 每 50 行 fsync 一次（周期持久化 = 封尾块 + Store 提交）。
        if i % 50 == 49 {
            ws.seal(store, ino, &params()).unwrap();
            store.fsync(ino).unwrap();
        }
    }
    // 收尾 fsync。
    ws.seal(store, ino, &params()).unwrap();
    store.fsync(ino).unwrap();
    expected
}

#[test]
fn shadow_append_recompress_count_far_fewer_than_lines_and_content_correct() {
    let dir = tempfile::tempdir().unwrap();
    let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), CHUNK_SIZE).unwrap();
    let ino = store.create(ROOT_INO, "t.jsonl", new_attr()).unwrap();
    let mut ws = WriteSession::new(true);

    let lines = 400usize; // 400 行 × 512B = 200KiB；4096B 块 → 满块约 50 个。
    let expected = run_append_workload(&mut ws, &store, ino, lines);

    assert_eq!(read_whole(&ws, &store, ino), expected, "整文件内容正确");
    let attr = store.lookup(ROOT_INO, "t.jsonl").unwrap();
    assert_eq!(attr.size as usize, expected.len(), "size 正确");

    let seals = ws.seal_count();
    // 关键：封块次数应在「满块数 + 周期 fsync 数」量级（~50 + 8），远少于 append 次数 400。
    // 旧路径会对每次 append 重压尾块（>= 400 次重压）。给宽松上界 200 仍 < lines/2。
    assert!(
        seals < lines as u64 / 2,
        "shadow 封块次数 {seals} 应远少于 append 次数 {lines}（证明尾块未被每次重压）"
    );
}

#[test]
fn container_append_recompress_count_far_fewer_than_lines_and_content_correct() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.redb");
    let store = ContainerStore::open_with_chunk_size(&path, CHUNK_SIZE).unwrap();
    let ino = store.create(ROOT_INO, "t.jsonl", new_attr()).unwrap();
    let mut ws = WriteSession::new(true);

    let lines = 400usize;
    let expected = run_append_workload(&mut ws, &store, ino, lines);

    assert_eq!(read_whole(&ws, &store, ino), expected, "整文件内容正确");
    let seals = ws.seal_count();
    assert!(
        seals < lines as u64 / 2,
        "container 封块次数 {seals} 应远少于 append 次数 {lines}"
    );

    // fsync 后重开容器，数据应已持久（封块落 Store 后 redb 已提交）。
    drop(store);
    let store2 = ContainerStore::open_with_chunk_size(&path, CHUNK_SIZE).unwrap();
    let attr = store2.lookup(ROOT_INO, "t.jsonl").unwrap();
    assert_eq!(attr.size as usize, expected.len(), "重开后 size 持久");
}

#[test]
fn tail_buffer_off_each_append_goes_straight_to_store_still_correct() {
    // --no-tail-buffer 对照：enabled=false，每次 append 直接走旧 rmw（落 Store），无封块计数。
    let dir = tempfile::tempdir().unwrap();
    let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), CHUNK_SIZE).unwrap();
    let ino = store.create(ROOT_INO, "t.jsonl", new_attr()).unwrap();
    let mut ws = WriteSession::new(false);

    let mut expected = Vec::new();
    for i in 0..120usize {
        let mut line = format!("row {i}\n").into_bytes();
        line.resize(100, b'x');
        let off = ws.geometry(&store, ino).unwrap().0;
        ws.write_at(&store, ino, off, &line, &params()).unwrap();
        expected.extend_from_slice(&line);
    }
    store.fsync(ino).unwrap();
    assert_eq!(
        read_whole(&ws, &store, ino),
        expected,
        "关闭优化时内容仍正确"
    );
    assert_eq!(ws.seal_count(), 0, "关闭优化时无封块计数（直走旧路径）");
}

/// 并发：reader 线程 + writer/sealer 线程共享同一 per-inode 写锁（镜像 rwfs），断言读路径
/// 永不读到「有数据块被零填充」的 torn read（rust-review HIGH-1：seal 改为先落 Store 再删缓冲）。
///
/// 读路径与 rwfs::read_range 一致：持写锁 → geometry → 逐块（尾块走缓冲，其余走 Store 解压）。
#[test]
fn concurrent_read_and_seal_no_torn_read() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let store: Arc<ShadowStore> =
        Arc::new(ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), CHUNK_SIZE).unwrap());
    let ino = store.create(ROOT_INO, "t.jsonl", new_attr()).unwrap();
    // 镜像 rwfs 的 per-inode 写锁：把「配置 + 开放尾块状态」的整个 WriteSession 放进一把 Mutex，
    // write/seal 走 `&mut`、read 走 `&`，二者互斥于同一把锁（read 持锁是 HIGH-1 的修复；共享的
    // InodeState 与 seal 缓冲的可变必须锁在同一处，否则重开 torn-read 空窗）。
    let ws = Arc::new(Mutex::new(WriteSession::new(true)));

    // 每行非零内容（用 i 的低 8 位 +1，保证恒非 0），便于断言「读到 0 = torn」。
    let line_byte = |i: usize| -> u8 { ((i % 255) + 1) as u8 };
    let line_len = 200usize;

    let stop = Arc::new(AtomicBool::new(false));

    // reader 线程：持锁做整文件读，断言每个「应有数据」的字节非 0。
    let reader = {
        let store = Arc::clone(&store);
        let ws = Arc::clone(&ws);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let g = ws.lock().unwrap();
                let got = read_whole(&g, store.as_ref(), ino);
                // 文件由整行写入，长度应是 line_len 的整数倍；任一字节为 0 即 torn read。
                assert!(
                    got.iter().all(|&b| b != 0),
                    "torn read：读到零字节（seal 空窗），len={}",
                    got.len()
                );
            }
        })
    };

    // writer 线程：持锁 append 整行 + 周期 seal/fsync。
    let lines = 2000usize;
    for i in 0..lines {
        let mut g = ws.lock().unwrap();
        let line = vec![line_byte(i); line_len];
        let off = g.geometry(store.as_ref(), ino).unwrap().0;
        g.write_at(store.as_ref(), ino, off, &line, &params())
            .unwrap();
        if i % 20 == 19 {
            g.seal(store.as_ref(), ino, &params()).unwrap();
            store.fsync(ino).unwrap();
        }
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();

    // 收尾一致性。
    let mut g = ws.lock().unwrap();
    g.seal(store.as_ref(), ino, &params()).unwrap();
    store.fsync(ino).unwrap();
    let final_len = read_whole(&g, store.as_ref(), ino).len();
    assert_eq!(final_len, lines * line_len, "末态长度正确");
}

// ===========================================================================
// 任务 A：fsync 碎片化修复（§A）。频繁 fsync 不应再把 BS archive 切成大量小块版本、
// 拖垮压缩比。下面三组测试守住：抗碎片化（物理体积/压缩比 ≈ 稀疏 fsync）、durable、续写
// 逐字节一致。
// ===========================================================================

/// 半可压缩行（与 append-bench `make_line` 同形），便于压缩比对 fsync 频率敏感。
fn semi_line(i: usize, size: usize) -> Vec<u8> {
    let mut l = format!("{{\"i\":{i},\"msg\":\"record line for append bench\"}}").into_bytes();
    l.resize(size, b' ');
    let n = l.len() - 1;
    l[n] = b'\n';
    l
}

/// 在新建 ShadowStore 上跑 `lines` 行 append，每 `fsync_every` 行 fsync 一次，返回
/// (archive 物理字节, footer 块数, 逻辑字节)。
fn shadow_append_run(
    lines: usize,
    line_size: usize,
    chunk_size: u32,
    fsync_every: usize,
) -> (u64, u64, u64) {
    use zipfs::archive::ArchiveReader;
    let dir = tempfile::tempdir().unwrap();
    let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), chunk_size).unwrap();
    let mut a = new_attr();
    a.chunk_size = chunk_size;
    let ino = store.create(ROOT_INO, "t.jsonl", a).unwrap();
    let mut ws = WriteSession::new(true);
    let mut logical = 0u64;
    for i in 0..lines {
        let line = semi_line(i, line_size);
        let off = ws.geometry(&store, ino).unwrap().0;
        ws.write_at(&store, ino, off, &line, &params()).unwrap();
        logical += line.len() as u64;
        if fsync_every > 0 && i % fsync_every == fsync_every - 1 {
            ws.seal(&store, ino, &params()).unwrap();
            store.fsync(ino).unwrap();
        }
    }
    ws.seal(&store, ino, &params()).unwrap();
    store.fsync(ino).unwrap();
    let path = dir.path().join("t.jsonl");
    let phys = std::fs::metadata(&path).unwrap().len();
    let f = ArchiveReader::open(&path).unwrap();
    (phys, f.chunk_count(), logical)
}

#[test]
fn shadow_frequent_fsync_block_count_consistent_append_only() {
    // §8.4 in-archive 尾日志：fsync 只追加未封尾块原始增量，不再每次重压尾块。故频繁/稀疏 fsync
    // 块数一致（只取决于逻辑量），且**物理体积接近**——写放大根治（§8.3 曾因 append-only 暂放宽，
    // journal 让它重新成立）。
    let lines = 5000usize;
    let (phys_freq, blocks_freq, _logical) = shadow_append_run(lines, 1024, 65536, 5);
    let (phys_sparse, blocks_sparse, _) = shadow_append_run(lines, 1024, 65536, 100);

    assert_eq!(
        blocks_freq, blocks_sparse,
        "频繁/稀疏 fsync 最终块数应一致：freq={blocks_freq} sparse={blocks_sparse}"
    );
    // 频繁 fsync 只追加原始增量（≤ 一个块的 raw + 8B/次记录头，封块时折叠重置），不再每次重压
    // 整块。旧 reuse 路径频繁 fsync ≈20x 体积；现以 5x 为回归上界（封块前 raw 与压缩块并存的稳态）。
    assert!(
        phys_freq < phys_sparse * 5,
        "写放大失控：频繁 fsync 物理 {phys_freq} 应被尾日志界住（旧 reuse ~20x，现应 <5x 稀疏 {phys_sparse}）"
    );
}

#[test]
fn shadow_remount_journal_rebuilds_tail_block_byte_for_byte_consistent() {
    // remount 安全网：多次 fsync 但不封块（行远小于块），尾块全在 journal。卸载（drop store）→
    // 重开 → 经 get_block 重放 journal 重建未封尾块，整文件逐字节与期望一致（§8.4 (2)：只做写放大
    // 不做重建会丢已 fsync 数据）。
    let dir = tempfile::tempdir().unwrap();
    let cs = 65536u32; // 大块，确保尾块不被封满
    let path = dir.path().join("t.jsonl");
    let mut expected = Vec::new();
    {
        let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), cs).unwrap();
        let mut a = new_attr();
        a.chunk_size = cs;
        let ino = store.create(ROOT_INO, "t.jsonl", a).unwrap();
        let mut ws = WriteSession::new(true);
        for i in 0..30usize {
            let line = semi_line(i, 200);
            let off = ws.geometry(&store, ino).unwrap().0;
            ws.write_at(&store, ino, off, &line, &params()).unwrap();
            expected.extend_from_slice(&line);
            ws.seal(&store, ino, &params()).unwrap(); // 每行 fsync → journal 增量，绝不封块
            store.fsync(ino).unwrap();
        }
    }
    // 重开 store（模拟 remount）：尾块仅存在于 journal。
    let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), cs).unwrap();
    let ino = store.lookup(ROOT_INO, "t.jsonl").unwrap().ino;
    use zipfs::archive::ArchiveReader;
    let r = ArchiveReader::open(&path).unwrap();
    assert_eq!(
        r.chunk_count(),
        0,
        "30×200B 远小于 64KiB，应无封块、全在 journal"
    );
    // 经 get_block(0) 取重建尾块；逐字节一致。
    let blk = store
        .get_block(ino, 0)
        .unwrap()
        .expect("get_block 应从 journal 重建尾块");
    let plain = decompress(&blk.bytes, Algo::Zstd, blk.stored_verbatim).unwrap();
    assert_eq!(plain, expected, "remount 后 journal 重建尾块应逐字节一致");
    assert_eq!(read_whole(&WriteSession::new(true), &store, ino), expected);
}

#[test]
fn shadow_content_durable_after_frequent_fsync_and_continued_write_byte_for_byte_consistent() {
    // durability + 续写正确性：频繁 fsync → 中途重开 store 读回（验证已 fsync 数据落盘）→
    // 继续 append → 读回整文件逐字节与期望一致（验证「fsync 后续写同一逻辑尾块」无错位）。
    use zipfs::archive::ArchiveReader;
    let dir = tempfile::tempdir().unwrap();
    let cs = 4096u32;
    let path = dir.path().join("t.jsonl");
    let mut expected = Vec::new();

    // 第一段：800 行，每 3 行 fsync（高频）。
    {
        let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), cs).unwrap();
        let mut a = new_attr();
        a.chunk_size = cs;
        let ino = store.create(ROOT_INO, "t.jsonl", a).unwrap();
        let mut ws = WriteSession::new(true);
        for i in 0..800usize {
            let line = semi_line(i, 300);
            let off = ws.geometry(&store, ino).unwrap().0;
            ws.write_at(&store, ino, off, &line, &params()).unwrap();
            expected.extend_from_slice(&line);
            if i % 3 == 2 {
                ws.seal(&store, ino, &params()).unwrap();
                store.fsync(ino).unwrap();
            }
        }
        ws.seal(&store, ino, &params()).unwrap();
        store.fsync(ino).unwrap();
    }

    // 重开（模拟重挂）：直接经 archive 读回全部已封块，逐字节比对（durable 验证）。
    let read_back_via_store = || -> Vec<u8> {
        let r = ArchiveReader::open(&path).unwrap();
        let mut out = Vec::new();
        for idx in 0..r.chunk_count() {
            let (bytes, entry) = r.read_block(idx).unwrap().unwrap();
            out.extend_from_slice(&decompress(&bytes, Algo::Zstd, entry.is_verbatim()).unwrap());
        }
        // 未封尾块在尾日志（fsync 只追加原始增量，不封块）：remount 重建须重放它（§8.4）。
        if let Some(tail) = r.read_tail().unwrap() {
            out.extend_from_slice(&tail);
        }
        out
    };
    assert_eq!(
        read_back_via_store(),
        expected,
        "重开后已 fsync 内容 durable 且逐字节一致"
    );

    // 第二段：在重开的 store 上继续 append 400 行（续写同一文件的尾块），再逐字节比对。
    {
        let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), cs).unwrap();
        let ino = store.lookup(ROOT_INO, "t.jsonl").unwrap().ino;
        let mut ws = WriteSession::new(true);
        for i in 800..1200usize {
            let line = semi_line(i, 300);
            let off = ws.geometry(&store, ino).unwrap().0;
            ws.write_at(&store, ino, off, &line, &params()).unwrap();
            expected.extend_from_slice(&line);
            if i % 7 == 6 {
                ws.seal(&store, ino, &params()).unwrap();
                store.fsync(ino).unwrap();
            }
        }
        ws.seal(&store, ino, &params()).unwrap();
        store.fsync(ino).unwrap();
        // 经会话读协调读回整文件（尾块走缓冲，其余走 store）。
        assert_eq!(
            read_whole(&ws, &store, ino),
            expected,
            "续写后整文件逐字节一致"
        );
    }
    assert_eq!(
        read_back_via_store(),
        expected,
        "续写后重读 archive 仍逐字节一致"
    );
}

/// 大文件边界（评审测试盲区）：>1MiB 默认块、多块、跨块 append、>5MB 体量。
/// 现有测试最大仅 ~200KiB，从不跨越 1MiB 默认块多次——本测试补该回归。
#[test]
fn large_file_multi_block_cross_block_append_byte_for_byte_correct() {
    use zipfs::core::rmw;
    let cs = 1024 * 1024u32; // 1MiB 默认块
    let dir = tempfile::tempdir().unwrap();
    let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), cs).unwrap();
    let mut attr = new_attr();
    attr.chunk_size = cs;
    let ino = store.create(ROOT_INO, "big.bin", attr).unwrap();
    let mut ws = WriteSession::new(true);

    // 写 5.5 MiB（跨 6 个 1MiB 块，末块部分）——半可压缩内容（确定性，避免依赖 rand）。
    let total = 5_500_000usize;
    let mut data: Vec<u8> = Vec::with_capacity(total);
    let mut x: u32 = 0x9e37_79b9;
    for i in 0..total {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // 混入可压缩前缀 + 伪随机字节，逼出真实多块压缩路径。
        data.push(if i % 16 < 10 { b'A' } else { (x >> 24) as u8 });
    }
    rmw::write_at(&store, ino, 0, &data, &params()).unwrap();
    store.fsync(ino).unwrap();

    // 整文件逐字节读回（跨 6 块）。
    assert_eq!(
        read_whole(&ws, &store, ino),
        data,
        "5.5MiB 多块写回逐字节一致"
    );

    // 跨块边界 append：在 EOF（5.5MiB，落在第 6 块中部）追加 3MiB，跨到第 8 块。
    let extra: Vec<u8> = (0..3_000_000u32)
        .map(|i| b"xyz \n"[(i % 5) as usize])
        .collect();
    let off = ws.geometry(&store, ino).unwrap().0;
    ws.write_at(&store, ino, off, &extra, &params()).unwrap();
    ws.seal(&store, ino, &params()).unwrap();
    store.fsync(ino).unwrap();

    let mut expected = data.clone();
    expected.extend_from_slice(&extra);
    assert_eq!(
        read_whole(&ws, &store, ino),
        expected,
        "跨块 append 后 8.5MiB 整文件逐字节一致"
    );
    let attr2 = store.lookup(ROOT_INO, "big.bin").unwrap();
    assert_eq!(
        attr2.size as usize,
        expected.len(),
        "size 反映 append 后总长"
    );

    // 随机中段读（跨块边界 [1MiB-10, 1MiB+10)）仍正确。
    let mid = 1024 * 1024 - 10;
    assert_eq!(
        read_range_helper(&ws, &store, ino, mid as u64, 20),
        expected[mid..mid + 20],
        "跨块边界中段读逐字节正确"
    );
}

/// 读 [off, off+len) 区间（复用 read_whole 的协调逻辑做切片）。
fn read_range_helper(
    ws: &WriteSession,
    store: &dyn Store,
    ino: u64,
    off: u64,
    len: usize,
) -> Vec<u8> {
    let whole = read_whole(ws, store, ino);
    whole[off as usize..(off as usize + len).min(whole.len())].to_vec()
}
