# 07 — Hang-free 分档卸载

> 设计文档。目标：把 zipfs 的卸载做成**不会被死/卡 daemon 拖住的分档升级梯**，用户按需选择档位，systemd 默认自动升级，正常关闭仍走会 flush 的耐久路径。

## 1. 背景与触发

一次真实事故：`zipfs@-home-xp-src-neighbors.service` 的守护被 `SIGTERM` 杀死，但 `ExecStop`（`zipfs umount-managed`）退出码 1 失败，留下一个**陈旧 FUSE 挂载**。任何进程（如 Claude 读该项目目录）访问该挂载点即在不可中断 I/O 上 hang。人工用 `fusermount -uz` 才摘除。

### 1.1 根因（经代码核对，纠正初判）

初判「`is_mounted()` 会 stat 挂载点导致 hang」**不成立**：`discovery::canonicalized_target` 只 `canonicalize(parent)` 后拼回叶子名，**不 stat 叶子**（有测试 `canonicalized_target_does_not_stat_leaf_segment` 佐证）。`is_mounted()` 纯读 `/proc/self/mountinfo` 做字符串比对，本身已 hang-free。

真实缺口收敛为三条：

1. **无 lazy、无 abort。** `daemon::unmount_path` 只尝试非-lazy 的 `fusermount3 -u` / `fusermount -u`；busy/broken 时返回失败（这次 status=1 的直接原因），既不 `-z` 懒卸载，也不先 abort FUSE 连接。
2. **外部子进程无超时兜底。** `unmount_path` 调用 `fusermount` 用 `.status()` 同步等待；当 daemon 带在飞请求 wedge 时，非-lazy `umount2` 可在内核侧阻塞，卸载命令自身即可能 hang。
3. **parent-canonicalize 的极端 hang。** `canonicalize(parent)` 仅在**祖先目录本身也是 wedge 挂载**的极端情形才阻塞；用超时包一层即可（belt-and-suspenders）。

## 2. 目标 / 非目标

**目标**

- 提供**用户可选的卸载档位**：从耐久（会 flush）到强制（与 daemon 存活彻底解耦）。
- 卸载引擎自身**永不 hang**：所有可能阻塞的步骤（外部 `fusermount`、`canonicalize`）均有超时上界。
- systemd `ExecStop` 默认走**自动升级**：正常关闭耐久，卡死场景自动升级到强制摘除，不再留陈旧挂载。
- 面向用户的手动兜底命令 `zipfs umount --name <inst> --level abort`。

**非目标**

- 不改挂载/写入路径的耐久性协议（尾日志、崩溃提交等另有文档 04）。
- 不引入新的 systemd 单元模板（决定：CLI 原语复用到现有 `ExecStop`，不新建 oneshot 单元）。
- 不改 `is_mounted()` 的语义，只做超时硬化。

## 3. 卸载档位（升级梯）

| 档位 | 动作 | 语义 / 代价 |
|---|---|---|
| `clean` | `fusermount -u`（超时上界内） | daemon 收 DESTROY → flush，**耐久**；busy/wedge 时失败或超时（不 hang） |
| `lazy` | `fusermount -uz` | 从命名空间摘除即使 busy；不 abort，无额外在飞丢失；已 hang 的读者不保证立即解除 |
| `abort` | 写 `/sys/fs/fuse/connections/<id>/abort` → `fusermount -uz` | 解除 hung 读者、与 daemon 存活解耦；**可能丢未 flush 的在飞写**，仅用于死/卡 daemon |
| `auto`（默认） | `clean` →(超时)→ `lazy` →(超时)→ `abort` | 耐久优先 + 保证摘除；正常 stop 走 `clean` flush，仅卡死才逐级升级 |

档位之间用 hang-free 的 mountinfo 复查 + 短超时（默认 `STEP_TIMEOUT = 3s`）判断是否需要升级。

### 3.1 耐久性权衡（显式）

`abort` 写连接 `abort` 会让内核错误化所有在飞请求：客户端未 ack 的写返回 EIO（符合语义——未 ack 即无保证）；daemon 自身缓冲但未 flush 的尾状态是否丢失，取决于 daemon 退出路径是否 flush（见文档 04 的尾日志/崩溃提交）。因此 `abort` **只应在无法干净卸载时**触发。`auto` 把这一取舍自动化：能 `clean` 就绝不 `abort`。

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

- `mount_connection_id(mountpoint) -> Option<u64>`：读 `/proc/self/mountinfo`，匹配挂载点（复用 `parse_mountinfo_line` 的 octal unescape + overmount 取末条逻辑），从第 3 字段 `major:minor` 取 `minor` 作 fuse 连接号。**不碰挂载点。**
- `abort_connection(id) -> io::Result<()>`：写 `/sys/fs/fuse/connections/<id>/abort`；文件不存在（连接已消失）视为成功（幂等）。
- `run_fusermount(args, timeout) -> Outcome`：spawn `fusermount3`/`fusermount`，用看门狗超时 kill 子进程后返回 `TimedOut`，避免外部命令拖住引擎。
- `still_mounted(mountpoint) -> bool`：复用 `discovery::is_mounted`（hang-free）。

