//! P1 集成测试：布局 S 影子树**只读**挂载 round-trip（§12 P1）。
//!
//! 流程：
//! 1. 造一棵已知源目录（含一个跨多块的大文件）。
//! 2. 用 lib 的 `fixture::build_tree` 离线把源目录转成 BACKING archive 树
//!    （等价于 `mkfixture` 工具，复用同一逻辑）。
//! 3. 启动已编译的 `zipfs --backend shadow --backing <archive 树> --mountpoint <mnt>` 只读挂载。
//! 4. 通过挂载点校验 read / readdir / getattr 与源一致；大文件跨 chunk 边界顺序读正确。
//! 5. 结束必卸载。
//!
//! 若环境不允许 FUSE 挂载（无 /dev/fuse 或无 fusermount），优雅跳过不 panic。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use zipfs::core::codec::Algo;
use zipfs::fixture::build_tree;

/// 小 chunk，让大文件跨多块，确保覆盖「顺序读跨 chunk 边界」路径。
const CHUNK_SIZE: u32 = 4096;
const ZSTD_LEVEL: i32 = 3;

fn zipfs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zipfs"))
}

fn skip_reason() -> Option<String> {
    if !Path::new("/dev/fuse").exists() {
        return Some("/dev/fuse 不存在".to_string());
    }
    let has = ["fusermount3", "fusermount"]
        .iter()
        .any(|b| which(b).is_some());
    if !has {
        return Some("找不到 fusermount3/fusermount".to_string());
    }
    None
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(bin))
        .find(|p| p.is_file())
}

fn wait_mounted(mountpoint: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            if mounts
                .lines()
                .any(|l| l.split_whitespace().nth(1) == mountpoint.to_str())
            {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn unmount(mountpoint: &Path) {
    for attempt in 0..5 {
        for bin in ["fusermount3", "fusermount"] {
            if which(bin).is_none() {
                continue;
            }
            if let Ok(status) = Command::new(bin).arg("-u").arg(mountpoint).status() {
                if status.success() {
                    return;
                }
            }
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn kill_child(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 造已知源数据：返回 (相对路径 → 内容) 映射。
fn source_files() -> Vec<(PathBuf, Vec<u8>)> {
    // 一个跨多块的大文件：3.5 个 chunk（伪随机但确定，含可压缩与不可压缩混合）。
    let big_len = (CHUNK_SIZE as usize) * 3 + 1234;
    let mut big = Vec::with_capacity(big_len);
    let mut x: u32 = 0xC0FF_EE11;
    for i in 0..big_len {
        // 前半段高度可压缩（重复），后半段伪随机（不可压缩），逼出 verbatim 与压缩混合。
        if i < big_len / 2 {
            big.push((i % 7) as u8);
        } else {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            big.push((x >> 24) as u8);
        }
    }
    vec![
        (PathBuf::from("hello.txt"), b"zipfs shadow P1\n".to_vec()),
        (PathBuf::from("empty.bin"), Vec::new()),
        (PathBuf::from("sub/inner.txt"), b"nested content".to_vec()),
        (PathBuf::from("sub/big.dat"), big),
    ]
}

#[test]
fn shadow_ro_round_trip_or_skip() {
    if let Some(reason) = skip_reason() {
        eprintln!("[SKIP] shadow_ro_round_trip：{reason}");
        return;
    }

    let src = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[SKIP] 无法创建 src 临时目录：{e}");
            return;
        }
    };
    let backing = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[SKIP] 无法创建 backing 临时目录：{e}");
            return;
        }
    };
    let mountdir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[SKIP] 无法创建 mount 临时目录：{e}");
            return;
        }
    };
    let mountpoint = mountdir.path().to_path_buf();

    // 1) 写源数据。
    let files = source_files();
    for (rel, content) in &files {
        let abs = src.path().join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).expect("建源子目录");
        }
        fs::write(&abs, content).expect("写源文件");
    }

    // 2) 离线建 BACKING archive 树（等价 mkfixture）。
    let n = build_tree(
        src.path(),
        backing.path(),
        CHUNK_SIZE,
        Algo::Zstd,
        ZSTD_LEVEL,
    )
    .expect("build_tree 应成功");
    assert_eq!(n, files.len(), "应为每个源文件写出一个 archive");

    // 3) 只读挂载。
    let child = match Command::new(zipfs_bin())
        .arg("--backend")
        .arg("shadow")
        .arg("--backing")
        .arg(backing.path())
        .arg("--mountpoint")
        .arg(&mountpoint)
        .env("RUST_LOG", "warn")
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[SKIP] 无法启动 zipfs 二进制：{e}");
            return;
        }
    };

    if !wait_mounted(&mountpoint, Duration::from_secs(5)) {
        kill_child(child);
        unmount(&mountpoint);
        eprintln!("[SKIP] 5s 内未观察到挂载，疑似环境不允许 FUSE 挂载，跳过");
        return;
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ro_assertions(&mountpoint, &files);
    }));

    unmount(&mountpoint);
    kill_child(child);

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn ro_assertions(mountpoint: &Path, files: &[(PathBuf, Vec<u8>)]) {
    // 1) 每个文件：getattr 大小一致 + read 全量一致（含跨多块的大文件）。
    for (rel, content) in files {
        let abs = mountpoint.join(rel);
        let meta = fs::metadata(&abs).unwrap_or_else(|e| panic!("getattr {rel:?} 失败：{e}"));
        assert_eq!(
            meta.len(),
            content.len() as u64,
            "{rel:?} 逻辑大小应等于源（getattr 取 archive footer 的 uncompressed_size）"
        );
        let got = fs::read(&abs).unwrap_or_else(|e| panic!("read {rel:?} 失败：{e}"));
        assert_eq!(got, *content, "{rel:?} 全量 read 必须 round-trip 一致");
    }

    // 2) 大文件的**部分区间 / 跨块边界**读：从 chunk 边界附近偏移读一段，验证顺序读跨块正确。
    let big_rel = PathBuf::from("sub/big.dat");
    let big_content = &files.iter().find(|(r, _)| *r == big_rel).unwrap().1;
    let big_abs = mountpoint.join(&big_rel);
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = fs::File::open(&big_abs).expect("open big");
        // 横跨第 0/1 块边界：从 4000 读 200 字节（chunk=4096）。
        let off = 4000u64;
        let len = 200usize;
        f.seek(SeekFrom::Start(off)).unwrap();
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(
            buf,
            big_content[off as usize..off as usize + len],
            "跨 chunk 边界的区间读必须正确"
        );
    }

    // 3) readdir：根目录应列出 hello.txt / empty.bin / sub。
    let root_names: HashSet<String> = fs::read_dir(mountpoint)
        .expect("readdir 根")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    for expect in ["hello.txt", "empty.bin", "sub"] {
        assert!(
            root_names.contains(expect),
            "readdir 根应含 {expect}，实际 {root_names:?}"
        );
    }
    // 子目录 readdir。
    let sub_names: HashSet<String> = fs::read_dir(mountpoint.join("sub"))
        .expect("readdir sub")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    for expect in ["inner.txt", "big.dat"] {
        assert!(
            sub_names.contains(expect),
            "readdir sub 应含 {expect}，实际 {sub_names:?}"
        );
    }

    // 4) 只读语义：写应失败（EROFS）。
    let write_res = fs::write(mountpoint.join("should_fail.txt"), b"x");
    assert!(write_res.is_err(), "只读挂载下创建/写文件必须失败");

    eprintln!("[OK] shadow 只读 round-trip 全部断言通过（真实挂载）");
}
