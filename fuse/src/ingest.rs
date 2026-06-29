//! 迁移灌入（ingest，T4）：把源目录**流式**转成布局 S archive 树 + 可选逐字节校验。
//!
//! 与 fixture.build_tree 的区别：后者 `fs::read` 整文件入内存，目标负载单文件可达数百 MB～GB
//! （docs/03 jsonl 838MB）会 OOM。本模块**逐 chunk 流式**读写（内存峰值 ~chunk_size），并可
//! `--verify` 灌后 read-back 逐字节比对。源只读、不可逆零丢失（写 dst 树，不动 src）。

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::archive::{ArchiveReader, ArchiveWriter};
use crate::core::codec::{compress, decompress_block, Algo};

/// 灌入汇总。
#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub files: u64,
    pub bytes_src: u64,
    pub bytes_archive: u64,
    pub verified: u64,
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
        }
        // symlink / 特殊文件跳过（同 fixture P1）。
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
}
