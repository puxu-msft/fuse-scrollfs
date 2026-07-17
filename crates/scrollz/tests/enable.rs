//! enable 子命令集成冒烟：驱动真实二进制，隔离 env（绝不碰真实 `~/.claude`）。
//!
//! 仅测**不需要 FUSE 挂载**的路径（list/status/参数校验/help）；真实挂载的 apply/restore
//! 由 lifecycle 单元测试（FakeMounter + 真实 ingest）覆盖，端到端 FUSE 由手测/README 烟测覆盖。

use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    // cargo 在 target/<profile>/zipfs 产出主二进制；测试时用 CARGO_BIN_EXE_scrollz。
    PathBuf::from(env!("CARGO_BIN_EXE_scrollz"))
}

/// 在隔离的 projects/scrollz_home 下跑 `enable <args>`，返回 (stdout, success)。
fn run_enable(tmp: &std::path::Path, args: &[&str]) -> (String, bool) {
    let projects = tmp.join("projects");
    let zip = tmp.join("zip");
    std::fs::create_dir_all(&projects).unwrap();
    let out = Command::new(bin())
        .arg("enable")
        .args(args)
        .env("CLAUDE_PROJECTS", &projects)
        .env("ZIPFS_HOME", &zip)
        .env("HOME", tmp)
        .output()
        .expect("run zipfs enable");
    (
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
        out.status.success(),
    )
}

#[test]
fn enable_help_lists_subactions() {
    let out = Command::new(bin())
        .args(["enable", "--help"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for sub in ["list", "apply", "restore", "remount", "status", "autostart"] {
        assert!(text.contains(sub), "help 应含子动作 {sub}：\n{text}");
    }
}

#[test]
fn list_empty_projects_is_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let (out, ok) = run_enable(tmp.path(), &["list"]);
    assert!(ok, "空 projects 应成功：{out}");
    assert!(
        out.contains("无项目") || out.contains("NAME"),
        "应提示空或表头：{out}"
    );
}

#[test]
fn list_shows_plain_project() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("projects").join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    std::fs::write(demo.join("a.jsonl"), b"{}\n").unwrap();
    let (out, ok) = run_enable(tmp.path(), &["list"]);
    assert!(ok, "{out}");
    assert!(out.contains("demo"), "应列出 demo：{out}");
    assert!(out.contains("PLAIN"), "demo 应为 PLAIN：{out}");
}

#[test]
fn status_reports_plain_and_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("projects").join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    // 非 jsonl/log 且无 open fd → Idle。
    std::fs::write(demo.join("note.txt"), b"x").unwrap();
    let (out, ok) = run_enable(tmp.path(), &["status", "demo"]);
    assert!(ok, "{out}");
    assert!(out.contains("PLAIN"), "{out}");
    assert!(out.contains("活跃: 否"), "应判 Idle：{out}");
}

#[test]
fn restore_without_backup_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("projects").join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let (out, ok) = run_enable(tmp.path(), &["restore", "demo"]);
    assert!(!ok, "无备份的 restore 应失败：{out}");
    assert!(out.contains("无备份"), "应报无备份：{out}");
}

#[test]
fn autostart_print_emits_wsl_snippet() {
    let tmp = tempfile::tempdir().unwrap();
    let (out, ok) = run_enable(tmp.path(), &["autostart", "print"]);
    assert!(ok, "{out}");
    assert!(out.contains("[boot]"), "应含 wsl.conf [boot]：{out}");
    assert!(out.contains("remount --all"), "{out}");
}

/// no-unconscious 数据丢失红线：穿越/绝对 name 必须在入口被拒，且绝不删树外目录。
#[test]
fn purge_rejects_traversal_name_and_preserves_outside_tree() {
    let tmp = tempfile::tempdir().unwrap();
    // 树外哨兵目录（位于 projects/zip 之外），含一个文件；必须在测试后原样幸存。
    let sentinel = tmp.path().join("sentinel");
    std::fs::create_dir_all(&sentinel).unwrap();
    std::fs::write(sentinel.join("keep.txt"), b"important").unwrap();

    // backing 默认在 ZIPFS_HOME/back/<name>；用 `../../sentinel` 试图逃逸到哨兵。
    let (out, ok) = run_enable(tmp.path(), &["purge", "../../sentinel", "--yes"]);
    assert!(!ok, "穿越 name 的 purge 应失败：{out}");
    assert!(out.contains("非法项目名"), "应报非法项目名：{out}");
    assert!(
        sentinel.join("keep.txt").exists(),
        "树外哨兵目录绝不能被删除"
    );

    // 绝对路径 name 同样拒绝。
    let (out2, ok2) = run_enable(tmp.path(), &["status", "/etc"]);
    assert!(!ok2, "绝对 name 应失败：{out2}");
    assert!(out2.contains("非法项目名"), "{out2}");
}

/// apply 在挂载之前 fail-fast：`--dict` 指向不存在文件应给清晰错误，绝不进入挂载。
#[test]
fn apply_rejects_missing_dict_file() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = tmp.path().join("projects").join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    std::fs::write(demo.join("a.jsonl"), b"{}\n").unwrap();

    let missing = tmp.path().join("nope.dict");
    let (out, ok) = run_enable(
        tmp.path(),
        &["apply", "demo", "--dict", missing.to_str().unwrap()],
    );
    assert!(!ok, "字典不存在的 apply 应失败：{out}");
    assert!(out.contains("字典文件不存在"), "应报字典文件不存在：{out}");
    // 未发生切换：源仍在原位、无备份。
    assert!(demo.join("a.jsonl").exists(), "源不应被动过");
    assert!(
        !tmp.path().join("projects").join("demo.scrollz-orig").exists(),
        "不应产生备份（未进入切换）"
    );
}
