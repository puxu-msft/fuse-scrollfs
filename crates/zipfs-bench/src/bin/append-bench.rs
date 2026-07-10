//! append-bench：append 优化（开放尾块缓冲）专项微基准。
//!
//! 量化目标负载「逐行 append 小记录到增长文件 + 周期 fsync」在**开启 / 关闭尾块缓冲**下的差异，
//! 跑在 BS（ShadowStore）与 BV（ContainerStore）上（C0 裸 ext4 顺序写作参照可选，未纳入此 bin）。
//!
//! 测量：
//! - **append 吞吐**（行/s、MiB/s）。
//! - **块压缩（重压）次数**（`rmw::block_compress_count`）—— 优化前后核心差异：关闭时每次 append 把
//!   尾块整块重压一遍（重压次数 ≈ append 次数）；开启时仅满块封一次（≈ 满块数）。
//! - **最终压缩比**（逻辑字节 / 后端物理字节）。
//!
//! 直接驱动 Core（`WriteSession` + `rmw`）+ Store，不经 FUSE 挂载——优化恰好落在这层，免挂载
//! 噪声，且能读到 `rmw::block_compress_count` 埋点。**默认单次短跑**（用户要求减少测试量），参数可调更长。
//!
//! 用法：
//! ```text
//! append-bench [--backend shadow|container|both] [--lines N] [--line-size B]
//!              [--chunk-size B] [--fsync-every K] [--level L]
//!              [--fsync-sweep K1,K2,...]   # fsync 频率扫描（§A 碎片化验证），输出块数/压缩比/重压/吞吐
//! ```
//! 默认对每个后端各跑「尾块缓冲 ON（after）」与「OFF（before）」两次，打印对照。
//! 给 `--fsync-sweep` 时改为：每个后端在 ON（修复后）下跑一组 fsync 频率，对照碎片化指标。

use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser, ValueEnum};

use zipfs::core::codec::Algo;
use zipfs::core::rmw::CodecParams;
use zipfs::core::wsession::WriteSession;
use zipfs::core::DEFAULT_CHUNK_SIZE;
use zipfs::store::container::ContainerStore;
use zipfs::store::shadow::ShadowStore;
use zipfs::store::{Attr, Store};

const ROOT_INO: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum BackendSel {
    Shadow,
    Container,
    Both,
}

#[derive(Parser, Debug)]
#[command(
    name = "append-bench",
    about = "append 优化（开放尾块缓冲）微基准：before/after 对照"
)]
struct Cli {
    /// 跑哪个后端（默认 both）。
    #[arg(long, value_enum, default_value_t = BackendSel::Both)]
    backend: BackendSel,

    /// append 的行数（默认 20000，约 1-2 分钟内完成单后端两次跑）。
    #[arg(long, default_value_t = 20_000)]
    lines: usize,

    /// 每行字节数（默认 1024，贴近 transcript 小记录）。
    #[arg(long, default_value_t = 1024)]
    line_size: usize,

    /// 逻辑块大小（默认 64KiB，§6.1 裁决）。
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE as u32)]
    chunk_size: u32,

    /// 每多少行 fsync 一次（默认 100，模拟周期持久化）。
    #[arg(long, default_value_t = 100)]
    fsync_every: usize,

    /// zstd 等级（默认 3）。
    #[arg(long, default_value_t = 3)]
    level: i32,

    /// fsync 频率扫描模式：对每个后端在 ON（修复后）下跑一组 fsync 频率，输出
    /// 「块数 / 压缩比 / 重压次数 / 吞吐」对照表，量化碎片化是否随 fsync 频率消除（§A）。
    /// 给定的逗号分隔频率列表覆盖默认 `--fsync-every`。
    #[arg(long, value_delimiter = ',')]
    fsync_sweep: Vec<usize>,
}

/// 一次跑的测量结果。
struct RunResult {
    label: String,
    elapsed_s: f64,
    lines: usize,
    logical_bytes: u64,
    physical_bytes: u64,
    /// 块压缩（重压）次数 —— before/after 核心差异（旧路径每次 append 重压尾块）。
    recompress: u64,
    /// 最终 archive/容器里的逻辑块数（碎片化指标：频繁 fsync 不应炸出额外块）。
    blocks: u64,
}

