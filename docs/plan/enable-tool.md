# 计划：`zipfs enable` —— Claude projects 透明压缩启用器（Rust + ratatui TUI）

## Context（为什么做这件事）

zipfs 已具备把目录可逆切换到透明压缩挂载的全部原语，但目前只散落为 3 个位置参数 bash 脚本（`bench/scripts/zipfs-{cutover,rollback,mount}.sh` + `zipfs.service`），没有发现 `~/.claude/projects/*`、没有状态总览、没有选项交互、没有活跃会话防护，也不可批量。ROADMAP **T4「迁移 `~/.claude/projects`（分层）…切换工具」** 正是此缺口。

本计划把切换/还原/重挂生命周期**内聚进 zipfs 二进制的 `enable` 子命令**（用户已选 Rust + ratatui），对外提供：
- 交互式 **ratatui TUI**（默认 `zipfs enable`）——列出每个 project 的状态、改选项、apply/restore/remount。
- 可脚本化子命令（`list/apply/restore/remount/status/autostart`）——便于测试、批处理、自挂载接线。

关键安全约束（用户已定：**默认拦截 + 显式放行**）：对正被 Claude 活跃写入的 project（如当前会话的 `-home-xp-src-zipfs`，60MB 实时 jsonl），apply 会把 FUSE 挂到活跃日志上 → 默认拒绝，需 `--force`（CLI）或键入 `APPLY`（TUI）才放行（no-unconscious）。

可逆切换的「mv 备份 → ingest --verify → mount」配方已被 crash-test 验证（cutover.sh），**按字节移植进 Rust，不重新发明**。

## 复用（已存在，不要重写）

- `zipfs::ingest::ingest_tree(src, dst, chunk_size, level, verify=true) -> io::Result<IngestStats>`（[fuse/src/ingest.rs](fuse/src/ingest.rs)）—— 流式灌入 + 逐字节校验，apply 的核心。`IngestStats{ files, bytes_src, bytes_archive, verified, errors, ratio() }`。
- **挂载守护**：不重构 `run_mount`，而是 `Command::new(current_exe())` 以现有 mount flag 重入自身（`--backend shadow --backing --mountpoint --chunk-size --level --pid-file`），detached 后台化 —— 守护即被验证过的 mount server（notifier/sd-notify/metrics 全保留），与 cutover.sh 的 `zipfs … &` 等价。
- **卸载**：`fusermount3 -u`（回退 `fusermount -u`），镜像 [bench/scripts/zipfs-rollback.sh](bench/scripts/zipfs-rollback.sh)。
- 配方逻辑参照 [zipfs-cutover.sh](bench/scripts/zipfs-cutover.sh) / [zipfs-rollback.sh](bench/scripts/zipfs-rollback.sh) / [zipfs-mount.sh](bench/scripts/zipfs-mount.sh)；systemd 模板参照 [zipfs.service](bench/scripts/zipfs.service)。

## 模块布局（新增 `fuse/src/enable/`，按域拆小文件）

| 文件 | 职责 |
|---|---|
| `enable/mod.rs` | 入口 `run(action: Option<EnableAction>)`；`None` → TUI，否则派发 CLI 子动作。re-export。 |
| `enable/model.rs` | `Paths{projects_root, zipfs_home}`（env `CLAUDE_PROJECTS`/`ZIPFS_HOME` 覆盖，默认 `~/.claude/projects`、`~/.claude-zip`）；`ProjectStatus{Plain,Active,Stopped,Broken}`；`ProjectInfo`；`Activity{Idle,Active(String)}`；`ApplyOptions{chunk_size,level}`；纯函数 `classify(orig_exists, mounted, endpoint_ok) -> ProjectStatus`（可单测）。 |
| `enable/discovery.rs` | 扫 `projects_root` → `Vec<ProjectInfo>`；`is_mounted(&Path)` 解析 `/proc/self/mountinfo`（挂载点匹配 + fstype=fuse/subtype 含 zipfs）；`detect_activity(&Path)`；sidecar 读写（手搓 `key=value`，**不引 serde**）。 |
| `enable/daemon.rs` | `Mounter` trait（`spawn/unmount/is_mounted`）；`RealMounter`：`current_exe()` + `pre_exec(setsid)` + stdio→/dev/null detached，轮询挂载点就绪；`unmount` 调 fusermount3。 |
| `enable/lifecycle.rs` | `apply/restore/remount/remount_all/purge_backing`，签名收 `&dyn Mounter`（可注入 FakeMounter 测试）。 |
| `enable/autostart.rs` | 生成 systemd user 模板 `zipfs-project@.service`；`install`（`systemctl --user enable …`，状态变更需确认）；`print` 输出 wsl.conf `[boot]` 片段（root 文件只打印不自动改）。 |
| `enable/tui.rs` | ratatui app：project 列表 + 状态色块 + 选项弹窗 + 确认弹窗（活跃项需键入 `APPLY`）；ingest 在 worker 线程跑、mpsc 回传进度避免冻结 UI。 |

