//! 自挂载接线：开机/登录后自动重挂所有已切换项目。
//!
//! 两条路径（与 docs/01 §T1 一致）：
//! - **systemd user**：装 **per-project 模板** `zipfs@.service`（`Type=notify`、`Restart=on-failure`、
//!   `WatchdogSec`），对每个已提交项目 `systemctl --user enable zipfs@<esc>.service`。单实例 +
//!   崩溃自动重启 + 监管，根治裸 spawn 孤儿守护（Bug C）。装时迁移掉旧聚合单元
//!   `zipfs-projects.service`。
//! - **WSL 无 systemd**：打印 `/etc/wsl.conf` 的 `[boot] command` 片段供用户粘贴（root 文件，**只打印不自动改**）。

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::enable::model::Paths;
use crate::enable::systemd::systemd_escape;
use crate::enable::{discovery, AutostartCmd};

/// 自挂载子动作入口。
pub fn run(home: &Path, cmd: AutostartCmd) -> io::Result<()> {
    match cmd {
        AutostartCmd::Install { all } => {
            let _ = all; // 模板对每个已提交项目逐一 enable，天然覆盖所有项目。
            install_systemd(home)
        }
        AutostartCmd::Print => {
            print_wsl_snippet();
            Ok(())
        }
    }
}

