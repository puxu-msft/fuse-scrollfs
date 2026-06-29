# zipfs

自研 Rust FUSE **透明压缩文件系统**，用于在 WSL/Linux 上把目录以透明压缩方式存储（上层普通 POSIX 读写，底层自动压缩/解压），并**横向评测两种磁盘布局**以决定最终路线。

最终目标负载：承载 `~/.claude/projects` 等 Claude 记录大目录（实测 8.7GB，文本为主，单文件 zstd:3 可达 31x，**追加写为主**）。

## 两种磁盘布局

| 布局 | 形态 | 后端 |
|---|---|---|
| **V（容器 / 虚拟盘）** | 整棵树落进一个容器 | redb 全包（64KiB 块 + 批事务） |
| **S（影子树）** | 每文件一个分块压缩包，目录沿用底层 FS | ext4 上的镜像目录树 + footer 索引 archive |

两者共享同一「分块 + 压缩 + 索引」内核（`fuse/src/core/`），仅在 `Store` 接缝处不同。

## 目录结构

```
fuse/        Rust FUSE 实现（核心）
bench/       基准脚本、fio job、结果报告
microbench/  redb 容器后端选型 microbench
docs/        设计与对照文档
```

## 文档入口

- [docs/00-overview.md](docs/00-overview.md) —— 对照实验总纲（条件 / 指标 / 数据集 / 场景适配框架）
- [docs/01-zipfs-design.md](docs/01-zipfs-design.md) —— 实现设计；**§14 是实现与实测进展日志**
- [docs/02-layered-chunking.md](docs/02-layered-chunking.md) —— 分层分块 / head 缓存 / 发现读快路径设计
- [docs/03-target-data-scope.md](docs/03-target-data-scope.md) —— **目标数据范围**（首要 projects/jsonl+log、后续 file-history、排除 plugins/已压缩类）
- [docs/ROADMAP.md](docs/ROADMAP.md) —— **未来方向路线图**（T0–T4 优先级 + 决策门，单一信息源）
- [docs/environment-snapshot.md](docs/environment-snapshot.md) —— 实测环境

## 构建 / 测试

```bash
cd fuse
cargo build --release
cargo test --release          # 单元 + model-based 差分（两后端）+ 真实挂载集成测试
```

## 挂载（无需 root，需 /dev/fuse + fusermount3）

```bash
# 布局 S（影子树，读写）
fuse/target/release/zipfs --backend shadow    --backing <底层目录> --mountpoint <挂载点> --chunk-size 65536
# 布局 V（容器，读写）
fuse/target/release/zipfs --backend container --backing <redb文件> --mountpoint <挂载点> --chunk-size 65536
# 容器离线压实（回收 redb MVCC 旧页）
fuse/target/release/zipfs compact --backend container --backing <redb文件>
```

## 启用到 Claude projects（`zipfs enable`，TUI / 子命令）

把 `~/.claude/projects/*` 目录**可逆**切换到透明压缩挂载：`mv 备份 → ingest --verify → 挂载`，
失败回滚、零丢失；backing 内 `.zipfs.meta` 提交标记使半灌可检测、绝不当权威挂出。

```bash
fuse/target/release/zipfs enable                 # 交互式 TUI（列表/状态/切换/还原/重挂/选项/批量）
fuse/target/release/zipfs enable list            # 状态总览（PLAIN/ZIPFS/STOPPED/BROKEN + 压缩比）
fuse/target/release/zipfs enable apply  <name>   # 切换（活跃会话默认拦截，需 --force）
fuse/target/release/zipfs enable restore <name>  # 还原（backing 保留，可 `enable purge` 清理）
fuse/target/release/zipfs enable remount --all   # 守护崩溃/重启后重挂所有 STOPPED
fuse/target/release/zipfs enable autostart install   # systemd user 登录自挂载（WSL 用 `autostart print`）
```

apply 选项（全部持久化到 backing sidecar，remount 原样复用；TUI 内 `o` 调 backend/chunk/level/threads/writeback）：

```bash
zipfs enable apply <name> --backend shadow|container \
  --chunk 1048576 --level 19 --dict shared.dict --threads 4 --writeback \
  --max-write 4194304 --no-tail-buffer --allow-other --auto-unmount --metrics-file z.prom
```

- **两种后端可选**：`shadow`（默认；真实目录树，支持 symlink，append 友好）/ `container`（redb 单文件，便于搬运；不支持 symlink）。
- **持久化默认**：`zipfs enable config set level 19` / `config show` —— 免去每次重复敲选项。
- **维护**：`zipfs enable compact <name>`（回收空间，两后端）、`zipfs enable seal <name>`（仅 shadow，冷文件大块重压）—— 自动卸载→操作→重挂。
- 透明支持 Claude 的 `memory` 外部软链（shadow：ingest 照原样重建、运行时经 readlink 服务）；真正特殊文件（FIFO/socket/设备）会被拒绝并回滚，避免静默丢失。

> 路径可经 env `CLAUDE_PROJECTS` / `ZIPFS_HOME` 覆盖（默认 `~/.claude/projects`、`~/.claude-zip`）。
> 取代了早期 `bench/scripts/zipfs-{cutover,rollback,mount}.sh`（保留作手动/参考）。


## 基准

```bash
bash bench/scripts/run-suite.sh         # 默认 1 轮；ROUNDS=N 多轮取中位数
bash bench/scripts/measure-a-ratio.sh   # 补 btrfs(A) 压缩比（需 sudo compsize）
```

## 关键实测结论（详见 `bench/results/*/`）

- **BS 读路径修复后与内核 btrfs 同档**，且压缩比最高（真实子集 5.42x）。
- **BV 干净/追加写 3.84x**；compact 仅对「随机覆盖写的 redb 膨胀」有意义。
- **写尾延迟是 FUSE（用户态）对内核 btrfs 的结构性劣势**（ms 级 vs 亚毫秒）。
- **append 尾块缓冲**把追加重压降 40x、吞吐最高 +2.5x；**fsync 抗碎片**让块数/压缩比与 fsync 频率无关。

## 状态

设计 + P0 透传 + 布局 S/V 读写 + append 优化 + 五条件基准均已完成、测试全绿。遗留 TODO 与未来方向见 [docs/ROADMAP.md](docs/ROADMAP.md)。
