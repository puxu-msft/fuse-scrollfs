//! 冷文件封存（seal）：把 shadow archive 树里的文件用**更大块 + 高等级**离线重编码，
//! 把活跃期为随机访问选的小块（1MiB）换成冷归档的大块（默认 8MiB / zstd-19），逼近整流上界。
//!
//! ## 动机（algo-compare 结论 #4 / dict-chunk-ratio）
//! 实时块 1MiB 取随机访问甜点（~13–16x）；但会话日志写完即冷、读为归档。冷文件用大块重压可
//! 把比值推向 ~25–30x（4–8MiB/zstd-19）乃至整流 35x。本模块做**离线**封存（backing 未挂载时跑），
//! 故无需在线读路径改动——读路径本就按每文件 footer 的 chunk_size 解块，封存后照常可读。
//!
//! ## LDM（长程匹配）：>8MiB 块自动开启
//! 8MiB 块落在 zstd-19 默认窗口（windowLog 23 = 8MiB）内，`encode_all(_, 19)` 已能捕获块内长程匹配，
//! 无需 LDM——故默认 8MiB 封存**不开 LDM，行为零回归**。但封存块若 > 8MiB（`--seal-chunk` 调大，
//! 上限 64MiB），默认窗口跨不出整块，>8MiB 距离的文件内长程重复（系统提示 / 重读文件逐轮重录相隔
//! ≫8MiB）吃不到。此时经 [`CompressParams::sealed`] 自动开 LDM + 更大 windowLog（`ceil(log2(chunk)`)，
//! **硬 clamp ≤27** = 解码 `window_log_max`，编 ≤ 解保证封存后必可解出），逼近整流上界。
//! codec 侧 LDM 实现见 [`crate::core::codec::CompressParams`]。
//!
//! ## 安全
//! 每文件**临时文件 + 原子 rename** 重写，全程不破坏原文件直到新文件 fsync 落盘；失败跳过该文件
//! 不影响其余（显式收集错误，不静默吞）。只处理能被 `ArchiveReader::open` 认出的 archive，
//! 跳过目录与非 archive 文件。

use std::io;
use std::path::{Path, PathBuf};

use crate::archive::{ArchiveReader, ArchiveWriter};
use crate::core::codec::{compress_with_params, decompress_block, Algo, CompressParams};

/// 默认封存块大小（8MiB）：落在 zstd-19 默认窗口内，单点读解压上限 8MiB（冷数据可接受）。
pub const DEFAULT_SEAL_CHUNK: u32 = 8 * 1024 * 1024;
/// 默认封存等级（19）：冷数据一次性付 CPU，换接近整流的比值。
pub const DEFAULT_SEAL_LEVEL: i32 = 19;

/// 一次封存的汇总统计。
#[derive(Debug, Default, Clone)]
pub struct SealStats {
    /// 成功封存的文件数。
    pub sealed: u64,
    /// 跳过的文件数（非 archive / 目录 / 已是更大块）。
    pub skipped: u64,
    /// 封存前后物理字节（archive 文件大小之和，仅计被封存的文件）。
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// 每文件错误（路径 + 错误信息），不中断整体。
    pub errors: Vec<(PathBuf, String)>,
}

impl SealStats {
    pub fn ratio(&self) -> f64 {
        if self.bytes_after == 0 {
            0.0
        } else {
            self.bytes_before as f64 / self.bytes_after as f64
        }
    }
}

/// 递归封存 `backing` 下所有 archive 文件。`seal_chunk`/`level` 为目标块大小/等级。
///
/// 已是 >= `seal_chunk` 块的文件跳过（幂等，避免重复重压）。返回汇总统计；
/// 单文件错误收进 `errors` 不中断（活跃数据丢失不可接受，但封存是优化、单文件失败可重试）。
pub fn seal_shadow_tree(backing: &Path, seal_chunk: u32, level: i32) -> io::Result<SealStats> {
    if seal_chunk == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seal_chunk 不能为 0（否则每个文件都因 cur_chunk >= 0 被静默跳过）",
        ));
    }
    if !backing.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("封存 backing 不是目录：{}", backing.display()),
        ));
    }
    let _lock = crate::store::lock::acquire_backing_retry(backing)?; // 评审 A3：与活守护互斥（见 compact）
    let mut stats = SealStats::default();
    seal_dir(backing, seal_chunk, level, &mut stats)?;
    Ok(stats)
}

