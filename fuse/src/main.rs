//! zipfs 入口：P0 透传挂载 + P2/P3 布局 S / 布局 V 读写挂载。
//!
//! 解析 `--backend {passthrough|shadow|container}` / `--backing` / `--mountpoint` / `--chunk-size`，
//! 初始化 logger，挂载。
//! - passthrough（P0，默认）：把 FUSE 操作转发到底层目录（B0 基线）。
//! - shadow（布局 S）：每文件分块压缩包，**读写**（append 只脏尾块 + footer 原子更新，§7）。
//! - container（布局 V）：redb 全包容器，**读写**（写批处理一事务，§6.1）。
//!
//! 见 docs/01-zipfs-design.md §11（模块布局）、§12 P2/P3。

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use fuser::MountOption;
use log::info;

use zipfs::core::codec::{train_dict, Algo, SharedDict};
use zipfs::core::{DEFAULT_CHUNK_SIZE, DEFAULT_ZSTD_LEVEL};
use zipfs::passthrough::PassthroughFs;
use zipfs::rwfs::ZipfsRw;
use zipfs::store::container::ContainerStore;
use zipfs::store::shadow::ShadowStore;
use zipfs::store::Store;

/// 后端布局选择。`--backend` 切换，见 §11。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// P0 透传（零压缩，B0 基线）。
    Passthrough,
    /// 布局 S 影子树，读写挂载。
    Shadow,
    /// 布局 V 容器（redb 全包），读写挂载。
    Container,
}

/// zipfs：fuser 透传（P0）/ 布局 S / 布局 V 读写 + 离线 compact。
#[derive(Parser, Debug)]
#[command(
    name = "zipfs",
    version,
    about = "zipfs：P0 透传 / 布局 S / 布局 V，把 FUSE 操作映射到底层目录、archive 树或 redb 容器"
)]
struct Cli {
    /// 子命令。缺省（不给子命令）= 挂载（向后兼容原有 `zipfs --backend ... --backing ...` 用法）。
    #[command(subcommand)]
    command: Option<Command>,

    /// 挂载参数（无子命令时使用；与 `compact` 子命令互斥）。
    #[command(flatten)]
    mount: MountArgs,
}

/// 子命令集合。挂载是默认（无子命令）路径，故此处只放非挂载操作。
#[derive(Subcommand, Debug)]
enum Command {
    /// 离线压实 container（布局 V / redb）容器文件，回收 MVCC 未引用页，收缩物理占用。
    ///
    /// 用法：`zipfs compact --backend container --backing <redb 文件>`。
    /// 须在容器**未被挂载**时执行（独占打开）。
    Compact(CompactArgs),

    /// 从语料目录训练共享 zstd 字典（T3 研究项），产出字典文件供 `--dict` 挂载使用。
    ///
    /// 用法：`zipfs train-dict --input <语料目录> --output <字典文件> [--max-dict 524288] [--chunk-size 65536]`。
    /// 把语料文件按 `--chunk-size` 切块作训练样本（对齐块独立压缩的粒度），训练上限 `--max-dict`。
    TrainDict(TrainDictArgs),

    /// 冷文件封存：把 shadow archive 树里的文件用更大块 + 高等级离线重编码（algo-compare 结论 #4）。
    ///
    /// 用法：`zipfs seal --backing <shadow 目录> [--seal-chunk 8388608] [--level 19]`。
    /// 须在 backing **未挂载**时跑（每文件临时文件 + 原子 rename）。活跃块 1MiB 换冷归档大块，
    /// 把比值从 ~16x 推向 ~25–30x。读路径无需改（按每文件 footer chunk_size 解块）。
    Seal(SealArgs),
}

/// 挂载所需参数（无子命令时生效）。所有字段 `Option`，便于在 `compact` 子命令下不强制提供。
#[derive(clap::Args, Debug)]
struct MountArgs {
    /// 后端布局。
    #[arg(long, value_enum)]
    backend: Option<Backend>,

    /// 底层对象：passthrough/shadow 下是目录；container 下是 redb 容器文件路径（不存在则创建）。
    #[arg(long)]
    backing: Option<PathBuf>,

    /// 挂载点。
    #[arg(long)]
    mountpoint: Option<PathBuf>,

