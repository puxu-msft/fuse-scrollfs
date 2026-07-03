# 07 — Hang-free 分档卸载

> 设计文档。目标：把 zipfs 的卸载做成**不会被死/卡 daemon 拖住的分档升级梯**，用户按需选择档位，systemd 默认自动升级，正常关闭仍走会 flush 的耐久路径。

## 1. 背景与触发

一次真实事故：`zipfs@-home-xp-src-neighbors.service` 的守护被 `SIGTERM` 杀死，但 `ExecStop`（`zipfs umount-managed`）退出码 1 失败，留下一个**陈旧 FUSE 挂载**。任何进程（如 Claude 读该项目目录）访问该挂载点即在不可中断 I/O 上 hang。人工用 `fusermount -uz` 才摘除。

### 1.1 根因（经代码核对，纠正初判）

初判「`is_mounted()` 会 stat 挂载点导致 hang」曾被误认为「已 hang-free」——**两者都不准**，起因是规划时读到了另一并发会话对 `discovery.rs` 的**未提交** WIP、误当已提交现状。核对**已提交基线**（本分支 checkout 的干净提交）后：

- `discovery::endpoint_ok` 是裸 `fs::symlink_metadata(path)`，**无超时 → 在 wedge FUSE 上会永久阻塞（D 睡眠）**。
- `discovery::canonicalized_target` **先对整叶子 `fs::canonicalize(path)`**（stat 叶子），wedge 下同样 hang；`is_mounted()` 经它间接**并非** hang-free。
- 已提交基线里**没有** `with_timeout`/`PROBE_TIMEOUT`。

真实缺口收敛为四条（前三是卸载路径，第四是探测基础）：

1. **无 lazy、无 abort。** `daemon::unmount_path` 只尝试非-lazy 的 `fusermount3 -u` / `fusermount -u`；busy/broken 时返回失败（这次 status=1 的直接原因），既不 `-z` 懒卸载，也不先 abort FUSE 连接。
2. **外部子进程无超时兜底。** `unmount_path` 调用 `fusermount` 用 `.status()` 同步等待；当 daemon 带在飞请求 wedge 时，非-lazy `umount2` 可在内核侧阻塞，卸载命令自身即可能 hang。
3. **探测本身会 hang。** `endpoint_ok`（无超时）与 `canonicalized_target`（stat 叶子）在 wedge 下阻塞——而本特性的 abort 守卫依赖 `endpoint_ok`、`is_mounted` 依赖 `canonicalized_target`，**探测不 hang-free 则整个引擎不成立**。故本特性须先建立 hang-free 探测基础（§4.2）。

> **并发说明**：另一会话正在 `discovery.rs` 未提交 WIP 里独立做同样的探测硬化（`with_timeout`/`PROBE_TIMEOUT` + 超时化 `endpoint_ok`/`canonicalized_target`）。本分支自建同契约的干净实现（模块化到 `hang_free.rs`），两者合入时 `endpoint_ok`/`canonicalized_target` 函数体会冲突，属预期的「并行重复硬化」合并点，见 §9。

## 2. 目标 / 非目标

**目标**

- 提供**用户可选的卸载档位**：从耐久（会 flush）到强制（与 daemon 存活彻底解耦）。
- 卸载引擎自身**永不 hang**：所有可能阻塞的步骤（外部 `fusermount`、`canonicalize`）均有超时上界。
- systemd `ExecStop` 默认走**自动升级**：正常关闭耐久，卡死场景自动升级到强制摘除，不再留陈旧挂载。
- 面向用户的手动兜底命令 `zipfs umount --name <inst> --level abort`。

**非目标**

- 不改挂载/写入路径的耐久性协议（尾日志、崩溃提交等另有文档 04）。
- 不引入新的 systemd 单元模板（决定：CLI 原语复用到现有 `ExecStop`，不新建 oneshot 单元）。
- 不改 `is_mounted()`/`endpoint_ok()` 的**语义**，只做超时硬化（返回值含义不变，仅在 wedge 下由「阻塞」变「快速失败」）。

## 3. 卸载档位（升级梯）

| 档位 | 动作 | 语义 / 代价 |
|---|---|---|
| `clean` | `fusermount -u`（超时上界内） | daemon 收 DESTROY → flush，**耐久**；busy/wedge 时失败或超时（不 hang） |
| `lazy` | `fusermount -uz` | 从命名空间摘除即使 busy；不 abort，无额外在飞丢失；已 hang 的读者不保证立即解除 |
| `abort` | 写 `/sys/fs/fuse/connections/<id>/abort` → `fusermount -uz` | 解除 hung 读者、与 daemon 存活解耦；**可能丢未 flush 的在飞写**，仅用于死/卡 daemon |
| `auto`（默认） | `clean` →(仍挂)→ `lazy` →(仍挂 **且 daemon 不存活**)→ `abort` | 耐久优先 + 保证摘除；正常 stop 走 `clean` flush，仅**确证卡死**才升级到 abort |