改动现有文件：
- [fuse/src/lib.rs](fuse/src/lib.rs)：加 `pub mod enable;`
- [fuse/src/main.rs](fuse/src/main.rs)：`Command` enum 加 `Enable(EnableArgs)`，`EnableArgs{ #[command(subcommand)] action: Option<EnableAction> }`，`main` 派发 `zipfs::enable::run(args.action)`；**抽出 `pub(crate)` mount-spec/argv builder**（现 `run_mount` 485–519 的 flags→argv 映射），供 re-exec 子进程与 `run_mount` 共用、防漂移（评审 M1）；**pid-file 写改原子 tmp+rename**（现 `main.rs:537` 裸 `fs::write` 可被读到空，评审 H3）。
- [fuse/Cargo.toml](fuse/Cargo.toml)：已加 `ratatui=0.30`、`crossterm=0.29`（build 已验证，crossterm 后端、无 termwiz/serde）。

### 评审采纳清单（architect review，已并入设计）
- **daemon 完全 detached（C3/H1）**：re-exec `current_exe()`，`pre_exec` 内只 `setsid()` + `dup2` stdio→/dev/null + 关闭继承 fd（防 TUI 终端 fd 泄漏给长寿守护）；**不保留 `Child` 句柄**（避免「半 detach + 僵尸」），pid 取自守护原子写的 pid-file。
- **就绪轮询（H2）**：每轮 `is_mounted(P) && endpoint_ok(P)`（`statfs` 非 ENOTCONN）且 daemon pid 存活；早死即止，不空等满 5s。
- **kill 前校验（H3）**：`SIGTERM` 前读 `/proc/<pid>/comm`/cmdline 确认是 zipfs 进程，绝不盲 kill 文件里的裸整数（no-unconscious）。
- **is_mounted 解析（H5）**：解析 `/proc/self/mountinfo` 时**反转义八进制**（`\040\011\012\134`）→ 与 `canonical(P)` **精确匹配**（非前缀，防 `foo` 误配 `foobar`）→ 同挂载点多行取最后（overmount）。
- **systemd 实例名（M5）**：`zipfs-project@.service` 的 `%i` 用 `systemd-escape` 处理 path-encoded 名；含 `:`/空格的名要测。

## 可逆配方（移植自 cutover/rollback；架构评审 C1/C2 强化为「commit-marker durability」）

**核心强化（评审 C1/C2，胜过 bash 原版）**：sidecar `backing/.zipfs.meta` 是**唯一可信的提交标记**——只在所有 fsync 完成后写 `committed=1` 并 fsync 自身 + 父目录。半途崩溃 → sidecar 缺失/无 committed → `classify` 判为 `Broken(half-ingest)`，**绝不自动挂载半灌的 backing 当权威数据**。

**apply(name, opts, force, mounter)**
1. 解析 `P`、`orig=P+".zipfs-orig"`、`backing=zipfs_home/back/name`。
2. 前置校验：`orig` 不存在；`P` 非挂载点；`backing` 不存在或空（**非空拒绝**，no-unconscious）；删除任何 stale `P.zipfs.pid`。
3. 活跃防护：`detect_activity(P)==Active && !force` → `Err`。
4. `rename(P→orig)` → **fsync `P` 父目录**（rename 持久化）→ `create_dir_all(backing)` → `create_dir(P)`。
5. `ingest_tree(orig, backing, chunk, level, verify=true)`；**任何错 → 回滚 `rename(orig→P)` + 删 `backing` + Err**。
6. **fsync backing 树**（各 archive 文件已 `sync_all`，但目录 dirent 未 sync）。
7. **写 sidecar 并 fsync + fsync 父目录**，含 `committed=1`（提交点）。此前崩溃皆 fail-closed 可恢复。
8. `mounter.spawn(MountSpec)`：**完全 detached**（不留 `Child` 句柄，pid 取自 daemon 原子写的 pid-file）；轮询 ≤5s，每轮同时校验 `is_mounted(P) && endpoint_ok(P)` 且 daemon pid 存活；超时 → 校验 pid 属 zipfs 进程后 `SIGTERM` + Err（`orig` 仍在，零丢失）。

**restore(name, mounter)**：要求 `orig` 存在 → `unmount(P)` 轮询完成 → `remove_dir(P)`（空，否则报「仍挂载」）→ `rename(orig→P)` → **fsync 父目录** → 删 pid。幂等：崩在 remove_dir 与 rename 间（`P` 缺、`orig` 在）→ `classify` 判可续 restore，重跑安全。`backing` 保留（`purge_backing` 另作动作）。

**remount(name, mounter)**：已挂 → 跳过；stale endpoint（`endpoint_ok` false）→ 先 unmount；读 sidecar 选项（须 `committed=1`）→ `spawn`。`remount_all` 仅遍历 `Stopped`（committed 且 daemon 死），**跳过 `Broken(half-ingest)`**（需人工 re-ingest 或 rollback）。

## classify 四态（评审 C2：加 `backing_committed` 输入）

纯函数 `classify(orig_exists, mounted, endpoint_ok, backing_committed) -> ProjectStatus`：
- `Plain`：!orig && !mounted（未管理）。
- `Active`：mounted && endpoint_ok && orig（正常压缩挂载中）。
- `Stopped`：orig && !mounted && backing_committed（守护死、可安全 remount）。
- `Broken`：orig && (stale endpoint **或** !backing_committed)（需人工：续 restore / re-ingest）。