    /// 逻辑块大小（字节），默认 **1MiB**（实测裁决：64KiB 砍掉长程冗余，1MiB 是甜点；
    /// 见 bench/results/dict-chunk-ratio + algo-compare）。仅 shadow/container 生效。
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE as u32)]
    chunk_size: u32,

    /// zstd 压缩等级，默认 3。可扫 1/3/9/19（btrfs 上限 15，zstd 库可到 22）。仅 shadow/container 生效。
    /// 实测：大块/字典叠加等级 19 可把 ~/.claude/projects 压缩比从 6x 推向 16–19x（docs 优化分析）。
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL)]
    level: i32,

    /// 共享 zstd 字典文件路径（`zipfs train-dict` 产出）。给定则所有块走字典压缩/解压：
    /// 在保持小块（append/RMW 友好、免 redb 膨胀）的同时把 boilerplate 长程冗余补回（T3 研究项）。
    /// **注意**：解压每块都需同一字典——字典文件须与挂载共存、不可丢失（首版由用户保管，未入容器）。
    #[arg(long)]
    dict: Option<PathBuf>,

    /// 进程退出时自动卸载（AutoUnmount）。
    #[arg(long, default_value_t = false)]
    auto_unmount: bool,

    /// 允许其他用户访问挂载点（allow_other，需 /etc/fuse.conf 放行）。
    #[arg(long, default_value_t = false)]
    allow_other: bool,

    /// 关闭未压缩开放尾块缓冲（append 优化）。默认**开启**优化；置此 flag 走旧路径
    /// （每次小 append 把尾块整块重压一遍），仅供基准前后对照（§1.1）。
    #[arg(long, default_value_t = false)]
    no_tail_buffer: bool,
}

/// `compact` 子命令参数。
#[derive(clap::Args, Debug)]
struct CompactArgs {
    /// 后端布局：当前仅 `container` 支持 compact（shadow 是每文件 archive，无全局容器可压实）。
    #[arg(long, value_enum, default_value_t = Backend::Container)]
    backend: Backend,

    /// 要压实的 container 容器文件路径（redb 文件，须已存在）。
    #[arg(long)]
    backing: PathBuf,
}

fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Compact(args)) => run_compact(args),
        Some(Command::TrainDict(args)) => run_train_dict(args),
        Some(Command::Seal(args)) => run_seal(args),
        None => run_mount(cli.mount),
    }
}

/// `seal` 子命令参数。
#[derive(clap::Args, Debug)]
struct SealArgs {
    /// 要封存的 shadow archive 树根目录（须未挂载）。
    #[arg(long)]
    backing: PathBuf,

    /// 封存目标块大小（字节），默认 8MiB（落在 zstd-19 默认窗口内）。
    #[arg(long, default_value_t = zipfs::seal::DEFAULT_SEAL_CHUNK)]
    seal_chunk: u32,

    /// 封存压缩等级，默认 19（冷数据一次性付 CPU 换高比值）。
    #[arg(long, default_value_t = zipfs::seal::DEFAULT_SEAL_LEVEL)]
    level: i32,
}

/// 离线封存 shadow 树：重编码冷文件为大块 + 高等级，报告前后大小。
fn run_seal(args: SealArgs) -> std::io::Result<()> {
    if !args.backing.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("seal backing 不是目录：{}", args.backing.display()),
        ));
    }
    info!(
        "封存 shadow 树：backing={} seal_chunk={}KiB level={}",
        args.backing.display(),
        args.seal_chunk / 1024,
        args.level
    );
    let stats = zipfs::seal::seal_shadow_tree(&args.backing, args.seal_chunk, args.level)?;
    for (path, err) in &stats.errors {
        log::warn!("封存失败（跳过）：{} — {err}", path.display());
    }
    info!(
        "封存完成：sealed={} skipped={} errors={} 物理 {} → {} 字节（{:.2}x）",
        stats.sealed,
        stats.skipped,
        stats.errors.len(),
        stats.bytes_before,
        stats.bytes_after,
        stats.ratio()
    );
    println!(
        "seal: sealed={} skipped={} errors={} bytes {} -> {} ({:.2}x smaller on sealed files)",
        stats.sealed,
        stats.skipped,
        stats.errors.len(),
        stats.bytes_before,
        stats.bytes_after,
        stats.ratio()
    );
    Ok(())
}