impl RunResult {
    fn lines_per_s(&self) -> f64 {
        self.lines as f64 / self.elapsed_s
    }
    fn mib_per_s(&self) -> f64 {
        (self.logical_bytes as f64 / (1024.0 * 1024.0)) / self.elapsed_s
    }
    fn ratio(&self) -> f64 {
        if self.physical_bytes == 0 {
            0.0
        } else {
            self.logical_bytes as f64 / self.physical_bytes as f64
        }
    }
    fn print_row(&self) {
        println!(
            "  {:<22} {:>8.2}s  {:>10.0} 行/s  {:>8.2} MiB/s  重压={:>8}  块数={:>5}  压缩比={:>7.2}x  物理={} B",
            self.label,
            self.elapsed_s,
            self.lines_per_s(),
            self.mib_per_s(),
            self.recompress,
            self.blocks,
            self.ratio(),
            self.physical_bytes,
        );
    }
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

/// 生成一行半可压缩内容（前缀可压缩，尾部填充），贴近真实文本记录。
fn make_line(i: usize, size: usize) -> Vec<u8> {
    let mut line = format!("{{\"i\":{i},\"msg\":\"record line for append bench\"}}").into_bytes();
    line.resize(size, b' ');
    if !line.is_empty() {
        let last = line.len() - 1;
        line[last] = b'\n';
    }
    line
}

/// 在给定 Store 上跑 append 负载，返回测量。`enabled` 控制尾块缓冲开关，`fsync_every`
/// 控制 fsync 频率（0=只收尾），`block_count` 在跑完后探出最终逻辑块数（碎片化指标）。
#[allow(clippy::too_many_arguments)]
fn run_on_store(
    label: &str,
    store: &dyn Store,
    ino: u64,
    backing_path: &Path,
    cli: &Cli,
    enabled: bool,
    fsync_every: usize,
    block_count: impl Fn() -> u64,
) -> RunResult {
    let params = CodecParams {
        algo: Algo::Zstd,
        level: cli.level,
        dict: None,
    };
    let mut ws = WriteSession::new(enabled);
    let mut logical_bytes = 0u64;

    // 隔离本段的块压缩（重压）计数。
    zipfs::core::rmw::reset_block_compress_count();

    let start = Instant::now();
    for i in 0..cli.lines {
        let line = make_line(i, cli.line_size);
        let off = ws.geometry(store, ino).unwrap().0;
        ws.write_at(store, ino, off, &line, &params).unwrap();
        logical_bytes += line.len() as u64;
        if fsync_every > 0 && i % fsync_every == fsync_every - 1 {
            ws.seal(store, ino, &params).unwrap();
            store.fsync(ino).unwrap();
        }
    }
    // 收尾 fsync。
    ws.seal(store, ino, &params).unwrap();
    store.fsync(ino).unwrap();
    let elapsed_s = start.elapsed().as_secs_f64();

    let physical_bytes = physical_size(backing_path);
    RunResult {
        label: label.to_string(),
        elapsed_s,
        lines: cli.lines,
        logical_bytes,
        physical_bytes,
        recompress: zipfs::core::rmw::block_compress_count(),
        blocks: block_count(),
    }
}

/// 后端物理占用：shadow 取 archive 文件大小，container 取 redb 文件大小。
fn physical_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// 简单 RAII 临时目录：唯一路径建目录，drop 时递归删除（仅删自己建的 bench 目录，路径已知非空）。
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(tag: &str) -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("zipfs-append-bench-{tag}-{nanos}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // 仅删本 bench 在系统临时目录下建的唯一目录（路径非空、由本进程创建），不通配。
        if self.path.starts_with(std::env::temp_dir()) && self.path.is_dir() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// 探 shadow archive 的物理块数（footer chunk_count）。文件不存在/损坏返回 0。
fn shadow_block_count(archive: &Path) -> u64 {
    zipfs::archive::ArchiveReader::open(archive)
        .map(|r| r.chunk_count())
        .unwrap_or(0)
}

/// 跑 ShadowStore 的「开启 vs 关闭」对照。
fn bench_shadow(cli: &Cli) {
    println!("[BS 影子树] backend=shadow chunk_size={}", cli.chunk_size);
    for (label, enabled) in [
        ("尾块缓冲 ON (after)", true),
        ("尾块缓冲 OFF (before)", false),
    ] {
        let scratch = ScratchDir::new("bs").unwrap();
        let store = ShadowStore::open_with_chunk_size(scratch.path().to_path_buf(), cli.chunk_size)
            .unwrap();
        let ino = store
            .create(ROOT_INO, "t.jsonl", new_attr(cli.chunk_size))
            .unwrap();
        let archive = scratch.path().join("t.jsonl");
        let r = run_on_store(
            label,
            &store,
            ino,
            &archive,
            cli,
            enabled,
            cli.fsync_every,
            || shadow_block_count(&archive),
        );
        r.print_row();
    }
}

/// 跑 ContainerStore 的「开启 vs 关闭」对照。
fn bench_container(cli: &Cli) {
    println!("[BV 容器] backend=container chunk_size={}", cli.chunk_size);
    for (label, enabled) in [
        ("尾块缓冲 ON (after)", true),
        ("尾块缓冲 OFF (before)", false),
    ] {
        let scratch = ScratchDir::new("bv").unwrap();
        let path = scratch.path().join("v.redb");
        let store = ContainerStore::open_with_chunk_size(&path, cli.chunk_size).unwrap();
        let ino = store
            .create(ROOT_INO, "t.jsonl", new_attr(cli.chunk_size))
            .unwrap();
        // BV 物理块数 = 逻辑块数（redb 覆盖同 key，无额外 slot）。
        let logical_blocks = || {
            store
                .block_geometry(ino)
                .map(|(sz, cs)| sz.div_ceil(cs as u64))
                .unwrap_or(0)
        };
        let r = run_on_store(
            label,
            &store,
            ino,
            &path,
            cli,
            enabled,
            cli.fsync_every,
            logical_blocks,
        );
        r.print_row();
    }
}

/// fsync 频率扫描：对每个后端在 ON（修复后）下跑一组 fsync 频率，输出
/// 「块数 / 压缩比 / 重压次数 / 吞吐」对照，证明碎片化是否随频率消除（§A）。
fn sweep_shadow(cli: &Cli, freqs: &[usize]) {
    println!(
        "[BS 影子树 · fsync 频率扫描] chunk_size={} lines={} line_size={}B",
        cli.chunk_size, cli.lines, cli.line_size
    );
    for &fe in freqs {
        let scratch = ScratchDir::new("bs-sweep").unwrap();
        let store = ShadowStore::open_with_chunk_size(scratch.path().to_path_buf(), cli.chunk_size)
            .unwrap();
        let ino = store
            .create(ROOT_INO, "t.jsonl", new_attr(cli.chunk_size))
            .unwrap();
        let archive = scratch.path().join("t.jsonl");
        let label = format!("fsync/{fe}");
        let r = run_on_store(&label, &store, ino, &archive, cli, true, fe, || {
            shadow_block_count(&archive)
        });
        r.print_row();
    }
}

/// 同 `sweep_shadow`，BV 容器后端。
fn sweep_container(cli: &Cli, freqs: &[usize]) {
    println!(
        "[BV 容器 · fsync 频率扫描] chunk_size={} lines={} line_size={}B",
        cli.chunk_size, cli.lines, cli.line_size
    );
    for &fe in freqs {
        let scratch = ScratchDir::new("bv-sweep").unwrap();
        let path = scratch.path().join("v.redb");
        let store = ContainerStore::open_with_chunk_size(&path, cli.chunk_size).unwrap();
        let ino = store
            .create(ROOT_INO, "t.jsonl", new_attr(cli.chunk_size))
            .unwrap();
        let logical_blocks = || {
            store
                .block_geometry(ino)
                .map(|(sz, cs)| sz.div_ceil(cs as u64))
                .unwrap_or(0)
        };
        let label = format!("fsync/{fe}");
        let r = run_on_store(&label, &store, ino, &path, cli, true, fe, logical_blocks);
        r.print_row();
    }
}

fn main() {
    let cli = Cli::parse();
    println!(
        "append-bench：lines={} line_size={}B fsync_every={} level={}（默认单次短跑）",
        cli.lines, cli.line_size, cli.fsync_every, cli.level
    );
    // fsync 频率扫描模式（§A 碎片化验证）：给了 --fsync-sweep 就只跑扫描，不跑 ON/OFF 对照。
    if !cli.fsync_sweep.is_empty() {
        println!("fsync 频率扫描：freqs={:?}（ON / 修复后）", cli.fsync_sweep);
        match cli.backend {
            BackendSel::Shadow => sweep_shadow(&cli, &cli.fsync_sweep),
            BackendSel::Container => sweep_container(&cli, &cli.fsync_sweep),
            BackendSel::Both => {
                sweep_shadow(&cli, &cli.fsync_sweep);
                sweep_container(&cli, &cli.fsync_sweep);
            }
        }
        return;
    }
    match cli.backend {
        BackendSel::Shadow => bench_shadow(&cli),
        BackendSel::Container => bench_container(&cli),
        BackendSel::Both => {
            bench_shadow(&cli);
            bench_container(&cli);
        }
    }
}
