# 实施计划的归档索引

本目录收纳本项目的实施计划（plan）与对应的 kick-off 提示词。它们多产于用户要求的 SDD、superpowers workflow 或 Claude Plan Mode，通常是针对某个特定问题的「怎么做」的实施方案。

用户决策：这些计划是快照还是活文档？当计划仍在进行中时，它们是活文档；当计划落地后，它们是快照，仅供追溯实施脉络，此时 `docs` 目录的活文档才是权威。

其中来自 Claude Plan Mode 的计划最初写在全局 `~/.claude/plans/<random>.md`。该目录无法在本项目内版本追踪，计划落地后需要主动归档、**移动**进本仓库。同时，把无语义的随机名重命名为语义化主题名。见下文索引。

## 索引

原随机名一列为补充信息，便于把旧会话、旧记忆里提到的随机名对应回本目录的文件。`—` 表示不可考。

| 归档文件 | 主题 | 原随机名 |
|---|---|---|
| [corruption-fix.md](./corruption-fix.md) | 数据损坏事故根因修复（TDD，A+B+C+D） | `cheeky-hatching-clock` |
| [corruption-fix-handoff.md](./corruption-fix-handoff.md) | 上者的会话交接文档（已完成，留档） | `cheeky-hatching-clock-handoff` |
| [concurrency-remediation.md](./concurrency-remediation.md) | 并发正确性整改（D1-D6） | `peppy-waddling-raven` |
| [enable-tool.md](./enable-tool.md) | `zipfs enable` 透明压缩启用器 TUI | `sequential-juggling-leaf` |
| [enable-probe-hardening.md](./enable-probe-hardening.md) | enable 探测层加固（熔断缓存 + 三态健康 + 探测编排反转） | `abundant-stargazing-mango` |
| [decompress-cache.md](./decompress-cache.md) | 压力感知解压块缓存 + release profile 调优 | `rosy-puzzling-karp` |
| [reversible-switch-prometheus.md](./reversible-switch-prometheus.md) | T4 可逆切换 + Prometheus 监控 + writeback | `polymorphic-wishing-shell` |
| [timestamp-1970-fix.md](./timestamp-1970-fix.md) | 挂载文件时间戳全为 1970 修复 | `imperative-floating-gem` |
| [workspace-restructure.md](./workspace-restructure.md) | 工程骨架现代化（PoC 转正遗留整改、Cargo workspace） | `shiny-crafting-thimble` |
| [session-reconcile.md](./session-reconcile.md) | 会话感知回落写重合并实施计划（分阶段 A-E） | — |
| [reconcile-undo.md](./reconcile-undo.md) | reconcile-undo + memory-symlink 短路（4 任务 TDD） | — |
| [hangfree-umount.md](./hangfree-umount.md) | hang-free 分档卸载 TDD 实施计划 | — |
| [fault-injection-kickoff.md](./fault-injection-kickoff.md) | 故障注入两层测试架构 kick-off 提示词 | — |
| [8.4b-tail-journal-kickoff.md](./8.4b-tail-journal-kickoff.md) | §8.4b 尾日志接线 kick-off 提示词 | — |
| [doc-layout-alignment.md](./doc-layout-alignment.md) | 文档布局对齐迁移方案（本会话产出，非 Plan Mode） | — |
