//! 离线 fixture 构建（§12 P1）：把原始数据 / 源目录转成布局 S 的 BACKING archive 树。
//!
//! 只读路径的「鸡生蛋」：没有在线写路径却要有可读数据，故离线生成测试数据
//! （见 docs/01-zipfs-design.md §12 P1「预置数据由独立离线 fixture 工具生成」）。
//! 本模块被 `mkfixture` 二进制与集成测试共享。

use std::fs;
use std::io;
use std::path::Path;

use crate::archive::ArchiveWriter;
use crate::core::chunk::block_range;
use crate::core::codec::{compress, Algo};

/// 把一段原始字节按 `chunk_size` 分块、逐块用 `algo` 压缩，写成一个 archive 文件。
///
/// 末块可不足 `chunk_size`（按真实长度累加 uncompressed_size）。空文件写成 0 块的
/// 合法 archive（footer 仍在，uncompressed_size=0）。
pub fn write_archive_from_bytes(
    dst: &Path,
    raw: &[u8],
    chunk_size: u32,
    algo: Algo,
    level: i32,
) -> io::Result<()> {
    let mut writer = ArchiveWriter::create(dst, chunk_size)?;
    let cs = chunk_size as usize;

    if !raw.is_empty() {
        // 用 block_range 求块数（与读路径同一套分块数学，避免 off-by-one 漂移）。
        let (_first, last) = block_range(0, raw.len() as u64, chunk_size as u64);
        for idx in 0..=last {
            let start = idx as usize * cs;
            let end = (start + cs).min(raw.len());
            let block = &raw[start..end];
            let (stored, verbatim) = compress(block, algo, level)?;
            writer.append_block(&stored, verbatim, block.len() as u64)?;
        }
    }

    let file = writer.finish()?;
    file.sync_all()?;
    Ok(())
}

/// 递归把源目录 `src` 转成 BACKING archive 树 `dst`：
/// - 源目录 → dst 下同名目录；
/// - 源普通文件 → dst 下同名 archive（每文件分块压缩包）。
///
/// 返回写出的 archive 文件数。符号链接 / 特殊文件 P1 跳过（§7 名字编码留作后续）。
pub fn build_tree(
    src: &Path,
    dst: &Path,
    chunk_size: u32,
    algo: Algo,
    level: i32,
) -> io::Result<usize> {
    fs::create_dir_all(dst)?;
    let mut count = 0usize;
    for dent in fs::read_dir(src)? {
        let dent = dent?;
        let ft = dent.file_type()?;
        let name = dent.file_name();
        let src_child = src.join(&name);
        let dst_child = dst.join(&name);
        if ft.is_dir() {
            count += build_tree(&src_child, &dst_child, chunk_size, algo, level)?;
        } else if ft.is_file() {
            let raw = fs::read(&src_child)?;
            write_archive_from_bytes(&dst_child, &raw, chunk_size, algo, level)?;
            count += 1;
        }
        // 符号链接 / FIFO / 设备：P1 跳过。
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ArchiveReader;
    use crate::core::codec::decompress;

    /// 读回整个 archive 的逻辑字节（解压全部块拼接），供 round-trip 校验。
    fn read_all(path: &Path, algo: Algo) -> Vec<u8> {
        let r = ArchiveReader::open(path).unwrap();
        let mut out = Vec::new();
        for idx in 0..r.chunk_count() {
            let (bytes, entry) = r.read_block(idx).unwrap().unwrap();
            out.extend_from_slice(&decompress(&bytes, algo, entry.is_verbatim()).unwrap());
        }
        out
    }

    #[test]
    fn 单文件分块_round_trip_跨多块() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("f.archive");
        // 用小 chunk 制造多块：250 字节 / chunk 64 → 4 块（末块 58）。
        let raw: Vec<u8> = (0..250u32).map(|i| (i % 251) as u8).collect();
        write_archive_from_bytes(&dst, &raw, 64, Algo::Zstd, 3).unwrap();
        let r = ArchiveReader::open(&dst).unwrap();
        assert_eq!(r.chunk_count(), 4);
        assert_eq!(r.footer().uncompressed_size, 250);
        assert_eq!(read_all(&dst, Algo::Zstd), raw);
    }

    #[test]
    fn 空文件写成零块合法_archive() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("empty.archive");
        write_archive_from_bytes(&dst, &[], 64, Algo::Zstd, 3).unwrap();
        let r = ArchiveReader::open(&dst).unwrap();
        assert_eq!(r.chunk_count(), 0);
        assert_eq!(r.footer().uncompressed_size, 0);
        assert_eq!(read_all(&dst, Algo::Zstd), Vec::<u8>::new());
    }

    #[test]
    fn build_tree_递归镜像目录() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.txt"), b"hello").unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        fs::write(src.path().join("sub").join("b.bin"), vec![7u8; 200]).unwrap();

        let n = build_tree(src.path(), dst.path(), 64, Algo::Zstd, 3).unwrap();
        assert_eq!(n, 2, "应写出 2 个 archive 文件");
        assert!(dst.path().join("a.txt").is_file());
        assert!(dst.path().join("sub").join("b.bin").is_file());
        assert_eq!(read_all(&dst.path().join("a.txt"), Algo::Zstd), b"hello");
        assert_eq!(
            read_all(&dst.path().join("sub").join("b.bin"), Algo::Zstd),
            vec![7u8; 200]
        );
    }
}