fn seal_dir(dir: &Path, seal_chunk: u32, level: i32, stats: &mut SealStats) -> io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        // 用 DirEntry::file_type（不跟随 symlink）判定，符号链接条目直接跳过——
        // 防 backing 树里指向祖先的 symlink 造成无限递归 / 栈溢出（rust-review M3）。
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                stats.errors.push((path, e.to_string()));
                continue;
            }
        };
        if ft.is_dir() {
            seal_dir(&path, seal_chunk, level, stats)?;
        } else if ft.is_file() {
            match seal_file(&path, seal_chunk, level) {
                Ok(Some((before, after))) => {
                    stats.sealed += 1;
                    stats.bytes_before += before;
                    stats.bytes_after += after;
                }
                Ok(None) => stats.skipped += 1,
                Err(e) => stats.errors.push((path.clone(), e.to_string())),
            }
        }
        // symlink / 其它类型：跳过（不计入 sealed/skipped）。
    }
    Ok(())
}

/// 封存单个文件。返回 `Some((before, after))` 已封存的物理字节；`None` 跳过（非 archive / 已更大块）。
///
/// 步骤：ArchiveReader 读全部块 → 解压拼接成全量 plain → 按 seal_chunk 重切 → 高等级重压 →
/// 写临时 archive → fsync → 原子 rename 覆盖原文件。
fn seal_file(path: &Path, seal_chunk: u32, level: i32) -> io::Result<Option<(u64, u64)>> {
    // Bug D：在**任何读操作前**捕获原 archive 文件的 mtime/atime（ArchiveReader::open 会在
    // strictatime 下更新 atime），rename 覆盖后须还原——否则封存把会话文件时间重置为 now，
    // 打乱按时间排序的会话列表。
    let orig_times = crate::core::read_file_times(path);
    // 只处理能被认出的 archive；否则跳过（非 archive 文件 / 损坏）。
    let reader = match ArchiveReader::open(path) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    let cur_chunk = reader.footer().chunk_size;
    // 幂等：已是 >= 目标块的文件无需再封（避免重复重压；相等也跳过）。
    if cur_chunk >= seal_chunk {
        return Ok(None);
    }
    let size_before = std::fs::metadata(path)?.len();

    // 临时文件写新 archive（同目录，便于原子 rename）。**流式封存**（rust-review H1）：逐源块解压、
    // 累进缓冲，攒满一个 seal 块即压缩落盘并 drain——内存峰值 ~seal_chunk + 一个源块，而非整文件
    // （会话日志单文件可达数百 MB～GB，全量驻留 + 重切峰值 ~2× 文件大小会 OOM）。
    let tmp = tmp_sibling(path);
    {
        let mut writer = ArchiveWriter::create(&tmp, seal_chunk)?;
        let seal = seal_chunk as usize;
        // 封存压缩参数：>8MiB 块自动开 LDM + 更大 windowLog（已 clamp ≤27）；≤8MiB 等价 plain。
        let params = CompressParams::sealed(level, seal_chunk);
        let mut buf: Vec<u8> = Vec::with_capacity(seal + cur_chunk as usize);
        let nblocks = reader.chunk_count();
        for idx in 0..nblocks {
            if let Some((bytes, entry)) = reader.read_block(idx)? {
                let block = decompress_block(&bytes, Algo::Zstd, entry.is_verbatim(), None)?;
                buf.extend_from_slice(&block);
            }
            // 攒满一个或多个 seal 块就压缩落盘（源块可能比 seal 块小，故 while）。
            while buf.len() >= seal {
                let (stored, verbatim) = compress_with_params(&buf[..seal], Algo::Zstd, &params)?;
                writer.append_block(&stored, verbatim, seal as u64)?;
                buf.drain(..seal);
            }
        }
        // 折叠未封尾块（尾日志 raw）：与 compact 一致。fsync/release 可能让未满尾块只以尾日志 raw
        // 存在、而非普通 chunk；若不读 read_tail() 就 seal，会丢失这段已 fsync 的尾部、且新 archive
        // 的 uncompressed_size 被按较小值重算（Bug：seal 丢尾日志）。并入缓冲后随普通块统一重切。
        if let Some(tail) = reader.read_tail()? {
            buf.extend_from_slice(&tail);
        }
        // 并入尾日志后可能又攒满整块，再 drain 一轮。
        while buf.len() >= seal {
            let (stored, verbatim) = compress_with_params(&buf[..seal], Algo::Zstd, &params)?;
            writer.append_block(&stored, verbatim, seal as u64)?;
            buf.drain(..seal);
        }
        // 末尾不足一个 seal 块的余量（空文件则 buf 空，写 0 块 archive）。
        if !buf.is_empty() {
            let (stored, verbatim) = compress_with_params(&buf, Algo::Zstd, &params)?;
            writer.append_block(&stored, verbatim, buf.len() as u64)?;
        }
        let file = writer.finish()?;
        file.sync_all()?; // 新 archive 落盘后再 rename，杜绝 mid-commit 丢数据。
    }
    drop(reader); // 释放对原文件的只读 fd，再 rename 覆盖。

    // 原子替换（同文件系统 rename）。失败则清理临时文件。
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // rename 持久化需父目录项落盘（docs/04 §6：崩溃后看到新文件而非旧）。
    // 与 compact.rs 一致；缺此步则崩溃后 seal 的 rename 可能丢失。
    crate::core::fsync_dir_of(path);
    // Bug D：还原原 archive 文件的 mtime/atime（rename 进来的新文件 mtime=now）。best-effort：
    // 设时间失败不该让整个封存失败（数据已正确落盘），但要 warn——否则该文件的 mtime 修复
    // 静默回退、Bug D 症状悄悄复发，不可观测。
    if let Some((atime, mtime)) = orig_times {
        if let Err(e) = crate::core::set_file_times(path, atime, mtime) {
            log::warn!(
                "seal: 还原 {} 的 mtime 失败：{e}（该文件时间可能退化为 now）",
                path.display()
            );
        }
    }
    let size_after = std::fs::metadata(path)?.len();
    Ok(Some((size_before, size_after)))
}