档位之间用 hang-free 的 mountinfo 复查 + 短超时（默认 `STEP_TIMEOUT = 3s`）判断是否需要升级。`clean`/`lazy` 的外部 `fusermount` 在 `STEP_TIMEOUT` 内**轮询重试**（对齐 `daemon.rs` 的 `POLL_STEP`/`POLL_MAX` 范式），吸收刚 detach 的瞬态 EBUSY，避免误判需升级。

### 3.1 耐久性权衡（显式）——`auto` 升 abort 的守卫

`abort` 写连接 `abort` 会让内核错误化所有在飞请求：客户端未 ack 的写返回 EIO（符合语义——未 ack 即无保证）；daemon 自身缓冲但未 flush 的尾状态是否丢失取决于 daemon 退出路径（见文档 04）。因此 `abort` **只应在 daemon 已死/卡死时**触发。

**关键守卫（防误 abort 健康挂载丢数据）**：`auto` 升级到 `abort` 前，必须叠加 **daemon 存活探测** `discovery::endpoint_ok(mountpoint)`（`with_timeout(PROBE_TIMEOUT)` 包裹的 `symlink_metadata`，hang-free）：

- `endpoint_ok == false`（stat 超时=hung，或 ENOTCONN=stale）→ daemon 死/卡 → 允许升级 `abort`。
- `endpoint_ok == true`（daemon 存活，只是 busy）→ **禁止 abort**：`auto` 停在 `lazy`。健康 daemon 的挂载即便 busy，也用 `lazy` 从命名空间摘除（引用归零后 daemon 自然收尾 flush），绝不 abort 其在飞写。

这把「能 clean/lazy 就绝不 abort」从口号落到可判定信号上：**只有 `is_mounted && !endpoint_ok` 才是「确证卡死」**，才触发 abort。仅凭 `is_mounted` 不足以区分 busy 与 wedge。

## 4. 组件与接口

### 4.1 新模块 `fuse/src/enable/force_umount.rs`

单一职责：按档位驱动一次卸载，全程 hang-free。

```rust
/// 卸载档位（CLI `--level` 值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum UmountLevel { Clean, Lazy, Abort, Auto }

/// 一次卸载的结果，供日志与测试断言。
#[derive(Debug, PartialEq, Eq)]
pub struct UmountReport {
    pub was_mounted: bool,
    pub connection_id: Option<u64>, // 从 mountinfo 解析出的 fuse 连接号
    pub level_reached: UmountLevel, // 实际生效到的档（auto 升级到哪一档）
    pub aborted: bool,
    pub unmounted: bool,
}

/// 按档位卸载 `mountpoint`。不 stat 挂载点；外部命令与 canonicalize 均有超时上界。
pub fn umount(mountpoint: &Path, level: UmountLevel) -> std::io::Result<UmountReport>;
```

内部子步骤（均纯 `/proc`、`/sys` 与带超时的外部命令）：

- `mount_connection_id(mountpoint) -> Option<u64>`：读 `/proc/self/mountinfo`，匹配挂载点（复用 `parse_mountinfo_line` 的 octal unescape + overmount 取末条逻辑），从第 3 字段 `major:minor` 取 `minor` 作 fuse 连接号。**不碰挂载点。** 本产品挂载点为单点（`projects_root/name`），无 overmount，故单 id 足够；取末条与 `is_mounted` 一致。
- `abort_connection(id) -> io::Result<()>`：写 `/sys/fs/fuse/connections/<id>/abort`；连接已消失/无权限/已断开（`NotFound`/`PermissionDenied`/`NotConnected`）均视为非致命（best-effort，调用方亦忽略其错误）。
- `run_fusermount(args, timeout) -> Outcome`：spawn `fusermount3`/`fusermount`，在 `timeout` 内**轮询重试**（吸收瞬态 EBUSY），子进程各自带看门狗超时 kill；正确区分 `Success`/`Failed`（跑过但非零）/`TimedOut`/`NotFound`（二进制皆缺）。
- `endpoint_ok(mountpoint) -> bool`（复用 `discovery`）：daemon 存活探测，`auto` 升 abort 的守卫（见 §3.1）。
- `still_mounted(mountpoint) -> bool`：复用 `discovery::is_mounted`（hang-free）。

### 4.2 hang-free 探测基础 `fuse/src/enable/hang_free.rs`（新模块）+ `discovery.rs` 硬化

新增小模块 `hang_free.rs`（单一职责，~40 行）承载通用 hang-free 原语，供 `discovery` 与 `force_umount` 共用：

