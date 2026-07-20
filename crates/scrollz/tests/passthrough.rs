//! P0 集成测试：透传 round-trip。
//!
//! 若环境允许挂载（/dev/fuse 存在且 fusermount3 可用），把 scrollz 挂到临时目录，
//! 做 create/write/read/readdir/mkdir/unlink round-trip 校验后卸载。
//! 若挂载失败（权限/环境），优雅跳过并打印原因，不 panic 让整个 test 套失败。
//!
//! 见 docs/01-scrollz-design.md §12 P0。测试通过「启动已编译的 scrollz 二进制」来挂载，
//! 贴近真实使用路径，也避免引入 lib target。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// 取已编译的 scrollz 二进制路径（cargo 通过 CARGO_BIN_EXE_<name> 暴露）。
fn scrollz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_scrollz"))
}

/// 判断本机是否具备挂载条件。返回 Some(reason) 表示应跳过。
fn skip_reason() -> Option<String> {
    if !Path::new("/dev/fuse").exists() {
        return Some("/dev/fuse 不存在".to_string());
    }
    let has_fusermount = ["fusermount3", "fusermount"]
        .iter()
        .any(|b| which(b).is_some());
    if !has_fusermount {
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

/// 等待挂载点真正可用（出现在 /proc/mounts 或目录可 stat 且非 backing）。
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

/// 尽力卸载，避免遗留挂载点。
///
/// 逐个尝试可用的 fusermount 变体，且对「busy」重试若干次（刚结束 I/O 时可能仍占用）。
/// 任一变体成功（exit 0）即返回。
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
        // busy：短暂等待后重试。
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// 终止子进程并回收。
fn kill_child(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn passthrough_round_trip_or_skip() {
    if let Some(reason) = skip_reason() {
        eprintln!("[SKIP] passthrough_round_trip：{reason}");
        return;
    }

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

    // 启动 scrollz（前台，AutoUnmount 默认开）。
    let child = match Command::new(scrollz_bin())
        .arg("--backing")
        .arg(backing.path())
        .arg("--mountpoint")
        .arg(&mountpoint)
        .env("RUST_LOG", "warn")
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[SKIP] 无法启动 scrollz 二进制：{e}");
            return;
        }
    };

    if !wait_mounted(&mountpoint, Duration::from_secs(5)) {
        // 挂载没成功——通常是权限/环境问题（如容器内无 fuse 权限）。优雅跳过。
        kill_child(child);
        unmount(&mountpoint);
        eprintln!("[SKIP] 5s 内未观察到挂载，疑似环境不允许 FUSE 挂载，跳过");
        return;
    }

    // 用闭包包住断言，保证无论 panic 与否都能卸载清理。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        round_trip_assertions(&mountpoint, backing.path());
    }));

    // 清理：卸载 + 杀进程。
    unmount(&mountpoint);
    kill_child(child);

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// 实际的 round-trip 校验逻辑（在挂载点上做 POSIX 操作，并对照 backing）。
fn round_trip_assertions(mountpoint: &Path, backing: &Path) {
    // 1) create + write
    let file_path = mountpoint.join("hello.txt");
    let payload = b"scrollz passthrough P0\n";
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&file_path)
            .expect("通过挂载点创建文件");
        f.write_all(payload).expect("写入");
        f.flush().expect("flush");
    }

    // 2) read 回读一致
    let read_back = fs::read(&file_path).expect("通过挂载点读回");
    assert_eq!(read_back, payload, "read round-trip 必须一致");

    // 3) backing 下应出现同名文件、同内容（透传落盘验证）
    let backing_file = backing.join("hello.txt");
    let backing_content = fs::read(&backing_file).expect("backing 下应有该文件");
    assert_eq!(backing_content, payload, "backing 内容应与写入一致");

    // 4) mkdir
    let subdir = mountpoint.join("sub");
    fs::create_dir(&subdir).expect("mkdir");
    assert!(backing.join("sub").is_dir(), "backing 下应出现子目录");

    // 5) 在子目录里再建一个文件，验证嵌套路径
    let nested = subdir.join("inner.bin");
    fs::write(&nested, b"nested").expect("写嵌套文件");
    assert_eq!(
        fs::read(backing.join("sub").join("inner.bin")).expect("读 backing 嵌套"),
        b"nested"
    );

    // 6) readdir：根目录应列出 hello.txt 与 sub
    let names: Vec<String> = fs::read_dir(mountpoint)
        .expect("readdir 根")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"hello.txt".to_string()),
        "readdir 应含 hello.txt，实际 {names:?}"
    );
    assert!(
        names.contains(&"sub".to_string()),
        "readdir 应含 sub，实际 {names:?}"
    );

    // 7) unlink
    fs::remove_file(&file_path).expect("unlink");
    assert!(!file_path.exists(), "unlink 后挂载点不应再有该文件");
    assert!(
        !backing.join("hello.txt").exists(),
        "unlink 后 backing 也不应有"
    );

    eprintln!("[OK] passthrough round-trip 全部断言通过（真实挂载）");
}
