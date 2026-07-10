# 实施计划归档索引 / Plan Archive Index

本目录收纳 zipfs 的实施计划（plan）与 kick-off 提示词。

这些计划多产于 Claude Plan Mode，最初写在全局 `~/.claude/plans/<random>.md`。该目录被 gitignore、**无版本追踪、只此一份**，归档进本仓库才受 git 保护。归档时把无语义的随机名重命名为语义化 topic 名。

本文件提供 **topic 名 ↔ 原随机名** 反查表，便于把旧会话、旧记忆里提到的随机名对应回仓库中的文件。

## 反查表 / Lookup Table

`—` 表示原随机名未留存（归档时未记录，且已从 `~/.claude/plans` 删除、转录与 git 历史均无踪），不可考。

| 归档文件 | 原随机名 | 主题 | 首次入库 |
|---|---|---|---|
| [corruption-fix.md](./corruption-fix.md) | `cheeky-hatching-clock` | 数据损坏事故根因修复（TDD，A+B+C+D） | `227f115` |
| [corruption-fix-handoff.md](./corruption-fix-handoff.md) | `cheeky-hatching-clock-handoff` | 上者的会话交接文档（已完成，留档） | `227f115` |
| [concurrency-remediation.md](./concurrency-remediation.md) | `peppy-waddling-raven` | 并发正确性整改（D1-D6） | `227f115` |
| [enable-tool.md](./enable-tool.md) | `sequential-juggling-leaf` | `zipfs enable` 透明压缩启用器 TUI | `227f115` |
| [enable-probe-hardening.md](./enable-probe-hardening.md) | `abundant-stargazing-mango` | enable 探测层加固（熔断缓存 + 三态健康 + 探测编排反转） | `227f115` |
| [decompress-cache.md](./decompress-cache.md) | `rosy-puzzling-karp` | 压力感知解压块缓存 + release profile 调优 | `227f115` |
| [reversible-switch-prometheus.md](./reversible-switch-prometheus.md) | `polymorphic-wishing-shell` | T4 可逆切换 + Prometheus 监控 + writeback | `227f115` |
| [timestamp-1970-fix.md](./timestamp-1970-fix.md) | `imperative-floating-gem` | 挂载文件时间戳全为 1970 修复 | `227f115` |
| [workspace-restructure.md](./workspace-restructure.md) | `shiny-crafting-thimble` | 工程骨架现代化（PoC 转正遗留整改、Cargo workspace） | `0835f09` |
| [session-reconcile.md](./session-reconcile.md) | — | 会话感知回落写重合并实施计划（分阶段 A-E） | `2b1b59e` |
| [reconcile-undo.md](./reconcile-undo.md) | — | reconcile-undo + memory-symlink 短路（4 任务 TDD） | `3718c29` |
| [hangfree-umount.md](./hangfree-umount.md) | — | hang-free 分档卸载 TDD 实施计划 | `0a1be98` |
| [fault-injection-kickoff.md](./fault-injection-kickoff.md) | — | 故障注入两层测试架构 kick-off 提示词 | `091e7d9` |
| [8.4b-tail-journal-kickoff.md](./8.4b-tail-journal-kickoff.md) | — | §8.4b 尾日志接线 kick-off 提示词 | `d248ad6` |

## 说明 / Notes

- **随机名溯源方式**：本次归档的 8 个 + `workspace-restructure`（`shiny-crafting-thimble`）在归档动作中经 `diff` 逐字节确认了源↔目标对应关系，映射可靠。早先 5 个（`session-reconcile` 起）的原随机名在 692M 会话转录与全量 git 历史中均无绝对路径记录，判定不可考。
- **计划 vs 活文档**：本目录是「怎么做」的实施计划快照。当前架构与设计的活文档见上级 [../ROADMAP.md](../ROADMAP.md)、[../01-zipfs-design.md](../01-zipfs-design.md) 等；计划落地后以活文档为准，本目录仅供追溯实施脉络。
- **新增计划归档时**：把 `~/.claude/plans/<random>.md` 复制进本目录并重命名为 topic 名，**在上表补一行登记原随机名**，再删源（先提交后删，勿丢历史）。
