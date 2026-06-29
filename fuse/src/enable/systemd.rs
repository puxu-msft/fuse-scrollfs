//! systemd user 服务托管（Bug C）：per-project 模板实例 `zipfs@<name>.service`。
//!
//! 根因：裸 spawn + `setsid` 产生无人监管的孤儿守护，父退出后 reparent 到 init、无人重启、
//! 无单实例保证（叠加 Bug A flock 才不会互相覆盖）。改用 systemd user 模板托管：单实例、
//! 崩溃自动重启（`Restart=on-failure` + `WatchdogSec`）、`systemctl --user` 统一管理。
//!
//! 本模块只放**纯逻辑**（可无 systemd 单测）：实例名 escape/unescape、systemctl argv 构造、
//! 模板 unit body、环境探测。真正的 `SystemdMounter`（实现 `Mounter`）也在此，但其行为靠
//! 集成测试（需 systemd + /dev/fuse）覆盖。

use crate::enable::discovery;

/// systemd 实例名允许**不转义**的字符：数字 / 字母 / `:` `_` `.`（对齐 systemd `VALID_CHARS`
/// 去掉 `-` `\`——这两者 systemd 总是转义）。
fn is_plain(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.')
}

/// 把 project name 转义成 systemd 实例名（对拍真实 `systemd-escape` 语义）：
/// `/`→`-`；`-`、`\`、其它非 `is_plain` 字节→`\xNN`（小写十六进制）；前导 `.`→`\x2e`。
///
/// 例：`-home-xp-src-neighbors` → `\x2dhome\x2dxp\x2dsrc\x2dneighbors`。
pub fn systemd_escape(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'/' => out.push('-'),
            // 前导 `.` 必须转义（systemd 不允许实例名以 `.` 开头）。
            b'.' if i == 0 => out.push_str("\\x2e"),
            _ if b != b'-' && b != b'\\' && is_plain(b) => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// 把 systemd 实例名还原回 project name（`systemd-escape -u` 语义）：
/// `-`→`/`；`\xNN`→对应字节；其余原样。非法 `\x` 序列原样保留（尽力而为，不 panic）。
///
/// Rust 侧自己 unescape（不依赖 unit 文件里的 `%I`），对 escape 严格 roundtrip、不受 systemd
/// 版本差异影响。
pub fn systemd_unescape(inst: &str) -> String {
    let bytes = inst.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'-' => {
                out.push(b'/');
                i += 1;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'x') => {
                // `\xNN`：取两位十六进制（越界则 get 返回 None，落到非法分支）。
                if let Some(byte) = bytes
                    .get(i + 2..i + 4)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                {
                    out.push(byte);
                    i += 4;
                    continue;
                }
                // 非法 \x 序列：原样保留反斜杠。
                out.push(b'\\');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 由 sidecar meta 解析出 managed mount 的 `MountSpec`（systemd `mount-managed` 子命令用）。
///
/// sidecar meta 是唯一真值源（对齐 `remount`）：未提交（半灌）→ Err，拒绝挂载半灌 backing。
/// `name` 是 project 原始名（调用方已 unescape）。
pub fn resolve_managed_spec(
    paths: &crate::enable::model::Paths,
    name: &str,
) -> std::io::Result<crate::enable::daemon::MountSpec> {
    crate::enable::model::validate_name(name)?;
    let meta = crate::enable::discovery::read_meta(&paths.meta_path(name))?
        .filter(|m| m.committed)
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "{name} backing 未提交（半灌），拒绝 managed 挂载；需 re-ingest 或 restore"
            ))
        })?;
    Ok(crate::enable::lifecycle::mount_spec(
        paths,
        name,
        &meta.options(),
    ))
}

/// 构造 `systemctl --user <verb> zipfs@<esc>.service` 的 argv（不含 `systemctl` 本身），
/// 实例名经 `systemd_escape`。纯函数，便于单测命令构造正确性（仿 daemon 的 mount_argv 模式）。
pub fn systemctl_args(verb: &str, name: &str) -> Vec<String> {
    vec![
        "--user".to_string(),
        verb.to_string(),
        format!("zipfs@{}.service", systemd_escape(name)),
    ]
}

