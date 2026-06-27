//! zipfs 入口：P0 透传挂载 + P1 布局 S 只读挂载。
//!
//! 解析 `--backend {passthrough|shadow}` / `--backing` / `--mountpoint`，初始化 logger，挂载。
//! - passthrough（P0，默认）：把 FUSE 操作转发到底层目录（B0 基线）。
//! - shadow（P1）：以**只读**方式挂 ShadowStore，读底层 archive 树（每文件分块压缩包）。
//!
//! 见 docs/01-zipfs-design.md §11（模块布局）、§12 P0/P1。
//! 写路径（P2）尚未实现；shadow 下所有写操作返回 EROFS。

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, ValueEnum};
use fuser::MountOption;
use log::info;

use zipfs::core::codec::Algo;
use zipfs::passthrough::PassthroughFs;
use zipfs::shadow_fs::ShadowRoFs;
use zipfs::store::shadow::ShadowStore;
use zipfs::store::Store;

/// 后端布局选择。`--backend` 切换，见 §11。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// P0 透传（零压缩，B0 基线）。
    Passthrough,
    /// P1 布局 S 影子树，只读挂载。
    Shadow,
}

/// zipfs：fuser 透传（P0）/ 布局 S 只读（P1）。
#[derive(Parser, Debug)]
#[command(
    name = "zipfs",
    version,
    about = "zipfs：P0 透传 / P1 布局 S 只读，把 FUSE 操作映射到底层目录或 archive 树"
)]
struct Args {
    /// 后端布局。
    #[arg(long, value_enum, default_value_t = Backend::Passthrough)]
    backend: Backend,

    /// 底层目录：passthrough 下是数据落盘目录；shadow 下是 archive 树根。
    #[arg(long)]
    backing: PathBuf,

    /// 挂载点。
    #[arg(long)]
    mountpoint: PathBuf,

    /// 进程退出时自动卸载（AutoUnmount）。
    ///
    /// 注意：本版 fuser 的 AutoUnmount 要求同时具备 allow_other / allow_root，
    /// 否则挂载会以 "auto_unmount requires acl != Owner" 失败。故默认关闭。
    #[arg(long, default_value_t = false)]
    auto_unmount: bool,

    /// 允许其他用户访问挂载点（allow_other，需 /etc/fuse.conf 放行）。
    #[arg(long, default_value_t = false)]
    allow_other: bool,
}

fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let backing = canonicalize_dir(&args.backing)?;

    let mut options = vec![
        MountOption::FSName("zipfs".to_string()),
        MountOption::Subtype(match args.backend {
            Backend::Passthrough => "zipfs-passthrough".to_string(),
            Backend::Shadow => "zipfs-shadow".to_string(),
        }),
    ];
    // shadow 是只读挂载：交给内核标 ro，写在 FUSE 层也返回 EROFS（双保险）。
    if args.backend == Backend::Shadow {
        options.push(MountOption::RO);
    }
    if args.auto_unmount {
        options.push(MountOption::AutoUnmount);
    }
    if args.allow_other {
        options.push(MountOption::CUSTOM("allow_other".to_string()));
    }

    let mut cfg = fuser::Config::default();
    cfg.mount_options.extend(options);

    info!(
        "挂载 zipfs：backend={:?} backing={} -> mountpoint={}",
        args.backend,
        backing.display(),
        args.mountpoint.display()
    );

    // mount2 阻塞直到卸载（Ctrl-C 或 fusermount -u）。
    match args.backend {
        Backend::Passthrough => {
            let fs = PassthroughFs::new(backing)?;
            fuser::mount2(fs, &args.mountpoint, &cfg)
        }
        Backend::Shadow => {
            let store: Arc<dyn Store> = Arc::new(ShadowStore::open(backing)?);
            // P1 固定 zstd（fixture 用 zstd 生成）；algo 后续随包头/参数可配。
            let fs = ShadowRoFs::new(store, Algo::Zstd);
            fuser::mount2(fs, &args.mountpoint, &cfg)
        }
    }
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