/// `train-dict` 子命令参数。
#[derive(clap::Args, Debug)]
struct TrainDictArgs {
    /// 语料目录：递归读取其中文件作训练样本（如 `~/.claude/projects` 副本）。
    #[arg(long)]
    input: PathBuf,

    /// 输出字典文件路径。
    #[arg(long)]
    output: PathBuf,

    /// 字典大小上限（字节），默认 512KiB（实测 512K 优于 110K）。
    #[arg(long, default_value_t = 512 * 1024)]
    max_dict: usize,

    /// 把语料文件按此块大小切块作样本（对齐块独立压缩粒度），默认 64KiB。
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE as u32)]
    chunk_size: u32,

    /// 读取语料的总字节上限（防 OOM / 控时），默认 512MiB。超过即停止采样。
    #[arg(long, default_value_t = 512 * 1024 * 1024)]
    max_sample_bytes: u64,
}

/// 从语料目录训练共享字典并写出。递归读文件 → 按 chunk_size 切样本 → `train_dict` → 落盘。
fn run_train_dict(args: TrainDictArgs) -> std::io::Result<()> {
    if !args.input.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("语料目录不存在或非目录：{}", args.input.display()),
        ));
    }
    let chunk = args.chunk_size.max(1) as usize;
    let mut samples: Vec<Vec<u8>> = Vec::new();
    let mut total: u64 = 0;
    collect_samples(
        &args.input,
        chunk,
        args.max_sample_bytes,
        &mut total,
        &mut samples,
    )?;
    if samples.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("语料目录无可读样本：{}", args.input.display()),
        ));
    }
    info!(
        "训练字典：样本 {} 块（共 {} MiB），上限 {} KiB",
        samples.len(),
        total / (1024 * 1024),
        args.max_dict / 1024
    );
    let dict = train_dict(&samples, args.max_dict)?;
    std::fs::write(&args.output, &dict)?;
    println!(
        "train-dict: wrote {} bytes dictionary to {} (from {} samples / {} MiB corpus)",
        dict.len(),
        args.output.display(),
        samples.len(),
        total / (1024 * 1024)
    );
    Ok(())
}

/// 递归采样：把目录下文件按 `chunk` 切块塞进 `samples`，受 `cap` 总字节上限约束。
fn collect_samples(
    dir: &std::path::Path,
    chunk: usize,
    cap: u64,
    total: &mut u64,
    samples: &mut Vec<Vec<u8>>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if *total >= cap {
            return Ok(());
        }
        if path.is_dir() {
            collect_samples(&path, chunk, cap, total, samples)?;
        } else if path.is_file() {
            let bytes = std::fs::read(&path)?;
            for piece in bytes.chunks(chunk) {
                if *total >= cap {
                    break;
                }
                *total += piece.len() as u64;
                samples.push(piece.to_vec());
            }
        }
    }
    Ok(())
}

/// 加载 `--dict` 字典文件并用挂载等级预消化为 `SharedDict`。无路径返回 None（不启用字典）。
fn load_dict(
    path: Option<&std::path::Path>,
    level: i32,
) -> std::io::Result<Option<Arc<SharedDict>>> {
    let Some(p) = path else {
        return Ok(None);
    };
    let raw = std::fs::read(p).map_err(|e| {
        std::io::Error::new(e.kind(), format!("无法读取字典文件 {}：{e}", p.display()))
    })?;
    let dict = SharedDict::new(raw, level).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("字典文件为空：{}", p.display()),
        )
    })?;
    info!(
        "已加载共享字典：{}（{} 字节）",
        p.display(),
        dict.raw().len()
    );
    Ok(Some(dict))
}

