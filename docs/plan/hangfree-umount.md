# Hang-free 分档卸载 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 zipfs 卸载加上用户可选的分档升级梯（clean/lazy/abort/auto），卸载引擎全程 hang-free，systemd `ExecStop` 默认 `auto`——正常关闭走会 flush 的耐久路径，卡死场景自动升级到强制摘除，不再留陈旧挂载。

**Architecture:** 新增 `force_umount` 模块承载分档引擎与 hang-free 原语（读 `/proc/self/mountinfo` 取 fuse 连接号、写 `/sys/fs/fuse/connections/<id>/abort`、带超时的外部 `fusermount`）。mountinfo 解析合并进 `discovery`（其 octal-unescape/overmount 逻辑所在）。CLI 增 `zipfs umount --name --level`，内部 `umount-managed` 与 systemd `ExecStop` 复用同一引擎。

**Tech Stack:** Rust 2021，clap（`ValueEnum`），std（`process::Command`、`fs`、`mpsc`/`try_wait` 超时），既有 `fuser` FUSE 栈；测试用 `#[cfg(test)]` 单测 + `tests/` 真挂载集成（参照 `tests/systemd_mount.rs`）。

## Global Constraints

- 设计文档：`docs/07-hangfree-umount.md`（本 plan 的唯一真源，冲突以文档为准）。
- rustfmt 默认（4 空格、100 列）；`cargo clippy -- -D warnings` 必须绿。
- 不 `unwrap()`/`expect()` 于非测试代码；错误用 `std::io::Result` 冒泡（沿用现有风格，本 crate 不引 anyhow）。
- **绝不 stat/opendir/realpath 挂载点叶子**——任何可能在 hung FUSE 上阻塞的调用必须有超时上界。
- 不改挂载/写入耐久性协议（尾日志、崩溃提交，见 `docs/04-crash-safe-commit.md`）。
- 提交只 `git add` 本任务涉及文件（共享 worktree，禁用 `git add -A`）。
- 中文 conventional commits；不加 `Co-authored-by`。

---

## File Structure

- `fuse/src/enable/hang_free.rs`（建）：通用 hang-free 原语 `with_timeout` + `PROBE_TIMEOUT`（供 discovery/force_umount 共用）。
- `fuse/src/enable/discovery.rs`（改）：新增 mountinfo → fuse 连接号解析（Task 1，已完成）；硬化 `endpoint_ok`/`canonicalized_target` 为 hang-free（Task 2）。
- `fuse/src/enable/force_umount.rs`（建）：`UmountLevel`、`UmountReport`、`umount()` 分档引擎、`abort_connection()`、带超时的 `run_fusermount()`。
- `fuse/src/enable/mod.rs`（改）：`pub(crate) mod hang_free;` + `pub mod force_umount;`。
- `fuse/src/main.rs`（改）：`Umount` 顶层子命令；`umount-managed` 加 `--level`；`run_umount_managed` 切到引擎。
- `fuse/src/enable/autostart.rs`（改）：`ExecStop` 加 `--level auto` + 更新单测断言。
- `fuse/tests/umount_levels.rs`（建）：真挂载集成——clean/lazy/abort/auto + wedge。

---

## Task 1: mountinfo → fuse 连接号解析（hang-free 取号）

**Files:**
- Modify: `fuse/src/enable/discovery.rs`（在 `parse_mountinfo_line` 附近加 `parse_connection_id` 纯函数与 `mount_connection_id` reader）
- Test: `fuse/src/enable/discovery.rs`（`#[cfg(test)]` 内）

**Interfaces:**
- Consumes: 既有 `unescape_octal(&str) -> String`、`canonicalized_target(&Path) -> PathBuf`（同文件私有，可直接调用）。
- Produces:
  - `pub(crate) fn parse_connection_id(mountinfo: &str, target: &Path) -> Option<u64>` — 纯函数，从 mountinfo 文本取 `target` 对应 fuse 挂载的连接号（`major:minor` 的 minor）；overmount 取末条；非 fuse 返回 None。
  - `pub fn mount_connection_id(path: &Path) -> Option<u64>` — 读 `/proc/self/mountinfo` 后调 `parse_connection_id`，`target` 用 `canonicalized_target(path)`。

- [ ] **Step 1: 写失败测试**

在 `discovery.rs` 的 `#[cfg(test)] mod tests` 里加：

```rust
#[test]
fn parse_connection_id_takes_minor_from_fuse_line() {
    let target = std::path::Path::new("/mnt/x");
    let mi = "36 35 0:44 / /mnt/x rw,nosuid shared:1 - fuse.zipfs-shadow zipfs rw,user_id=1000\n";
    assert_eq!(parse_connection_id(mi, target), Some(44));
}

#[test]
fn parse_connection_id_none_for_non_fuse() {
    let target = std::path::Path::new("/mnt/x");
    let mi = "36 35 0:44 / /mnt/x rw - ext4 /dev/sda1 rw\n";
    assert_eq!(parse_connection_id(mi, target), None);
}

#[test]
fn parse_connection_id_overmount_takes_last() {
    let target = std::path::Path::new("/mnt/x");
    let mi = "36 35 0:44 / /mnt/x rw - fuse.zipfs-shadow z rw\n\
              37 35 0:55 / /mnt/x rw - fuse.zipfs-shadow z rw\n";
    assert_eq!(parse_connection_id(mi, target), Some(55));
}

#[test]
fn parse_connection_id_handles_octal_escaped_path() {
    let target = std::path::Path::new("/mnt/a b"); // 含空格
    let mi = "36 35 0:44 / /mnt/a\\040b rw - fuse zipfs rw\n";
    assert_eq!(parse_connection_id(mi, target), Some(44));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd fuse && cargo test --lib parse_connection_id`
