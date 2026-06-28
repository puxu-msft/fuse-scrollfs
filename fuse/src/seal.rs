//! 冷文件封存（seal）：把 shadow archive 树里的文件用**更大块 + 高等级**离线重编码，
//! 把活跃期为随机访问选的小块（1MiB）换成冷归档的大块（默认 8MiB / zstd-19），逼近整流上界。
//!
//! ## 动机（algo-compare 结论 #4 / dict-chunk-ratio）
//! 实时块 1MiB 取随机访问甜点（~13–16x）；但会话日志写完即冷、读为归档。冷文件用大块重压可
//! 把比值推向 ~25–30x（4–8MiB/zstd-19）乃至整流 35x。本模块做**离线**封存（backing 未挂载时跑），
//! 故无需在线读路径改动——读路径本就按每文件 footer 的 chunk_size 解块，封存后照常可读。
//!
//! ## 为何无需改 codec
//! seal 块默认 8MiB，落在 zstd-19 默认窗口（windowLog 23 = 8MiB）内，`encode_all(_, 19)` 已能
//! 捕获块内长程匹配，无需显式 `--long`（仅整文件单块 > 8MiB 才需要，留作后续）。
//!
//! ## 安全
//! 每文件**临时文件 + 原子 rename** 重写，全程不破坏原文件直到新文件 fsync 落盘；失败跳过该文件
//! 不影响其余（显式收集错误，不静默吞）。只处理能被 `ArchiveReader::open` 认出的 archive，
//! 跳过目录与非 archive 文件。

use std::io;
use std::path::{Path, PathBuf};

use crate::archive::{ArchiveReader, ArchiveWriter};
use crate::core::codec::{compress, decompress_block, Algo};

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
        let mut buf: Vec<u8> = Vec::with_capacity(seal + cur_chunk as usize);
        let nblocks = reader.chunk_count();
        for idx in 0..nblocks {
            if let Some((bytes, entry)) = reader.read_block(idx)? {
                let block = decompress_block(&bytes, Algo::Zstd, entry.is_verbatim(), None)?;
                buf.extend_from_slice(&block);
            }
            // 攒满一个或多个 seal 块就压缩落盘（源块可能比 seal 块小，故 while）。
            while buf.len() >= seal {
                let (stored, verbatim) = compress(&buf[..seal], Algo::Zstd, level)?;
                writer.append_block(&stored, verbatim, seal as u64)?;
                buf.drain(..seal);
            }
        }
        // 末尾不足一个 seal 块的余量（空文件则 buf 空，写 0 块 archive）。
        if !buf.is_empty() {
            let (stored, verbatim) = compress(&buf, Algo::Zstd, level)?;
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
    fn 封存后内容_round_trip_一致_且更小() {
        let dir = tempfile::tempdir().unwrap();
        let small_chunk = 64 * 1024;
        let content = redundant_content(50_000); // 约数 MB，多个 64KiB 块
        let backing = dir.path().to_path_buf();
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
    fn 已是大块的文件被跳过_幂等() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().to_path_buf();
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
    fn 非archive文件被安全跳过() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("not-an-archive.txt"), b"hello world").unwrap();
        let stats = seal_shadow_tree(dir.path(), 1024 * 1024, 19).unwrap();
        assert_eq!(stats.sealed, 0);
        assert_eq!(stats.skipped, 1);
        assert!(stats.errors.is_empty());
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
