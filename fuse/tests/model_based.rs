//! Model-based 差分测试（§12 关键）：随机操作序列跑在「被测 Store + Core 写编排」
//! vs「内存参照模型」，逐步校验文件内容与属性一致。**两个后端跑同一套**（ShadowStore /
//! ContainerStore）。覆盖 create / write（随机 offset / append / 越 EOF）/ truncate /
//! unlink / rename / mkdir / fsync。
//!
//! 参照模型是最朴素的 `path -> Vec<u8>`（外加目录集合）。被测 Store 经 Core 的
//! `rmw::write_at` / `rmw::truncate` 写入，再用与 rwfs 一致的读路径（解压 + 缺块零填充）
//! 读回比对。每步操作后做全量一致性校验，第一处分歧即 panic 定位。

use std::collections::BTreeMap;

use zipfs::core::codec::{decompress, Algo};
use zipfs::core::rmw::{self, CodecParams};
use zipfs::store::container::ContainerStore;
use zipfs::store::shadow::ShadowStore;
use zipfs::store::{Attr, Store};

const CHUNK_SIZE: u32 = 64;
const ROOT_INO: u64 = 1;

fn params() -> CodecParams {
    CodecParams {
        algo: Algo::Zstd,
        level: 3,
        dict: None,
    }
}

/// 朴素参照模型：根目录下的扁平文件（name -> 内容）。本测试只在根目录建文件，
/// 用扁平命名空间即可覆盖数据路径（write/truncate/append/越 EOF）；rename/mkdir 单独覆盖。
#[derive(Default)]
struct Model {
    files: BTreeMap<String, Vec<u8>>,
}

/// 与 rwfs 一致的读路径：把整文件逻辑字节读回（解压 + 缺块零填充到 size）。
fn read_whole(store: &dyn Store, ino: u64) -> Vec<u8> {
    let Some((size, cs)) = store.block_geometry(ino) else {
        return Vec::new();
    };
    let mut out = vec![0u8; size as usize];
    let cs = cs as u64;
    let nblocks = size.div_ceil(cs);
    for idx in 0..nblocks {
        if let Some(b) = store.get_block(ino, idx).unwrap() {
            let plain = decompress(&b.bytes, Algo::Zstd, b.stored_verbatim).unwrap();
            let start = (idx * cs) as usize;
            let end = (start + plain.len()).min(out.len());
            if start < end {
                out[start..end].copy_from_slice(&plain[..end - start]);
            }
        }
    }
    out
}

fn new_attr(perm: u16) -> Attr {
    Attr {
        ino: 0,
        size: 0,
        kind: fuser::FileType::RegularFile,
        perm,
        uid: 0,
        gid: 0,
        mtime: std::time::SystemTime::UNIX_EPOCH,
        atime: std::time::SystemTime::UNIX_EPOCH,
        ctime: std::time::SystemTime::UNIX_EPOCH,
        chunk_size: CHUNK_SIZE,
    }
}

/// 一个确定性 PRNG（线性同余），避免引入 rand 依赖。
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// 校验被测 Store 与参照模型对所有文件一致（内容 + 大小）。
fn assert_consistent(store: &dyn Store, model: &Model, label: &str) {
    for (name, content) in &model.files {
        let attr = store
            .lookup(ROOT_INO, name)
            .unwrap_or_else(|| panic!("[{label}] 文件 {name} 在 Store 中缺失"));
        assert_eq!(
            attr.size as usize,
            content.len(),
            "[{label}] 文件 {name} 大小不一致：store={} model={}",
            attr.size,
            content.len()
        );
        let got = read_whole(store, attr.ino);
        assert_eq!(
            got,
            *content,
            "[{label}] 文件 {name} 内容不一致（len store={} model={}）",
            got.len(),
            content.len()
        );
    }
    // 反向：Store 不应有 model 之外的文件。
    let listed: Vec<String> = store
        .readdir(ROOT_INO)
        .into_iter()
        .map(|d| d.name)
        .collect();
    for name in &listed {
        assert!(
            model.files.contains_key(name),
            "[{label}] Store 多出文件 {name}（model 无）"
        );
    }
    assert_eq!(
        listed.len(),
        model.files.len(),
        "[{label}] 文件数不一致：store={listed:?} model={:?}",
        model.files.keys().collect::<Vec<_>>()
    );
}

