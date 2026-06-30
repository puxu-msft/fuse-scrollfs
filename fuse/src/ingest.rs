//! 迁移灌入（ingest，T4）：把源目录**流式**转成布局 S archive 树 + 可选逐字节校验。
//!
//! 与 fixture.build_tree 的区别：后者 `fs::read` 整文件入内存，目标负载单文件可达数百 MB～GB
//! （docs/03 jsonl 838MB）会 OOM。本模块**逐 chunk 流式**读写（内存峰值 ~chunk_size），并可
//! `--verify` 灌后 read-back 逐字节比对。源只读、不可逆零丢失（写 dst 树，不动 src）。

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::archive::{ArchiveReader, ArchiveWriter};
use crate::core::codec::{compress, decompress_block, Algo};
use crate::core::inode::Ino;
use crate::store::container::ContainerStore;
use crate::store::{Attr, Store, StoredBlock};

/// 灌入汇总。
#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub files: u64,
    pub bytes_src: u64,
    pub bytes_archive: u64,
    pub verified: u64,
    /// 重建的符号链接数（shadow backing 是真实目录树，照原样重建，运行时经 readlink 透明服务）。
    pub symlinks: u64,
    /// 跳过的真正特殊条目数（FIFO / socket / 设备）。shadow 无法表示，调用方据此判完整性。
    pub skipped: u64,
    pub errors: Vec<(PathBuf, String)>,
}

impl IngestStats {
    pub fn ratio(&self) -> f64 {
        if self.bytes_archive == 0 {
            0.0
        } else {
            self.bytes_src as f64 / self.bytes_archive as f64
        }
    }
}

/// 递归把 `src` 目录流式灌入 `dst` archive 树。`verify` 开则逐文件 read-back 逐字节校验。
pub fn ingest_tree(
    src: &Path,
    dst: &Path,
    chunk_size: u32,
    level: i32,
    verify: bool,
) -> io::Result<IngestStats> {
    if chunk_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk_size 不能为 0",
        ));
    }
    let mut stats = IngestStats::default();
    ingest_dir(src, dst, chunk_size, level, verify, &mut stats)?;
    Ok(stats)
}

/// 把 `src` 目录灌入 **container（布局 V，redb 文件）**。经 Store API 建树 + 逐块压缩写入，
/// `verify` 开则逐文件 read-back 比对。container 无法表示符号链接/特殊文件 → 计入 `skipped`，
/// 调用方据此拒绝（避免静默丢失），与 shadow 的 special 处理一致。
pub fn ingest_tree_to_container(
    src: &Path,
    redb: &Path,
    chunk_size: u32,
    level: i32,
    verify: bool,
) -> io::Result<IngestStats> {
    if chunk_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk_size 不能为 0",
        ));
    }
    let store = ContainerStore::open_with_chunk_size(redb, chunk_size)?;
    let mut stats = IngestStats::default();
    // 根 ino 约定为 1（与 ShadowStore/容器一致）。
    ingest_dir_into_store(&store, src, 1, chunk_size, level, verify, &mut stats)?;
    store.sync_all()?;
    Ok(stats)
}

