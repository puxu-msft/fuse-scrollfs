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

use zipfs::core::blockcache::DEFAULT_CACHE_BYTES;
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
    /// 离线压实：container 回收 redb MVCC 未引用页；shadow 回收 append-only 空洞（temp+rename）。
    ///
    /// 用法：`zipfs compact --backend {container|shadow} --backing <文件或目录>`。须**未挂载**。
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

    /// 迁移灌入：把源目录流式转成布局 S archive 树（可选逐字节校验）。
    ///
    /// 用法：`zipfs ingest --src <目录> --backing <dst 树> [--verify] [--chunk-size --level]`。
    /// 源只读、流式（单文件内存 ~chunk），适合大 jsonl；`--verify` 灌后逐字节比对。
    Ingest(IngestArgs),

    /// Claude projects 透明压缩启用器：可逆切换/还原/重挂 + 状态总览 + 自挂载（TUI / 子动作）。
    ///
    /// 用法：`zipfs enable`（TUI）或 `zipfs enable list|apply|restore|remount|status|purge|autostart`。
    Enable(EnableArgs),

    /// systemd 托管挂载（内部子命令，由 `zipfs@<inst>.service` 的 ExecStart 调用）。
    ///
    /// `--name` 取 systemd **实例字符串**（escaped，模板里的 `%i`）；Rust 侧 unescape 回原名，
    /// 读 sidecar meta 自拼挂载参数后复用 `run_mount`。半灌（未提交）拒绝挂载。
    MountManaged(MountManagedArgs),

    /// systemd 托管卸载（内部子命令，供 `zipfs@<inst>.service` 的 ExecStop 调用）。
    UmountManaged(MountManagedArgs),
}

/// `mount-managed` / `umount-managed` 子命令参数。
#[derive(clap::Args, Debug)]
struct MountManagedArgs {
    /// systemd 实例字符串（escaped 形态，即模板里的 `%i`）。
    #[arg(long)]
    name: String,
}

/// `enable` 子命令参数：无子动作 → TUI。
#[derive(clap::Args, Debug)]
struct EnableArgs {
    #[command(subcommand)]
    action: Option<zipfs::enable::EnableAction>,
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

    /// FUSE 工作线程数（默认 = CPU 数，下限 4）。多线程派发降写尾 p99（单线程是结构瓶颈，
    /// ROADMAP T2）；per-inode RwLock 保并发安全（不同 inode 并行、同 inode 读读并行、写排他）。
    /// 0 = 取默认。
    #[arg(long, default_value_t = 0)]
    threads: usize,

    /// 写入自身 PID 到此文件（自挂载脚本/ systemd 用以监控、SIGTERM 干净卸载）。退出时尽力删除。
    #[arg(long)]
    pid_file: Option<PathBuf>,

    /// 协商最大单次 write 字节（默认 0=沿用 fuser/内核默认 128KiB；上限 16MiB）。显式调大可减
    /// 大行 append 拆分（2–4MiB 单条 json 少 8–32x 回调）。仅 shadow/container 生效。
    #[arg(long, default_value_t = 0)]
    max_write: u32,

    /// 启用 FUSE 写回缓存（内核合并小写、async 回刷，降写尾 p99）。去写 fd direct_io 改用 page cache；
    /// 默认关（direct_io 求 RMW 精确）。仅 shadow/container 生效。
    #[arg(long, default_value_t = false)]
    writeback: bool,

    /// Prometheus textfile 指标输出路径（.prom）。给定则守护后台每 15s 写逻辑/物理字节/压缩比，
    /// 供 node_exporter textfile collector 抓取（dep-free，仅 shadow 后端有压缩比）。
    #[arg(long)]
    metrics_file: Option<PathBuf>,

    /// 解压块缓存字节上限（perf #1），默认 **128MiB**，0 = 关闭。缓存已解压的不可变内部块明文，
    /// 消除 resume 顺序读对同一大块的重复解压（内核 ~128KiB 粒度 vs 1MiB 块 → 约 8x 解压放大）。
    /// **感知内存压力自动缩减**：按 `/proc/meminfo` MemAvailable 动态压低预算，低内存时清空自身占用。
    /// 仅 shadow/container 生效。
    #[arg(long, default_value_t = DEFAULT_CACHE_BYTES)]
    block_cache_bytes: usize,
}

/// `compact` 子命令参数。
#[derive(clap::Args, Debug)]
struct CompactArgs {
    /// 后端布局：container（redb 全包，回收 MVCC）或 shadow（每文件 archive，回收 append-only 空洞）。
    #[arg(long, value_enum, default_value_t = Backend::Container)]
    backend: Backend,

