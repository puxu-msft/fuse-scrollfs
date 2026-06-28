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

use zipfs::core::codec::Algo;
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

    /// 逻辑块大小（字节），默认 64KiB（§6.1 裁决：不默认 256KiB）。仅 shadow/container 生效。
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE as u32)]
    chunk_size: u32,

    /// zstd 压缩等级，默认 3。可扫 1/3/9/19（btrfs 上限 15，zstd 库可到 22）。仅 shadow/container 生效。
    /// 实测：大块/字典叠加等级 19 可把 ~/.claude/projects 压缩比从 6x 推向 16–19x（docs 优化分析）。
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL)]
    level: i32,

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
        None => run_mount(cli.mount),
    }
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