### 4.2 探测硬化 `fuse/src/enable/discovery.rs`

`canonicalized_target` 的 `fs::canonicalize(parent)` 包进现有 `with_timeout(PROBE_TIMEOUT, ...)`；超时则回退未规范化父路径（宁可偶发漏判也不 hang）。`is_mounted` 语义不变。

### 4.3 CLI `fuse/src/main.rs`

- 新增顶层子命令 `Umount(UmountArgs)`：`zipfs umount --name <inst> [--level clean|lazy|abort|auto]`（默认 `auto`）。解析实例名 → `Paths::resolve` 算挂载点 → `force_umount::umount`。面向用户。
- `umount-managed`（`ExecStop` 用，内部）改为接受可选 `--level`（默认 `auto`），复用同一引擎；`run_umount_managed` 从 `RealMounter.unmount` 切到 `force_umount::umount(mp, level)`。
- `Mounter::unmount` trait 默认实现相应调整（或保留 `unmount` 走 `clean`，另加 `umount_leveled`）——实现期择一，保持 trait 现有测试（fake mounter）不破。

### 4.4 systemd 模板 `fuse/src/enable/autostart.rs`

`ExecStop` 由 `{exe} umount-managed --name %i` 改为 `{exe} umount-managed --name %i --level auto`。既有单元升级：文档说明 `enable autostart install` 会重写模板（现存实例需 `systemctl --user daemon-reload` + 重装）。

## 5. 数据流

```
zipfs umount --name X --level auto
  → Paths::resolve → mountpoint(X)
  → force_umount::umount(mp, Auto)
       ├─ id = mount_connection_id(mp)            # /proc/self/mountinfo
       ├─ clean: run_fusermount(["-u", mp], 3s)   # 会 flush；超时/失败则↓
       ├─ (still_mounted?) lazy: run_fusermount(["-u","-z", mp], 3s)
       ├─ (still_mounted?) abort: abort_connection(id) → run_fusermount(["-u","-z", mp], 3s)
       └─ report{ level_reached, aborted, unmounted }
```

systemd 路径 `umount-managed --name %i --level auto` 走同一引擎。

## 6. 错误处理

- 每个外部命令的失败/超时**不冒泡为致命**，而是驱动 `auto` 升级；仅当升级到 `abort` 后仍 `still_mounted` 才返回 `io::Error`。
- 显式单档（`clean`/`lazy`/`abort`）失败**如实返回错误**（不自动升级——用户显式选档即接受其语义）。
- `abort_connection` 对「连接已不存在」幂等成功。
- 实例名经 `model::validate_name` 校验（防 systemd 实例名穿越），沿用现有逻辑。

## 7. 测试策略（TDD）

**单元（`force_umount.rs` 内 `#[cfg(test)]`）**

- `mount_connection_id` 解析：给定合成 mountinfo 文本，正确取 `minor`；overmount 取末条；octal 转义路径；非 fuse 行返回 None。（解析逻辑抽成纯函数 `parse_connection_id(mountinfo: &str, target: &Path) -> Option<u64>` 以便无 IO 单测。）
- `UmountLevel` clap 解析与默认值 `auto`。
- `abort_connection` 幂等：指向不存在的连接 id → Ok。

**集成（`tests/umount_levels.rs`，参照 `tests/systemd_mount.rs` 真挂载）**

- 真起一个 zipfs 挂载 → `--level clean` 干净卸载成功、mountinfo 清零。
- 真挂载 → `--level lazy` 摘除、mountinfo 清零。
- **wedge 模拟**：挂载后杀 daemon（SIGKILL）留陈旧挂载 → `--level abort`（或 `auto`）在超时上界内摘除、mountinfo 清零、`report.aborted == true`。
- `auto` 在健康挂载上停在 `clean`（`level_reached == Clean`，`aborted == false`）——验证正常关闭不误触 abort。

**探测硬化回归（`discovery.rs`）**

- `canonicalized_target` 在正常父目录下行为不变（现有测试保留）。
- 新增：canonicalize 超时分支返回未规范化路径（用 `with_timeout` 的既有测试范式，注入慢 canonicalize 或以短 timeout 驱动）。

**覆盖率**：目标 ≥80% 行覆盖（`cargo llvm-cov`）；解析与档位选择逻辑纯函数化以拉满。

## 8. 风险

- **abort 丢在飞写**：以档位隔离——仅 `abort`/升级到 `abort` 才触发；`auto` 默认耐久优先。文档 3.1 已显式。
- **连接 id 解析漂移**：不同内核 fuse 挂载 `major:minor` 约定若变，解析可能取错号；`abort` best-effort，取号失败退化为 `lazy`（仍能摘除，只是不解除 hung 读者）。
- **既有单元不自动升级**：模板改动需重装；文档与 `enable autostart install` 输出提示。

## 9. 交付边界

新增：`force_umount.rs` + `tests/umount_levels.rs`。改动：`main.rs`（`Umount` 子命令 + `umount-managed --level`）、`autostart.rs`（ExecStop `--level auto`）、`discovery.rs`（canonicalize 超时）、`daemon.rs`（`Mounter` 接线）。不动挂载/写入耐久性协议。