## 活跃会话检测（零依赖；评审 H4）

`detect_activity(P)`：
1. **无条件硬拦当前会话自身项目**：由 `CLAUDE_PROJECTS` + 本进程 cwd 反推当前 session 的 project 目录，命中即 `Active`（独立于 mtime，绝不误放）。
2. 扫 `/proc/[0-9]*/fd/*` 与 `/cwd` readlink，命中 `canonical(P)` 前缀 → `Active("pid N 持有 fd")`（他人 proc EACCES 静默跳过）。
3. 辅以「P 下任一 `*.jsonl`/`*.log` mtime 在 60s 内」→ `Active("近期写入")`。
**TOCTOU 收窄**：rename(步骤4) 前**紧贴再查一次** activity。`--force` 放行时打印**醒目警告并指明持有进程**（L4），防反射式覆盖自己的活跃会话。

## CLI 子动作

```
zipfs enable                          # 无动作 → TUI
zipfs enable list                     # 状态表：name 状态色 ratio(at ingest) 大小
zipfs enable apply  <name> [--chunk N] [--level L] [--force]
zipfs enable restore <name>
zipfs enable remount [<name> | --all]
zipfs enable status  <name>
zipfs enable purge   <name>           # 删 backing（仅在已 restore 后；二次确认）
zipfs enable autostart install [--all] | print
```
注：apply 仅 **shadow** 布局（ingest 只产 shadow archive 树）；container 不在 apply 范围（README 亦荐 shadow 承载 projects）。

## 测试（test-driven；FUSE 不可单测，故测纯逻辑 + 注入 FakeMounter）

先写测试，再实现：
- `model.rs`：`classify()` 四态真值表全覆盖。
- `discovery.rs`：构造 tmp 假树断言状态分类；`detect_activity` —— 打开一个文件断言 `Active`、关闭且 mtime 旧 → `Idle`；sidecar round-trip。
- `lifecycle.rs`（核心）：注入 `FakeMounter`（marker 文件模拟挂载）+ **真实 `ingest_tree`** 于小 tmp 树：
  - apply 成功 → `orig` 备份在、archive 已灌、sidecar 写入、FakeMounter 标记挂载。
  - apply 中 ingest 失败（喂只读/坏 backing）→ `orig` 还原、`backing` 清除（验回滚）。
  - restore → `orig` 复位、挂载标记清除。
  - 这条**完整覆盖可逆配方**，无需 /dev/fuse。
- `tests/enable.rs`（集成）：`zipfs enable --help`/`list` 冒烟（指向 tmp `CLAUDE_PROJECTS`/`ZIPFS_HOME`）。
- 真实端到端（手动 / 文档化）：对某小 project 的丢弃副本 apply → 透明读 == 原文 → restore；TUI 手测。
- 目标 80%+ 行覆盖于 `enable/`（mod-logic，排除 TUI 渲染与 RealMounter 的 FUSE 段）。

## Verification

```bash
cd fuse
cargo fmt && cargo clippy -- -D warnings
cargo test enable            # 新增单元 + 集成
cargo test --release         # 全量回归（确认未破坏既有两后端差分/挂载测试）
cargo build --release

# 脚本化烟测（隔离 env，不碰真 ~/.claude）
export CLAUDE_PROJECTS=$(mktemp -d)/projects ZIPFS_HOME=$(mktemp -d)/zip
mkdir -p "$CLAUDE_PROJECTS/demo"; printf 'a\nb\n' > "$CLAUDE_PROJECTS/demo/x.jsonl"
target/release/zipfs enable list
target/release/zipfs enable apply demo --chunk 1048576     # 应 ingest+verify+挂载
target/release/zipfs enable status demo                    # 显示 ZIPFS + ratio
cat "$CLAUDE_PROJECTS/demo/x.jsonl"                          # 透明读 == 原文
target/release/zipfs enable restore demo                   # 还原，目录复位

# TUI 手测
target/release/zipfs enable        # ↑↓ 选择，o 改选项，a/r/m 动作，活跃项需键入 APPLY，q 退出
```

## 交付物清单
- 新增 `fuse/src/enable/{mod,model,discovery,daemon,lifecycle,autostart,tui}.rs` + 测试。
- 改 `fuse/src/{lib.rs,main.rs}`、`fuse/Cargo.toml`。
- 新增 `fuse/tests/enable.rs`。
- 文档：README 挂载节加 `zipfs enable` 用法；ROADMAP T4「切换工具」标 ☑/◐ 并指向新子命令；保留旧 bash 脚本并注明「已被 `zipfs enable` 取代，留作手动/参考」。

## 不做（YAGNI / 范围外）
- container 布局 apply（无 ingest 路径）。
- 自动改 `/etc/wsl.conf`（root 文件，仅打印片段）。
- 实时 du 重算压缩比（list 显示 ingest 时记录值，标注 "at ingest"；实时重算作显式动作可后续加）。
