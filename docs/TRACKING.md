# scrollz 进行中工作跟踪 / TRACKING

> **本文回答「正在做什么」**——跨会话的进行中工作（WIP）指针。只放**指向权威源的链接 + 一句现状**，不复制/镜像状态，避免与 [ROADMAP.md](./ROADMAP.md) 双写漂移。
>
> 职责边界：长期方向与优先级看 [ROADMAP.md](./ROADMAP.md)（T0–T4）；推迟项看 [BACKLOG.md](./BACKLOG.md)；已完成看 [CHANGELOG.md](./CHANGELOG.md)。短期任务级进度（单会话内）仍由 Claude memory 承载，本文只登记**需跨会话交接**的活。

## 进行中（◐，链接回 ROADMAP 权威行）

| 工作 | 待推进（剩余动作，状态见权威源） | 权威源 |
|---|---|---|
| FUSE 写尾延迟优化 | 待 `fuser` 升级后接 writeback/passthrough | [ROADMAP.md](./ROADMAP.md) T2 |
| mmap 跨 fd 并发写陈旧页 | 待 `notify_inval` | [ROADMAP.md](./ROADMAP.md) T2 |
| algo/chunk 自适应 | lz4 codec 实现 + 自动选择 | [ROADMAP.md](./ROADMAP.md) T3 |
| 可观测性扩展 | 吞吐/比值监控 | [ROADMAP.md](./ROADMAP.md) T4、[08-observability.md](./08-observability.md) |
| enable 分层批量灌入 + 活跃会话长压测 | 分层批量策略 + 长压测 | [ROADMAP.md](./ROADMAP.md) T4 |

## 实现缺陷（来自 2026-07-13 gpt-souls 实现分析，触及已确认写入的数据，需修）

| 缺陷 | 现象与位置 | 修复方向 | 状态 |
|---|---|---|---|
| **D-seal-drops-journal**（高） | `seal_file` 只遍历 `0..chunk_count()`，不读尾日志；未满尾块若只存在于 journal，seal 后丢失尾部、`uncompressed_size` 被算小。对照 `compact_file` 已 `read_tail()` 折叠末块。`seal.rs:133-160` vs `compact.rs:95-110` | 与 compact 对齐：seal 流式读完普通块后把 `read_tail()` 明文加入重切缓冲；加「含 journal 的 archive 经 seal 后逐字节不变」回归测试 | ✅ 已修复（含幂等缺口：目标块已达但仍有尾日志 raw 时不再跳过、`max(cur,seal)` 防降级；2 回归测试） |
| **D-shadow-lost-session**（高） | `commit_session` 先 `sessions.remove(&ino)` 再 open updater/写块/barrier；任一步失败则已移除的 `WriteSession` 不放回，已返回应用的 RMW 写在内存层消失、无法重试。`shadow.rs:352-425` | 已升级为 active/flushing 双缓冲 + 全局 archive commit lock：IO 期间三层读可见，未确认提交失败按操作序合并且 active 新写/截断优先，已确认后 sync 错误只清 flushing；head cache 用 Keep/Set/Clear 显式操作；FaultIo 验证 barrier1 旧盘与 sync#3 新 durable 版本 | ✅ 已修复（含第二轮并发/字段组合审查整改） |
| **D-journal-corrupt-no-fallback**（中高） | Reader 级联校验只验 journal 物理范围，不验每条 record 可重放；`replay_journal` 遇错只返回最近完整前缀、不报损坏，较新 SB 即使指向 CRC 坏 journal 仍被选中 → 读到截短尾块被补零成错误数据。`reader.rs:151-227`、`journal.rs:22-45` | `replay_journal` 返回明文与干净消费字节数；`load_active` 只接受 `consumed == tail_journal_len` 的槽，坏新槽回落旧槽、两槽皆坏 fail-closed，且只校验 SB 引用范围以免误拒范围外 torn tail | ✅ 已修复（真实字节翻转、双槽损坏、范围外半写回归测试） |
| **D-journal-ghost-record-on-continuation**（高，审查 d5abb7d 时发现） | 崩溃后继续 append 复活未提交 journal record。`from_io` 设 `write_cursor=EOF` 但 `journal_offset/len` 恢复为已提交值；若 EOF 与已提交 journal 末端间夹着未提交后缀（ghost），`append_journal` 在 ghost 之后写新记录，`commit_journal` 的 SB 引用从 `journal_offset` 起的连续前缀会把 ghost 纳入、挤掉新记录（等长时 CRC 全合法、连 D-journal-corrupt 校验也通过）→ 读到 ghost 而 `uncompressed_size` 对应新记录，静默错误数据。`updater.rs:append_journal` | `append_journal` 加恢复后连续性守卫：`journal_offset` 存在且 `write_cursor != journal_offset+journal_len` 时，先把已提交 journal 原始字节（保 record 边界/CRC）复制到 EOF 建立新连续区间再追加，ghost 留作物理空洞由 compact 回收；崩溃中途幂等（旧槽仍引用原位完整字节）| ✅ 已修复（本会话独立复现 RED→GREEN：等长 ghost 读到 AAAAGGGG→修后 AAAAFFFF；含 ghost 长/短/半写/多次续写 5 回归测试；GPT implementer 未实际执行该修复，由主会话接手）|

## 开放决策门（待触发，详情在 ROADMAP / ADR）

- **G1 布局取向**（V / S / 并存）——T0 收尾评估补齐后定。
- **G2 自写数据区**——仅当 redb 真实规模不达标；默认不做。
- **G3 去重投入**——G1 选含 V 后再定；先做编码侧 `--long`。

详见 [ADR.md](./ADR.md) §2 与 [ROADMAP.md](./ROADMAP.md) 决策门汇总。
