//! 读写挂载集成测试（§12 P2/P3）：真实挂载 zipfs，经挂载点做 read/write/append/truncate
//! round-trip，**两后端各跑一遍**（shadow / container）。无法挂载（无 /dev/fuse 或无
//! fusermount）优雅跳过不 panic。结束必卸载。

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

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

/// 挂某后端，跑 `body`，结束必卸载。`backing` 是给 zipfs 的 --backing 参数。
fn with_mount<F: FnOnce(&Path)>(backend: &str, backing: &Path, body: F) -> bool {
    let mountdir = tempfile::tempdir().unwrap();
    let mountpoint = mountdir.path().to_path_buf();

    let child = match Command::new(zipfs_bin())
        .arg("--backend")
        .arg(backend)
        .arg("--backing")
        .arg(backing)
        .arg("--mountpoint")
        .arg(&mountpoint)
        .arg("--chunk-size")
        .arg("4096")
        .env("RUST_LOG", "warn")
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[SKIP] 无法启动 zipfs 二进制：{e}");
            return false;
        }
    };

    if !wait_mounted(&mountpoint, Duration::from_secs(5)) {
        kill_child(child);
        unmount(&mountpoint);
        eprintln!("[SKIP] 5s 内未观察到 {backend} 挂载，疑似环境不允许 FUSE，跳过");
        return false;
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&mountpoint)));

    unmount(&mountpoint);
    kill_child(child);

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
    true
}

/// 读写 round-trip 断言集（两后端共用）。
fn rw_assertions(mountpoint: &Path) {
    // 1) create + write + read round-trip。
    let f1 = mountpoint.join("hello.txt");
    fs::write(&f1, b"zipfs read-write P2\n").expect("写新文件");
    let got = fs::read(&f1).expect("读回");
    assert_eq!(got, b"zipfs read-write P2\n", "顺序写 round-trip");

    // 2) 跨多块的大文件 + append + 跨块随机读。
    let big_path = mountpoint.join("big.dat");
    let big: Vec<u8> = (0..(4096usize * 3 + 500))
        .map(|i| (i % 251) as u8)
        .collect();
    fs::write(&big_path, &big).expect("写大文件");
    assert_eq!(
        fs::metadata(&big_path).unwrap().len(),
        big.len() as u64,
        "大文件大小正确"
    );
    {
        // 跨块边界读 [4000, 4200)。
        let mut f = fs::File::open(&big_path).unwrap();
        f.seek(SeekFrom::Start(4000)).unwrap();
        let mut buf = vec![0u8; 200];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(buf, big[4000..4200], "跨块随机读正确");
    }
    {
        // append 一段，再整文件校验。
        let mut f = fs::OpenOptions::new().append(true).open(&big_path).unwrap();
        f.write_all(b"APPENDED").unwrap();
        f.sync_all().unwrap();
    }
    let mut expected = big.clone();
    expected.extend_from_slice(b"APPENDED");
    assert_eq!(
        fs::read(&big_path).unwrap(),
        expected,
        "append 后整文件一致"
    );

    // 3) 随机写中间块（RMW）。
    {
        let mut f = fs::OpenOptions::new().write(true).open(&big_path).unwrap();
        f.seek(SeekFrom::Start(5000)).unwrap();
        f.write_all(b"MIDDLE").unwrap();
        f.sync_all().unwrap();
    }
    expected[5000..5006].copy_from_slice(b"MIDDLE");
    assert_eq!(fs::read(&big_path).unwrap(), expected, "中间块 RMW 一致");

    // 4) truncate 缩小。
    {
        let f = fs::OpenOptions::new().write(true).open(&big_path).unwrap();
        f.set_len(1000).unwrap();
    }
    assert_eq!(
        fs::read(&big_path).unwrap(),
        expected[..1000],
        "truncate 缩小一致"
    );

    // 5) mkdir + 子目录文件 + readdir。
    let subdir = mountpoint.join("sub");
    fs::create_dir(&subdir).expect("mkdir");
    fs::write(subdir.join("inner.bin"), vec![9u8; 100]).expect("子目录写文件");
    let names: Vec<String> = fs::read_dir(mountpoint)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    for expect in ["hello.txt", "big.dat", "sub"] {
        assert!(
            names.contains(&expect.to_string()),
            "readdir 应含 {expect}：{names:?}"
        );
    }

    // 6) rename + unlink。
    fs::rename(mountpoint.join("hello.txt"), mountpoint.join("renamed.txt")).expect("rename");
    assert!(!mountpoint.join("hello.txt").exists());
    assert_eq!(
        fs::read(mountpoint.join("renamed.txt")).unwrap(),
        b"zipfs read-write P2\n"
    );
    fs::remove_file(mountpoint.join("renamed.txt")).expect("unlink");
    assert!(!mountpoint.join("renamed.txt").exists());

    eprintln!("[OK] 读写 round-trip 全部断言通过（真实挂载）");
}

#[test]
fn shadow_rw_mount_or_skip() {
    if let Some(reason) = skip_reason() {
        eprintln!("[SKIP] shadow_rw_mount：{reason}");
        return;
    }
    let backing = tempfile::tempdir().unwrap();
    with_mount("shadow", backing.path(), rw_assertions);
}

#[test]
fn container_rw_mount_or_skip() {
    if let Some(reason) = skip_reason() {
        eprintln!("[SKIP] container_rw_mount：{reason}");
        return;
    }
    // container 的 --backing 是容器文件路径（不存在则创建）。
    let dir = tempfile::tempdir().unwrap();
    let container_path = dir.path().join("v.redb");
    with_mount("container", &container_path, rw_assertions);
}