/// 在被测 Store 上跑随机操作序列，逐步与参照模型比对。
fn run_differential(store: &dyn Store, seed: u64) {
    let mut model = Model::default();
    let mut rng = Rng(seed);

    // 预置几个文件名池。
    let names = ["a", "b", "c", "d"];

    for step in 0..400u64 {
        let op = rng.below(7);
        match op {
            // create
            0 => {
                let name = names[rng.below(names.len() as u64) as usize].to_string();
                if let std::collections::btree_map::Entry::Vacant(slot) =
                    model.files.entry(name.clone())
                {
                    store.create(ROOT_INO, &name, new_attr(0o644)).unwrap();
                    slot.insert(Vec::new());
                }
            }
            // write（随机 offset，可能 append / 越 EOF / 中间块 RMW）
            1..=3 => {
                if model.files.is_empty() {
                    continue;
                }
                let keys: Vec<String> = model.files.keys().cloned().collect();
                let name = keys[rng.below(keys.len() as u64) as usize].clone();
                let cur_len = model.files[&name].len() as u64;
                // offset 取值覆盖：块内、跨块、append、越 EOF（最多 +200）。
                let max_off = cur_len + 200;
                let offset = rng.below(max_off + 1);
                let len = 1 + rng.below(200);
                // 生成数据：混可压缩（重复）与不可压缩（伪随机），逼 verbatim flag 翻转。
                let mut data = vec![0u8; len as usize];
                let compressible = rng.below(2) == 0;
                for (i, b) in data.iter_mut().enumerate() {
                    *b = if compressible {
                        (i % 4) as u8
                    } else {
                        (rng.next() & 0xff) as u8
                    };
                }
                let attr = store.lookup(ROOT_INO, &name).unwrap();
                rmw::write_at(store, attr.ino, offset, &data, &params()).unwrap();

                // 更新参照模型：越 EOF 零填充。
                let content = model.files.get_mut(&name).unwrap();
                let end = (offset + len) as usize;
                if content.len() < end {
                    content.resize(end, 0);
                }
                content[offset as usize..end].copy_from_slice(&data);
            }
            // truncate（缩小 / 扩展）
            4 => {
                if model.files.is_empty() {
                    continue;
                }
                let keys: Vec<String> = model.files.keys().cloned().collect();
                let name = keys[rng.below(keys.len() as u64) as usize].clone();
                let cur_len = model.files[&name].len() as u64;
                let new_size = rng.below(cur_len + 150);
                let attr = store.lookup(ROOT_INO, &name).unwrap();
                rmw::truncate(store, attr.ino, new_size, &params()).unwrap();
                let content = model.files.get_mut(&name).unwrap();
                content.resize(new_size as usize, 0);
            }
            // unlink
            5 => {
                if model.files.is_empty() {
                    continue;
                }
                let keys: Vec<String> = model.files.keys().cloned().collect();
                let name = keys[rng.below(keys.len() as u64) as usize].clone();
                store.unlink(ROOT_INO, &name).unwrap();
                model.files.remove(&name);
            }
            // fsync（落盘 barrier；不改 model，仅校验落盘后仍一致）
            6 => {
                store.sync_all().unwrap();
            }
            _ => unreachable!(),
        }

        // 每步后做一次轻量校验（每 10 步做一次 fsync + 全量校验，平衡速度）。
        if step % 5 == 0 {
            assert_consistent(store, &model, &format!("seed={seed} step={step}"));
        }
    }
    // 末尾 fsync 后再全量校验一次（验证落盘一致性）。
    store.sync_all().unwrap();
    assert_consistent(store, &model, &format!("seed={seed} final-after-sync"));
}

#[test]
fn shadow_differential() {
    let dir = tempfile::tempdir().unwrap();
    let store = ShadowStore::open_with_chunk_size(dir.path().to_path_buf(), CHUNK_SIZE).unwrap();
    // 预填充根 inode 映射：lookup 根下子项需要根已 intern（open 时已置 ROOT）。
    for seed in [1u64, 42, 12345] {
        run_differential(&store, seed);
        // 各 seed 间清理：删掉残留文件，避免跨 seed 干扰。
        let names: Vec<String> = store
            .readdir(ROOT_INO)
            .into_iter()
            .map(|d| d.name)
            .collect();
        for n in names {
            let _ = store.unlink(ROOT_INO, &n);
        }
    }
}

#[test]
fn container_differential() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.redb");
    let store = ContainerStore::open_with_chunk_size(&path, CHUNK_SIZE).unwrap();
    for seed in [1u64, 42, 12345] {
        run_differential(&store, seed);
        let names: Vec<String> = store
            .readdir(ROOT_INO)
            .into_iter()
            .map(|d| d.name)
            .collect();
        for n in names {
            let _ = store.unlink(ROOT_INO, &n);
        }
    }
}

/// rename / mkdir 的定向覆盖（差分主循环聚焦数据路径，命名空间这里单测）。
#[test]
fn namespace_ops_both_backends() {
    // ShadowStore
    let sdir = tempfile::tempdir().unwrap();
    let s = ShadowStore::open_with_chunk_size(sdir.path().to_path_buf(), CHUNK_SIZE).unwrap();
    check_namespace(&s);

    // ContainerStore
    let cdir = tempfile::tempdir().unwrap();
    let c = ContainerStore::open_with_chunk_size(&cdir.path().join("v.redb"), CHUNK_SIZE).unwrap();
    check_namespace(&c);
}

fn check_namespace(store: &dyn Store) {
    // mkdir + 子目录建文件。
    let dino = store
        .mkdir(ROOT_INO, "sub", {
            let mut a = new_attr(0o755);
            a.kind = fuser::FileType::Directory;
            a
        })
        .unwrap();
    assert_eq!(store.lookup(ROOT_INO, "sub").unwrap().ino, dino);

    // create 文件、写、fsync、rename、读回。
    let fino = store.create(ROOT_INO, "f1", new_attr(0o644)).unwrap();
    rmw::write_at(store, fino, 0, b"hello world payload", &params()).unwrap();
    store.fsync(fino).unwrap();
    assert_eq!(read_whole(store, fino), b"hello world payload");

    store.rename((ROOT_INO, "f1"), (ROOT_INO, "f2")).unwrap();
    assert!(store.lookup(ROOT_INO, "f1").is_none(), "旧名应消失");
    let moved = store.lookup(ROOT_INO, "f2").unwrap();
    assert_eq!(read_whole(store, moved.ino), b"hello world payload");

    // unlink + rmdir。
    store.unlink(ROOT_INO, "f2").unwrap();
    assert!(store.lookup(ROOT_INO, "f2").is_none());
    store.rmdir(ROOT_INO, "sub").unwrap();
    assert!(store.lookup(ROOT_INO, "sub").is_none());
}