/// 同目录唯一临时文件名（`.<name>.seal-tmp`），用于原子 rename。
fn tmp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!(".{name}.seal-tmp-{nanos}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::shadow::ShadowStore;
    use crate::store::{Attr, Store};

    fn write_file(store: &ShadowStore, name: &str, content: &[u8], chunk_size: u32) -> u64 {
        let attr = Attr {
            ino: 0,
            size: 0,
            kind: fuser::FileType::RegularFile,
            perm: 0o644,
            uid: 0,
            gid: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            atime: std::time::SystemTime::UNIX_EPOCH,
            ctime: std::time::SystemTime::UNIX_EPOCH,
            chunk_size,
        };
        let ino = store.create(1, name, attr).unwrap();
        // 经 rmw 写路径整文件写（分块压缩落 archive）。
        let params = crate::core::rmw::CodecParams {
            algo: Algo::Zstd,
            level: 3,
            dict: None,
        };
        crate::core::rmw::write_at(store, ino, 0, content, &params).unwrap();
        store.fsync(ino).unwrap();
        ino
    }

    /// 高冗余内容：重复 boilerplate（封存大块应显著优于小块）。
    fn redundant_content(n: usize) -> Vec<u8> {
        let line =
            b"SYSTEM: you are helpful. CLAUDE.md: no-hard-wrap. tool schema {read,write}. \n";
        let mut v = Vec::new();
        for i in 0..n {
            v.extend_from_slice(line);
            v.extend_from_slice(format!("event {i}\n").as_bytes());
        }
        v
    }

    #[test]
    fn content_round_trip_consistent_and_smaller_after_seal() {
        let dir = tempfile::tempdir().unwrap();
        let small_chunk = 64 * 1024;
        let content = redundant_content(50_000); // 约数 MB，多个 64KiB 块
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        {
            let store = ShadowStore::open_with_chunk_size(backing.clone(), small_chunk).unwrap();
            write_file(&store, "t.jsonl", &content, small_chunk);
        }
        let before = dir_bytes(&backing);

        // 封存到 1MiB 块（> 64KiB，会触发）。
        let stats = seal_shadow_tree(&backing, 1024 * 1024, 19).unwrap();
        assert_eq!(stats.sealed, 1, "应封存 1 个文件");
        assert!(stats.errors.is_empty(), "无错误：{:?}", stats.errors);

        let after = dir_bytes(&backing);
        assert!(
            after < before,
            "封存后应更小：before={before} after={after}"
        );

        // round-trip：重开 store 读回，内容必须一致（读路径按新 footer chunk_size 解块）。
        let store = ShadowStore::open_with_chunk_size(backing.clone(), small_chunk).unwrap();
        let attr = store.lookup(1, "t.jsonl").unwrap();
        assert_eq!(
            attr.chunk_size,
            1024 * 1024,
            "footer chunk_size 应更新为封存块"
        );
        let got = read_whole(&store, attr.ino, attr.size);
        assert_eq!(got, content, "封存后读回内容必须一致");
    }

    #[test]
    fn seal_preserves_file_mtime() {
        // Bug D 延伸：seal 重写 archive（temp+rename）会把文件 mtime 重置为 now，
        // 与 ingest 同样打乱按时间排序的会话列表。封存后须保留原 archive 文件 mtime。
        let dir = tempfile::tempdir().unwrap();
        let small_chunk = 64 * 1024;
        let content = redundant_content(50_000);
        // backing 用 tempdir 内子目录：`.zipfs.lock` 落 tempdir 内（唯一+随清理），避免
        // backing=tempdir 时 lock 落共享 temp 根被并发测试碰撞（测试隔离缺陷，非生产 bug）。
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        {
            let store = ShadowStore::open_with_chunk_size(backing.clone(), small_chunk).unwrap();
            write_file(&store, "t.jsonl", &content, small_chunk);
        }
        let file = backing.join("t.jsonl");
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800);
        crate::core::set_file_times(&file, past, past).unwrap();

        let stats = seal_shadow_tree(&backing, 1024 * 1024, 19).unwrap();
        assert_eq!(stats.sealed, 1, "应封存 1 个文件");

        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(mtime, past, "seal 重写后应保留原 archive 文件 mtime");
    }

    /// Bug 回归：含尾日志（未封尾块只以 journal raw 存在）的 archive 经 seal 后必须逐字节不变。
    /// 用 WriteSession 每行 fsync 制造尾日志 raw（未满尾块不重压、只追加 journal），再 seal，
    /// 读回须与原始内容一致——修复前 seal 只遍历 chunk_count() 会丢掉尾日志尾部。
    #[test]
    fn seal_preserves_tail_journal_content() {
        use crate::core::wsession::WriteSession;
        let dir = tempfile::tempdir().unwrap();
        let cs = 1024 * 1024u32; // 大尾块：400 短行远不足一块，全部滞留尾日志 raw
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let params = crate::core::rmw::CodecParams {
            algo: Algo::Zstd,
            level: 3,
            dict: None,
        };
        let mut expected = Vec::new();
        {
            let store = ShadowStore::open_with_chunk_size(backing.clone(), cs).unwrap();
            let attr = Attr {
                ino: 0,
                size: 0,
                kind: fuser::FileType::RegularFile,
                perm: 0o644,
                uid: 0,
                gid: 0,
                mtime: std::time::SystemTime::UNIX_EPOCH,
                atime: std::time::SystemTime::UNIX_EPOCH,
                ctime: std::time::SystemTime::UNIX_EPOCH,
                chunk_size: cs,
            };
            let ino = store.create(1, "t.jsonl", attr).unwrap();
            let mut ws = WriteSession::new(true);
            for i in 0..400u32 {
                let line = format!("line {i:04} payload............\n").into_bytes();
                let off = ws.geometry(&store, ino).unwrap().0;
                ws.write_at(&store, ino, off, &line, &params).unwrap();
                expected.extend_from_slice(&line);
                ws.seal(&store, ino, &params).unwrap(); // 每行 fsync → 追加尾日志 raw
                store.fsync(ino).unwrap();
            }
        }
        // 前置：确认尾日志确实非空（否则测试无意义）。
        let path = backing.join("t.jsonl");
        let r = ArchiveReader::open(&path).unwrap();
        assert!(
            r.read_tail().unwrap().map(|t| !t.is_empty()).unwrap_or(false),
            "前置：seal 前 archive 应存在非空尾日志"
        );
        drop(r);

        // 封存到 4MiB 块（> 1MiB，触发）。
        let stats = seal_shadow_tree(&backing, 4 * 1024 * 1024, 19).unwrap();
        assert_eq!(stats.sealed, 1, "应封存 1 个文件：{:?}", stats.errors);

        // 读回：seal 后尾日志内容必须并入普通块、逐字节一致。
        let store = ShadowStore::open_with_chunk_size(backing.clone(), cs).unwrap();
        let attr = store.lookup(1, "t.jsonl").unwrap();
        assert_eq!(
            attr.size as usize,
            expected.len(),
            "seal 后逻辑大小须含尾日志（修复前会被算小）"
        );
        let got = read_whole(&store, attr.ino, attr.size);
        assert_eq!(got, expected, "seal 后读回必须含尾日志、逐字节一致");
    }

    #[test]
    fn already_large_block_file_skipped_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let content = redundant_content(1000);
        {
            let store = ShadowStore::open_with_chunk_size(backing.clone(), 1024 * 1024).unwrap();
            write_file(&store, "big.jsonl", &content, 1024 * 1024);
        }
        // 目标块 = 1MiB，文件已是 1MiB → 跳过。
        let stats = seal_shadow_tree(&backing, 1024 * 1024, 19).unwrap();
        assert_eq!(stats.sealed, 0);
        assert_eq!(stats.skipped, 1, "已达目标块应跳过（幂等）");
    }

    #[test]
    fn non_archive_file_safely_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        std::fs::write(backing.join("not-an-archive.txt"), b"hello world").unwrap();
        let stats = seal_shadow_tree(&backing, 1024 * 1024, 19).unwrap();
        assert_eq!(stats.sealed, 0);
        assert_eq!(stats.skipped, 1);
        assert!(stats.errors.is_empty());
    }

    /// >8MiB 距离自重复内容：两段相同伪随机块，中间夹不可压缩填充，迫使长程重复跨越 8MiB 默认窗口。
    fn long_range_dup_content() -> Vec<u8> {
        let seg_len = 4 * 1024 * 1024;
        let mut seg = Vec::with_capacity(seg_len);
        let mut x: u64 = 0xabcd_1234_5678_9f01;
        for _ in 0..seg_len {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seg.push((x >> 56) as u8);
        }
        let fill_len = 10 * 1024 * 1024;
        let mut fill = Vec::with_capacity(fill_len);
        let mut y: u64 = 0x1357_9bdf_2468_ace0;
        for _ in 0..fill_len {
            y = y
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(3_037_000_493);
            fill.push((y >> 56) as u8);
        }
        let mut out = Vec::with_capacity(seg_len * 2 + fill_len);
        out.extend_from_slice(&seg);
        out.extend_from_slice(&fill);
        out.extend_from_slice(&seg);
        out
    }

    #[test]
    fn seal_large_block_ldm_auto_on_round_trip_consistent() {
        // 封存到 16MiB 单块（>8MiB → seal 自动开 LDM）。整文件 ~18MiB 含 >8MiB 距离的 4MiB 自重复。
        // 红线：经完整 seal 管线（temp+fsync+rename）写出的 LDM 大块，读路径必须逐字节 round-trip。
        let dir = tempfile::tempdir().unwrap();
        let small_chunk = 1024 * 1024; // 1MiB 活跃块
        let content = long_range_dup_content();
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        {
            let store = ShadowStore::open_with_chunk_size(backing.clone(), small_chunk).unwrap();
            write_file(&store, "big.jsonl", &content, small_chunk);
        }
        let seal_chunk = 16 * 1024 * 1024; // >8MiB，触发 LDM
        let stats = seal_shadow_tree(&backing, seal_chunk, 19).unwrap();
        assert_eq!(stats.sealed, 1, "应封存 1 个文件");
        assert!(stats.errors.is_empty(), "无错误：{:?}", stats.errors);

        // round-trip：读路径按新 footer chunk_size（16MiB，含 LDM 帧）解块，必须逐字节一致。
        let store = ShadowStore::open_with_chunk_size(backing.clone(), small_chunk).unwrap();
        let attr = store.lookup(1, "big.jsonl").unwrap();
        assert_eq!(
            attr.chunk_size, seal_chunk,
            "footer chunk_size 应更新为封存块"
        );
        let got = read_whole(&store, attr.ino, attr.size);
        assert_eq!(got, content, "LDM 大块封存后读回必须逐字节一致");
    }

    fn dir_bytes(dir: &Path) -> u64 {
        let mut sum = 0;
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                sum += dir_bytes(&p);
            } else {
                sum += e.metadata().unwrap().len();
            }
        }
        sum
    }

    fn read_whole(store: &ShadowStore, ino: u64, size: u64) -> Vec<u8> {
        let (_, cs) = store.block_geometry(ino).unwrap();
        let cs = cs as u64;
        let mut out = vec![0u8; size as usize];
        for idx in 0..size.div_ceil(cs) {
            if let Some(b) = store.get_block(ino, idx).unwrap() {
                let plain =
                    decompress_block(&b.bytes, Algo::Zstd, b.stored_verbatim, None).unwrap();
                let start = (idx * cs) as usize;
                let end = (start + plain.len()).min(out.len());
                out[start..end].copy_from_slice(&plain[..end - start]);
            }
        }
        out
    }
}