/// 递归把 `src` 目录灌入已打开的 `store`（parent ino 已建）。
fn ingest_dir_into_store(
    store: &ContainerStore,
    src: &Path,
    parent_ino: Ino,
    chunk_size: u32,
    level: i32,
    verify: bool,
    stats: &mut IngestStats,
) -> io::Result<()> {
    for dent in fs::read_dir(src)? {
        let dent = dent?;
        let ft = dent.file_type()?;
        let name = dent.file_name();
        let Some(name_str) = name.to_str() else {
            stats.skipped += 1; // 非 UTF-8 名跳过（避免静默：计入 skipped）。
            continue;
        };
        let s = src.join(&name);
        if ft.is_dir() {
            let meta = fs::symlink_metadata(&s)?;
            let attr = dir_attr(&meta);
            let child = store.mkdir(parent_ino, name_str, attr)?;
            ingest_dir_into_store(store, &s, child, chunk_size, level, verify, stats)?;
        } else if ft.is_file() {
            match ingest_file_into_store(store, &s, parent_ino, name_str, chunk_size, level, verify)
            {
                Ok((src_b, arch_b, ok)) => {
                    stats.files += 1;
                    stats.bytes_src += src_b;
                    stats.bytes_archive += arch_b;
                    stats.verified += u64::from(ok);
                }
                Err(e) => stats.errors.push((s, e.to_string())),
            }
        } else {
            // 符号链接/特殊文件：container 无法表示 → 计入 skipped（调用方拒绝，避免静默丢失）。
            stats.skipped += 1;
        }
    }
    Ok(())
}

/// 灌一个文件进 container：create → 逐 chunk 压缩 put_block → fsync；verify 则 read-back 比对。
/// 返回 (源字节, 压缩字节, 是否校验通过)。
fn ingest_file_into_store(
    store: &ContainerStore,
    src: &Path,
    parent_ino: Ino,
    name: &str,
    chunk_size: u32,
    level: i32,
    verify: bool,
) -> io::Result<(u64, u64, bool)> {
    let meta = fs::symlink_metadata(src)?;
    let attr = file_attr(&meta, chunk_size);
    let ino = store.create(parent_ino, name, attr)?;
    let mut f = fs::File::open(src)?;
    let mut buf = vec![0u8; chunk_size as usize];
    let mut idx = 0u64;
    let mut written = 0u64;
    let mut archive = 0u64;
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        let (bytes, stored_verbatim) = compress(&buf[..n], Algo::Zstd, level)?;
        archive += bytes.len() as u64;
        written += n as u64;
        store.put_block(
            ino,
            idx,
            StoredBlock {
                bytes,
                stored_verbatim,
            },
            written,
        )?;
        idx += 1;
    }
    store.fsync(ino)?;
    let ok = if verify {
        verify_file_in_store(store, src, ino)?
    } else {
        true
    };
    Ok((written, archive, ok))
}

/// 逐字节校验 container 内某 ino 与源文件一致（流式，内存 ~chunk）。
fn verify_file_in_store(store: &ContainerStore, src: &Path, ino: Ino) -> io::Result<bool> {
    let Some((size, cs)) = store.block_geometry(ino) else {
        return Ok(false);
    };
    let mut f = fs::File::open(src)?;
    let mut buf = vec![0u8; cs as usize];
    let mut idx = 0u64;
    let mut total = 0u64;
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        let blk = store
            .get_block(ino, idx)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("校验缺块 {idx}")))?;
        let plain = decompress_block(&blk.bytes, Algo::Zstd, blk.stored_verbatim, None)?;
        if plain != buf[..n] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("校验失败：{} 块 {idx} 不一致", src.display()),
            ));
        }
        idx += 1;
        total += n as u64;
    }
    // 评审 C1：源已到 EOF；还须确认 archive 不比源长（无残留多余块）且总长一致，否则
    // archive 多出的块/字节漏检。
    if store.get_block(ino, idx)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "校验失败：{} archive 比源长（块 {idx} 多余）",
                src.display()
            ),
        ));
    }
    if total != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "校验失败：{} 总长不符（校验 {total} != archive {size}）",
                src.display()
            ),
        ));
    }
    Ok(true)
}

/// 由源 metadata 构造目录 Attr。
fn dir_attr(meta: &fs::Metadata) -> Attr {
    Attr {
        ino: 0,
        size: 0,
        kind: fuser::FileType::Directory,
        perm: (meta.mode() & 0o7777) as u16,
        uid: meta.uid(),
        gid: meta.gid(),
        mtime: crate::core::system_time_from(meta.mtime(), meta.mtime_nsec()),
        atime: crate::core::system_time_from(meta.atime(), meta.atime_nsec()),
        ctime: crate::core::system_time_from(meta.ctime(), meta.ctime_nsec()),
        chunk_size: 0,
    }
}

