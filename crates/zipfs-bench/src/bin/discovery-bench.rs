//! discovery-bench：会话发现读 micro-bench（docs/02-layered-chunking.md §6.1，第 0 步门控）。
//!
//! 量化「head 缓存」对发现读的真实收益，**零格式改动**——用现有 v1 archive + 离线对照。
//! harness 发现会话时对每个 .jsonl 读首/尾 64KB（`tan` line 30799），经 zipfs(BS) 即：
//! `ArchiveReader::open`（解析 footer+index+CRC+越界校验）+ 读块 0(1MiB) 解压取首 64KB。
//! head 缓存方案把第二项换成「解压一段独立 64KB zstd 流」。本 bench 把三段成本**分离**：
//!   (1) open 解析    —— head 缓存救不了的固定开销（审查 H2，必须独立测）
//!   (2) 块 0 读+解压取 64KB  —— 现状第二项
//!   (3) 独立 64KB 流读+解压  —— head 缓存模拟第二项
//! 决策量 = (2)−(3) 的单文件节省 × 典型 N（选择器扫描文件数），并对照 (1) 看是否真是大头。
//!
//! 热缓存（重复跑取中位）+ 冷缓存（`posix_fadvise(DONTNEED)` 免 sudo 驱逐页缓存）各一组。
//!
//! 用法：
//! ```text
//! discovery-bench --input <大 jsonl> [--chunk-size B] [--level L] [--head-bytes B] [--iters K]
//! ```

use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;

use zipfs::archive::{ArchiveReader, ArchiveWriter};
use zipfs::core::codec::{compress, decompress, Algo};
use zipfs::core::DEFAULT_CHUNK_SIZE;

#[derive(Parser, Debug)]
#[command(
    name = "discovery-bench",
    about = "会话发现读 micro-bench：head 缓存收益门控"
)]
struct Cli {
    /// 真实大 jsonl 文件（如旗舰 96MB transcript）。
    #[arg(long)]
    input: PathBuf,

    /// 逻辑块大小（字节），默认 1MiB。
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE as u32)]
    chunk_size: u32,

    /// zstd 等级，默认 3。
    #[arg(long, default_value_t = 3)]
    level: i32,

    /// 发现读窗口字节数（= harness Rv），默认 64KiB。
    #[arg(long, default_value_t = 65536)]
    head_bytes: usize,

    /// 每段测量重复次数，取中位，默认 9。
    #[arg(long, default_value_t = 9)]
    iters: usize,

    /// 外推选择器扫描的文件数列表（逗号分隔），默认 50,200,500。
    #[arg(long, default_value = "50,200,500")]
    n_files: String,
}

/// RAII 临时目录（drop 只删自己建的唯一目录，不通配）。
struct ScratchDir {
    path: PathBuf,
}
impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("zipfs-discovery-bench-{nanos}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        if self.path.starts_with(std::env::temp_dir()) && self.path.is_dir() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// `posix_fadvise(DONTNEED)`：驱逐该文件的页缓存（免 sudo），用于冷态测量。
fn evict_cache(path: &Path) {
    if let Ok(f) = std::fs::File::open(path) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        // SAFETY: fd 有效且在本作用域存活；offset=0,len=0 表示整文件；DONTNEED 仅丢弃干净页，
        // 不影响正确性（数据仍在盘上），只为制造冷缓存。
        unsafe {
            libc::posix_fadvise(
                f.as_raw_fd(),
                0,
                len as libc::off_t,
                libc::POSIX_FADV_DONTNEED,
            );
        }
    }
}

