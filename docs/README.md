# zipfs 文档索引 / Docs Index

> 按**种类**分组；编号 `00–09` 只标阅读顺序，不代表同类。想找「决定了什么」看 [ADR.md](./ADR.md)、「下一步」看 [ROADMAP.md](./ROADMAP.md)、「建成了什么」看 [CHANGELOG.md](./CHANGELOG.md)。

## 运行文档（live，持续更新）

| 文档 | 回答 | 状态 |
|---|---|---|
| [ADR.md](./ADR.md) | 决定了什么 / 为什么 / 现在还算不算数（含被推翻项） | 运行中 |
| [ROADMAP.md](./ROADMAP.md) | 下一步做什么（T0–T4 优先级 + 开放决策门 G1–G3） | 运行中 |
| [CHANGELOG.md](./CHANGELOG.md) | 建成了什么、何时（实现/实测进展日志） | 运行中 |

## 总纲与范围（intent / what）

| 文档 | 内容 | 状态 |
|---|---|---|
| [00-overview.md](./00-overview.md) | 前期总纲：两路线对照的「比什么/怎么比/判据」 | 2026-06-27 冻结快照 |
| [03-target-data-scope.md](./03-target-data-scope.md) | 目标数据范围（Tier 1 jsonl / Tier 2 file-history / 排除项） | 生效范围决策 |
| [environment-snapshot.md](./environment-snapshot.md) | 实测环境事实（会随系统漂移，跑前 `probe-env.sh` 刷新） | 参考 |

## 实现设计（how）

| 文档 | 内容 | 状态 |
|---|---|---|
| [01-zipfs-design.md](./01-zipfs-design.md) | 核心实现设计（两布局 / Store 接缝 / 分块内核） | §1–13 为 2026-06-27 冻结快照 |
| [02-layered-chunking.md](./02-layered-chunking.md) | 分层分块 / head 缓存 / 发现读快路径 | 部分实现（head 缓存已落地） |

## 崩溃安全 / 测试 spec

| 文档 | 内容 | 状态 |
|---|---|---|
| [04-crash-safe-commit.md](./04-crash-safe-commit.md) | 崩溃安全提交协议（双 superblock + 尾日志） | 已实现 |
| [05-fault-injection-testing.md](./05-fault-injection-testing.md) | 故障注入测试规格（BlockIo/FaultIo + dm-* 层） | 已实现 |

## 特性设计（feature）

| 文档 | 内容 | 状态 |
|---|---|---|
| [07-hangfree-umount.md](./07-hangfree-umount.md) | Hang-free 分档卸载（clean/lazy/abort/auto） | 已实现 |
| [08-observability.md](./08-observability.md) | 可观测性 / Prometheus 指标出口 | 部分实现 |
| [09-session-reconcile.md](./09-session-reconcile.md) | 会话感知的停用期回落写重合并 | 已实现 |

## 计划 / 归档

| 位置 | 内容 |
|---|---|
| [plan/](./plan/) | 各特性的实施计划 / kickoff（含已完成的历史计划，原样保留） |
| [archive/](./archive/) | 归档快照：`06-defect-audit.md`（两轮缺陷审查台账）、reconcile 专项评审报告等，**冻结不改** |

> 注：编号缺 `06`——原缺陷审查台账已核验闭环、移入 `archive/06-defect-audit.md`。