Expected: 编译失败 / `cannot find function parse_connection_id`。

- [ ] **Step 3: 写最小实现**

在 `discovery.rs` `parse_mountinfo_line` 之后加：

```rust
/// 从 mountinfo 文本取 `target` 对应 fuse 挂载的连接号（`major:minor` 的 minor，即
/// `/sys/fs/fuse/connections/<minor>`）。overmount 取末条；非 fuse / 无匹配 → None。
/// 纯函数（无 IO）以便单测。
pub(crate) fn parse_connection_id(mountinfo: &str, target: &Path) -> Option<u64> {
    let mut found = None;
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() < 7 {
            continue;
        }
        let Some(sep) = fields.iter().position(|&f| f == "-") else {
            continue;
        };
        let Some(fstype) = fields.get(sep + 1) else {
            continue;
        };
        if !fstype.starts_with("fuse") {
            continue;
        }
        if Path::new(&unescape_octal(fields[4])) != target {
            continue;
        }
        // 字段 2 = `major:minor`；fuse 的 minor 即连接号。
        if let Some((_, minor)) = fields[2].split_once(':') {
            if let Ok(id) = minor.parse::<u64>() {
                found = Some(id); // 不 break：overmount 取末条。
            }
        }
    }
    found
}

/// 读 `/proc/self/mountinfo` 取挂载点的 fuse 连接号。不 stat 挂载点叶子。
pub fn mount_connection_id(path: &Path) -> Option<u64> {
    let target = canonicalized_target(path);
    let content = fs::read_to_string("/proc/self/mountinfo").ok()?;
    parse_connection_id(&content, &target)
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cd fuse && cargo test --lib parse_connection_id`
Expected: 4 测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add fuse/src/enable/discovery.rs
git commit -m "feat(discovery): mountinfo 解析 fuse 连接号（hang-free 取号）"
```

---

## Task 2: hang-free 探测基础（`hang_free.rs` 模块 + 硬化 `endpoint_ok`/`canonicalized_target`）

> **背景（实施者必读）**：已提交基线的 `discovery.rs` **不是** hang-free：`endpoint_ok` 是裸 `fs::symlink_metadata`（无超时，wedge 下 D 睡眠永阻塞）；`canonicalized_target` 先对整叶子 `fs::canonicalize(path)`（stat 叶子，wedge 下 hang）。基线**没有** `with_timeout`/`PROBE_TIMEOUT`。本任务建立 hang-free 探测基础，供后续 Task 4 的 abort 守卫（`endpoint_ok`）与 `is_mounted`（经 `canonicalized_target`）安全使用。

**Files:**
- Create: `fuse/src/enable/hang_free.rs`
- Modify: `fuse/src/enable/mod.rs`（加 `pub(crate) mod hang_free;`，在 `pub mod discovery;` 之前）
- Modify: `fuse/src/enable/discovery.rs`（硬化 `endpoint_ok`、`canonicalized_target`；顶部加 `use`）
- Test: `fuse/src/enable/hang_free.rs` 与 `fuse/src/enable/discovery.rs`（各自 `#[cfg(test)]`）

**Interfaces:**
- Produces:
  - `pub(crate) const PROBE_TIMEOUT: std::time::Duration`（800ms）
  - `pub(crate) fn with_timeout<T, F>(dur: Duration, f: F) -> Option<T> where T: Send + 'static, F: FnOnce() -> T + Send + 'static`
  - `endpoint_ok`/`canonicalized_target` 签名不变，语义不变（wedge 下由「阻塞」变「快速失败/回退」）。

- [ ] **Step 1: 写 `hang_free.rs` + 失败测试**

新建 `fuse/src/enable/hang_free.rs`：

```rust
//! Hang-free 原语：把可能在 wedge FUSE 上永久阻塞（D 睡眠）的调用包进带超时的工作线程。
//! wedged 挂载下 stat/canonicalize/opendir 会不可中断阻塞；本模块提供统一的超时逃逸，
//! 供 discovery 探测与 force_umount 卸载引擎共用。

use std::sync::mpsc;
use std::time::Duration;

/// 探测类操作（stat/canonicalize）的默认超时上界。超时即视为「不可达/卡死」。
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// 在独立线程运行 `f`，最多等 `dur`。超时返回 `None`。
///
/// 取舍：超时时工作线程可能仍卡在 D 睡眠里无法回收（线程泄漏），这是刻意的——
/// 宁可泄漏一个短命进程里的线程，也绝不让主线程被 hung FUSE 永久拖住。
pub(crate) fn with_timeout<T, F>(dur: Duration, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(dur).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_timeout_returns_none_when_closure_exceeds_deadline() {
        let got = with_timeout(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(5));
            42u32
        });
        assert_eq!(got, None, "超时应返回 None");
    }

    #[test]
    fn with_timeout_returns_some_when_closure_finishes_in_time() {
        assert_eq!(with_timeout(Duration::from_secs(5), || 7u32), Some(7));
    }
}
```

在 `fuse/src/enable/mod.rs` 加模块声明（在 `pub mod discovery;` 之前，保持字母序不强制）：

```rust
pub(crate) mod hang_free;
```

- [ ] **Step 2: 运行确认新模块测试**

Run: `cd fuse && cargo test --lib hang_free`
Expected: 2 测试 PASS（首次 fresh worktree 全量编译，较慢，正常）。

- [ ] **Step 3: 硬化 `discovery.rs` 的 `endpoint_ok` 与 `canonicalized_target`**

在 `discovery.rs` 顶部 `use` 区加：

```rust
use super::hang_free::{with_timeout, PROBE_TIMEOUT};
```

