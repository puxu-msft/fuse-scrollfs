//! scrollz microbench 驱动。
//!
//! 设计闸门问题：「redb 作为布局 V 容器、存变长压缩 chunk blob 并做随机更新，
//! 性能是否够用，还是需要自写数据区」（见 docs/01-scrollz-design.md §6/§6.1）。
//!
//! 用法：
//!   cargo run --release                        # 默认参数全跑（约 1-2GB）
//!   cargo run --release -- --backend sqlite     # 换 rusqlite 对照
//!   cargo run --release -- --quick              # 小规模冒烟（CI/调试）
//!   cargo run --release -- --chunk 256k         # 256KiB 源块档

use std::time::Instant;

use clap::Parser;
use tempfile::TempDir;

use scrollz_microbench::backend::{Backend, BackendKind, CommitPolicy};
use scrollz_microbench::blobgen::BlobSizeRange;
use scrollz_microbench::metrics::{human_bytes, ns_to_ms};
use scrollz_microbench::redb_backend::RedbBackend;
use scrollz_microbench::scenario::{
    build_initial_blocks, scenario_bulk_insert, scenario_random_rmw, RunParams,
};
use scrollz_microbench::sqlite_backend::SqliteBackend;

/// 源块档：64KiB 或 256KiB，对应不同的「压缩后变长区间」。
#[derive(Clone, Copy, Debug)]
struct ChunkProfile {
    label: &'static str,
    source: usize,
    size: BlobSizeRange,
}

fn profile_64k() -> ChunkProfile {
    // 64KiB 源块，压缩后约 8-64KB 变长。
    ChunkProfile {
        label: "64KiB",
        source: 64 * 1024,
        size: BlobSizeRange {
            min: 8 * 1024,
            max: 64 * 1024,
        },
    }
}

fn profile_256k() -> ChunkProfile {
    // 256KiB 源块，压缩后约 30-200KB 变长。
    ChunkProfile {
        label: "256KiB",
        source: 256 * 1024,
        size: BlobSizeRange {
            min: 30 * 1024,
            max: 200 * 1024,
        },
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "scrollz-microbench",
    about = "redb/sqlite 布局 V 容器：变长压缩 chunk blob 批量插入 + 随机 RMW microbench"
)]
struct Cli {
    /// 后端：redb | sqlite。
    #[arg(long, default_value = "redb")]
    backend: String,

    /// 块大小档：64k | 256k | both。
    #[arg(long, default_value = "both")]
    chunk: String,

    /// 文件数 N。
    #[arg(long, default_value_t = 200)]
    files: u64,

    /// 每文件块数 M（默认 0 = 按目标总量自动推导到约 1.5GB）。
    #[arg(long, default_value_t = 0)]
    blocks: u64,

    /// 批量事务 K。
    #[arg(long, default_value_t = 64)]
    k: usize,

    /// RMW 操作次数（默认 0 = 取总块数的 1 倍，封顶 200k）。
    #[arg(long, default_value_t = 0)]
    rmw: u64,

    /// 固定随机种子（确定性可复现）。
    #[arg(long, default_value_t = 0x5EED_1234_ABCD_0001)]
    seed: u64,

    /// 冒烟模式：极小规模，用于 CI / 快速验证管线。
    #[arg(long, default_value_t = false)]
    quick: bool,
}

/// 目标总量（字节），用于自动推导 M。默认约 1.5GiB（安全留余）。
const TARGET_TOTAL_BYTES: u64 = 1_500 * 1024 * 1024;

fn derive_blocks_per_file(cli: &Cli, prof: &ChunkProfile) -> u64 {
    if cli.blocks > 0 {
        return cli.blocks;
    }
    // 用区间中点估平均 blob 大小，反推 M 使总量≈ TARGET。
    let avg = ((prof.size.min + prof.size.max) / 2) as u64;
    let m = TARGET_TOTAL_BYTES / (cli.files * avg);
    m.max(1)
}

