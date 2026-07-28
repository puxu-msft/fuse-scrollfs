# scrollz 自主改进 harness / Spec

> 状态：**草案，待用户复核**。撰写日期 2026-07-29。
> 本文回答「做什么、为什么」；「怎么做」由后续 [plan.md](./plan.md) 承载。

## 一、目标与问题陈述

让本项目在**无人值守**状态下持续自我推进：定时起一轮，agent 自己找到可行的下一步改进、在主树留下提案文档、去独立 worktree 开发、提起 PR；用户只在 GitHub 上审阅并合并；合并后由下一轮自动收尾（回收文档状态、更新 ROADMAP/CHANGELOG、清理分支与 worktree）。

要解决的三个真问题：

1. **选题质量**——无人值守下没人拦住 agent 挑伪需求、重复已完成项、或踩已冻结的设计红线。
2. **上下文耗尽**——长循环会话必然撑爆上下文，靠「写交接文档」只是缓解。
3. **中断韧性**——网络/进程中断随时可能发生，任何一轮都必须能被下一轮无损接续。

## 二、已裁定决策（用户 2026-07-27/29 裁定，本 spec 视为前提，不再重开）

| 维度 | 裁定 | 备注 |
|---|---|---|
| 驱动方式 | **无人值守循环** | 非手动起轮 |
| 循环载体 | **headless 每轮一个新进程**（systemd user timer + `claude -p`） | 从根上消除上下文累积 |
| 一轮的本体 | **内置 `Workflow` 工具**（确定性 JS 编排脚本） | 用户明确要求先建 agent workflow；控制流不交给模型每轮自由发挥 |
| 提案队列真值源 | **GitHub Issue + label 状态机**，Issue 正文链回主树文档 | 手机/网页可见完整队列 |
| 提案文档落点 | **新建 `docs/proposals/`** 轻量提案卡；中大型再升级 `docs/plan/<topic>.md` | 不冲淡现有 plan/ |
| 节拍 | **30 分钟一轮，最多 5 个在飞 PR** | 满 5 后自动降为「只扫描写提案」 |
| 选题边界 | 已登记待办（ROADMAP/TRACKING/BACKLOG） + 允许发现新问题 + 允许卫生型改进；**不碰冻结红线** | 红线项只允许开 Issue 提请用户裁决 |
| GitHub Actions CI | **要建**，先跑 PoC 探清 runner 能力边界再分层 | 无人值守 PR 的可信凭据 |
| 分支策略 | **merge-based**：本地主树用 merge 回收远端，不做 rebase 纪律 | 主树历史允许大量 merge 提交 |

## 三、架构总览

五个组件，各自边界清晰：

| 组件 | 位置 | 职责 | 依赖 |
|---|---|---|---|
| 定时器与单例闸 | `~/.config/systemd/user/scrollz-harness.{timer,service}` | 每 30 分钟起一个 oneshot 进程；`flock` 保证全局单例；`TimeoutStartSec` 兜底 | systemd user（已在用，`scrollz@.service` 同款） |
| 轮次入口 | `.claude/skills/scrollz-round/SKILL.md` | `claude -p "/scrollz-round"` 的落点：盘点状态 → 路由模式 → 调 Workflow → 记账退出 | Claude CLI 2.1.220 |
| 一轮的控制流 | `.claude/workflows/scrollz-round.js` | 确定性编排：扫描 → 去重 → 对抗裁决 → 选题 → 落文档 → worktree 实现 → 评审 → 开 PR | 内置 `Workflow` 工具 |
| 队列与状态 | GitHub Issues（label 状态机） + `docs/proposals/` | 跨进程唯一真值源 | `gh` 已认证（`repo` scope） |
| 开发隔离 | `git worktree` at `../zipfs-wt/<issue>-<slug>` | 每个在飞提案一个 worktree + 一条分支 | 主树不受污染 |

轮次日志（非真值，仅排障）落 `~/.local/state/scrollz-harness/rounds/<ts>.log`，不进版本库。

### 为什么这个形状

- **Workflow 而非「让模型自由发挥」**：无人值守最大的失效模式是流程漂移——某一轮模型跳过了对抗裁决直接开干。控制流写死在 JS 脚本里，模型只在每个节点内部自由。
- **headless 每轮新进程而非长会话**：上下文问题被根除而不是缓解；代价是 `Workflow` 的 `resumeFromRunId` 跨进程失效，所以续跑必须靠 GitHub 状态重建（见 §七）。
- **GitHub 为真值源而非本地文件**：本地文件会被并行进程和 worktree 切换搅乱；GitHub 的 Issue/label/PR 是唯一在所有进程外部、可原子读写、且用户可直接干预的状态存储。

## 四、GitHub 状态机

每个提案 = 一个 Issue，状态由 label 表达（单一状态 label + 若干标记 label）：

