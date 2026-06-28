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

设计 + P0 透传 + 布局 S/V 读写 + append 优化 + 五条件基准均已完成、测试全绿。遗留 TODO 见设计文档 §14.4。