- `pub(crate) const PROBE_TIMEOUT: Duration = 800ms`。
- `pub(crate) fn with_timeout<T,F>(dur, f) -> Option<T>`：独立线程跑 `f`，`recv_timeout(dur)` 逃逸；超时返回 None（工作线程可能仍卡 D 睡眠泄漏，刻意取舍——主线程绝不被 hung FUSE 拖住）。

`discovery.rs` 硬化（用上述原语）：

- `endpoint_ok`：`with_timeout(PROBE_TIMEOUT, || symlink_metadata(path))`；`Some(Ok)`→true，`Some(Err(ENOTCONN))`→false，`None`（超时=hung）→false。
- `canonicalized_target`：**不再对叶子 `canonicalize`**；仅 `with_timeout` 包 `canonicalize(parent)` 再拼回叶子名，超时/失败回退未规范化原路径。`is_mounted` 经它间接 hang-free；语义不变（现有 `canonicalized_target_*` 测试保持绿）。

### 4.3 CLI `fuse/src/main.rs`

- 新增顶层子命令 `Umount(UmountArgs)`：`zipfs umount --name <inst> [--level clean|lazy|abort|auto]`（默认 `auto`）。解析实例名 → `Paths::resolve` 算挂载点 → `force_umount::umount`。面向用户。
- `umount-managed`（`ExecStop` 用，内部）改为接受可选 `--level`（默认 `auto`），复用同一引擎；`run_umount_managed` 从 `RealMounter.unmount` 切到 `force_umount::umount(mp, level)`，并对错误 `.map_err` 附 `%i`/解码名上下文（与 `run_mount_managed` 一致，便于 ExecStop 失败日志定位实例）。
- `Mounter::unmount` trait 与其 `RealMounter`/`SystemdMounter` 实现**本次不动**（仍供 lifecycle 的 restore/remount/compact/reingest/seal 调用）；见 §9 已知债务。

**`--name` 约定**：接受 systemd **escaped 实例名**（模板 `%i` 形态）；经 `systemd_unescape` 解码（与 `mount-managed` 同款、有损：裸 `-`→`/`）。用户应传 escaped 名，勿传 path-encoded 原名，否则解码漂移可能算错挂载点（此时 `was_mounted=false` 直接 no-op 返回，不误伤别处）。

**托管实例警告（C1）**：`zipfs umount` **不** `systemctl stop`。对由 systemd 模板托管（`Restart=on-failure`）的实例，直接跑引擎与 systemd 的自动重挂可能竞态。命令输出显式提示：托管实例请优先 `systemctl --user stop zipfs@<esc>`；`zipfs umount --level abort` 仅作 systemd 也失效时的强制兜底。`umount-managed`（ExecStop 用）本就运行在 `systemctl stop` 生命周期内，直调引擎无此竞态。

### 4.4 systemd 模板 `fuse/src/enable/autostart.rs`

`ExecStop` 由 `{exe} umount-managed --name %i` 改为 `{exe} umount-managed --name %i --level auto`。既有单元升级：文档说明 `enable autostart install` 会重写模板（现存实例需 `systemctl --user daemon-reload` + 重装）。

## 5. 数据流

```
zipfs umount --name X --level auto
  → Paths::resolve → mountpoint(X)
  → force_umount::umount(mp, Auto)
       ├─ id = mount_connection_id(mp)                  # /proc/self/mountinfo，不碰挂载点
       ├─ clean: run_fusermount(["-u", mp], 3s 重试)     # 会 flush；仍挂则↓
       ├─ (still_mounted?) lazy: run_fusermount(["-u","-z", mp], 3s 重试)
       ├─ (still_mounted? && !endpoint_ok?) abort:       # 仅 daemon 确证死/卡才 abort
       │     abort_connection(id) → run_fusermount(["-u","-z", mp], 3s)
       │   (still_mounted && endpoint_ok) → 停在 lazy，不 abort（健康 busy，护在飞写）
       └─ report{ level_reached, aborted, unmounted }
```

systemd 路径 `umount-managed --name %i --level auto` 走同一引擎。

## 6. 错误处理

- 每个外部命令的失败/超时**不冒泡为致命**，而是驱动 `auto` 升级；`auto` 升级到 `abort` 前先过 `endpoint_ok` 守卫（见 §3.1）；仅当「daemon 死/卡 + abort 后仍 `still_mounted`」才返回 `io::Error`（daemon 存活但 busy 时停在 lazy，返回 Ok 或明确的「健康 busy 未强卸」错误由实现定，不 abort）。
- 显式单档（`clean`/`lazy`/`abort`）失败**如实返回错误**（不自动升级——用户显式选档即接受其语义）。
- `abort_connection` best-effort：连接已消失/无权限/已断开（`NotFound`/`PermissionDenied`/`NotConnected`）均视为非致命；调用方（`attempt` 的 abort 档）亦忽略其错误。
- `run_umount_managed` 对引擎错误 `.map_err` 附 `%i`/解码名上下文（与 `run_mount_managed` 对齐）。
- 实例名经 `model::validate_name` 校验（防 systemd 实例名穿越），沿用现有逻辑。

