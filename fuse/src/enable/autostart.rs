//! 自挂载接线：开机/登录后自动重挂所有已切换项目。
//!
//! 两条路径（与 docs/01 §T1 一致）：
//! - **systemd user**：装一个聚合 oneshot 单元 `zipfs-projects.service`，`ExecStart=zipfs enable
//!   remount --all`，登录时把所有 STOPPED 项目重挂（避开 per-instance `%i` 转义复杂度）。
//! - **WSL 无 systemd**：打印 `/etc/wsl.conf` 的 `[boot] command` 片段供用户粘贴（root 文件，**只打印不自动改**）。

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::enable::AutostartCmd;

/// 自挂载子动作入口。
pub fn run(home: &Path, cmd: AutostartCmd) -> io::Result<()> {
    match cmd {
        AutostartCmd::Install { all } => {
            let _ = all; // 聚合单元天然覆盖所有项目。
            install_systemd(home)
        }
        AutostartCmd::Print => {
            print_wsl_snippet();
            Ok(())
        }
    }
}

/// 聚合 oneshot 单元正文。
fn unit_body(exe: &Path) -> String {
    format!(
        "# zipfs 自挂载：登录时重挂所有已切换的 Claude 项目（生成自 `zipfs enable autostart install`）。\n\
         [Unit]\n\
         Description=zipfs remount all switched Claude projects (layout S)\n\
         After=default.target\n\n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart={} enable remount --all\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    )
}

/// 写 systemd user 单元并尝试 `daemon-reload` + `enable`。无 systemctl → 打印手动指引。
fn install_systemd(home: &Path) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let dir = home.join(".config").join("systemd").join("user");
    std::fs::create_dir_all(&dir)?;
    let unit_path = dir.join("zipfs-projects.service");
    std::fs::write(&unit_path, unit_body(&exe))?;
    println!("已写入 {}", unit_path.display());

    if which("systemctl") {
        run_quiet("systemctl", &["--user", "daemon-reload"]);
        let st = Command::new("systemctl")
            .args(["--user", "enable", "zipfs-projects.service"])
            .status();
        match st {
            Ok(s) if s.success() => {
                println!("已 enable zipfs-projects.service（登录时重挂所有项目）。");
                println!("立即生效：systemctl --user start zipfs-projects.service");
            }
            _ => print_systemd_manual(&unit_path),
        }
    } else {
        print_systemd_manual(&unit_path);
    }
    Ok(())
}

fn print_systemd_manual(unit_path: &Path) {
    println!("未检测到 systemctl（或 enable 失败）。手动：");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable --now zipfs-projects.service");
    println!("（单元已在 {}）", unit_path.display());
}

/// 打印 WSL `/etc/wsl.conf` 片段（不自动改 root 文件）。
fn print_wsl_snippet() {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/path/to/zipfs".into());
    println!("# 把以下片段加入 /etc/wsl.conf（需 root；WSL 无 systemd 时用）：");
    println!("[boot]");
    println!("command = {exe} enable remount --all");
    println!();
    println!("# 然后在 Windows 侧 `wsl --shutdown` 重启发行版生效。");
    let _ = io::stdout().flush();
}

/// PATH 中是否有某可执行（轻量 which）。
fn which(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_quiet(bin: &str, args: &[&str]) {
    let _ = Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_body_has_remount_all_execstart() {
        let body = unit_body(Path::new("/usr/bin/zipfs"));
        assert!(body.contains("ExecStart=/usr/bin/zipfs enable remount --all"));
        assert!(body.contains("Type=oneshot"));
        assert!(body.contains("WantedBy=default.target"));
    }
}