/// 取中位（拷贝排序，不改入参）。
fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// 微秒计时一个闭包。
fn time_us<F: FnMut()>(mut f: F) -> f64 {
    let t = Instant::now();
    f();
    t.elapsed().as_secs_f64() * 1e6
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    let content = std::fs::read(&cli.input)?;
    if content.len() < cli.head_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "输入文件 {}B 小于 head 窗口 {}B，无放大可言",
                content.len(),
                cli.head_bytes
            ),
        ));
    }
    let cs = cli.chunk_size as usize;
    let algo = Algo::Zstd;
    let level = cli.level;

    let scratch = ScratchDir::new()?;
    let arc_path = scratch.path.join("file.archive");
    let head_path = scratch.path.join("head.zst");

    // 建 v1 archive：逐块压缩 append（复用真实 codec + Archive 写路径）。
    {
        let mut w = ArchiveWriter::create(&arc_path, cli.chunk_size)?;
        let mut off = 0usize;
        while off < content.len() {
            let end = (off + cs).min(content.len());
            let (bytes, verbatim) = compress(&content[off..end], algo, level)?;
            w.append_block(&bytes, verbatim, (end - off) as u64)?;
            off = end;
        }
        w.finish()?.sync_all()?;
    }

    // 独立 head 缓存流：首 head_bytes 单独压一份（= head 缓存模拟），写到 sidecar 文件。
    let (head_stored, head_verbatim) = compress(&content[..cli.head_bytes], algo, level)?;
    std::fs::write(&head_path, &head_stored)?;

    let arc_total = std::fs::metadata(&arc_path)?.len();
    let block0_clen = ArchiveReader::open(&arc_path)?
        .entry(0)
        .map(|e| e.clen)
        .unwrap_or(0);

    println!(
        "# input={} {}MiB chunk={}KiB level={} head_window={}KiB",
        cli.input.display(),
        content.len() / (1024 * 1024),
        cli.chunk_size / 1024,
        level,
        cli.head_bytes / 1024,
    );
    println!(
        "# archive_total={}MiB  block0_clen={}KiB  head_cache_clen={}B (压缩后)  head_raw={}KiB",
        arc_total / (1024 * 1024),
        block0_clen / 1024,
        head_stored.len(),
        cli.head_bytes / 1024,
    );

    // ---- 三段成本，热 + 冷各测 ----
    // (1) open 解析：每次重新 open（解析 footer+index+CRC+越界）。
    // (2) 块 0 读+解压取 head_bytes：open 一次（不计时）后，反复 read_block(0)+decompress。
    // (3) 独立 head 流读+解压取 head_bytes：pread sidecar + decompress。
    for cold in [false, true] {
        let tag = if cold { "COLD" } else { "HOT " };

        // (1) open 解析
        let mut open_us = Vec::with_capacity(cli.iters);
        for _ in 0..cli.iters {
            if cold {
                evict_cache(&arc_path);
            }
            open_us.push(time_us(|| {
                let _ = ArchiveReader::open(&arc_path).unwrap();
            }));
        }

        // (2) 块 0 读+解压
        let mut blk0_us = Vec::with_capacity(cli.iters);
        for _ in 0..cli.iters {
            if cold {
                evict_cache(&arc_path);
            }
            // open 不计入本段（已在 (1) 单列）；但冷态下 open 也会预热部分页，故重新 open。
            let r = ArchiveReader::open(&arc_path).unwrap();
            blk0_us.push(time_us(|| {
                let (bytes, entry) = r.read_block(0).unwrap().unwrap();
                let plain = decompress(&bytes, algo, entry.is_verbatim()).unwrap();
                std::hint::black_box(&plain[..cli.head_bytes]);
            }));
        }

        // (3) 独立 head 流读+解压
        let mut head_us = Vec::with_capacity(cli.iters);
        for _ in 0..cli.iters {
            if cold {
                evict_cache(&head_path);
            }
            let f = std::fs::File::open(&head_path).unwrap();
            let clen = f.metadata().unwrap().len() as usize;
            head_us.push(time_us(|| {
                let mut buf = vec![0u8; clen];
                f.read_exact_at(&mut buf, 0).unwrap();
                let plain = decompress(&buf, algo, head_verbatim).unwrap();
                std::hint::black_box(&plain[..cli.head_bytes]);
            }));
        }

        let m_open = median(open_us);
        let m_blk0 = median(blk0_us);
        let m_head = median(head_us);
        let save = m_blk0 - m_head;
        println!(
            "[{tag}] open_parse={:.1}us  block0={:.1}us  headcache={:.1}us  | 单文件节省(block0-head)={:.1}us",
            m_open, m_blk0, m_head, save
        );
        // 现状 vs 带缓存的单文件发现读总成本（open 两者都付）。
        println!(
            "[{tag}]   现状/文件={:.1}us  带缓存/文件={:.1}us  节省占比={:.0}%",
            m_open + m_blk0,
            m_open + m_head,
            if m_open + m_blk0 > 0.0 {
                100.0 * save / (m_open + m_blk0)
            } else {
                0.0
            }
        );
        for n in cli
            .n_files
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
        {
            // 发现读首+尾各一次；尾对活跃文件廉价、对封存文件同块 0 成本，这里给「首尾都需解块」的上界。
            let cur = (m_open + m_blk0) * n as f64 / 1000.0;
            let with = (m_open + m_head) * n as f64 / 1000.0;
            println!(
                "[{tag}]     N={n:>4} 文件: 现状≈{cur:.1}ms  带缓存≈{with:.1}ms  省≈{:.1}ms",
                cur - with
            );
        }
    }
    Ok(())
}
