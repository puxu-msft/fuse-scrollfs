//! mkfixture：离线把源目录递归转成布局 S 的 BACKING archive 树（§12 P1）。
//!
//! 只读路径的「鸡生蛋」工具：没有在线写路径却要有可读数据，故离线生成。
//! 用法：`mkfixture --src <源目录> --dst <BACKING 输出目录> [--chunk-size N] [--level L]`。
//! 之后可 `zipfs --backend shadow --backing <dst> --mountpoint <mnt>` 只读挂载验证。

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use zipfs::core::codec::Algo;
use zipfs::core::DEFAULT_CHUNK_SIZE;
use zipfs::fixture::build_tree;

#[derive(Parser, Debug)]
#[command(
    name = "mkfixture",
    version,
    about = "把源目录递归转成布局 S 的 archive 树（每文件分块 zstd 压缩包）"
)]
struct Args {
    /// 源目录（真实数据）。
    #[arg(long)]
    src: PathBuf,

    /// 输出 BACKING 目录（archive 树根）。
    #[arg(long)]
    dst: PathBuf,

    /// 逻辑块大小（字节），默认 64KiB。
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE as u32)]
    chunk_size: u32,

    /// zstd 压缩等级。
    #[arg(long, default_value_t = 3)]
    level: i32,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    if !args.src.is_dir() {
        eprintln!("源目录不存在或不是目录：{}", args.src.display());
        return ExitCode::FAILURE;
    }
    if args.chunk_size == 0 {
        eprintln!("chunk_size 必须为正");
        return ExitCode::FAILURE;
    }

    match build_tree(
        &args.src,
        &args.dst,
        args.chunk_size,
        Algo::Zstd,
        args.level,
    ) {
        Ok(n) => {
            println!(
                "已写出 {n} 个 archive 文件到 {}（chunk_size={}, zstd level={}）",
                args.dst.display(),
                args.chunk_size,
                args.level
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("构建 fixture 失败：{e}");
            ExitCode::FAILURE
        }
    }
}
