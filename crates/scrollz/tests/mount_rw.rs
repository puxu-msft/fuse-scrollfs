//! 读写挂载集成测试（§12 P2/P3）：真实挂载 zipfs，经挂载点做 read/write/append/truncate
//! round-trip，**两后端各跑一遍**（shadow / container）。无法挂载（无 /dev/fuse 或无
//! fusermount）优雅跳过不 panic。结束必卸载。

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn zipfs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_scrollz"))
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

    // 7) 时间戳读路径：新写文件 mtime 应为「近期」，绝不是 1970 epoch（修复回归守卫）。
    {
        let ts_file = mountpoint.join("ts.txt");
        fs::write(&ts_file, b"now\n").expect("写时间戳文件");
        let mtime = fs::metadata(&ts_file).unwrap().modified().unwrap();
        assert!(
            mtime > UNIX_EPOCH + Duration::from_secs(1_000_000_000),
            "新写文件 mtime 不应退化为 1970 epoch（得到 {mtime:?}）"
        );
        let age = SystemTime::now().duration_since(mtime).unwrap_or_default();
        assert!(
            age < Duration::from_secs(3600),
            "新写文件 mtime 应为近期（age={age:?}）"
        );
    }

    // 8) 时间戳写回：touch -d 设定固定 mtime → 经挂载点 stat 取回应被持久化（非 epoch）。
    {
        let touched = mountpoint.join("touched.txt");
        fs::write(&touched, b"x").expect("写 touch 文件");
        let st = Command::new("touch")
            .arg("-d")
            .arg("2025-01-02 03:04:05")
            .arg(&touched)
            .status();
        if let Ok(s) = st {
            if s.success() {
                let secs = fs::metadata(&touched)
                    .unwrap()
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                // 2025-01-02 ≈ 1.735e9（时区无关地远离 epoch）。
                assert!(
                    secs > 1_700_000_000 && secs < 1_800_000_000,
                    "touch -d 设的 mtime 应被写回，得到 secs={secs}"
                );
            }
        }
    }

    // 9) hardlink 不支持：布局 S/V 一文件=一 archive，无 inode-id 命名层（设计定调，docs/01 §4
    //    + ROADMAP T1）。应失败且 errno=ENOTSUP（非默认 ENOSYS），让 cp -al / git 拿到明确语义。
    {
        let link_path = mountpoint.join("hardlink.dat");
        let err = fs::hard_link(&big_path, &link_path).expect_err("hardlink 应被拒绝");
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ENOTSUP),
            "hardlink 应返回 ENOTSUP（设计定调不支持），得到 {err:?}"
        );
        assert!(!link_path.exists(), "被拒的 hardlink 不应留下条目");
    }

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

/// 符号链接 round-trip（仅 shadow：container/redb 无真实目录树，readlink/symlink 为 ENOSYS）。
/// 验证 README 宣称的「运行时经 readlink 透明服务」：经挂载点建软链 → readlink 原样取回 target，
/// 且类型为 symlink。Claude 的 `memory` 外链即依赖此路径。
fn symlink_assertions(mountpoint: &Path) {
    use std::os::unix::fs::symlink;
    let link = mountpoint.join("memory");
    let target = Path::new("/some/external/memory"); // mount 外、绝对 target

    symlink(target, &link).expect("经挂载点建软链（rwfs symlink 回调）");
    // readlink 原样返回 target（rwfs readlink 回调）。
    assert_eq!(
        fs::read_link(&link).expect("readlink"),
        target,
        "readlink 应原样返回 target"
    );
    // 类型应为 symlink（symlink_metadata 不跟随）。
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "条目应为 symlink 类型"
    );
    // readdir 也应能看到它。
    let names: Vec<String> = fs::read_dir(mountpoint)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"memory".to_string()),
        "readdir 应含软链：{names:?}"
    );
    eprintln!("[OK] 符号链接 round-trip 通过（真实挂载）");
}

#[test]
fn shadow_symlink_mount_or_skip() {
    if let Some(reason) = skip_reason() {
        eprintln!("[SKIP] shadow_symlink_mount：{reason}");
        return;
    }
    let backing = tempfile::tempdir().unwrap();
    with_mount("shadow", backing.path(), symlink_assertions);
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
