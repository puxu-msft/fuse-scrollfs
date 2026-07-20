//! 增量增长验证（§1.1 硬约束）：append 不重写全文。
//!
//! 对 838MB 巨文件每次 append 重写全文 = 灾难。这里在小规模上断言「append 只让后端
//! 文件/容器增量增长」，而非整文件重写：写一个大文件 → fsync → 记录后端物理大小 →
//! 再 append 一小段 → fsync → 后端只应**增加约一个尾块**的量，远小于整文件大小。

use std::fs;

use scrollz::core::codec::Algo;
use scrollz::core::rmw::{self, CodecParams};
use scrollz::store::container::ContainerStore;
use scrollz::store::shadow::ShadowStore;
use scrollz::store::{Attr, Store};

const CHUNK_SIZE: u32 = 4096;
const ROOT_INO: u64 = 1;

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

/// 不可压缩数据，避免压缩把「增量大小」掩盖（确定性伪随机）。
fn incompressible(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            (x >> 33) as u8
        })
        .collect()
}

#[test]
fn shadow_append_grows_append_only_no_full_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), CHUNK_SIZE).unwrap();
    let ino = store.create(ROOT_INO, "big.dat", new_attr()).unwrap();

    // 写一个「大」文件（200KiB 不可压缩）。
    let big = incompressible(200 * 1024, 0xABCD);
    rmw::write_at(&store, ino, 0, &big, &params()).unwrap();
    store.fsync(ino).unwrap();

    let archive_path = dir.path().join("big.dat");
    let size_before = fs::metadata(&archive_path).unwrap().len();

    // append 一小段（100 字节）。
    let tail = incompressible(100, 0x1234);
    rmw::write_at(&store, ino, big.len() as u64, &tail, &params()).unwrap();
    store.fsync(ino).unwrap();
    let size_after = fs::metadata(&archive_path).unwrap().len();

    let delta = size_after - size_before;
    // 增量应远小于整文件（< 3 个块 + footer 余量），证明没有重写全文。
    let bound = (CHUNK_SIZE as u64) * 3 + 4096;
    assert!(
        delta < bound,
        "shadow append 增量 {delta} 应远小于整文件 {size_before}（重写全文 = 灾难）"
    );
    // 但确实增长了（append 写了尾块）。
    assert!(delta > 0, "append 后后端文件应增长");
}

#[test]
fn container_append_grows_append_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.redb");
    let store = ContainerStore::open_with_chunk_size(&path, CHUNK_SIZE).unwrap();
    let ino = store.create(ROOT_INO, "big.dat", new_attr()).unwrap();

    let big = incompressible(200 * 1024, 0xABCD);
    rmw::write_at(&store, ino, 0, &big, &params()).unwrap();
    store.fsync(ino).unwrap();
    let size_before = fs::metadata(&path).unwrap().len();

    let tail = incompressible(100, 0x1234);
    rmw::write_at(&store, ino, big.len() as u64, &tail, &params()).unwrap();
    store.fsync(ino).unwrap();
    let size_after = fs::metadata(&path).unwrap().len();

    // redb 容器可能因 MVCC 页预留略增，但不应翻倍重写全部块；放宽到 < 半个整文件。
    let delta = size_after.saturating_sub(size_before);
    assert!(
        delta < size_before / 2 + 64 * 1024,
        "container append 增量 {delta} 不应接近整文件 {size_before}"
    );
}

/// redb 写批处理正确性：一次会话内多块 put 合并一事务（fsync 才落盘），
/// fsync 前后读一致（read-through 挂起），fsync 后重开容器仍可读。
#[test]
fn container_batch_transaction_consistent_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.redb");
    let payload = incompressible(10 * 1024, 7);
    let ino;
    {
        let store = ContainerStore::open_with_chunk_size(&path, CHUNK_SIZE).unwrap();
        ino = store.create(ROOT_INO, "f", new_attr()).unwrap();
        rmw::write_at(&store, ino, 0, &payload, &params()).unwrap();
        // fsync 前：read-through 挂起暂存应已可见。
        let (size, _) = store.block_geometry(ino).unwrap();
        assert_eq!(
            size as usize,
            payload.len(),
            "fsync 前 size 应 read-through"
        );
        store.fsync(ino).unwrap();
    }
    // 重开容器，数据应已持久。
    let store = ContainerStore::open_with_chunk_size(&path, CHUNK_SIZE).unwrap();
    let attr = store.lookup(ROOT_INO, "f").expect("重开后文件应在");
    assert_eq!(attr.size as usize, payload.len());
    // 读回首块校验。
    let blk = store.get_block(attr.ino, 0).unwrap().unwrap();
    let plain =
        scrollz::core::codec::decompress(&blk.bytes, Algo::Zstd, blk.stored_verbatim).unwrap();
    assert_eq!(&plain[..], &payload[..CHUNK_SIZE as usize]);
}