    /// 要压实的对象：container 是 redb 文件，shadow 是 backing 目录树（须未挂载）。
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
        Some(Command::Ingest(args)) => run_ingest(args),
        Some(Command::Enable(args)) => {
            // HOME 缺失时不猜测 /root：enable 操作真实用户数据，错树即误操作（fail-closed）。
            // 需要非默认根时显式用 env CLAUDE_PROJECTS / ZIPFS_HOME 覆盖。
            let home = home_or_err()?;
            zipfs::enable::run(args.action, home)
        }
        Some(Command::MountManaged(args)) => run_mount_managed(args),
        Some(Command::UmountManaged(args)) => run_umount_managed(args),
        None => run_mount(cli.mount),
    }
}

/// 解析 `$HOME`，缺失则 fail-closed（不猜测 /root；enable 系操作真实用户数据，错树即误操作）。
fn home_or_err() -> std::io::Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "未设置 HOME，拒绝猜测路径；请设 HOME，或用 CLAUDE_PROJECTS / ZIPFS_HOME 显式指定",
        )
    })
}

/// systemd 托管挂载：unescape 实例名 → 读 sidecar meta 自拼 spec → 复用 run_mount。
fn run_mount_managed(args: MountManagedArgs) -> std::io::Result<()> {
    let paths = zipfs::enable::model::Paths::resolve(&home_or_err()?);
    let name = zipfs::enable::systemd::systemd_unescape(&args.name);
    // 评审 H1：unescape 有损（裸 `-`→`/`），非法 %i 会被扭曲。出错时附原始实例名便于反查。
    let spec = zipfs::enable::systemd::resolve_managed_spec(&paths, &name).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("{e}（systemd 实例 %i={:?} → 解码名 {name:?}）", args.name),
        )
    })?;
    info!(
        "systemd 托管挂载：name={name} backing={}",
        spec.backing.display()
    );
    run_mount(mount_args_from_spec(&spec))
}

/// systemd 托管卸载（ExecStop）：unescape 实例名 → 卸载其挂载点。
fn run_umount_managed(args: MountManagedArgs) -> std::io::Result<()> {
    use zipfs::enable::daemon::Mounter;
    let paths = zipfs::enable::model::Paths::resolve(&home_or_err()?);
    let name = zipfs::enable::systemd::systemd_unescape(&args.name);
    zipfs::enable::model::validate_name(&name).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("{e}（systemd 实例 %i={:?} → 解码名 {name:?}）", args.name),
        )
    })?;
    zipfs::enable::daemon::RealMounter.unmount(&name, &paths.mountpoint(&name))
}

/// 由 `MountSpec` 构造等价的 `MountArgs`，让 managed 挂载复用 `run_mount` 全部 FUSE 装配逻辑。
fn mount_args_from_spec(spec: &zipfs::enable::daemon::MountSpec) -> MountArgs {
    use zipfs::enable::model::Backend as MBackend;
    let backend = match spec.backend {
        MBackend::Shadow => Backend::Shadow,
        MBackend::Container => Backend::Container,
    };
    MountArgs {
        backend: Some(backend),
        backing: Some(spec.backing.clone()),
        mountpoint: Some(spec.mountpoint.clone()),
        chunk_size: spec.chunk_size,
        level: spec.level,
        dict: spec.dict.clone(),
        auto_unmount: spec.auto_unmount,
        allow_other: spec.allow_other,
        no_tail_buffer: spec.no_tail_buffer,
        threads: spec.threads,
        pid_file: Some(spec.pid_file.clone()),
        max_write: spec.max_write,
        writeback: spec.writeback,
        metrics_file: spec.metrics_file.clone(),
        block_cache_bytes: spec.block_cache_bytes,
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
    // 校验 seal_chunk ≤ MAX_CHUNK_SIZE（64MiB）：过大块缓冲会 OOM，且封存逐块驻留更敏感。
    zipfs::core::validate_chunk_size(args.seal_chunk)?;
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

/// `ingest` 子命令参数：源目录 → dst archive 树流式灌入。
#[derive(clap::Args, Debug)]
struct IngestArgs {
    /// 源目录（只读，递归灌入）。
    #[arg(long)]
    src: PathBuf,
    /// 目标 backing 树（不存在则建）。
    #[arg(long)]
    backing: PathBuf,
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE as u32)]
    chunk_size: u32,
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL)]
    level: i32,
    /// 灌后逐字节 read-back 校验。
    #[arg(long, default_value_t = false)]
    verify: bool,
}

