//! 持久化默认选项（`ZIPFS_HOME/config`）：apply 的起点，命令行覆盖之。
//!
//! 手搓 key=value（无 serde），键名与 `ApplyOptions` 一一对应。`config show` 打印，
//! `config set <key> <value>` 校验后写回。让用户不必每次 apply 重复敲 `--level 19 --threads 4`。

use std::io;
use std::path::PathBuf;

use crate::enable::model::{ApplyOptions, Backend, Paths};
use crate::enable::ConfigCmd;

/// config 文件路径 = ZIPFS_HOME/config。
fn config_path(paths: &Paths) -> PathBuf {
    paths.zipfs_home.join("config")
}

/// 已知配置键（`config set` 校验用，拒绝拼错的键静默丢失）。
const KEYS: &[&str] = &[
    "backend",
    "chunk_size",
    "level",
    "dict",
    "threads",
    "writeback",
    "max_write",
    "no_tail_buffer",
    "allow_other",
    "auto_unmount",
    "metrics_file",
];

/// 读持久化默认（不存在 → 内置默认）。
pub fn load_defaults(paths: &Paths) -> ApplyOptions {
    let mut o = ApplyOptions::default();
    if let Ok(s) = std::fs::read_to_string(config_path(paths)) {
        apply_kv(&mut o, &s);
    }
    o
}

/// 把 key=value 文本叠加到 opts（未知键忽略，bool 接受 1/true）。
fn apply_kv(o: &mut ApplyOptions, content: &str) {
    let is_true = |v: &str| v == "1" || v.eq_ignore_ascii_case("true");
    for line in content.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "backend" => {
                if let Some(b) = Backend::parse(v) {
                    o.backend = b;
                }
            }
            "chunk_size" => {
                if let Ok(x) = v.parse() {
                    o.chunk_size = x;
                }
            }
            "level" => {
                if let Ok(x) = v.parse() {
                    o.level = x;
                }
            }
            "dict" => o.dict = if v.is_empty() { None } else { Some(v.into()) },
            "threads" => {
                if let Ok(x) = v.parse() {
                    o.threads = x;
                }
            }
            "writeback" => o.writeback = is_true(v),
            "max_write" => {
                if let Ok(x) = v.parse() {
                    o.max_write = x;
                }
            }
            "no_tail_buffer" => o.no_tail_buffer = is_true(v),
            "allow_other" => o.allow_other = is_true(v),
            "auto_unmount" => o.auto_unmount = is_true(v),
            "metrics_file" => o.metrics_file = if v.is_empty() { None } else { Some(v.into()) },
            _ => {}
        }
    }
}

/// 序列化 opts 为 config 文件文本。
fn serialize(o: &ApplyOptions) -> String {
    format!(
        "backend={}\nchunk_size={}\nlevel={}\ndict={}\nthreads={}\nwriteback={}\nmax_write={}\nno_tail_buffer={}\nallow_other={}\nauto_unmount={}\nmetrics_file={}\n",
        o.backend.flag(),
        o.chunk_size,
        o.level,
        o.dict.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        o.threads,
        o.writeback,
        o.max_write,
        o.no_tail_buffer,
        o.allow_other,
        o.auto_unmount,
        o.metrics_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
    )
}

/// config 子动作入口。
pub fn run(paths: &Paths, cmd: ConfigCmd) -> io::Result<()> {
    match cmd {
        ConfigCmd::Show => {
            let o = load_defaults(paths);
            print!("{}", serialize(&o));
            println!("# 文件: {}", config_path(paths).display());
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            if !KEYS.contains(&key.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("未知配置键 {key:?}；可用：{}", KEYS.join(", ")),
                ));
            }
            // 评审 M4：含换行的值会注入伪造 config 行（key=value 解析被污染）。fail-closed。
            if value.contains('\n') || value.contains('\r') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "配置值含换行/回车，拒绝（防注入伪造配置行）",
                ));
            }
            let mut o = load_defaults(paths);
            apply_kv(&mut o, &format!("{key}={value}"));
            std::fs::create_dir_all(&paths.zipfs_home)?;
            std::fs::write(config_path(paths), serialize(&o))?;
            println!("config: 已设 {key}={value}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(root: &std::path::Path) -> Paths {
        Paths {
            projects_root: root.join("projects"),
            zipfs_home: root.join("zip"),
        }
    }

    #[test]
    fn defaults_when_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let o = load_defaults(&paths_in(tmp.path()));
        assert_eq!(o.backend, Backend::Shadow);
        assert_eq!(o.level, ApplyOptions::default().level);
    }

    #[test]
    fn set_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        run(
            &paths,
            ConfigCmd::Set {
                key: "level".into(),
                value: "19".into(),
            },
        )
        .unwrap();
        run(
            &paths,
            ConfigCmd::Set {
                key: "backend".into(),
                value: "container".into(),
            },
        )
        .unwrap();
        let o = load_defaults(&paths);
        assert_eq!(o.level, 19);
        assert_eq!(o.backend, Backend::Container);
    }

    #[test]
    fn set_rejects_unknown_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let r = run(
            &paths,
            ConfigCmd::Set {
                key: "nonsense".into(),
                value: "x".into(),
            },
        );
        assert!(r.is_err());
    }
}