fn make_backend(kind: BackendKind, dir: &std::path::Path) -> Box<dyn Backend> {
    match kind {
        BackendKind::Redb => {
            let path = dir.join("container.redb");
            Box::new(RedbBackend::create(&path))
        }
        BackendKind::Sqlite => {
            let path = dir.join("container.sqlite");
            Box::new(SqliteBackend::create(&path))
        }
    }
}

/// 跑完一个 (后端, 块档) 组合的全部场景，打印结果行。
fn run_combo(kind: BackendKind, prof: ChunkProfile, cli: &Cli) {
    let blocks_per_file = derive_blocks_per_file(cli, &prof);
    let rmw_ops = if cli.rmw > 0 {
        cli.rmw
    } else {
        (cli.files * blocks_per_file).min(200_000)
    };

    let params = RunParams {
        seed: cli.seed,
        num_files: cli.files,
        blocks_per_file,
        size: prof.size,
        batch_k: cli.k,
        rmw_ops,
    };

    let total_blocks = params.total_blocks();
    let avg = ((prof.size.min + prof.size.max) / 2) as u64;
    let est_total = total_blocks * avg;

    println!("\n========================================================");
    println!(
        "后端={} | 块档={} (源 {}) | N={} 文件 × M={} 块 = {} 块",
        match kind {
            BackendKind::Redb => "redb",
            BackendKind::Sqlite => "sqlite",
        },
        prof.label,
        human_bytes(prof.source as u64),
        params.num_files,
        params.blocks_per_file,
        total_blocks
    );
    println!(
        "blob 区间={}..{} | 估算逻辑总量≈{} | RMW 次数={} | 批 K={}",
        human_bytes(prof.size.min as u64),
        human_bytes(prof.size.max as u64),
        human_bytes(est_total),
        params.rmw_ops,
        params.batch_k
    );
    println!("--------------------------------------------------------");

    // 预生成全部初始块（不计入插入计时）。
    let gen_start = Instant::now();
    let blocks = build_initial_blocks(&params);
    println!(
        "  [prep] 生成 {} 块初始数据耗时 {:.2}s",
        blocks.len(),
        gen_start.elapsed().as_secs_f64()
    );

    // --- 场景 1+3a：批量插入，Batched(K) 策略（推荐路径）---
    let dir_batched = TempDir::new().expect("创建临时目录失败");
    let mut be = make_backend(kind, dir_batched.path());
    let s_ins_batch = scenario_bulk_insert(be.as_mut(), &blocks, CommitPolicy::Batched(cli.k));
    be.sync();
    let size_after_insert = be.file_size();
    println!(
        "  [插入·批K={}]  {:>10.1} blk/s | {:>8.1} MiB/s | 容器={}",
        cli.k,
        s_ins_batch.ops_per_sec(),
        s_ins_batch.mib_per_sec(),
        human_bytes(size_after_insert)
    );

    // --- 场景 2+3：随机 RMW，Batched(K) ---
    let s_rmw_batch = scenario_random_rmw(be.as_mut(), &params, CommitPolicy::Batched(cli.k));
    be.sync();
    let size_after_rmw = be.file_size();
    let bloat = if size_after_insert > 0 {
        size_after_rmw as f64 / size_after_insert as f64
    } else {
        0.0
    };
    println!(
        "  [RMW·批K={}]  {:>10.1} op/s | p50={}ms p99={}ms p999={}ms max={}ms",
        cli.k,
        s_rmw_batch.ops_per_sec(),
        ns_to_ms(s_rmw_batch.p50_ns.unwrap_or(0)),
        ns_to_ms(s_rmw_batch.p99_ns.unwrap_or(0)),
        ns_to_ms(s_rmw_batch.p999_ns.unwrap_or(0)),
        ns_to_ms(s_rmw_batch.max_ns.unwrap_or(0))
    );

    // --- compact ---
    let size_compact = be.compact();
    println!(
        "  [空间]  插入后={} | RMW后={} (膨胀 {:.2}x) | compact后={}",
        human_bytes(size_after_insert),
        human_bytes(size_after_rmw),
        bloat,
        size_compact.map(human_bytes).unwrap_or_else(|| "n/a".into())
    );
    drop(be);
    drop(dir_batched);

    // --- 场景 3b：PerBlock 对照（陷阱量化）---
    // 为省时间，PerBlock 的插入只用一个子集（陷阱在「每块一 fsync」，子集足以量化每块成本）。
    let subset_target = (total_blocks / 20).clamp(500, 5_000) as usize;
    let subset: Vec<_> = blocks.iter().take(subset_target).cloned().collect();
    let subset_n = subset.len();
    let dir_perblk = TempDir::new().expect("创建临时目录失败");
    let mut be2 = make_backend(kind, dir_perblk.path());
    let s_ins_perblk = scenario_bulk_insert(be2.as_mut(), &subset, CommitPolicy::PerBlock);

    // PerBlock RMW：同样取子集次数，逐块独立 commit 测延迟。
    let perblk_rmw_params = RunParams {
        rmw_ops: subset_n as u64,
        ..params
    };
    let s_rmw_perblk =
        scenario_random_rmw(be2.as_mut(), &perblk_rmw_params, CommitPolicy::PerBlock);
    be2.sync();
    println!(
        "  [插入·每块]  {:>10.1} blk/s ({} 块子集)",
        s_ins_perblk.ops_per_sec(),
        subset_n
    );
    println!(
        "  [RMW·每块]  {:>10.1} op/s | p50={}ms p99={}ms p999={}ms max={}ms ({} 次)",
        s_rmw_perblk.ops_per_sec(),
        ns_to_ms(s_rmw_perblk.p50_ns.unwrap_or(0)),
        ns_to_ms(s_rmw_perblk.p99_ns.unwrap_or(0)),
        ns_to_ms(s_rmw_perblk.p999_ns.unwrap_or(0)),
        ns_to_ms(s_rmw_perblk.max_ns.unwrap_or(0)),
        subset_n
    );

    // 陷阱倍率：批量相对每块的 RMW 吞吐提升。
    let speedup = if s_rmw_perblk.ops_per_sec() > 0.0 {
        s_rmw_batch.ops_per_sec() / s_rmw_perblk.ops_per_sec()
    } else {
        0.0
    };
    println!(
        "  >>> 批量 K={} 相对每块一事务的 RMW 吞吐提升: {:.1}x",
        cli.k, speedup
    );
    drop(be2);
    drop(dir_perblk);
}

fn main() {
    let mut cli = Cli::parse();

    if cli.quick {
        // 冒烟：极小，几秒跑完，验证管线。
        cli.files = 8;
        cli.blocks = 16;
        cli.rmw = 200;
    }

    let kind = match BackendKind::parse(&cli.backend) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("参数错误: {e}");
            std::process::exit(2);
        }
    };

    let profiles: Vec<ChunkProfile> = match cli.chunk.as_str() {
        "64k" | "64K" => vec![profile_64k()],
        "256k" | "256K" => vec![profile_256k()],
        "both" => vec![profile_64k(), profile_256k()],
        other => {
            eprintln!("未知块档: {other}（可选 64k|256k|both）");
            std::process::exit(2);
        }
    };

    println!("scrollz microbench — 布局 V 容器后端闸门测试");
    println!(
        "后端={} | 种子=0x{:016X} | 临时目录(tempfile，跑完自动清理)",
        cli.backend, cli.seed
    );

    let wall = Instant::now();
    for prof in &profiles {
        run_combo(kind, *prof, &cli);
    }
    println!(
        "\n全部完成，总墙钟 {:.1}s。临时容器文件已随 TempDir 清理。",
        wall.elapsed().as_secs_f64()
    );
}