/// 跑一条 `systemctl --user …`，非零退出 → Err（带 argv 上下文）。
fn run_systemctl(args: &[String]) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let status = Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "systemctl {} 失败（{status}）",
            args.join(" ")
        )))
    }
}

/// systemd user 托管挂载器：per-project 模板实例 `zipfs@<esc>.service`（单实例 + 自动重启 +
/// 监管）。`spawn`/`unmount` 走 `systemctl --user start/stop`；`is_mounted` 查 /proc 地面真值
/// （非 unit active 状态，避免 unit 报 active 但挂载实际已 stale）。
pub struct SystemdMounter;

impl crate::enable::daemon::Mounter for SystemdMounter {
    fn spawn(&self, spec: &crate::enable::daemon::MountSpec) -> std::io::Result<()> {
        // 先 reset-failed 清掉上次失败计数，否则触发 start-limit 时 start 直接被拒（评审建议）。
        let _ = run_systemctl(&systemctl_args("reset-failed", &spec.name));
        // Type=notify：start 阻塞到 main.rs sd_notify READY，比轮询更可靠。
        run_systemctl(&systemctl_args("start", &spec.name))?;
        // 再校验地面真值（/proc mountinfo）：unit active 不等于挂载就绪。
        if discovery::is_mounted(&spec.mountpoint) && discovery::endpoint_ok(&spec.mountpoint) {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "systemd start 后挂载点未就绪：{}",
                spec.mountpoint.display()
            )))
        }
    }

    fn unmount(&self, name: &str, _mountpoint: &std::path::Path) -> std::io::Result<()> {
        // 必须 systemctl stop（而非直接 fusermount -u），否则 Restart=on-failure 会与卸载抢挂。
        run_systemctl(&systemctl_args("stop", name))
    }

    fn is_mounted(&self, mountpoint: &std::path::Path) -> bool {
        discovery::is_mounted(mountpoint)
    }
}

/// 选哪个 mounter 的纯决策（真值表可单测）：三个探测都通过才用 systemd，否则降级 Real。
/// 降级不劣化——RealMounter 叠加 Bug A flock 仍杜绝双守护覆盖，只是少了崩溃自愈/监管。
/// `template_installed` 必查：SystemdMounter.spawn 走 `systemctl start zipfs@<esc>`，模板单元
/// 不在则 start 报 "Unit not found"——故 systemd 路径是 opt-in（先 `enable autostart install`）。
fn pick_systemd(has_systemd_run_dir: bool, user_bus_ok: bool, template_installed: bool) -> bool {
    has_systemd_run_dir && user_bus_ok && template_installed
}

/// `systemctl --user is-system-running` 能连上 user bus 即返回状态词（running/degraded/…），
/// 连不上（无 user systemd）则非零 + stderr。任一已知状态词视为 user bus 可达。
fn user_bus_reachable() -> bool {
    use std::process::Command;
    Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            matches!(
                s.trim(),
                "running" | "degraded" | "starting" | "stopping" | "maintenance" | "initializing"
            )
        })
        .unwrap_or(false)
}

/// 模板单元 `~/.config/systemd/user/zipfs@.service` 是否已安装（`enable autostart install` 装）。
fn template_installed() -> bool {
    std::env::var_os("HOME")
        .map(|home| {
            std::path::Path::new(&home)
                .join(".config/systemd/user/zipfs@.service")
                .is_file()
        })
        .unwrap_or(false)
}

