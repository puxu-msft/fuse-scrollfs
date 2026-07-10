//! ratio-bench：压缩比矩阵基准（T3「共享字典压缩」/「大块」研究项）。
//!
//! 把真实语料（如 `~/.claude/projects` 副本）经**实际 Store + Core 写路径**写入一个临时后端，
//! 量「块大小 × zstd 等级 × 共享字典 on/off」组合下的**物理压缩比**。回答 docs 优化分析的核心
//! 研究问题：**字典能否在真实数据 + 真实代码路径上把 64KiB 小块从 ~6x 拉到 ~16x**，以及大块各档
//! 的实际收益——不是 zstd CLI 的估算，而是过本项目分块/启发式/Store 的端到端实测。
//!
//! 同时回读首个文件全量做 round-trip 校验（顺带抓字典压缩/解压路径的正确性 bug）。
//!
//! 用法：
//! ```text
//! ratio-bench --input <语料目录> [--backend shadow|container] [--chunk-size B]
//!             [--level L] [--dict <字典文件>] [--max-bytes N]
//! ```
//! 单配置单行输出；矩阵由 `bench/scripts/ratio-matrix.sh` 驱动。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::{Parser, ValueEnum};

use zipfs::core::codec::{Algo, SharedDict};
use zipfs::core::rmw::{self, CodecParams};
use zipfs::core::DEFAULT_CHUNK_SIZE;
use zipfs::store::container::ContainerStore;
use zipfs::store::shadow::ShadowStore;
use zipfs::store::{Attr, Store};

const ROOT_INO: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum BackendSel {
    Shadow,
    Container,
}

#[derive(Parser, Debug)]
#[command(name = "ratio-bench", about = "压缩比矩阵基准：块大小×等级×字典")]
struct Cli {
    /// 语料目录（递归读取其中文件，经实际写路径灌入后端）。
    #[arg(long)]
    input: PathBuf,

    /// 后端：shadow（影子树，比值冠军）或 container（redb 容器）。
    #[arg(long, value_enum, default_value_t = BackendSel::Shadow)]
    backend: BackendSel,

    /// 逻辑块大小（字节），默认 64KiB。
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE as u32)]
    chunk_size: u32,

    /// zstd 等级，默认 3。
    #[arg(long, default_value_t = 3)]
    level: i32,

    /// 共享字典文件（`zipfs train-dict` 产出）。给定则走字典压缩路径。
    #[arg(long)]
    dict: Option<PathBuf>,

    /// 读取语料总字节上限（控时 / 防 OOM），默认 256MiB。
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    max_bytes: u64,
}

fn new_attr(chunk_size: u32) -> Attr {
    let now = std::time::SystemTime::now();
    Attr {
        ino: 0,
        size: 0,
        kind: fuser::FileType::RegularFile,
        perm: 0o644,
        uid: 0,
        gid: 0,
        mtime: now,
        atime: now,
        ctime: now,
        chunk_size,
    }
}

/// RAII 临时目录（drop 递归删自己建的唯一目录，不通配）。
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(tag: &str) -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("zipfs-ratio-bench-{tag}-{nanos}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if self.path.starts_with(std::env::temp_dir()) && self.path.is_dir() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// 递归收集普通文件路径（确定性排序），受总字节上限约束。
fn collect_files(
    dir: &Path,
    cap: u64,
    total: &mut u64,
    out: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        if *total >= cap {
            return Ok(());
        }
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, cap, total, out)?;
        } else if path.is_file() {
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            *total += len;
            out.push(path);
        }
    }
    Ok(())
}

/// 递归求目录下所有文件的 apparent 字节和（逻辑物理占用，确定性，对齐 du -sb）。
fn dir_apparent_bytes(dir: &Path) -> u64 {
    let mut sum = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                sum += dir_apparent_bytes(&p);
            } else if let Ok(m) = entry.metadata() {
                sum += m.len();
            }
        }
    }
    sum
}

fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();

    if !cli.input.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("语料目录不存在：{}", cli.input.display()),
        ));
    }

    // 加载字典（若给定）。
    let dict: Option<Arc<SharedDict>> = match &cli.dict {
        Some(p) => {
            let raw = std::fs::read(p)?;
            SharedDict::new(raw, cli.level)
        }
        None => None,
    };
    let params = CodecParams {
        algo: Algo::Zstd,
        level: cli.level,
        dict: dict.clone(),
    };

    // 收集语料文件。
    let mut files = Vec::new();
    let mut scanned = 0u64;
    collect_files(&cli.input, cli.max_bytes, &mut scanned, &mut files)?;
    if files.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "语料目录无可读文件",
        ));
    }

    // 建后端，写入全部文件，量物理占用。
    let scratch = ScratchDir::new("rb")?;
    let (store, backing): (Arc<dyn Store>, PathBuf) = match cli.backend {
        BackendSel::Shadow => {
            let s =
                ShadowStore::open_with_chunk_size(scratch.path().to_path_buf(), cli.chunk_size)?;
            (Arc::new(s), scratch.path().to_path_buf())
        }
        BackendSel::Container => {
            let backing = scratch.path().join("zipfs.redb");
            let s = ContainerStore::open_with_chunk_size(&backing, cli.chunk_size)?;
            (Arc::new(s), backing)
        }
    };

    let mut logical = 0u64;
    let mut first_check: Option<(u64, Vec<u8>)> = None;
    let start = Instant::now();
    for (i, path) in files.iter().enumerate() {
        let content = std::fs::read(path)?;
        if content.is_empty() {
            continue;
        }
        let name = format!("f{i}");
        let ino = store.create(ROOT_INO, &name, new_attr(cli.chunk_size))?;
        // 整文件单次写：write_at 内部按 chunk_size 分块 + 压缩（带/不带字典）。
        rmw::write_at(store.as_ref(), ino, 0, &content, &params)?;
        store.fsync(ino)?;
        logical += content.len() as u64;
        if first_check.is_none() {
            first_check = Some((ino, content));
        }
    }
    let write_s = start.elapsed().as_secs_f64();

    // round-trip 校验首个文件（抓字典压缩/解压路径正确性）。
    if let Some((ino, expect)) = first_check {
        let got = read_back(store.as_ref(), ino, expect.len() as u64, &params)?;
        if got != expect {
            return Err(std::io::Error::other(format!(
                "round-trip 校验失败：ino={ino} 读回 {} 字节 != 原始 {} 字节（字典路径疑有 bug）",
                got.len(),
                expect.len()
            )));
        }
    }

    let physical = match cli.backend {
        BackendSel::Shadow => dir_apparent_bytes(&backing),
        BackendSel::Container => std::fs::metadata(&backing).map(|m| m.len()).unwrap_or(0),
    };
    let dict_bytes = dict.as_ref().map(|d| d.raw().len() as u64).unwrap_or(0);
    // 物理占用含字典（字典须随数据持久化才能解压；跨整库摊薄，单库也如实计入）。
    let physical_with_dict = physical + dict_bytes;
    let ratio = if physical_with_dict == 0 {
        0.0
    } else {
        logical as f64 / physical_with_dict as f64
    };

    println!(
        "backend={:?} chunk={}KiB level={} dict={} files={} logical={}MiB physical={}MiB(+dict {}KiB) ratio={:.2}x write={:.1}s",
        cli.backend,
        cli.chunk_size / 1024,
        cli.level,
        if dict_bytes > 0 { "on" } else { "off" },
        files.len(),
        logical / (1024 * 1024),
        physical / (1024 * 1024),
        dict_bytes / 1024,
        ratio,
        write_s,
    );
    Ok(())
}

/// 经 Store 读回整文件逻辑字节（解压全部块拼接，带字典则走字典解压）。
fn read_back(
    store: &dyn Store,
    ino: u64,
    size: u64,
    params: &CodecParams,
) -> std::io::Result<Vec<u8>> {
    use zipfs::core::codec::decompress_block;
    let (_, cs) = store
        .block_geometry(ino)
        .ok_or_else(|| std::io::Error::other("read_back：几何缺失"))?;
    let cs = cs as u64;
    let mut out = vec![0u8; size as usize];
    let nblocks = size.div_ceil(cs);
    for idx in 0..nblocks {
        if let Some(b) = store.get_block(ino, idx)? {
            let plain = decompress_block(
                &b.bytes,
                params.algo,
                b.stored_verbatim,
                params.dict.as_deref(),
            )?;
            let start = (idx * cs) as usize;
            let end = (start + plain.len()).min(out.len());
            if start < end {
                out[start..end].copy_from_slice(&plain[..end - start]);
            }
        }
    }
    Ok(out)
}
