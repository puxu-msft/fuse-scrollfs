//! 布局 S 压实（compact）：append-only archive 把被取代旧块/旧 index/旧尾日志 raw 留成空洞，
//! 文件单调增长（docs/04 §6/§12）。压实=离线 temp+rename 整文件重写，只保活块 + 当前尾块，
//! 丢空洞 → 物理回收，频繁 fsync 的写放大（<5x）压回 ≈稀疏。
//!
//! 与 seal 的区别：seal 换**大块 + 高等级**（冷归档）；compact 保**原 chunk_size + 原等级**，只
//! 去碎片。未封尾块（尾日志 raw）折叠为末尾封块——逻辑内容不变，读路径照常。须 backing 未挂载。

use std::io;
use std::path::{Path, PathBuf};

use crate::archive::{ArchiveReader, ArchiveWriter};
use crate::core::codec::{compress, Algo};

/// 一次压实汇总。
#[derive(Debug, Default, Clone)]
pub struct CompactStats {
    pub compacted: u64,
    pub skipped: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub errors: Vec<(PathBuf, String)>,
}

impl CompactStats {
    pub fn ratio(&self) -> f64 {
        if self.bytes_after == 0 {
            0.0
        } else {
            self.bytes_before as f64 / self.bytes_after as f64
        }
    }
}

/// 递归压实 `backing` 下所有 archive。`level` 重压等级（保活跃默认 3）。只回收空洞，不改 chunk_size。
pub fn compact_shadow_tree(backing: &Path, level: i32) -> io::Result<CompactStats> {
    if !backing.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("压实 backing 不是目录：{}", backing.display()),
        ));
    }
    // 评审 A3：取 backing 排他锁，与活守护（ShadowStore::open）互斥。否则离线 compact 的
    // temp+rename 会整文件覆盖守护正在写的版本（Bug A 同构损坏）。WouldBlock = 守护仍在。
    let _lock = crate::store::lock::acquire_backing(backing)?;
    let mut stats = CompactStats::default();
    compact_dir(backing, level, &mut stats)?;
    Ok(stats)
}

fn compact_dir(dir: &Path, level: i32, stats: &mut CompactStats) -> io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                stats.errors.push((path, e.to_string()));
                continue;
            }
        };
        if ft.is_dir() {
            if let Err(e) = compact_dir(&path, level, stats) {
                stats.errors.push((path, e.to_string())); // 子目录 IO 错也续跑
            }
        } else if ft.is_file() {
            match compact_file(&path, level) {
                Ok(Some((before, after))) => {
                    stats.compacted += 1;
                    stats.bytes_before += before;
                    stats.bytes_after += after;
                }
                Ok(None) => stats.skipped += 1,
                Err(e) => stats.errors.push((path.clone(), e.to_string())),
            }
        }
    }
    Ok(())
}