把 `endpoint_ok` 改为（超时包裹 `symlink_metadata`）：

```rust
/// 挂载点是否可 stat（stale FUSE endpoint → ENOTCONN → false；hung → 超时 → false）。
pub fn endpoint_ok(path: &Path) -> bool {
    let p = path.to_path_buf();
    match with_timeout(PROBE_TIMEOUT, move || fs::symlink_metadata(&p)) {
        Some(Ok(_)) => true,
        Some(Err(e)) => e.raw_os_error() != Some(libc::ENOTCONN),
        None => false, // 超时=hung → 视为不健康。
    }
}
```

把 `canonicalized_target` 改为（**不再 stat 叶子**；仅超时包裹 `canonicalize(parent)`）：

```rust
/// 规范化挂载点用于与 mountinfo（内核规范路径）精确比对。**不对叶子 canonicalize**
/// （hung FUSE 下 stat 叶子会 D 睡眠永阻塞）；仅规范化父目录再拼回末段，父目录也 wedge 的
/// 极端情形由超时兜底回退未规范化原路径（宁可偶发漏判也不 hang）。
fn canonicalized_target(path: &Path) -> std::path::PathBuf {
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        let parent = parent.to_path_buf();
        if let Some(Ok(cp)) = with_timeout(PROBE_TIMEOUT, move || fs::canonicalize(&parent)) {
            return cp.join(name);
        }
    }
    path.to_path_buf()
}
```

- [ ] **Step 4: 加「不 stat 叶子」回归测试**

在 `discovery.rs` 的 `#[cfg(test)] mod tests` 加（锁定叶子坏 symlink 也不报错、不 stat）：

```rust
#[test]
fn canonicalized_target_does_not_stat_leaf_segment() {
    // 末段是坏 symlink（指向不存在目标）：整路径 canonicalize 会失败，但本函数只规范化父目录，
    // 故仍返回 canonicalize(父)/末段，且不因坏 symlink 报错。
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent");
    std::fs::create_dir(&parent).unwrap();
    let leaf = parent.join("mnt");
    std::os::unix::fs::symlink("/no/such/target/anywhere", &leaf).unwrap();
    let got = canonicalized_target(&leaf);
    let want = std::fs::canonicalize(&parent).unwrap().join("mnt");
    assert_eq!(got, want, "应仅规范化父目录、原样拼回末段，不解析/stat 末段");
}
```

- [ ] **Step 5: 全量 discovery 测试确认无回归**

Run: `cd fuse && cargo test --lib discovery && cargo test --lib hang_free`
Expected: 既有 `canonicalized_target_*`/`endpoint_ok_*` + 新增全 PASS。`cargo clippy --lib -- -D warnings` 与 `cargo fmt` 干净。

> 说明：`endpoint_ok`/`canonicalized_target` 的**超时分支**无法在单测里确定性构造 hung 挂载；`with_timeout` 的超时语义已由 Step 1 两测直接覆盖，wedge 下的端到端行为由 Task 7 集成 + 审查兜底。

- [ ] **Step 6: 提交**

```bash
git add fuse/src/enable/hang_free.rs fuse/src/enable/mod.rs fuse/src/enable/discovery.rs
git commit -m "feat(discovery): hang-free 探测基础（hang_free 模块 + 超时化 endpoint_ok/canonicalized_target）"
```

---

## Task 3: `force_umount` 原语——abort 连接 + 带超时的 fusermount

**Files:**
- Create: `fuse/src/enable/force_umount.rs`
- Modify: `fuse/src/enable/mod.rs`（加 `pub mod force_umount;`，紧接 `pub mod discovery;` 后）
- Test: `fuse/src/enable/force_umount.rs`（`#[cfg(test)]` 内）

**Interfaces:**
- Consumes: `super::discovery::{is_mounted, mount_connection_id}`。
- Produces:
  - `pub enum CmdOutcome { Success, Failed, TimedOut, NotFound }`
  - `pub fn abort_connection(id: u64) -> std::io::Result<()>` — 写 `/sys/fs/fuse/connections/<id>/abort`；文件不存在幂等成功。
  - `pub(crate) fn run_fusermount(args: &[&std::ffi::OsStr], timeout: Duration) -> CmdOutcome` — spawn `fusermount3` 回退 `fusermount`，带看门狗超时 kill。

- [ ] **Step 1: 写失败测试**

新建 `fuse/src/enable/force_umount.rs`，先只放骨架 + 测试：

```rust
//! Hang-free 分档卸载引擎（见 docs/07-hangfree-umount.md）。

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_connection_is_idempotent_for_missing_conn() {
        // 不存在的连接号 → 视为已消失，Ok。
        assert!(abort_connection(u64::MAX).is_ok());
    }

    #[test]
    fn run_fusermount_notfound_when_binary_absent() {
        // 用一个不存在的挂载点 + 极短超时；真实 fusermount 会快速失败（非 hang）。
        // 断言不 panic 且返回可判定的 outcome（Failed / NotFound）。
        let mp = Path::new("/nonexistent/zipfs/mp");
        let out = run_fusermount(&[OsStr::new("-u"), mp.as_os_str()], Duration::from_secs(2));
        assert!(matches!(out, CmdOutcome::Failed | CmdOutcome::NotFound));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd fuse && cargo test --lib force_umount`
Expected: 编译失败（`abort_connection`/`run_fusermount`/`CmdOutcome` 未定义）。

- [ ] **Step 3: 写最小实现**

在 `force_umount.rs`（`mod tests` 之上）加：