/// 流式灌入源目录到 shadow 树，报告文件数/压缩比/校验。
fn run_ingest(args: IngestArgs) -> std::io::Result<()> {
    zipfs::core::validate_chunk_size(args.chunk_size)?;
    let s = zipfs::ingest::ingest_tree(
        &args.src,
        &args.backing,
        args.chunk_size,
        args.level,
        args.verify,
    )?;
    for (p, e) in &s.errors {
        log::warn!("灌入失败（跳过）：{} — {e}", p.display());
    }
    println!(
        "ingest: files={} verified={} symlinks={} skipped={} errors={} bytes {} -> {} ({:.2}x)",
        s.files,
        s.verified,
        s.symlinks,
        s.skipped,
        s.errors.len(),
        s.bytes_src,
        s.bytes_archive,
        s.ratio()
    );
    // 评审 C2：退出码必须反映失败，否则外层脚本据 `ingest` 成功删/盖源 → 真实数据丢失。
    // errors（个别文件灌入/校验失败）或 skipped（特殊文件无法表示）任一非零都判失败。
    if !s.errors.is_empty() || s.skipped > 0 {
        return Err(std::io::Error::other(format!(
            "ingest 未完全成功：errors={} skipped={}（退出非零，勿据此删除源）",
            s.errors.len(),
            s.skipped
        )));
    }
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
    if args.backend == Backend::Shadow {
        // 布局 S：递归压实 backing 目录树（temp+rename，回收 append-only 空洞）。须未挂载。
        if !args.backing.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("shadow 压实 backing 须为目录：{}", args.backing.display()),
            ));
        }
        let s = zipfs::compact::compact_shadow_tree(&args.backing, DEFAULT_ZSTD_LEVEL)?;
        for (p, e) in &s.errors {
            log::warn!("压实失败（跳过）：{} — {e}", p.display());
        }
        info!(
            "compact shadow：compacted={} skipped={} errors={} 物理 {} → {} 字节（{:.2}x）",
            s.compacted,
            s.skipped,
            s.errors.len(),
            s.bytes_before,
            s.bytes_after,
            s.ratio()
        );
        println!(
            "compact shadow: compacted={} skipped={} bytes {} -> {} ({:.2}x)",
            s.compacted,
            s.skipped,
            s.bytes_before,
            s.bytes_after,
            s.ratio()
        );
        return Ok(());
    }
    if args.backend != Backend::Container {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "compact 支持 --backend container（布局 V）或 shadow（布局 S 目录树），收到 {:?}",
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
    zipfs::core::validate_chunk_size(args.chunk_size)?;
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
    // 多线程派发：默认取 CPU 数（下限 4、上限 64），降写尾 p99。clone_fd 让各线程独立 fd 通道。
    // 仅 Linux：fuser 0.17 对非 Linux 的 n_threads>1 / clone_fd 直接报错，故非 Linux 退回单线程。
    let mut threads = if args.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(4, 64)
    } else {
        args.threads
    };
    if !cfg!(target_os = "linux") {
        threads = 1;
    }
    cfg.n_threads = Some(threads);
    cfg.clone_fd = threads > 1;

    info!(
        "挂载 zipfs：backend={:?} backing={} -> mountpoint={} chunk_size={} level={} tail_buffer={} threads={}",
        backend,
        backing.display(),
        mountpoint.display(),
        args.chunk_size,
        args.level,
        !args.no_tail_buffer,
        threads,
    );

    let tail_buffer = !args.no_tail_buffer;
    // 加载共享字典（若 --dict 给定）：读原始字节 → 用挂载等级预消化 CDict/DDict。
    let dict = load_dict(args.dict.as_deref(), args.level)?;
    // 统一指标注册表：全 crate 单一 Arc。container 分支注入 store 用于 commit 埋点；
    // 两个读写分支都把同一 clone 传给 serve_rw 作 .prom 出口（shadow 暂无埋点但格式一致）。
    let metrics = zipfs::core::metrics::Metrics::new();
    // 写 PID 文件（自挂载脚本/systemd 监控用），退出时尽力删除。SIGKILL/panic 下 remove 不可达
    // → PID 文件可能残留；监控方须校验 PID 存活，勿仅凭文件存在判定守护活着。
    if let Some(pf) = &args.pid_file {
        std::fs::write(pf, format!("{}\n", std::process::id()))?;
    }
    let res = match backend {
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
            )
            .with_max_write(args.max_write)
            .with_writeback(args.writeback)
            .with_block_cache(args.block_cache_bytes)
            .with_metrics(metrics.clone());
            serve_rw(fs, &mountpoint, &cfg, args.metrics_file.clone(), metrics)
        }
        Backend::Container => {
            let store: Arc<dyn Store> = Arc::new(
                ContainerStore::open_with_chunk_size(&backing, args.chunk_size)?
                    .with_metrics(metrics.clone()),
            );
            let fs = ZipfsRw::with_tail_buffer(
                store,
                Algo::Zstd,
                args.level,
                args.chunk_size,
                tail_buffer,
                dict,
            )
            .with_max_write(args.max_write)
            .with_writeback(args.writeback)
            .with_block_cache(args.block_cache_bytes)
            .with_metrics(metrics.clone());
            serve_rw(fs, &mountpoint, &cfg, args.metrics_file.clone(), metrics)
        }
    };
    if let Some(pf) = &args.pid_file {
        let _ = std::fs::remove_file(pf); // 尽力清理，缺失不算错
    }
    res
}