/// 由源 metadata 构造普通文件 Attr。
fn file_attr(meta: &fs::Metadata, chunk_size: u32) -> Attr {
    Attr {
        ino: 0,
        size: 0,
        kind: fuser::FileType::RegularFile,
        perm: (meta.mode() & 0o7777) as u16,
        uid: meta.uid(),
        gid: meta.gid(),
        mtime: crate::core::system_time_from(meta.mtime(), meta.mtime_nsec()),
        atime: crate::core::system_time_from(meta.atime(), meta.atime_nsec()),
        ctime: crate::core::system_time_from(meta.ctime(), meta.ctime_nsec()),
        chunk_size,
    }
}

fn ingest_dir(
    src: &Path,
    dst: &Path,
    chunk_size: u32,
    level: i32,
    verify: bool,
    stats: &mut IngestStats,
) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for dent in fs::read_dir(src)? {
        let dent = dent?;
        let ft = dent.file_type()?;
        let name = dent.file_name();
        let s = src.join(&name);
        let d = dst.join(&name);
        if ft.is_dir() {
            ingest_dir(&s, &d, chunk_size, level, verify, stats)?;
        } else if ft.is_file() {
            match ingest_file(&s, &d, chunk_size, level, verify) {
                Ok((src_b, arch_b, ok)) => {
                    stats.files += 1;
                    stats.bytes_src += src_b;
                    stats.bytes_archive += arch_b;
                    stats.verified += u64::from(ok);
                }
                Err(e) => stats.errors.push((s, e.to_string())),
            }
        } else if ft.is_symlink() {
            // 符号链接：照原样在 backing 真实目录树里重建（target 可指向 mount 外，内核自行解析）。
            // 运行时由 shadow store + rwfs readlink 透明服务（Claude 的 `memory` 外链即此类）。
            match fs::read_link(&s).and_then(|target| std::os::unix::fs::symlink(target, &d)) {
                Ok(()) => stats.symlinks += 1,
                Err(e) => stats.errors.push((s, e.to_string())),
            }
        } else {
            // 真正特殊文件（FIFO/socket/设备）：shadow 无法表示，计入 skipped（调用方据此拒绝，
            // 避免静默丢失）。Claude projects 实测无此类。
            stats.skipped += 1;
        }
    }
    Ok(())
}

/// 流式灌一个文件：源只读、按 chunk_size 切块压缩写 dst archive。verify 则比对。返回
/// (源字节, archive 字节, 是否校验通过)。内存峰值 ~chunk_size。
fn ingest_file(
    src: &Path,
    dst: &Path,
    chunk_size: u32,
    level: i32,
    verify: bool,
) -> io::Result<(u64, u64, bool)> {
    let mut f = fs::File::open(src)?;
    let mut writer = ArchiveWriter::create(dst, chunk_size)?;
    let mut buf = vec![0u8; chunk_size as usize];
    let mut src_bytes = 0u64;
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        let (stored, verbatim) = compress(&buf[..n], Algo::Zstd, level)?;
        writer.append_block(&stored, verbatim, n as u64)?;
        src_bytes += n as u64;
    }
    writer.finish()?.sync_all()?;
    // Bug D：保留源文件 mtime/atime 到 dst archive 文件。shadow getattr 由底层文件 fs
    // metadata 取时间真值，不设则挂载点文件时间退化为注入时刻、打乱按时间排序的会话列表。
    // 诚实标注：这是**冷会话近似**——首次 append 写入即被改回 now（写会话提交重写
    // archive）。compact/seal 重写 archive 同样需补设，否则复发（见对应路径）。
    let src_meta = fs::symlink_metadata(src)?;
    let mtime = crate::core::system_time_from(src_meta.mtime(), src_meta.mtime_nsec());
    let atime = crate::core::system_time_from(src_meta.atime(), src_meta.atime_nsec());
    crate::core::set_file_times(dst, atime, mtime)?;
    let arch_bytes = fs::metadata(dst)?.len();
    let ok = if verify { verify_file(src, dst)? } else { true };
    Ok((src_bytes, arch_bytes, ok))
}