/// 压实单文件：读活块 round-trip + 折叠尾日志为末块（同 chunk_size 重压）→ temp → rename → fsync 父目录。
/// 物理无空洞者跳过（after 不小于 before）。错误清理 temp，不破坏原文件。
fn compact_file(path: &Path, level: i32) -> io::Result<Option<(u64, u64)>> {
    // Bug D：在任何读操作前捕获原文件 mtime/atime（见 seal.rs 同理），rename 覆盖后还原。
    let orig_times = crate::core::read_file_times(path);
    let reader = match ArchiveReader::open(path) {
        Ok(r) => r,
        Err(_) => return Ok(None), // 非 archive / 损坏 → 跳过
    };
    let chunk_size = reader.footer().chunk_size;
    let size_before = std::fs::metadata(path)?.len();
    let tmp = tmp_sibling(path);
    {
        let mut writer = ArchiveWriter::create(&tmp, chunk_size)?;
        // 活块原样重写（按当前 index，自然丢被取代旧块空洞）。
        for idx in 0..reader.chunk_count() {
            if let Some((bytes, entry)) = reader.read_block(idx)? {
                writer.append_block(
                    &bytes,
                    entry.is_verbatim(),
                    block_rawlen(&reader, idx, chunk_size),
                )?;
            }
        }
        // 未封尾块（尾日志 raw）折叠为末尾封块：逻辑内容不变，去掉 journal 碎片。
        if let Some(tail) = reader.read_tail()? {
            if !tail.is_empty() {
                let (stored, verbatim) = compress(&tail, Algo::Zstd, level)?;
                writer.append_block(&stored, verbatim, tail.len() as u64)?;
            }
        }
        let file = writer.finish()?;
        file.sync_all()?;
    }
    drop(reader);
    let size_after_tmp = std::fs::metadata(&tmp)?.len();
    // 无收益（无空洞）→ 丢弃 temp，跳过。
    if size_after_tmp >= size_before {
        let _ = std::fs::remove_file(&tmp);
        return Ok(None);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // rename 持久化需父目录项落盘（§6：崩溃后看到新文件而非旧）。
    crate::core::fsync_dir_of(path);
    // Bug D：还原原 archive 文件的 mtime/atime（rename 进来的新文件 mtime=now）。best-effort +
    // warn（见 seal.rs 同理）：失败不让压实失败，但要可观测、不让 Bug D 静默复发。
    if let Some((atime, mtime)) = orig_times {
        if let Err(e) = crate::core::set_file_times(path, atime, mtime) {
            log::warn!(
                "compact: 还原 {} 的 mtime 失败：{e}（该文件时间可能退化为 now）",
                path.display()
            );
        }
    }
    Ok(Some((size_before, std::fs::metadata(path)?.len())))
}

/// 第 idx 块解压后逻辑长度：末块可不足 chunk_size，用 footer uncompressed_size 推。
fn block_rawlen(reader: &ArchiveReader, idx: u64, chunk_size: u32) -> u64 {
    let cs = chunk_size as u64;
    let total = reader.footer().uncompressed_size;
    let start = idx * cs;
    (total.saturating_sub(start)).min(cs)
}

fn tmp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!(
        ".{name}.compact-tmp-{}-{nanos}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::codec::decompress_block;
    use crate::core::wsession::WriteSession;
    use crate::store::shadow::ShadowStore;
    use crate::store::Store;

    fn params() -> crate::core::rmw::CodecParams {
        crate::core::rmw::CodecParams {
            algo: Algo::Zstd,
            level: 3,
            dict: None,
        }
    }

    #[test]
    fn 压实回收频繁fsync空洞_内容一致() {
        let dir = tempfile::tempdir().unwrap();
        // backing 用 tempdir 内子目录，令 `.zipfs.lock` 落 tempdir 内（唯一+随清理），
        // 避免 backing=tempdir 时 lock 落共享 temp 根被并发测试碰撞（测试隔离缺陷）。
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let cs = 4096u32;
        let store = ShadowStore::open_with_chunk_size(backing.clone(), cs).unwrap();
        let attr = crate::store::Attr {
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
        let mut expected = Vec::new();
        for i in 0..400u32 {
            let line = format!("line {i:04} payload............\n").into_bytes();
            let off = ws.geometry(&store, ino).unwrap().0;
            ws.write_at(&store, ino, off, &line, &params()).unwrap();
            expected.extend_from_slice(&line);
            ws.seal(&store, ino, &params()).unwrap(); // 每行 fsync → 膨胀
            store.fsync(ino).unwrap();
        }
        let path = backing.join("t.jsonl");
        let before = std::fs::metadata(&path).unwrap().len();
        drop(store); // 评审 A3：compact 需 backing 锁，先释放活守护（= 卸载守护）
        let stats = compact_shadow_tree(&backing, 3).unwrap();
        assert_eq!(stats.compacted, 1, "应压实 1 文件：{:?}", stats.errors);
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "压实后更小：{before}->{after}");
        // 内容逐字节一致。
        let r = ArchiveReader::open(&path).unwrap();
        let mut got = Vec::new();
        for idx in 0..r.chunk_count() {
            let (b, e) = r.read_block(idx).unwrap().unwrap();
            got.extend_from_slice(
                &decompress_block(&b, Algo::Zstd, e.is_verbatim(), None).unwrap(),
            );
        }
        if let Some(t) = r.read_tail().unwrap() {
            got.extend_from_slice(&t);
        }
        assert_eq!(got, expected, "压实后逐字节一致");
    }

    #[test]
    fn compact_shadow_tree_blocked_while_backing_locked() {
        // 评审 A3：离线 compact 必须与活守护互斥，否则 temp+rename 覆盖守护刚写的版本
        // （Bug A 在维护路径复发）。活守护 = 一个仍持有 backing flock 的 ShadowStore。
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let _live = ShadowStore::open_with_chunk_size(backing.clone(), 4096).unwrap();
        let res = compact_shadow_tree(&backing, 3);
        assert!(
            matches!(
                res.as_ref().map_err(|e| e.kind()),
                Err(io::ErrorKind::WouldBlock)
            ),
            "backing 被活守护持锁时 compact 应得 WouldBlock，实际：{res:?}"
        );
    }

    #[test]
    fn compact_preserves_file_mtime() {
        // Bug D 延伸：compact 重写 archive（temp+rename）会把文件 mtime 重置为 now，
        // 与 ingest/seal 同样打乱按时间排序的会话列表。压实后须保留原 archive 文件 mtime。
        let dir = tempfile::tempdir().unwrap();
        // backing 用 tempdir 内子目录：`.zipfs.lock`（backing 的 sibling）随之落 tempdir 内、
        // 随清理且路径唯一。若 backing=tempdir，lock 落共享 temp 根，既残留又与其它测试
        // （tempfile 名复用）碰撞同一 flock → 偶发 WouldBlock（测试隔离缺陷，非生产 bug）。
        let backing = dir.path().join("backing");
        std::fs::create_dir(&backing).unwrap();
        let cs = 4096u32;
        let store = ShadowStore::open_with_chunk_size(backing.clone(), cs).unwrap();
        let attr = crate::store::Attr {
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
            ws.write_at(&store, ino, off, &line, &params()).unwrap();
            ws.seal(&store, ino, &params()).unwrap(); // 每行 fsync → 膨胀，制造空洞
            store.fsync(ino).unwrap();
        }
        let path = backing.join("t.jsonl");
        // 盖一个已知的过去 mtime（模拟真实会话文件已被 ingest 保留的源时间）。
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800);
        crate::core::set_file_times(&path, past, past).unwrap();

        drop(store); // 评审 A3：compact 需 backing 锁，先释放活守护
        let stats = compact_shadow_tree(&backing, 3).unwrap();
        assert_eq!(stats.compacted, 1, "应压实 1 文件：{:?}", stats.errors);

        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime, past, "compact 重写后应保留原 archive 文件 mtime");
    }
}
