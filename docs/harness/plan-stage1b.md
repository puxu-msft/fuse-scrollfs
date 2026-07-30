# scrollz 自主改进 harness · Stage 1b 实施计划（治理与可观测）

> 状态：**范围已冻结，实施待 1a 稳定运行后开始**。用户 2026-07-30 裁定把 Stage 1 拆为 1a/1b。
> 前置：[plan-stage1a.md](./plan-stage1a.md) 全部任务完成并已真实运行若干轮。
> 规格来源：[spec.md](./spec.md) §12.1、§13.1、§十、§6.1。

## 为什么这些不能被静默省略

1a 用「2 小时低频 + 每轮预算硬上限 + 预检 fail closed」兜住了**失控**，但兜不住**退化**：队列会在本地 DB 与远端漂移后错误放行或永久阻塞、被用户否决的提案会被反复重提、提案质量下滑无人察觉、连续失败只会安静地每 2 小时烧一次钱。**1b 未完成前，节拍不得提到 30 分钟**——这是 1a 与 1b 之间唯一的硬约束。

## 任务清单

### B1 · 远端队列对账

每轮开头把 GitHub 的真实队列拉回本地：带 `harness` label 的开放 Issue、最近关闭的 Issue（含关闭者与关闭原因）、被用户手动改过 label 的 Issue。

- 本地 `proposals` 表按远端事实更新 `state`；本地有而远端已关 → `closed-by-user`，写入拒绝记忆。
- 本地有而远端**根本不存在**（人工删除）→ `superseded`，释放 lane 配额。
- lane 上限改为按**远端事实**计算，不再只信本地 DB。
- 验收：本地 DB 人为写脏（多一条 / 少一条 / 状态不符）后跑一轮，三种漂移都能被纠正。

### B2 · 拒绝记忆与 possible_duplicate 回路

- `Queue.classify` 真正返回 `possible_duplicate`（当前实现从不返回，属自审假覆盖）：精确指纹命中 → `exact_duplicate`；规范化目标高度相似但指纹不同 → `possible_duplicate`，交 judge 复核后再定。
- 用户关闭 Issue / 关闭 PR 的原因文本入库，作为 finder 与 judge 的负反馈上下文。
- `known_keys` 从本地 DB + 远端对账结果联合产出，传给 Workflow（1a 传空数组）。
- 统一指纹协议：JS 侧 `canonicalKey` 与 Python 侧 `queue.fingerprint` 必须对同一候选产出一致的规范化串，加一条跨语言一致性测试（Python 生成样本 → Node 计算 → 比对）。

### B3 · 机器红线 gate

- 控制器解析 `docs/harness/redlines.yaml`（受控子集格式或改 JSON，避免手搓通用 YAML parser）。
- 对候选的 `touched_paths` 做确定性判定：命中 `requires_decision` / `deny_change` → 强制 `harness:needs-decision`，不依赖 model judge 的自觉。
- 表驱动测试：每种 oracle 类型各一组命中/未命中样本。
- 边界要写明：gate 的结论只能是「规则命中/未命中」，**不得声称自然语言不变量已被证明**。

### B4 · 质量指标与阈值

- 新增 `round_candidates` 与 `verdicts` 表，持久化 finder 产出、judge 裁决与候选来源——当前 `rounds`/`proposals` 两张表算不出各 lens 采纳率。
- Stage 1 指标：提案被用户保留/关闭比例、重复率、`needs-decision` 率、各 lens 采纳率。
- **未定义值不得按 0 处理**（样本不足时指标为 `None`，不参与熔断判定）。

### B5 · 熔断、告警与 paused 哨兵

- 同类错误连续 N 次 → 自动创建 `harness:paused` 哨兵 Issue 并停轮。
- rolling-24h 预算（1a 只有自然日预算）。
- systemd `OnFailure=` 指向一个告警单元；预检失败必须列明缺失项后告警，而非静默重试。
- stale `proposed`（超期未被处理）告警。

### B6 · 真实 API 契约 smoke

- 针对 `GhCli` 的真实 wire shape 冒烟：建 Issue / 写评论 / 读 label / 分页 / 404 / 422（label 不存在），跑完清理。
- 覆盖 `FakeGitHub` 表达不了的：search 索引延迟、分页、并发修改。
- 目的不是替代 Fake，而是防止 Fake 与真实 adapter 漂移后「Fake 契约测试全绿、生产全错」。

### B7 · binding_ok 的真实计算

- 发布收据记录 operation ID、proposal path、proposal blob SHA、远端 commit SHA 四项。
- `collect_facts` 独立查询远端并逐项比对，`binding_ok` 由比对结果得出（1a 恒为 `True`）。
- 测试：过早收据、错误 path、blob 被后续提交覆盖、错误 operation marker 四种伪造场景均判 `inconsistent`。

### B8 · 提节拍

以上全部通过后，把 `OnUnitActiveSec` 从 `2h` 改为 `30min`，并观察一天的指标与成本。

## 验收

1. 本地 DB 三种漂移（多、少、状态不符）都能被 B1 纠正。
2. 用户关闭一个 Issue 后，同一提案不再被重提，除非 `reconsider_when` 谓词成立。
3. 塞一个触及 `redlines.yaml` 路径的候选，控制器 gate 独立拦下（不依赖 judge）。
4. 人为制造连续 N 次同类失败，harness 自动 paused 并留下哨兵 Issue。
5. 跨语言指纹一致性测试通过。
6. 提节拍后连续运行 24 小时，成本与指标在阈值内。