/// per-project 模板单元 `zipfs@.service` 正文。`%i` = systemd 实例字符串（escaped），
/// `mount-managed`/`umount-managed`/`guard-check` 在 Rust 侧 unescape 回原名。`Type=notify` 依赖守护
/// 的 sd_notify READY（main.rs serve 路径）；`WatchdogSec` 启用心跳监管；`Restart=on-failure`
/// 崩溃自愈。
///
/// **防 crash-loop 双层守卫（Task 12）**，把「underlay 含停用期回落写」这一需人工的稳定态既 fail-closed
/// 又不 crash-loop：
/// - `ExecCondition=... enable guard-check`：在 ExecStart 前显式守卫。underlay 非空时以独特码 75 退出；
///   `ExecCondition` 控制进程以 1–254 退出使 unit **skipped（非 failed）**，故 `Restart=on-failure` **不触发**
///   （这是正确的 systemd 原语——`RestartPreventExitStatus` 只作用于**主进程**，对 `ExecStartPre`/`ExecCondition`
///   控制进程无效，见 systemd.service(5)，故不能用 `ExecStartPre` 承担此职）。
/// - `RestartPreventExitStatus={guard_exit}`：兜底 **ExecStart 主进程**（`run_mount_managed`）自身的 underlay
///   守卫——覆盖 `ExecCondition` 通过后到真正挂载之间 underlay 又生回落写的 TOCTOU 窗口；主进程以 75 退出时
///   本项拦住 Restart（对主进程有效）。两层都指向同一独特码，正确防风暴。
fn template_unit_body(exe: &Path) -> String {
    let exe = exe.display();
    let guard_exit = crate::enable::model::GUARD_CHECK_NEEDS_RECONCILE_EXIT;
    format!(
        "# zipfs per-project 托管模板（生成自 `zipfs enable autostart install`）。\n\
         # 实例名 = systemd-escaped 的 Claude 项目目录名；用 `systemctl --user enable zipfs@<esc>` 接管。\n\
         [Unit]\n\
         Description=zipfs transparent-compression mount for Claude project %i\n\
         After=default.target\n\n\
         [Service]\n\
         Type=notify\n\
         ExecCondition={exe} enable guard-check --name %i\n\
         ExecStart={exe} mount-managed --name %i\n\
         ExecStop={exe} umount-managed --name %i --level auto\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         RestartPreventExitStatus={guard_exit}\n\
         WatchdogSec=30\n\n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// 写 per-project 模板单元并对每个已提交项目 `enable`。无 systemctl → 打印手动指引。
fn install_systemd(home: &Path) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let dir = home.join(".config").join("systemd").join("user");
    std::fs::create_dir_all(&dir)?;
    let unit_path = dir.join("zipfs@.service");
    std::fs::write(&unit_path, template_unit_body(&exe))?;
    println!("已写入模板单元 {}", unit_path.display());

    if !which("systemctl") {
        print_systemd_manual(&unit_path);
        return Ok(());
    }

    // 迁移：disable 旧聚合单元 zipfs-projects.service（被 per-project 模板取代）。
    migrate_off_aggregate_unit(&dir);

    run_quiet("systemctl", &["--user", "daemon-reload"]);

    // 对每个已提交（committed）项目 enable 模板实例（崩溃自愈 + 登录自起）。
    let paths = Paths::resolve(home);
    let committed = committed_project_names(&paths);
    if committed.is_empty() {
        println!("当前无已提交项目；apply 后会自动 enable 对应 zipfs@<name>.service。");
        println!("（也可手动：systemctl --user enable --now zipfs@<esc>.service）");
        return Ok(());
    }
    for name in &committed {
        let unit = format!("zipfs@{}.service", systemd_escape(name));
        match Command::new("systemctl")
            .args(["--user", "enable", &unit])
            .status()
        {
            Ok(s) if s.success() => println!("已 enable {unit}（{name}）"),
            _ => println!("enable {unit} 失败，请手动：systemctl --user enable --now {unit}"),
        }
    }
    println!(
        "立即生效：systemctl --user start zipfs@<esc>.service（或重新登录）。共 {} 个项目。",
        committed.len()
    );
    Ok(())
}

/// 迁移掉旧聚合单元 `zipfs-projects.service`：best-effort `disable --now` 并删其单元文件。
/// 旧单元只是自挂载编排（非数据），安全移除（no-unconscious：不碰任何 backing/项目数据）。
fn migrate_off_aggregate_unit(user_unit_dir: &Path) {
    let legacy = user_unit_dir.join("zipfs-projects.service");
    if legacy.exists() || which("systemctl") {
        run_quiet(
            "systemctl",
            &["--user", "disable", "--now", "zipfs-projects.service"],
        );
    }
    if legacy.exists() {
        if let Err(e) = std::fs::remove_file(&legacy) {
            println!("（提示）旧单元 {} 删除失败：{e}", legacy.display());
        } else {
            println!("已移除旧聚合单元 {}", legacy.display());
        }
    }
}

/// 扫描已提交（committed）项目名（apply 完成、可被 systemd 托管的）。
fn committed_project_names(paths: &Paths) -> Vec<String> {
    discovery::scan(paths)
        .map(|infos| {
            infos
                .into_iter()
                .filter(|i| i.meta.as_ref().map(|m| m.committed).unwrap_or(false))
                .map(|i| i.name)
                .collect()
        })
        .unwrap_or_default()
}

fn print_systemd_manual(unit_path: &Path) {
    println!("未检测到 systemctl。手动：");
    println!("  systemctl --user daemon-reload");
    println!("  对每个项目：systemctl --user enable --now zipfs@<systemd-escaped-name>.service");
    println!("（模板单元已在 {}）", unit_path.display());
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
    fn template_unit_body_has_managed_execstart_and_supervision() {
        let body = template_unit_body(Path::new("/usr/bin/zipfs"));
        assert!(body.contains("ExecStart=/usr/bin/zipfs mount-managed --name %i"));
        assert!(body.contains("ExecStop=/usr/bin/zipfs umount-managed --name %i --level auto"));
        assert!(body.contains("Type=notify"));
        assert!(body.contains("Restart=on-failure"));
        assert!(body.contains("WatchdogSec=30"));
        assert!(body.contains("WantedBy=default.target"));
    }

    #[test]
    fn template_unit_body_has_guard_check_execcondition_and_restart_prevent() {
        // Task 12：ExecCondition 守卫 + RestartPreventExitStatus 防 crash-loop。ExecCondition（非
        // ExecStartPre）是关键——控制进程 1–254 退出使 unit skipped 不重启，而 RestartPreventExitStatus
        // 只对主进程有效，故 ExecStartPre 无法防风暴（systemd.service(5)）。
        let body = template_unit_body(Path::new("/usr/bin/zipfs"));
        assert!(
            body.contains("ExecCondition=/usr/bin/zipfs enable guard-check --name %i"),
            "应含 ExecCondition guard-check（而非 ExecStartPre）"
        );
        assert!(
            !body.contains("ExecStartPre="),
            "不应用 ExecStartPre（RestartPreventExitStatus 对控制进程无效，会 crash-loop）"
        );
        assert!(
            body.contains("RestartPreventExitStatus=75"),
            "应含 RestartPreventExitStatus=75 拦住主进程 ExecStart 守卫的退出码"
        );
        // ExecCondition 必须排在 ExecStart 之前（systemd 按书写顺序执行前置钩子）。
        let cond = body.find("ExecCondition=").expect("有 ExecCondition");
        let start = body.find("ExecStart=").expect("有 ExecStart");
        assert!(cond < start, "ExecCondition 应排在 ExecStart 之前");
    }
}