/// 后台挂载读写 fs + 注入内核失效通知器（fsync 后失效只读 mmap 缓存）后阻塞，等价 mount2 前台。
/// 挂载就绪后向 systemd 发 READY，并起看门狗周期心跳（无 systemd / 未配 WATCHDOG 时静默降级）。
fn serve_rw(
    fs: ZipfsRw,
    mountpoint: &std::path::Path,
    cfg: &fuser::Config,
    metrics_file: Option<PathBuf>,
    metrics: Arc<zipfs::core::metrics::Metrics>,
) -> std::io::Result<()> {
    let slot = fs.notifier_slot();
    let store = fs.store_handle();
    let session = fuser::spawn_mount2(fs, mountpoint, cfg)?;
    let _ = slot.set(session.notifier()); // 注入后 fsync/flush 可发 inval_inode
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]); // 就绪（非 systemd 下静默失败）
    if let Some(usec) = systemd_watchdog_usec() {
        // detached 心跳线程：进程退出（join 返回）即随之回收，故无显式停止条件。
        let half = std::time::Duration::from_micros(usec / 2);
        std::thread::spawn(move || loop {
            std::thread::sleep(half);
            let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
        });
    }
    if let Some(path) = metrics_file {
        // detached 指标线程：每 15s 写 prometheus textfile。单一装配点——统一注册表计数
        // （廉价原子）+ compression_stats（昂贵按需 gauge）合并成一份 body，tmp+rename 原子写。
        std::thread::spawn(move || loop {
            let mut body = String::new();
            // 注册表计数（commit ok/failed、块数、flushing 峰值）。shadow 分支恒 0，格式一致。
            metrics.write_prometheus(&mut body);
            // 压缩比三 gauge（仅 shadow 有意义；container/无数据返回 None 时跳过这三行）。
            if let Some((phys, logical)) = store.compression_stats() {
                use std::fmt::Write;
                let ratio = if phys > 0 {
                    logical as f64 / phys as f64
                } else {
                    0.0
                };
                let _ = write!(
                    body,
                    "# HELP zipfs_logical_bytes 逻辑字节\n# TYPE zipfs_logical_bytes gauge\nzipfs_logical_bytes {logical}\n\
                     # HELP zipfs_physical_bytes 物理字节\n# TYPE zipfs_physical_bytes gauge\nzipfs_physical_bytes {phys}\n\
                     # HELP zipfs_compression_ratio 压缩比\n# TYPE zipfs_compression_ratio gauge\nzipfs_compression_ratio {ratio:.4}\n"
                );
            }
            let tmp = path.with_extension("prom.tmp");
            if std::fs::write(&tmp, &body)
                .and_then(|_| std::fs::rename(&tmp, &path))
                .is_err()
            {
                eprintln!("[zipfs] 写 metrics 文件失败：{}", path.display());
            }
            std::thread::sleep(std::time::Duration::from_secs(15));
        });
    }
    session.join() // 阻塞至卸载（前台守护语义不变）
}

/// systemd 看门狗周期（µs），仅当 WATCHDOG_USEC 已设；非 systemd 下 None（不起心跳）。
fn systemd_watchdog_usec() -> Option<u64> {
    let mut usec = 0u64;
    if sd_notify::watchdog_enabled(false, &mut usec) {
        Some(usec)
    } else {
        None
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