/// 离线 compact：打开 container 容器、调 redb `compact()`、报告前后大小。
fn run_compact(args: CompactArgs) -> std::io::Result<()> {
    if args.backend != Backend::Container {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "compact 仅支持 --backend container（布局 V）；shadow/passthrough 无全局容器可压实，收到 {:?}",
                args.backend
            ),
        ));
    }
    if !args.backing.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("container 容器文件不存在：{}", args.backing.display()),
        ));
    }
    let size_before = std::fs::metadata(&args.backing)?.len();
    info!(
        "compact container：backing={}，压实前 {} 字节",
        args.backing.display(),
        size_before
    );

    // 独占打开容器并压实（compact 须独占 &mut，无活跃事务）。
    let mut store = ContainerStore::open(&args.backing)?;
    let compacted = store.compact()?;
    drop(store); // 释放对容器文件的句柄，确保大小读数稳定。

    let size_after = std::fs::metadata(&args.backing)?.len();
    let ratio = if size_after == 0 {
        0.0
    } else {
        size_before as f64 / size_after as f64
    };
    info!(
        "compact 完成：{}（{} → {} 字节，收缩 {:.2}x，compacted={}）",
        args.backing.display(),
        size_before,
        size_after,
        ratio,
        compacted
    );
    println!(
        "compact: {} -> {} bytes ({:.2}x smaller, compacted={})",
        size_before, size_after, ratio, compacted
    );
    Ok(())
}

/// 挂载路径（无子命令）：校验必填挂载参数后按 backend 挂载。
fn run_mount(args: MountArgs) -> std::io::Result<()> {
    // backend 缺省回退 passthrough（保持原 `default_value_t = Backend::Passthrough` 行为，
    // 向后兼容 `zipfs --backing ... --mountpoint ...` 不带 --backend 的 P0 透传用法）。
    let backend = args.backend.unwrap_or(Backend::Passthrough);
    let backing = args.backing.ok_or_else(|| missing("--backing"))?;
    let mountpoint = args.mountpoint.ok_or_else(|| missing("--mountpoint"))?;

    let mut options = vec![
        MountOption::FSName("zipfs".to_string()),
        MountOption::Subtype(
            match backend {
                Backend::Passthrough => "zipfs-passthrough",
                Backend::Shadow => "zipfs-shadow",
                Backend::Container => "zipfs-container",
            }
            .to_string(),
        ),
    ];
    if args.auto_unmount {
        options.push(MountOption::AutoUnmount);
    }
    if args.allow_other {
        options.push(MountOption::CUSTOM("allow_other".to_string()));
    }

    let mut cfg = fuser::Config::default();
    cfg.mount_options.extend(options);

    info!(
        "挂载 zipfs：backend={:?} backing={} -> mountpoint={} chunk_size={} level={} tail_buffer={}",
        backend,
        backing.display(),
        mountpoint.display(),
        args.chunk_size,
        args.level,
        !args.no_tail_buffer,
    );

    let tail_buffer = !args.no_tail_buffer;
    // 加载共享字典（若 --dict 给定）：读原始字节 → 用挂载等级预消化 CDict/DDict。
    let dict = load_dict(args.dict.as_deref(), args.level)?;
    match backend {
        Backend::Passthrough => {
            let backing = canonicalize_dir(&backing)?;
            let fs = PassthroughFs::new(backing)?;
            fuser::mount2(fs, &mountpoint, &cfg)
        }
        Backend::Shadow => {
            let backing = canonicalize_dir(&backing)?;
            let store: Arc<dyn Store> =
                Arc::new(ShadowStore::open_with_chunk_size(backing, args.chunk_size)?);
            let fs = ZipfsRw::with_tail_buffer(
                store,
                Algo::Zstd,
                args.level,
                args.chunk_size,
                tail_buffer,
                dict,
            );
            fuser::mount2(fs, &mountpoint, &cfg)
        }
        Backend::Container => {
            let store: Arc<dyn Store> = Arc::new(ContainerStore::open_with_chunk_size(
                &backing,
                args.chunk_size,
            )?);
            let fs = ZipfsRw::with_tail_buffer(
                store,
                Algo::Zstd,
                args.level,
                args.chunk_size,
                tail_buffer,
                dict,
            );
            fuser::mount2(fs, &mountpoint, &cfg)
        }
    }
}

/// 构造「挂载参数缺失」错误。
fn missing(flag: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("挂载缺少必填参数 {flag}（或用子命令，如 `zipfs compact ...`）"),
    )
}

/// 规范化并校验 backing 为已存在目录，给出清晰错误。
fn canonicalize_dir(path: &PathBuf) -> std::io::Result<PathBuf> {
    let abs = std::fs::canonicalize(path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("无法访问 backing 目录 {}：{e}", path.display()),
        )
    })?;
    if !abs.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("backing 不是目录：{}", abs.display()),
        ));
    }
    Ok(abs)
}