/// 运行时选 mounter：探测 systemd user 会话可达性 + 模板已装。systemd → `SystemdMounter`
/// （单实例 + 自愈 + 监管）；否则 `RealMounter`（叠加 Bug A flock 兜底，行为不劣于改造前）。
/// 单点供 `enable::run` / TUI 用。
pub fn select_mounter() -> Box<dyn crate::enable::daemon::Mounter> {
    let has_dir = std::path::Path::new("/run/systemd/system").is_dir();
    let bus_ok = has_dir && user_bus_reachable();
    let tmpl = bus_ok && template_installed();
    if pick_systemd(has_dir, bus_ok, tmpl) {
        Box::new(SystemdMounter)
    } else {
        Box::new(crate::enable::daemon::RealMounter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escape_matches_systemd_oracle() {
        // 硬编码 oracle = 真实 `systemd-escape -- <s>` 输出（见 cheeky-hatching-clock.md）。
        assert_eq!(
            systemd_escape("-home-xp-src-neighbors"),
            "\\x2dhome\\x2dxp\\x2dsrc\\x2dneighbors"
        );
        assert_eq!(systemd_escape("foo.bar:baz_qux"), "foo.bar:baz_qux");
        assert_eq!(systemd_escape(".hidden"), "\\x2ehidden");
        // `/` → `-`（systemd 路径转义本义）。
        assert_eq!(systemd_escape("a/b"), "a-b");
    }

    #[test]
    fn unescape_inverts_escape_roundtrip() {
        for name in [
            "-home-xp-src-neighbors",
            "foo.bar:baz_qux",
            ".hidden",
            "-a-b-c",
            "plain",
        ] {
            assert_eq!(
                systemd_unescape(&systemd_escape(name)),
                name,
                "roundtrip 失败：{name}"
            );
        }
    }

    #[test]
    fn unescape_matches_systemd_oracle() {
        assert_eq!(
            systemd_unescape("\\x2dhome\\x2dxp\\x2dsrc\\x2dneighbors"),
            "-home-xp-src-neighbors"
        );
    }

    #[test]
    fn systemctl_args_builds_escaped_instance_unit() {
        assert_eq!(
            systemctl_args("start", "-home-xp-src-neighbors"),
            vec![
                "--user",
                "start",
                "zipfs@\\x2dhome\\x2dxp\\x2dsrc\\x2dneighbors.service",
            ]
        );
        assert_eq!(
            systemctl_args("stop", "plain"),
            vec!["--user", "stop", "zipfs@plain.service"]
        );
    }

    #[test]
    fn pick_systemd_requires_all_probes() {
        assert!(
            pick_systemd(true, true, true),
            "systemd 目录 + user bus + 模板已装 → systemd"
        );
        assert!(!pick_systemd(true, true, false), "模板未装 → 降级 Real");
        assert!(!pick_systemd(true, false, true), "user bus 不可达 → Real");
        assert!(
            !pick_systemd(false, true, true),
            "无 /run/systemd/system → Real"
        );
        assert!(!pick_systemd(false, false, false), "都无 → Real");
    }

    #[test]
    fn resolve_managed_spec_from_committed_meta() {
        use crate::enable::discovery::{write_meta, Meta};
        use crate::enable::model::{ApplyOptions, Backend, Paths};
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            projects_root: tmp.path().join("projects"),
            zipfs_home: tmp.path().join("zip"),
        };
        std::fs::create_dir_all(paths.back_root()).unwrap();
        let opts = ApplyOptions {
            backend: Backend::Shadow,
            chunk_size: 65536,
            level: 7,
            ..ApplyOptions::default()
        };
        let meta = Meta::from_apply(&opts, 100, 50, 0);
        write_meta(&paths.meta_path("demo"), &meta).unwrap();

        let spec = resolve_managed_spec(&paths, "demo").unwrap();
        assert_eq!(spec.name, "demo");
        assert_eq!(spec.backend, Backend::Shadow);
        assert_eq!(spec.chunk_size, 65536);
        assert_eq!(spec.level, 7);
        assert_eq!(spec.backing, paths.backing("demo", Backend::Shadow));
        assert_eq!(spec.mountpoint, paths.mountpoint("demo"));
    }

    #[test]
    fn resolve_managed_spec_rejects_uncommitted() {
        use crate::enable::discovery::{write_meta, Meta};
        use crate::enable::model::Paths;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            projects_root: tmp.path().join("projects"),
            zipfs_home: tmp.path().join("zip"),
        };
        std::fs::create_dir_all(paths.back_root()).unwrap();
        // 半灌：committed=false（默认）。
        let meta = Meta::default();
        write_meta(&paths.meta_path("demo"), &meta).unwrap();
        assert!(
            resolve_managed_spec(&paths, "demo").is_err(),
            "未提交 meta 应拒绝 managed 挂载"
        );
        // 完全无 meta → 也 Err。
        assert!(resolve_managed_spec(&paths, "nope").is_err());
    }
}
