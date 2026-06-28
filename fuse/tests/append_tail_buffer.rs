//! append 优化端到端：开放尾块缓冲在两后端（ShadowStore / ContainerStore）上的重压次数 +
//! 正确性（§1.1）。模拟目标负载「逐行 append 小记录 + 周期 fsync」，断言：
//! 1. 开启优化时，封块（重压）次数 ≈ 满块数，**远少于 append 次数**（旧路径 = 每次 append 重压）。
//! 2. 整文件内容、size 正确（读协调：未封尾块走缓冲）。
//! 3. fsync 后尾块已封、可经 Store 读出。
//! 4. `--no-tail-buffer`（enabled=false）退化为旧路径仍正确（差分覆盖在 model_based）。

use zipfs::core::codec::{decompress, Algo};
use zipfs::core::rmw::CodecParams;
use zipfs::core::wsession::TailSessions;
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
        chunk_size: CHUNK_SIZE,
    }
}

/// 与 rwfs 读协调一致的整文件读回：尾块走缓冲，其余走 Store 解压，缺块零填充。
fn read_whole(ws: &TailSessions, store: &dyn Store, ino: u64) -> Vec<u8> {
    let (size, cs) = ws.geometry(store, ino).unwrap();
    let mut out = vec![0u8; size as usize];
    let cs = cs as u64;
    let nblocks = size.div_ceil(cs);
    for idx in 0..nblocks {
        let start = (idx * cs) as usize;
        let plain = if let Some(p) = ws.read_tail_block(ino, idx) {
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
fn run_append_workload(ws: &TailSessions, store: &dyn Store, ino: u64, lines: usize) -> Vec<u8> {
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
fn shadow_append_重压次数远少于行数_且内容正确() {
    let dir = tempfile::tempdir().unwrap();
    let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), CHUNK_SIZE).unwrap();
    let ino = store.create(ROOT_INO, "t.jsonl", new_attr()).unwrap();
    let ws = TailSessions::new(true);

    let lines = 400usize; // 400 行 × 512B = 200KiB；4096B 块 → 满块约 50 个。
    let expected = run_append_workload(&ws, &store, ino, lines);

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
fn container_append_重压次数远少于行数_且内容正确() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.redb");
    let store = ContainerStore::open_with_chunk_size(&path, CHUNK_SIZE).unwrap();
    let ino = store.create(ROOT_INO, "t.jsonl", new_attr()).unwrap();
    let ws = TailSessions::new(true);

    let lines = 400usize;
    let expected = run_append_workload(&ws, &store, ino, lines);

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
fn 关闭尾块缓冲_每次_append_直接落_store_仍正确() {
    // --no-tail-buffer 对照：enabled=false，每次 append 直接走旧 rmw（落 Store），无封块计数。
    let dir = tempfile::tempdir().unwrap();
    let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), CHUNK_SIZE).unwrap();
    let ino = store.create(ROOT_INO, "t.jsonl", new_attr()).unwrap();
    let ws = TailSessions::new(false);

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
fn 并发_读与_seal_无_torn_read() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let store: Arc<ShadowStore> =
        Arc::new(ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), CHUNK_SIZE).unwrap());
    let ino = store.create(ROOT_INO, "t.jsonl", new_attr()).unwrap();
    let ws = Arc::new(TailSessions::new(true));
    // 镜像 rwfs 的 per-inode 写锁：write/seal/read 都持它（read 持锁是 HIGH-1 的修复）。
    let lock = Arc::new(Mutex::new(()));

    // 每行非零内容（用 i 的低 8 位 +1，保证恒非 0），便于断言「读到 0 = torn」。
    let line_byte = |i: usize| -> u8 { ((i % 255) + 1) as u8 };
    let line_len = 200usize;

    let stop = Arc::new(AtomicBool::new(false));

    // reader 线程：持锁做整文件读，断言每个「应有数据」的字节非 0。
    let reader = {
        let store = Arc::clone(&store);
        let ws = Arc::clone(&ws);
        let lock = Arc::clone(&lock);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _g = lock.lock().unwrap();
                let got = read_whole(&ws, store.as_ref(), ino);
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
        let _g = lock.lock().unwrap();
        let line = vec![line_byte(i); line_len];
        let off = ws.geometry(store.as_ref(), ino).unwrap().0;
        ws.write_at(store.as_ref(), ino, off, &line, &params())
            .unwrap();
        if i % 20 == 19 {
            ws.seal(store.as_ref(), ino, &params()).unwrap();
            store.fsync(ino).unwrap();
        }
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();

    // 收尾一致性。
    let _g = lock.lock().unwrap();
    ws.seal(store.as_ref(), ino, &params()).unwrap();
    store.fsync(ino).unwrap();
    let final_len = read_whole(&ws, store.as_ref(), ino).len();
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
    let ws = TailSessions::new(true);
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
fn shadow_频繁_fsync_不碎片化_物理体积与块数对齐稀疏_fsync() {
    // 同一负载（5000 行 × 1KB / 64KiB 块），分别在 fsync/5（频繁）与 fsync/100（稀疏）下跑。
    // 修复前：fsync/5 把渐增尾块逐版本追加成永久空洞，物理膨胀 ~15x、压缩比从 ~76x 崩到 ~5x。
    // 修复后：尾块重写复用 slot，最终块只剩满块版本 → 两种频率物理体积/块数应几乎一致。
    let lines = 5000usize;
    let (phys_freq, blocks_freq, logical) = shadow_append_run(lines, 1024, 65536, 5);
    let (phys_sparse, blocks_sparse, _) = shadow_append_run(lines, 1024, 65536, 100);

    // 块数应一致（同样的逻辑数据 → 同样的满块数 + 1 尾块）。
    assert_eq!(
        blocks_freq, blocks_sparse,
        "频繁/稀疏 fsync 最终块数应一致：freq={blocks_freq} sparse={blocks_sparse}"
    );
    // 物理体积应接近（容许 5% 误差：不同 fsync 边界处尾块压缩态略有差异）。
    let ratio = phys_freq as f64 / phys_sparse as f64;
    assert!(
        ratio < 1.05,
        "频繁 fsync 物理体积不应明显大于稀疏：freq={phys_freq} sparse={phys_sparse} ratio={ratio:.3}"
    );
    // 压缩比也应接近（直接由物理体积推出，作为可读断言）。
    let cr_freq = logical as f64 / phys_freq as f64;
    let cr_sparse = logical as f64 / phys_sparse as f64;
    assert!(
        cr_freq > cr_sparse * 0.95,
        "频繁 fsync 压缩比不应被拖垮：freq={cr_freq:.1}x sparse={cr_sparse:.1}x"
    );
}

#[test]
fn shadow_频繁_fsync_后内容_durable_且续写逐字节一致() {
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
        let ws = TailSessions::new(true);
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
        let ws = TailSessions::new(true);
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