## 7. 测试策略（TDD）

**单元（`force_umount.rs` 内 `#[cfg(test)]`）**

- `mount_connection_id` 解析：给定合成 mountinfo 文本，正确取 `minor`；overmount 取末条；octal 转义路径；非 fuse 行返回 None。（解析逻辑抽成纯函数 `parse_connection_id(mountinfo: &str, target: &Path) -> Option<u64>` 以便无 IO 单测。）
- `UmountLevel` clap 解析与默认值 `auto`。
- `abort_connection` 幂等：指向不存在的连接 id → Ok。

**集成（`tests/umount_levels.rs`，参照 `tests/systemd_mount.rs` 真挂载）**

- 真起一个 zipfs 挂载 → `--level clean` 干净卸载成功、mountinfo 清零。
- 真挂载 → `--level lazy` 摘除、mountinfo 清零。
- **wedge 模拟**：挂载后杀 daemon（SIGKILL）留陈旧挂载。**显式 `--level abort`** → 摘除、mountinfo 清零、`report.aborted == true`；**`auto`** → 摘除、mountinfo 清零（daemon 死后 `lazy` 通常即摘除，故 `level_reached ∈ {Lazy, Abort}`，不锁定 abort——真实事故正是 `fusermount -uz` 生效）。**helper 需新写**（现有 `mount_rw.rs`/`systemd_mount.rs` 起挂载不带 pid-file）：起挂载时传 `--pid-file <tmp>`，从中读 daemon PID 供 SIGKILL；加 skip 门控（`/dev/fuse` + fusermount + `/sys/fs/fuse/connections` 可写，对齐 `systemd_mount.rs` 的 `skip_reason`）。
- `auto` 在健康挂载上停在 `clean`（`level_reached == Clean`，`aborted == false`）——验证正常关闭不误触 abort。

**探测硬化回归（`discovery.rs`）**

- `canonicalized_target` 在正常父目录下行为不变（现有测试保留）。
- 新增：canonicalize 超时分支返回未规范化路径（用 `with_timeout` 的既有测试范式，注入慢 canonicalize 或以短 timeout 驱动）。

**覆盖率**：目标 ≥80% 行覆盖（`cargo llvm-cov`）；解析与档位选择逻辑纯函数化以拉满。

## 8. 风险

- **abort 丢在飞写**：以档位隔离——仅 `abort`/升级到 `abort` 才触发；`auto` 默认耐久优先。文档 3.1 已显式。
- **连接 id 解析漂移**：不同内核 fuse 挂载 `major:minor` 约定若变，解析可能取错号；`abort` best-effort，取号失败退化为 `lazy`（仍能摘除，只是不解除 hung 读者）。
- **既有单元不自动升级**：模板改动需重装；文档与 `enable autostart install` 输出提示。

## 9. 交付边界与已知债务

**本次交付** — 新增：`hang_free.rs`（with_timeout/PROBE_TIMEOUT）+ `force_umount.rs` + `tests/umount_levels.rs`。改动：`discovery.rs`（连接号解析 + 硬化 endpoint_ok/canonicalized_target）、`main.rs`（`Umount` 子命令 + `umount-managed --level` + `%i` 错误上下文）、`autostart.rs`（ExecStop `--level auto`）、`mod.rs`（模块声明）。不动挂载/写入耐久性协议。

**并发合并点（已知）** — 另一会话在 `discovery.rs` 未提交 WIP 里独立硬化 `endpoint_ok`/`canonicalized_target` 并内联加了 `with_timeout`/`PROBE_TIMEOUT`。本分支把原语模块化到 `hang_free.rs` 并硬化同两函数；合入 main 时两函数体冲突、且可能出现两份 `with_timeout`（对方内联于 discovery、本方在 hang_free）。解冲突策略：保留 `hang_free.rs` 单份原语，删对方内联副本，两函数体取任一等价实现即可（契约一致）。这是并行重复硬化的固有冲突，非设计缺陷。

**后续债务（本次不收敛）** — `Mounter::unmount`（`daemon.rs`）及 `RealMounter`/`SystemdMounter` 实现仍走**可 hang 的旧路径**（`unmount_path` 的非-lazy `fusermount -u` + `.status()` 同步等待，无超时/无 abort），被 `lifecycle.rs` 的 restore/remount/compact/reingest/seal 共 5 处调用。本次只修好 `ExecStop`/用户 `umount`；后续应把这些回退分支收敛到 `force_umount::umount(mp, Clean)` 消除双路径漂移。