```rust
use std::io::ErrorKind;
use std::process::{Command, Stdio};

/// 外部卸载命令的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum CmdOutcome {
    Success,
    Failed,
    TimedOut,
    NotFound,
}

/// 写 `/sys/fs/fuse/connections/<id>/abort` 解除在飞/hung 请求。best-effort：
/// 连接已消失/无权限/已断开（NotFound/PermissionDenied/NotConnected）均视为非致命。
pub fn abort_connection(id: u64) -> std::io::Result<()> {
    let path = format!("/sys/fs/fuse/connections/{id}/abort");
    match std::fs::write(&path, b"1") {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::NotConnected
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

const CMD_POLL: Duration = Duration::from_millis(50);

/// 单次 spawn 一个 fusermount 二进制并等其退出（带看门狗子超时）。
/// 返回 None 表示该二进制不存在（应换下一个）；Some(outcome) 为已执行的结果。
fn spawn_once(bin: &str, args: &[&OsStr], deadline: std::time::Instant) -> Option<CmdOutcome> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(_) => return Some(CmdOutcome::Failed),
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Some(CmdOutcome::Success),
            Ok(Some(_)) => return Some(CmdOutcome::Failed), // 跑过但非零（busy）。
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Some(CmdOutcome::TimedOut);
                }
                std::thread::sleep(CMD_POLL);
            }
            Err(_) => return Some(CmdOutcome::Failed),
        }
    }
}

/// spawn `fusermount3` 回退 `fusermount`，在 `timeout` 内**轮询重试**吸收瞬态 EBUSY。
/// 正确区分：`Success` / `Failed`（跑过但始终非零/超时）/ `NotFound`（两二进制皆缺）。
pub(crate) fn run_fusermount(args: &[&OsStr], timeout: Duration) -> CmdOutcome {
    let deadline = std::time::Instant::now() + timeout;
    let mut ran = false; // 是否至少成功 spawn 过一个二进制（区分 Failed vs NotFound）。
    loop {
        for bin in ["fusermount3", "fusermount"] {
            match spawn_once(bin, args, deadline) {
                None => continue, // 该二进制缺失，试下一个。
                Some(CmdOutcome::Success) => return CmdOutcome::Success,
                Some(_) => {
                    ran = true; // 跑过但失败/超时 → 重试直到 deadline。
                    break;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return if ran { CmdOutcome::Failed } else { CmdOutcome::NotFound };
        }
        std::thread::sleep(CMD_POLL);
    }
}
```

在 `mod.rs` 加模块声明：

```rust
pub mod discovery;
pub mod force_umount;
```

- [ ] **Step 4: 运行确认通过**

Run: `cd fuse && cargo test --lib force_umount`
Expected: 2 测试 PASS。

> 注：`run_fusermount_notfound_when_binary_absent` 若环境装了 fusermount，则对不存在挂载点返回 `Failed`；未装则 `NotFound`——两者都被断言接受。

- [ ] **Step 5: 提交**

```bash
git add fuse/src/enable/force_umount.rs fuse/src/enable/mod.rs
git commit -m "feat(force_umount): abort 连接 + 带超时兜底的 fusermount 原语"
```

---

## Task 4: `force_umount` 分档引擎 + 升级决策

**Files:**
- Modify: `fuse/src/enable/force_umount.rs`
- Test: `fuse/src/enable/force_umount.rs`（`#[cfg(test)]` 内）

**Interfaces:**
- Consumes: `CmdOutcome`、`abort_connection`、`run_fusermount`（Task 3）；`super::discovery::{is_mounted, endpoint_ok, mount_connection_id}`（Task 1 + 既有 `endpoint_ok`）。
- Produces:
  - `pub enum UmountLevel { Clean, Lazy, Abort, Auto }`（`derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)`）
  - `pub struct UmountReport { was_mounted, connection_id: Option<u64>, level_reached: UmountLevel, aborted: bool, unmounted: bool }`
  - `pub(crate) fn next_level(cur: UmountLevel) -> Option<UmountLevel>` — auto 升级链的纯决策：Clean→Lazy→Abort→None；`Auto` 输入返回 None（不参与升级链，由 `umount()` 展开）。
  - `pub fn umount(mountpoint: &Path, level: UmountLevel) -> std::io::Result<UmountReport>`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn next_level_escalation_chain() {
    assert_eq!(next_level(UmountLevel::Clean), Some(UmountLevel::Lazy));
    assert_eq!(next_level(UmountLevel::Lazy), Some(UmountLevel::Abort));
    assert_eq!(next_level(UmountLevel::Abort), None);
    assert_eq!(next_level(UmountLevel::Auto), None); // Auto 不参与升级链决策。
}

#[test]
fn umount_reports_not_mounted_as_success() {
    // 未挂载的路径：was_mounted=false、unmounted=true、不 abort。
    let mp = Path::new("/definitely/not/mounted/zipfs");
    let r = umount(mp, UmountLevel::Auto).unwrap();
    assert!(!r.was_mounted);
    assert!(r.unmounted);
    assert!(!r.aborted);
}