/// 逐字节校验：解压 dst 每块与 src 对应区段比对（流式，内存 ~chunk）。任一不符即 Err。
fn verify_file(src: &Path, dst: &Path) -> io::Result<bool> {
    let r = ArchiveReader::open(dst)?;
    let mut f = fs::File::open(src)?;
    let cs = r.footer().chunk_size as usize;
    let mut buf = vec![0u8; cs];
    let mut total = 0u64;
    for idx in 0..r.chunk_count() {
        let n = read_full(&mut f, &mut buf)?;
        let (bytes, entry) = r.read_block(idx)?.expect("idx < chunk_count");
        let plain = decompress_block(&bytes, Algo::Zstd, entry.is_verbatim(), None)?;
        if plain != buf[..n] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("校验失败：{} 块 {idx} 不一致", src.display()),
            ));
        }
        total += n as u64;
    }
    // 评审 C1：archive 块读尽后，源必须也到 EOF——否则 archive 块数少于源、源尾部多余字节
    // 漏检（旧码在此静默 Ok，使"逐字节校验"形同虚设）。再交叉核对总长 == archive 逻辑大小，
    // 兼防意外尾日志（ingest 不产生尾日志，故 total 应恰等 uncompressed_size）。
    let mut extra = [0u8; 1];
    if read_full(&mut f, &mut extra)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "校验失败：{} 源比 archive 长，archive 丢失尾部数据",
                src.display()
            ),
        ));
    }
    if total != r.footer().uncompressed_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "校验失败：{} 总长不符（校验 {total} != archive {}）",
                src.display(),
                r.footer().uncompressed_size
            ),
        ));
    }
    Ok(true)
}

