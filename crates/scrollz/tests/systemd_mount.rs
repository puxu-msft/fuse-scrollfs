//! systemd 托管挂载路径集成测试（Bug C）：聚焦 `zipfs mount-managed` 守护入口——
//! 由 sidecar meta 自拼参数挂载一个**已提交**项目、经挂载点读到原数据、卸载干净、backing 保留。
//!
//! 不依赖 systemd（`systemctl --user start` 不会继承本测试进程的 CLAUDE_PROJECTS/SCROLLZ_HOME
//! 环境，无法 hermetic 测）。这里直接以测试 env 拉起 `mount-managed` 子进程，覆盖真正的新代码
//! 路径：`resolve_managed_spec` → `run_mount` → 真实 FUSE。SystemdMounter 的 systemctl 编排很薄，
//! 由 C8 手动冒烟覆盖。
//!
//! 无 /dev/fuse 或 fusermount 时优雅 SKIP，不 panic。结束必卸载。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn zipfs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_scrollz"))
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(bin))
        .find(|p| p.is_file())
}

fn skip_reason() -> Option<String> {
    if !Path::new("/dev/fuse").exists() {
        return Some("/dev/fuse 不存在".to_string());
    }
    if !["fusermount3", "fusermount"]
        .iter()
        .any(|b| which(b).is_some())
    {
        return Some("找不到 fusermount3/fusermount".to_string());
    }
    None
}

fn is_mounted(mp: &Path) -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|m| {
            m.lines()
                .any(|l| l.split_whitespace().nth(1) == mp.to_str())
        })
        .unwrap_or(false)
}

fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

fn fusermount_u(mp: &Path) {
    for bin in ["fusermount3", "fusermount"] {
        if which(bin).is_some() {
            let _ = Command::new(bin).arg("-u").arg(mp).status();
        }
    }
}

/// 以隔离 env 跑一条 `zipfs <args…>` 到结束，返回是否成功。
fn run_cli(home: &Path, proj: &Path, zip: &Path, args: &[&str]) -> bool {
    Command::new(zipfs_bin())
        .args(args)
        .env("HOME", home)
        .env("CLAUDE_PROJECTS", proj)
        .env("SCROLLZ_HOME", zip)
        .env("RUST_LOG", "warn")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn mount_managed_serves_committed_project_and_unmounts_clean() {
    if let Some(r) = skip_reason() {
        eprintln!("SKIP mount_managed 集成测试：{r}");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let proj = home.join(".claude/projects");
    let zip = home.join(".local/claude-scrollz");
    fs::create_dir_all(&proj).unwrap();

    // 源项目：proj/demo/a.txt。
    let name = "demo";
    let src = proj.join(name);
    fs::create_dir_all(&src).unwrap();
    let payload = b"managed mount payload\nline2\n";
    fs::write(src.join("a.txt"), payload).unwrap();

    // 1) apply（--force：新建文件 mtime=now 判活跃）：bare daemon 挂载 + 写 committed meta。
    assert!(
        run_cli(&home, &proj, &zip, &["enable", "apply", name, "--force"]),
        "enable apply 应成功"
    );
    let mp = proj.join(name);
    assert!(
        wait_until(|| is_mounted(&mp), Duration::from_secs(5)),
        "apply 后应已挂载"
    );
    assert_eq!(
        fs::read(mp.join("a.txt")).unwrap(),
        payload,
        "挂载点应读到原数据"
    );

    // 2) 卸载 bare daemon → STOPPED（orig 备份 + committed backing + 空挂载点）。
    fusermount_u(&mp);
    assert!(
        wait_until(|| !is_mounted(&mp), Duration::from_secs(5)),
        "卸载后挂载应消失"
    );

    // 3) mount-managed 守护：由 sidecar meta 自拼参数重挂（真正的新代码路径）。
    //    escape("demo") == "demo"，故 --name demo。
    let mut child: Child = Command::new(zipfs_bin())
        .args(["mount-managed", "--name", name])
        .env("HOME", &home)
        .env("CLAUDE_PROJECTS", &proj)
        .env("SCROLLZ_HOME", &zip)
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("spawn mount-managed");

    if !wait_until(|| is_mounted(&mp), Duration::from_secs(5)) {
        let _ = child.kill();
        let _ = child.wait();
        panic!("mount-managed 未在超时内挂载");
    }
    assert_eq!(
        fs::read(mp.join("a.txt")).unwrap(),
        payload,
        "managed 挂载应读到原数据（无损保留）"
    );

    // 4) fusermount -u 结束 FUSE 会话 → 守护退出、挂载消失（干净卸载路径）。
    fusermount_u(&mp);
    let unmounted = wait_until(|| !is_mounted(&mp), Duration::from_secs(5));
    let _ = child.wait();
    assert!(unmounted, "卸载后挂载应消失");

    // 5) backing 数据保留在盘上（卸载不删数据）。
    let backing = zip.join("back").join(name);
    assert!(backing.is_dir(), "shadow backing 目录应保留");
    assert!(
        fs::read_dir(&backing).unwrap().next().is_some(),
        "backing 应非空（archive 数据保留）"
    );
}