#[test]
fn umount_level_parses_from_str() {
    use clap::ValueEnum;
    assert_eq!(UmountLevel::from_str("auto", true).unwrap(), UmountLevel::Auto);
    assert_eq!(UmountLevel::from_str("abort", true).unwrap(), UmountLevel::Abort);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd fuse && cargo test --lib force_umount`
Expected: 编译失败（`UmountLevel`/`umount`/`next_level` 未定义）。

- [ ] **Step 3: 写最小实现**

在 `force_umount.rs` 顶部 `use` 补 `use super::discovery;`，并加：

```rust
/// 卸载档位（CLI `--level` 值）。见 docs/07-hangfree-umount.md §3。
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UmountLevel {
    /// fusermount -u：daemon flush，耐久；busy 失败。
    Clean,
    /// fusermount -uz：懒摘除，无 abort。
    Lazy,
    /// abort 连接 → fusermount -uz：解耦 daemon 存活，可能丢在飞写。
    Abort,
    /// clean →(仍挂)→ lazy →(仍挂且 daemon 不存活)→ abort。默认。
    Auto,
}

/// 一次卸载的结果，供日志与测试断言。
#[derive(Debug, PartialEq, Eq)]
pub struct UmountReport {
    pub was_mounted: bool,
    pub connection_id: Option<u64>,
    pub level_reached: UmountLevel,
    pub aborted: bool,
    pub unmounted: bool,
}

/// auto 升级链的纯决策：Clean→Lazy→Abort→None。`Auto` 不参与（由 umount() 展开）。
pub(crate) fn next_level(cur: UmountLevel) -> Option<UmountLevel> {
    match cur {
        UmountLevel::Clean => Some(UmountLevel::Lazy),
        UmountLevel::Lazy => Some(UmountLevel::Abort),
        UmountLevel::Abort | UmountLevel::Auto => None,
    }
}

/// 单档超时上界（clean/lazy/abort 各自的摘除等待，内含轮询重试）。
const STEP_TIMEOUT: Duration = Duration::from_secs(3);

/// 执行单一档位的一次尝试；返回该档结束后是否已卸载。
fn attempt(mountpoint: &Path, level: UmountLevel, report: &mut UmountReport) -> bool {
    let mp = mountpoint.as_os_str();
    match level {
        UmountLevel::Clean => {
            run_fusermount(&[OsStr::new("-u"), mp], STEP_TIMEOUT);
        }
        UmountLevel::Lazy => {
            run_fusermount(&[OsStr::new("-u"), OsStr::new("-z"), mp], STEP_TIMEOUT);
        }
        UmountLevel::Abort => {
            if let Some(id) = report.connection_id {
                let _ = abort_connection(id); // best-effort
                report.aborted = true;
            }
            run_fusermount(&[OsStr::new("-u"), OsStr::new("-z"), mp], STEP_TIMEOUT);
        }
        UmountLevel::Auto => unreachable!("auto 由 umount() 展开为具体档"),
    }
    !discovery::is_mounted(mountpoint)
}

/// 按档位卸载 `mountpoint`。全程 hang-free（外部命令带超时、探测读 /proc）。
pub fn umount(mountpoint: &Path, level: UmountLevel) -> std::io::Result<UmountReport> {
    let mut report = UmountReport {
        was_mounted: discovery::is_mounted(mountpoint),
        connection_id: discovery::mount_connection_id(mountpoint),
        level_reached: level,
        aborted: false,
        unmounted: false,
    };
    if !report.was_mounted {
        report.unmounted = true;
        return Ok(report);
    }

    if level == UmountLevel::Auto {
        // clean → lazy 逐级；升级到 abort 前必须确证 daemon 死/卡（endpoint_ok 守卫），
        // 否则健康 busy 挂载会被误 abort 丢在飞写（见 docs/07 §3.1）。
        for cur in [UmountLevel::Clean, UmountLevel::Lazy] {
            report.level_reached = cur;
            if attempt(mountpoint, cur, &mut report) {
                report.unmounted = true;
                return Ok(report);
            }
        }
        // 仍挂：只有 daemon 不存活才允许 abort。
        if discovery::endpoint_ok(mountpoint) {
            return Err(std::io::Error::other(format!(
                "daemon 存活但挂载仍 busy，拒绝 abort（护在飞写）；请释放占用后重试或用 --level abort 强制：{}",
                mountpoint.display()
            )));
        }
        report.level_reached = UmountLevel::Abort;
        if attempt(mountpoint, UmountLevel::Abort, &mut report) {
            report.unmounted = true;
            return Ok(report);
        }
        return Err(std::io::Error::other(format!(
            "auto 升级至 abort 仍未摘除：{}",
            mountpoint.display()
        )));
    }

    // 显式单档：不自动升级，失败如实报错。
    report.level_reached = level;
    if attempt(mountpoint, level, &mut report) {
        report.unmounted = true;
        Ok(report)
    } else {
        Err(std::io::Error::other(format!(
            "{level:?} 卸载未摘除：{}",
            mountpoint.display()
        )))
    }
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cd fuse && cargo test --lib force_umount`
Expected: 全部 PASS（Task 3 的 2 个 + 本任务 3 个）。

- [ ] **Step 5: clippy 确认**

Run: `cd fuse && cargo clippy --lib -- -D warnings`
Expected: 无告警。

- [ ] **Step 6: 提交**

```bash
git add fuse/src/enable/force_umount.rs
git commit -m "feat(force_umount): 分档卸载引擎 clean/lazy/abort/auto + 升级链"
```

---

## Task 5: CLI `zipfs umount --level` + `umount-managed --level auto` 接线

**Files:**
- Modify: `fuse/src/main.rs`（`Command` enum 加 `Umount`；`MountManagedArgs` 加 `--level`；`run_umount_managed` 切引擎；新增 `run_umount`）
- Test: `fuse/tests/enable.rs` 或 `fuse/src/main.rs` 无法直接单测 clap → 由 Task 6 集成覆盖；本任务加一个 `#[cfg(test)]` 的 clap 解析测试到 main.rs。

**Interfaces:**
- Consumes: `zipfs::enable::force_umount::{umount, UmountLevel}`、`zipfs::enable::model::{Paths, validate_name}`、`zipfs::enable::systemd::{systemd_unescape, systemd_escape}`。
- Produces: 顶层子命令 `zipfs umount --name <raw-project-name> [--level <UmountLevel>]`；`umount-managed` 支持 `--level`（默认 `Auto`）。

> **命名约定（关键，勿混淆）**：`systemd_unescape` 对**裸 `-` 会解成 `/`**（有损），故只有 systemd 传的 **escaped `%i`**（形如 `\x2dhome\x2dxp\x2dsrc\x2dfoo`）才可 unescape。与既有 `enable apply/restore/status` 一致，**面向用户的 `zipfs umount --name` 收的是 RAW project 名**（如 `-home-xp-src-foo`，即 projects 目录名），**不 unescape**，直接 `validate_name`+`mountpoint`。因 raw 名以 `-` 开头，`UmountArgs.name` 须 `allow_hyphen_values`。仅 `umount-managed`（systemd ExecStop 用，收 escaped `%i`）才 `systemd_unescape`。C1 提示里的 systemd 单元名用 `systemd_escape(raw)` 反算。

- [ ] **Step 1: 写失败测试**

在 `main.rs` 末尾 `#[cfg(test)] mod cli_tests` 加（clap 解析冒烟；raw 名以 `-` 开头需 allow_hyphen_values）：

```rust
#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_umount_with_default_level() {
        // 面向用户：RAW project 名（以 - 开头，来自 projects 目录名）；默认档 auto。
        let cli = Cli::parse_from(["zipfs", "umount", "--name", "-home-xp-src-foo"]);
        match cli.command {
            Some(Command::Umount(a)) => {
                assert_eq!(a.name, "-home-xp-src-foo");
                assert_eq!(a.level, zipfs::enable::force_umount::UmountLevel::Auto);
            }
            _ => panic!("应解析为 Umount 子命令"),
        }
    }

    #[test]
    fn parses_umount_managed_with_level() {
        // systemd ExecStop：escaped %i（无前导 -）+ 显式档。
        let cli = Cli::parse_from([
            "zipfs", "umount-managed", "--name", "\\x2dhome\\x2dxp", "--level", "abort",
        ]);
        match cli.command {
            Some(Command::UmountManaged(a)) => {
                assert_eq!(a.level, zipfs::enable::force_umount::UmountLevel::Abort);
            }
            _ => panic!("应解析为 UmountManaged"),
        }
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd fuse && cargo test --bin zipfs cli_tests`
Expected: 编译失败（`Command::Umount` 不存在、`MountManagedArgs` 无 `level`）。

- [ ] **Step 3: 写实现**

改 `MountManagedArgs`（按内容定位，非行号）加 `level` 字段：

```rust
#[derive(clap::Args, Debug)]
struct MountManagedArgs {
    /// systemd 实例字符串（escaped 形态，即模板里的 `%i`）。
    #[arg(long)]
    name: String,
    /// 卸载档位（仅 umount-managed 用；mount-managed 忽略）。默认 auto。
    #[arg(long, value_enum, default_value = "auto")]
    level: zipfs::enable::force_umount::UmountLevel,
}
```

新增 `UmountArgs` 与 `Command::Umount`（在 `Command` enum 内 `UmountManaged` 之后）：

```rust
    /// 按档位卸载某项目挂载（hang-free）：clean/lazy/abort/auto。见 docs/07。
    ///
    /// 用法：`zipfs umount --name <项目名> [--level clean|lazy|abort|auto]`。
    Umount(UmountArgs),
```

```rust
/// `umount` 子命令参数。
#[derive(clap::Args, Debug)]
struct UmountArgs {
    /// RAW project 名（projects 目录名，如 `-home-xp-src-foo`）。与 enable 一致，不 unescape。
    /// 名以 `-` 开头，故 `allow_hyphen_values` 让 clap 接受前导短横值。
    #[arg(long, allow_hyphen_values = true)]
    name: String,
    /// 卸载档位，默认 auto（clean→lazy→abort 升级）。
    #[arg(long, value_enum, default_value = "auto")]
    level: zipfs::enable::force_umount::UmountLevel,
}
```

`main()` 的 `match` 加分支（在 `UmountManaged` 后）：

```rust
        Some(Command::Umount(args)) => run_umount(args),
```

改 `run_umount_managed`（按内容定位）切引擎，并加 `run_umount`：

```rust
/// systemd 托管卸载（ExecStop）：unescape escaped `%i` → 按 --level 走 hang-free 引擎。
fn run_umount_managed(args: MountManagedArgs) -> std::io::Result<()> {
    let paths = zipfs::enable::model::Paths::resolve(&home_or_err()?);
    let name = zipfs::enable::systemd::systemd_unescape(&args.name);
    zipfs::enable::model::validate_name(&name).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("{e}（systemd 实例 %i={:?} → 解码名 {name:?}）", args.name),
        )
    })?;
    let mp = paths.mountpoint(&name);
    // 引擎错误也附 %i/解码名上下文，便于 ExecStop 失败日志定位实例（与 run_mount_managed 对齐）。
    let report = zipfs::enable::force_umount::umount(&mp, args.level).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("{e}（systemd 实例 %i={:?} → 解码名 {name:?}）", args.name),
        )
    })?;
    info!(
        "umount-managed: name={name} level={:?} reached={:?} aborted={} unmounted={}",
        args.level, report.level_reached, report.aborted, report.unmounted
    );
    Ok(())
}

/// 面向用户的按档位卸载。`name` 是 RAW project 名（与 enable 一致，不 unescape）。
fn run_umount(args: UmountArgs) -> std::io::Result<()> {
    let paths = zipfs::enable::model::Paths::resolve(&home_or_err()?);
    let name = &args.name;
    zipfs::enable::model::validate_name(name)?;
    let mp = paths.mountpoint(name);
    // C1：本命令不 systemctl stop；托管实例（Restart=on-failure）直卸可能与自动重挂竞态。
    eprintln!(
        "提示：若 {name} 由 systemd 托管，请优先 `systemctl --user stop zipfs@{}.service`；\
         本命令仅作强制兜底。",
        zipfs::enable::systemd::systemd_escape(name)
    );
    let report = zipfs::enable::force_umount::umount(&mp, args.level)?;
    info!(
        "umount: name={name} level={:?} reached={:?} aborted={} unmounted={}",
        args.level, report.level_reached, report.aborted, report.unmounted
    );
    Ok(())
}
```

> 删掉 `run_umount_managed` 原来的 `use zipfs::enable::daemon::Mounter;` 与 `RealMounter.unmount` 调用（已被引擎取代）。`mount_args_from_spec`/`run_mount_managed` 不动。

- [ ] **Step 4: 运行确认通过**

Run: `cd fuse && cargo test --bin zipfs cli_tests`
Expected: 2 测试 PASS。

- [ ] **Step 5: 全量构建 + clippy**

Run: `cd fuse && cargo build && cargo clippy --bin zipfs -- -D warnings`
Expected: 构建绿、无告警。

- [ ] **Step 6: 提交**

```bash
git add fuse/src/main.rs
git commit -m "feat(cli): zipfs umount --level + umount-managed 走分档引擎"
```

---

## Task 6: systemd `ExecStop` 默认 `--level auto`

**Files:**
- Modify: `fuse/src/enable/autostart.rs:47`（`ExecStop` 模板）与 `:179`（断言）
- Test: `fuse/src/enable/autostart.rs`（既有 `#[cfg(test)]` 断言更新）

**Interfaces:**
- Consumes: 无新增。
- Produces: 生成的单元 `ExecStop={exe} umount-managed --name %i --level auto`。

- [ ] **Step 1: 改测试断言（先 RED）**

将 `autostart.rs:179` 断言改为：

```rust
        assert!(body.contains("ExecStop=/usr/bin/zipfs umount-managed --name %i --level auto"));
```

- [ ] **Step 2: 运行确认失败**

Run: `cd fuse && cargo test --lib autostart`
Expected: FAIL（当前模板无 `--level auto`）。

- [ ] **Step 3: 改模板**

`autostart.rs:47` 改为：

```rust
         ExecStop={exe} umount-managed --name %i --level auto\n\
```

- [ ] **Step 4: 运行确认通过**

Run: `cd fuse && cargo test --lib autostart`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add fuse/src/enable/autostart.rs
git commit -m "feat(autostart): ExecStop 默认 --level auto，卡死自动升级摘除"
```

---

## Task 7: 集成测试——真挂载分档 + wedge 恢复

**Files:**
- Create: `fuse/tests/umount_levels.rs`
- 参照：`fuse/tests/systemd_mount.rs`、`fuse/tests/mount_rw.rs`（真挂载起停范式）

**Interfaces:**
- Consumes: 已构建的 `zipfs` 二进制（`env!("CARGO_BIN_EXE_zipfs")`）、`zipfs::enable::force_umount::{umount, UmountLevel}`、`zipfs::enable::discovery::is_mounted`。

> **起挂载 helper 需新写**（现有 `tests/systemd_mount.rs`/`tests/mount_rw.rs` 起挂载**不带 `--pid-file`**，无法取 daemon PID）。新 helper `common::mount_shadow()`：起 `zipfs mount --backend shadow ... --pid-file <tmp>`（起停/等就绪范式照抄 `mount_rw.rs`），返回 `{ mountpoint, daemon_pid, _backing, _tmp }`；`daemon_pid` 从 pid-file 读。`common::sigkill(pid)` 发 `SIGKILL`（`libc::kill` 或 `nix`）。整个测试文件加 `skip_reason()` 门控（`/dev/fuse` 可用 + `fusermount(3)` 在 PATH + `/sys/fs/fuse/connections` 可写），对齐 `systemd_mount.rs` 的 skip 范式；无 FUSE 的 CI 上 skip。

- [ ] **Step 1: 写测试骨架 + clean/lazy 用例**

```rust
//! 分档卸载集成：真起 zipfs 挂载，验证各档摘除与 wedge 恢复。见 docs/07。
mod common; // 若现有 tests 有共享 helper，则复用；否则内联最小起挂载 helper（照抄 mount_rw.rs）。

use std::path::Path;
use zipfs::enable::discovery::is_mounted;
use zipfs::enable::force_umount::{umount, UmountLevel};

#[test]
fn clean_level_unmounts_healthy_mount() {
    let m = common::mount_shadow(); // 起一个健康 shadow 挂载，返回 { mountpoint, .. }
    assert!(is_mounted(&m.mountpoint));
    let r = umount(&m.mountpoint, UmountLevel::Clean).unwrap();
    assert!(r.unmounted);
    assert!(!r.aborted);
    assert!(!is_mounted(&m.mountpoint));
}

#[test]
fn lazy_level_unmounts_healthy_mount() {
    let m = common::mount_shadow();
    let r = umount(&m.mountpoint, UmountLevel::Lazy).unwrap();
    assert!(r.unmounted);
    assert!(!is_mounted(&m.mountpoint));
}
```

- [ ] **Step 2: 运行确认（起挂载 helper 就绪后）通过**

Run: `cd fuse && cargo test --test umount_levels clean_level lazy_level -- --test-threads=1`
Expected: 2 PASS。若 CI 无 FUSE，测试用 `#[ignore]` 标注并在本地跑（照抄 `systemd_mount.rs` 的门控方式）。

- [ ] **Step 3: 加 auto-健康「停在 clean、不误触 abort」用例**

```rust
#[test]
fn auto_stops_at_clean_for_healthy_mount() {
    let m = common::mount_shadow();
    let r = umount(&m.mountpoint, UmountLevel::Auto).unwrap();
    assert_eq!(r.level_reached, UmountLevel::Clean);
    assert!(!r.aborted, "健康挂载不应升级到 abort");
    assert!(!is_mounted(&m.mountpoint));
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cd fuse && cargo test --test umount_levels auto_stops_at_clean -- --test-threads=1`
Expected: PASS。

- [ ] **Step 5: 加 wedge 恢复用例（SIGKILL daemon → 显式 abort 档 + auto 兜底）**

```rust
#[test]
fn explicit_abort_recovers_wedged_mount() {
    let m = common::mount_shadow();
    common::sigkill(m.daemon_pid); // 守护死 → 留陈旧挂载。
    assert!(is_mounted(&m.mountpoint));
    // 显式 abort 档：无条件 abort 连接 + lazy，断言 aborted 走过。
    let r = umount(&m.mountpoint, UmountLevel::Abort).unwrap();
    assert!(r.unmounted);
    assert!(r.aborted, "显式 abort 档应写过连接 abort");
    assert!(!is_mounted(&m.mountpoint));
}

#[test]
fn auto_recovers_wedged_mount() {
    let m = common::mount_shadow();
    common::sigkill(m.daemon_pid);
    assert!(is_mounted(&m.mountpoint));
    // auto 对 wedge：daemon 已死（endpoint_ok=false），守卫放行；
    // 实际多半 lazy 即摘除（正是真实事故里 fusermount -uz 生效的情形），
    // 故只断言最终摘除，不锁定停在哪一档（Lazy 或 Abort 皆合法）。
    let r = umount(&m.mountpoint, UmountLevel::Auto).unwrap();
    assert!(r.unmounted);
    assert!(!is_mounted(&m.mountpoint));
    assert!(
        matches!(r.level_reached, UmountLevel::Lazy | UmountLevel::Abort),
        "wedge 恢复应停在 lazy 或 abort，实得 {:?}",
        r.level_reached
    );
}
```

> `common::sigkill(pid)` 与 `daemon_pid` 由 Step 1 新写的 helper 提供（`mount_shadow()` 用 `--pid-file` 读 PID）。**关键**：daemon 死后 `fusermount -uz`（lazy，MNT_DETACH）通常即可摘除陈旧挂载（真实事故正是如此），所以 `auto` 多半停在 **lazy** 而非 abort——故 abort 档的 `aborted` 断言用**显式 `--level abort`** 驱动；`auto` 用例只验「最终摘除」。健康 busy 挂载（daemon 活、endpoint_ok=true）则被守卫拦在 lazy 之前不 abort（由 `auto_stops_at_clean` 侧证正常关闭不误触）。

- [ ] **Step 6: 运行确认通过**

Run: `cd fuse && cargo test --test umount_levels _wedged_mount -- --test-threads=1`
Expected: `explicit_abort_recovers_wedged_mount` 与 `auto_recovers_wedged_mount` 均 PASS。

- [ ] **Step 7: 全量测试 + 提交**

Run: `cd fuse && cargo test`
Expected: 既有 + 新增全绿。

```bash
git add fuse/tests/umount_levels.rs
git commit -m "test(umount): 真挂载分档 + wedge 恢复集成"
```

---

## Self-Review

**Spec coverage（对照 docs/07）：**
- §3 四档 clean/lazy/abort/auto → Task 4（引擎）+ Task 5（CLI 暴露）。✓
- §3 升级梯 + STEP_TIMEOUT + clean/lazy 重试吸收瞬态 EBUSY → Task 3 `run_fusermount`/Task 4 `attempt`/`umount`。✓
- §3.1 abort 守卫（`endpoint_ok`：daemon 活则停 lazy 不 abort，护在飞写）→ Task 4 `umount` auto 分支 + Task 7 `auto_stops_at_clean` / `abort_recovers`（SIGKILL 后 endpoint_ok=false 才 abort）。✓
- §4.1 force_umount 模块与接口 → Task 3+4。✓
- §4.1 `mount_connection_id` → Task 1。✓
- §4.2 探测硬化（canonicalize 超时）→ Task 2。✓
- §4.3 CLI `umount`（+ C1 托管警告、escaped name 约定）+ `umount-managed --level`（+ %i 错误上下文）→ Task 5。✓
- §4.4 systemd ExecStop `--level auto` → Task 6。✓
- §6 错误处理（auto 升级不冒泡、单档如实报错、abort best-effort 多 errno、%i 上下文、validate_name）→ Task 3/4 + Task 5。✓
- §7 测试策略（解析单测 + 真挂载 + wedge，新写 pid-file helper + skip 门控）→ Task 1/3/4/7。✓
- §9 已知债务（`Mounter::unmount`/lifecycle 旧路径未收敛）→ 显式不在本 plan 范围，文档标注。✓

**Placeholder scan：** 无 TBD/TODO；每个改码步骤含完整代码。Task 7 的起挂载 helper 显式标注**需新写**（`--pid-file` 读 PID + skip 门控），非占位、非「照抄」。

**Type consistency：** `UmountLevel`/`UmountReport`/`CmdOutcome` 字段与方法名跨 Task 1/3/4/5/7 一致；`umount(&Path, UmountLevel) -> io::Result<UmountReport>`、`mount_connection_id(&Path) -> Option<u64>`、`parse_connection_id(&str, &Path) -> Option<u64>`、`run_fusermount(&[&OsStr], Duration) -> CmdOutcome`、`spawn_once(&str, &[&OsStr], Instant) -> Option<CmdOutcome>`、`abort_connection(u64) -> io::Result<()>`、`next_level(UmountLevel) -> Option<UmountLevel>`（`Auto`→None）全对齐；auto 分支消费既有 `discovery::endpoint_ok(&Path) -> bool`。