| label | 含义 | 迁移条件 |
|---|---|---|
| `harness:proposed` | 已通过对抗裁决、进入队列，未开工 | 扫描轮写入 |
| `harness:picked` | 本轮选中、正在 worktree 开发 | 选题后立即打，防并行轮重复选 |
| `harness:in-pr` | PR 已开，等待用户合并 | `gh pr create` 成功后 |
| `harness:blocked` | 卡住，需要用户裁决或外部条件 | 实现失败 / 触碰红线 / 超时 |
| `harness:needs-decision` | 触及冻结红线或架构决策，agent 不得自行实施 | 裁决阶段识别 |
| 关闭 | 完成 | PR 合并（`Closes #N` 自动关） |

辅助 label：`harness`（来源标记，全部 harness 产出都带）、`T0`–`T4`（沿用 ROADMAP 优先级）、`size:S/M/L`。

Issue 正文固定结构（便于机器解析）：意图 / 证据 / 验收判据 / **触碰文件面** / 风险 / 主树提案卡路径。其中**触碰文件面**是并行 PR 互斥的依据。

## 五、一轮的流程

### Phase A · 盘点与路由（主循环内，廉价，不进 Workflow）

用 `gh` 拉三份快照：开放 PR（含合并状态）、带 `harness` label 的 Issue、最近合并的 PR。据此路由到一个模式：

| 优先级 | 模式 | 触发条件 | 本轮做什么 |
|---|---|---|---|
| 1 | **收尾** | 存在已合并但未收尾的 PR | 见 §六 |
| 2 | **接续** | 存在 `harness:picked` 但无对应 PR 的 Issue（上一轮中断） | 从已推送分支接着做完并开 PR |
| 3 | **只扫描** | 在飞 PR ≥ 5 | 只跑扫描+裁决，产出新 `harness:proposed` Issue，不开发 |
| 4 | **正常开发轮** | 其余 | 全流程 |

一轮只处理一个模式、一个提案——保证轮次短、可中断、易归因。

### Phase B · Workflow 编排（正常开发轮）

```
扫描（4 个并行 finder，各自视角）
  ├ lens-1 已登记待办：ROADMAP T0–T4 的 ☐/◐ 行、TRACKING 待推进、BACKLOG 成熟项
  ├ lens-2 代码与测试空白：新引入的未覆盖路径、TODO/FIXME、已知语义缺口
  ├ lens-3 实测信号：bench/results 报告里的未闭环结论、性能回归
  └ lens-4 文档-代码漂移与卫生：doc 与实现不一致、陈旧描述、小型重构
        ↓ 各返回结构化候选（意图/证据/触碰面/规模/风险/验收判据）
去重（脚本内纯 JS：与现存 Issue 标题+触碰面比对，跨 finder 合并）
        ↓
对抗裁决（3 个并行 judge，任一否决即淘汰）
  ├ judge-1 伪需求与已完成：是不是已经做了？证据是否站得住？
  ├ judge-2 红线守卫：是否触及磁盘格式/魔数/冻结 ADR/生产数据 → 改判 needs-decision
  └ judge-3 冲突与可验收：触碰面是否与在飞 PR 重叠？验收判据是否可证伪？
        ↓
排序选一（T 优先级 → 风险低 → 规模小）
        ↓
落主树文档：写 docs/proposals/<NNN>-<slug>.md，直接提交并推送 main；开 Issue（打 harness:picked）
        ↓
worktree 实现：git worktree add ../zipfs-wt/<issue>-<slug>，TDD 实现，跑测试
        ↓
独立评审（reviewer agent，对抗视角，未参与实现）→ 据 Critical/Important 返工
        ↓
gh pr create（正文含 Closes #N、测试输出证据、触碰面、评审结论）→ Issue 改 harness:in-pr
```

规模约束：单轮 agent 数 ≤ 12（工具建议 <15；机器 20 核，并发上限 min(16, 18)）。

### Phase C · 记账退出

写轮次日志，更新 Issue label，进程退出。不做任何跨轮的内存状态。

## 六、收尾模式（用户合并之后）

检测到已合并 PR 时，本轮全部工作量给收尾：

1. 本地主树 `git pull`（**merge，不 rebase**），允许产生 merge 提交。
2. 更新 [ROADMAP.md](../ROADMAP.md) 对应行状态、[CHANGELOG.md](../CHANGELOG.md) 追加条目、[TRACKING.md](../TRACKING.md) 摘掉已完成的 WIP 行。
3. 提案卡标记完成并移入 `docs/proposals/archive/`。
4. 删除远端分支与本地 worktree（`git worktree remove` + `git branch -d`）。
5. 若该 PR 属高风险（触及并发/崩溃安全/磁盘格式），派一次**合并态评审** agent，发现问题则开新 Issue，而不是当场改。
6. 把可复用的经验写入项目 memory 或 skill。

收尾产生的文档改动直接提交推送 main（与提案卡同路径，不走 PR）。

## 七、中断韧性与上下文

这是本设计的核心约束，逐条给出机制：

