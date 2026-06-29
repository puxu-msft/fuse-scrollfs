//! systemd user 服务托管（Bug C）：per-project 模板实例 `zipfs@<name>.service`。
//!
//! 根因：裸 spawn + `setsid` 产生无人监管的孤儿守护，父退出后 reparent 到 init、无人重启、
//! 无单实例保证（叠加 Bug A flock 才不会互相覆盖）。改用 systemd user 模板托管：单实例、
//! 崩溃自动重启（`Restart=on-failure` + `WatchdogSec`）、`systemctl --user` 统一管理。
//!
//! 本模块只放**纯逻辑**（可无 systemd 单测）：实例名 escape/unescape、systemctl argv 构造、
//! 模板 unit body、环境探测。真正的 `SystemdMounter`（实现 `Mounter`）也在此，但其行为靠
//! 集成测试（需 systemd + /dev/fuse）覆盖。

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
}