/// 读满 buf 或到 EOF，返回实际字节数（处理短读，块边界精确）。
fn read_full(f: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_流式_round_trip_verify() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let big: Vec<u8> = (0..300_000).map(|i| b"jsonl line \n"[i % 12]).collect();
        fs::write(src.path().join("a.jsonl"), &big).unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        fs::write(src.path().join("sub/b.txt"), b"hi").unwrap();
        let s = ingest_tree(src.path(), dst.path(), 65536, 3, true).unwrap();
        assert_eq!(s.files, 2);
        assert_eq!(s.verified, 2, "verify 应全通过");
        assert!(s.errors.is_empty());
        assert!(s.ratio() > 1.0, "可压缩内容比值>1：{}", s.ratio());
        // 读回逐字节。
        let r = ArchiveReader::open(&dst.path().join("a.jsonl")).unwrap();
        let mut got = Vec::new();
        for i in 0..r.chunk_count() {
            let (b, e) = r.read_block(i).unwrap().unwrap();
            got.extend_from_slice(
                &decompress_block(&b, Algo::Zstd, e.is_verbatim(), None).unwrap(),
            );
        }
        assert_eq!(got, big);
    }

    #[test]
    fn verify_rejects_archive_shorter_than_source() {
        // 评审 C1：archive 块数少于源时，旧 verify_file 只循环 archive 块、循环后静默 Ok，
        // 源尾部多余字节漏检。手造一个仅含源前缀的 archive，verify 必须报错。
        let dst = tempfile::tempdir().unwrap();
        let srcdir = tempfile::tempdir().unwrap();
        let cs = 4u32;
        let arch = dst.path().join("short.archive");
        // archive 只封了 "AAAA"（1 块，逻辑 4 字节）。
        {
            let mut w = ArchiveWriter::create(&arch, cs).unwrap();
            let (stored, verbatim) = compress(b"AAAA", Algo::Zstd, 3).unwrap();
            w.append_block(&stored, verbatim, 4).unwrap();
            w.finish().unwrap().sync_all().unwrap();
        }
        // 源是 "AAAABBBB"（8 字节，archive 丢了尾部 "BBBB"）。
        let src = srcdir.path().join("src.bin");
        fs::write(&src, b"AAAABBBB").unwrap();

        let res = verify_file(&src, &arch);
        assert!(
            res.is_err(),
            "archive 比源短，verify 必须报错而非静默通过，实际：{res:?}"
        );
    }

    #[test]
    fn ingest_preserves_source_mtime_on_shadow() {
        // Bug D：shadow ingest 写 dst archive 文件却从不设其 mtime，挂载点文件时间
        // 退化为注入时刻，打乱 Claude Code 按时间排序会话。dst archive 文件的 fs mtime
        // 必须等于源文件 mtime（shadow getattr 由底层文件 metadata 取真值）。
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let file = src.path().join("a.jsonl");
        fs::write(&file, b"jsonl content for a session log\n").unwrap();
        // 给源文件盖一个已知的过去 mtime（2020-01-01T00:00:00Z = 1577836800）。
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800);
        crate::core::set_file_times(&file, past, past).unwrap();

        ingest_tree(src.path(), dst.path(), 65536, 3, false).unwrap();

        let dst_mtime = fs::metadata(dst.path().join("a.jsonl"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            dst_mtime, past,
            "ingest 后 dst archive 文件 mtime 应保留源文件 mtime（当前=注入时刻）"
        );
    }

    #[test]
    fn ingest_container_round_trip_verify() {
        let src = tempfile::tempdir().unwrap();
        let cdir = tempfile::tempdir().unwrap();
        let redb = cdir.path().join("c.redb");
        let big: Vec<u8> = (0..200_000).map(|i| b"jsonl line \n"[i % 12]).collect();
        fs::write(src.path().join("a.jsonl"), &big).unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        fs::write(src.path().join("sub/b.txt"), b"hello container").unwrap();

        let s = ingest_tree_to_container(src.path(), &redb, 65536, 3, true).unwrap();
        assert_eq!(s.files, 2, "两个常规文件");
        assert_eq!(s.verified, 2, "verify 全通过");
        assert!(s.errors.is_empty());
        assert_eq!(s.skipped, 0);
        assert!(s.ratio() > 1.0);

        // 重开容器逐字节读回 a.jsonl 验证持久化。
        let store = ContainerStore::open_with_chunk_size(&redb, 65536).unwrap();
        let root = store.lookup(1, "a.jsonl").unwrap();
        let (size, cs) = store.block_geometry(root.ino).unwrap();
        assert_eq!(size, big.len() as u64);
        let mut got = Vec::new();
        let nblk = size.div_ceil(cs as u64);
        for i in 0..nblk {
            let blk = store.get_block(root.ino, i).unwrap().unwrap();
            got.extend_from_slice(
                &decompress_block(&blk.bytes, Algo::Zstd, blk.stored_verbatim, None).unwrap(),
            );
        }
        assert_eq!(got, big);
    }

    #[test]
    fn ingest_container_counts_symlink_as_skipped() {
        let src = tempfile::tempdir().unwrap();
        let cdir = tempfile::tempdir().unwrap();
        let redb = cdir.path().join("c.redb");
        fs::write(src.path().join("a.jsonl"), b"hi").unwrap();
        std::os::unix::fs::symlink("/ext", src.path().join("memory")).unwrap();
        let s = ingest_tree_to_container(src.path(), &redb, 65536, 3, false).unwrap();
        assert_eq!(s.files, 1);
        assert_eq!(s.skipped, 1, "container 无法表示 symlink → 计入 skipped");
    }
}