| 失效场景 | 机制 |
|---|---|
| 进程中途被杀 / 网络断 | 下一轮 Phase A 从 GitHub 重建状态；`harness:picked` 无 PR ⇒ 走接续模式，从已推送分支继续。**要求：实现 agent 必须尽早推送分支**（第一个可编译提交就 push），而不是憋到最后 |
| 一轮内上下文将满 | Workflow 的重活全在 subagent，主循环只持有摘要；仍不够时把进度写成 Issue 评论（固定「HANDOVER」小节）后主动退出，交给接续模式 |
| 两个轮次撞车 | `flock` 全局单例；systemd oneshot `Restart=no` |
| Workflow 跨进程续跑 | 明确不依赖 `resumeFromRunId`（同会话限定）；一律靠 GitHub 状态重建 |
| 一轮跑太久 | `TimeoutStartSec`（建议 25 分钟，小于 30 分钟间隔）→ SIGTERM → 下一轮接续 |
| 主树被并行搅乱 | 只有主循环进程碰主树，且只写 `docs/`；所有代码改动在 worktree |

## 八、护栏（无人值守红线）

以下是**硬禁止**，写进 skill 与 workflow 的每个 agent 提示词，并由 judge-2 专项守卫：

1. **绝不碰生产数据**：`~/.claude/projects` 是 scrollz 的真实挂载目标。任何测试的 backing 必须建在 tempdir 的**子目录**内（已有教训：落共享 temp 根会导致 `.scrollz.lock` 残留与 flock 偶发失败）。禁止对生产挂载点执行 mount/umount/reconcile/purge。
2. **绝不改冻结契约**：磁盘格式魔数、superblock 布局、崩溃安全提交协议、已生效 ADR 决策。发现应改 ⇒ 开 `harness:needs-decision` Issue 交用户。
3. **绝不合并**：禁止 `gh pr merge`、禁止 `git push --force`、禁止改写 main 历史、禁止删非自己创建的分支。
4. **绝不扩权**：不改 `~/.claude` 下的全局配置、不装系统包、不改他人 systemd 单元。
5. **PR 必须自证**：正文必须含实际测试命令与输出摘要；未跑通不得开 PR，改为 `harness:blocked`。

## 九、GitHub Actions CI（并行子项，Round 0）

先做一次 PoC 探清 GitHub runner 的能力边界，再据实测分层：

- 探测项：`/dev/fuse` 可用性、`fusermount3`、loop 设备、`dm-flakey`/`dm-log-writes`、sudo 权限、单次 job 时长。
- 预期分层：L0 `fmt`/`clippy`/`build`（必绿）；L1 不需 FUSE 的单测与集成测试；L2 需 `/dev/fuse` 的挂载测试（视 PoC 结果）；L3 systemd/dm-* 类**留本地**，由实现 agent 跑完把输出贴进 PR。
- CI 结论写入 `docs/harness/ci-boundary.md`，作为后续 PR 证据要求的依据。

## 十、验收判据

harness 建成的标志（可证伪）：

1. 关闭终端、断开会话，30 分钟后 `gh issue list --label harness` 出现新提案，且提案卡已在 main 上。
2. 连续 3 轮内至少产生 1 个 PR，PR 正文含测试证据与 `Closes #N`。
3. 人为 `kill -9` 一轮进程后，下一轮自动进入接续模式并完成同一提案，无重复 Issue、无孤儿 worktree。
4. 用户合并一个 PR 后，下一轮自动完成收尾：ROADMAP/CHANGELOG 已更新、分支与 worktree 已清理、Issue 已关。
5. 故意在 ROADMAP 塞一条触及磁盘格式的诱饵项，harness 产出的是 `harness:needs-decision` Issue 而非 PR。
6. 在飞 PR 达 5 个时，后续轮次只产出提案不开 PR。

## 十一、未采纳方案

| 方案 | 未采纳理由 |
|---|---|
| 当前会话 `/loop` 驱动 | 上下文必然耗尽；关终端即停，不满足无人值守 |
| 只用主树文档做队列 | 并行进程与 worktree 切换会搅乱本地状态；且用户无法在网页/手机上看队列 |
| GitHub Projects 看板 | 需 `project` scope，当前 token 无，需重新授权，收益不抵成本 |
| 严格串行（有在飞 PR 就空转） | 用户选择允许 5 个并行 PR，空转浪费循环 |
| 依赖 `Workflow` 的 `resumeFromRunId` 续跑 | 仅同会话有效，headless 每轮新进程下失效 |
| 用工具内置的 `isolation:'worktree'` 做开发隔离 | 其临时 worktree 生命周期由工具管理，与「分支要推远端并长期存活到 PR 合并」冲突；改用显式 `git worktree add` |

## 十二、开放项（实施期确认，不阻塞本 spec）

- headless `claude -p` 进程在等待 Workflow 后台完成期间是否会提前退出——需 Round 0 实测；若会，则退化为「主循环内串行调用 subagent」的备用编排。
- 提案卡编号策略（递增 `NNN` 在并行下的分配）——建议直接用 Issue 号，避免分配冲突。
- 每轮 token 预算与失败重试次数的具体数值，待前几轮实测后定。
