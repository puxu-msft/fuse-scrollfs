//! ldm-ratio：zstd 长程匹配（LDM / `--long`）在真实语料上的压缩比补测（M2）。
//!
//! 回答「>8MiB 封存块开 LDM 相比不开，比值提升多少」，作为「是否调大默认
//! [`DEFAULT_SEAL_CHUNK`](scrollz::seal::DEFAULT_SEAL_CHUNK)」的决策依据。
//!
//! 与 [`ratio-bench`](../ratio-bench) 的关键区别：ratio-bench 走 Store+rmw 的 live 写路径
//! （`compress_block` / `CompressParams::plain`，**不含 LDM**），量不了 LDM。本 bin 直接对每个
//! 文件按 `chunk_size` 切块、每块调 [`compress_with_params`] 求和存储字节，用
//! [`CompressParams::sized`] 只切换 `enable_ldm` 一个变量做**同基准**对照（同 chunk、同等级、
//! 同方法），LDM on 与 off 唯一差别就是长程匹配开关。
//!
//! ## 方法学（务必守）
//! - LDM 仅对 ≥8MiB 的**单块**有效：块 <8MiB 时 windowLog<23，LDM 近乎 no-op。故收益只可能来自
//!   语料里的大 transcript 文件。输出同时报告 `ldm_eligible_files`（≥8MiB 文件数）。
//! - 尊重 verbatim：不可压缩块原样存，`stored.len() == raw.len()`，直接计入物理字节。
//! - 顺带对**首个 ≥8MiB 文件**逐块 round-trip（[`decompress_block`] with window_log_max=27）逐字节
//!   校验，防「测量用的压缩帧其实解不出」。
//!
//! 用法：
//! ```text
//! ldm-ratio --input <语料目录> --chunk-size <bytes> [--level 19] [--long] [--max-bytes N]
//! ```
//! 单配置单行输出；矩阵由 `bench/scripts/ldm-ratio-matrix.sh` 驱动。

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;

use scrollz::core::codec::{compress_with_params, decompress_block, Algo, CompressParams};

/// LDM 起效阈值（8MiB）：块 ≤ 此值时 windowLog ≤23，落在 zstd 默认窗口内，LDM 近乎 no-op。
/// 与 [`CompressParams::sealed`] / [`scrollz::seal::DEFAULT_SEAL_CHUNK`] 的阈值一致。
const LDM_ELIGIBLE_THRESHOLD: u64 = 8 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "ldm-ratio",
    about = "LDM 压缩比补测：逐块 compress_with_params，只切换 enable_ldm"
)]
struct Cli {
    /// 语料目录（递归读取其中文件）。
    #[arg(long)]
    input: PathBuf,

    /// 逻辑块大小（字节）。LDM 只对 >8MiB 的块起效。
    #[arg(long)]
    chunk_size: u32,

    /// zstd 等级，默认 19（对齐 seal 真实等级）。
    #[arg(long, default_value_t = 19)]
    level: i32,

    /// 开启长程匹配（LDM / `--long`）；不传即关（`--no-long` 语义）。
    #[arg(long, default_value_t = false)]
    long: bool,

    /// 读取语料总字节上限（控时 / 防 OOM），默认 256MiB。
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    max_bytes: u64,
}

/// 递归收集普通文件路径（确定性排序），受总字节上限约束（对齐 ratio-bench 收集语义）。
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
        // 跳过符号链接：不跟进语料外目录（如 `memory -> /home/.../ghc2api-go/docs/memory`），
        // 否则多计外部文件、污染 files 计数。`is_dir`/`is_file` 会跟随 symlink，故先判类型。
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            continue;
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

/// 逐块压缩一个文件，返回 `(logical, physical)`。同时（若 `verify`）对每块 round-trip 逐字节校验。
fn compress_file(
    content: &[u8],
    chunk_size: usize,
    params: &CompressParams,
    verify: bool,
) -> std::io::Result<(u64, u64)> {
    let mut logical = 0u64;
    let mut physical = 0u64;
    for chunk in content.chunks(chunk_size) {
        let (stored, verbatim) = compress_with_params(chunk, Algo::Zstd, params)?;
        // verbatim 时 stored 即原始字节，len 与 raw 相同；直接计入物理字节。
        physical += stored.len() as u64;
        logical += chunk.len() as u64;
        if verify {
            let back = decompress_block(&stored, Algo::Zstd, verbatim, None)?;
            if back != chunk {
                return Err(std::io::Error::other(format!(
                    "round-trip 校验失败：块 {} 字节解回 != 原始（LDM 帧解不出 = 损坏）",
                    chunk.len()
                )));
            }
        }
    }
    Ok((logical, physical))
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

    let mut files = Vec::new();
    let mut scanned = 0u64;
    collect_files(&cli.input, cli.max_bytes, &mut scanned, &mut files)?;
    if files.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "语料目录无可读文件",
        ));
    }

    // 同基准对照：唯一变量是 enable_ldm。sized 忠实按 --long 开/关，不做 sealed 的 8MiB 自动回退。
    let params = CompressParams::sized(cli.level, cli.chunk_size, cli.long);
    let chunk_size = cli.chunk_size as usize;

    let mut logical = 0u64;
    let mut physical = 0u64;
    let mut eligible = 0u64;
    let mut verified_first = false;
    let start = Instant::now();
    for path in &files {
        let content = std::fs::read(path)?;
        if content.is_empty() {
            continue;
        }
        let is_eligible = content.len() as u64 >= LDM_ELIGIBLE_THRESHOLD;
        if is_eligible {
            eligible += 1;
        }
        // 仅对首个 ≥8MiB 文件做逐块 round-trip 校验（抓 LDM 帧解码正确性 bug）。
        let verify = is_eligible && !verified_first;
        let (l, p) = compress_file(&content, chunk_size, &params, verify)?;
        if verify {
            verified_first = true;
        }
        logical += l;
        physical += p;
    }
    let elapsed = start.elapsed().as_secs_f64();

    let ratio = if physical == 0 {
        0.0
    } else {
        logical as f64 / physical as f64
    };
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);

    println!(
        "chunk={}MiB level={} long={} files={} ldm_eligible_files={}(>=8MiB) logical={:.1}MiB physical={:.1}MiB ratio={:.3}x time={:.1}s",
        cli.chunk_size / (1024 * 1024),
        cli.level,
        if cli.long { "on" } else { "off" },
        files.len(),
        eligible,
        mib(logical),
        mib(physical),
        ratio,
        elapsed,
    );
    Ok(())
}
