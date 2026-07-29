# scrollz 自主改进 harness / Spec

> 状态：**草案 v2，待用户复核**。v1（9c5e1ab）经 gpt-souls:reviewer 对抗评审判定 needs-rework，本版按 Critical 5 / Important 11 / Minor 1 逐条处置后重写；处置结论见 §十四。
> 撰写日期 2026-07-29。本文回答「做什么、为什么」；「怎么做」由后续 [plan.md](./plan.md) 承载。

## 一、目标与问题陈述

让本项目在**无人值守**状态下持续自我推进：定时起一轮，agent 自己找到可行的下一步改进、在主分支留下提案文档、去独立 worktree 开发、提起 PR；用户只在 GitHub 上审阅并合并；合并后由后续轮次自动收尾（回收文档状态、更新 ROADMAP/CHANGELOG、清理分支与 worktree）。

要解决的五个真问题：

1. **选题质量**——无人值守下没人拦住 agent 挑伪需求、重复已完成项、或踩已冻结的设计红线。
2. **上下文耗尽**——长循环会话必然撑爆上下文，靠「写交接文档」只是缓解。
3. **中断韧性**——网络/进程中断随时可能发生，任何一轮都必须能被下一轮无损接续。
4. **状态可信**——模型的自我报告（「我推了分支」「测试绿了」）不能直接作为状态迁移依据。
5. **不失控**——失败要有预算和熔断，成本要有硬上限，队列不能无界膨胀，提案质量不能随时间静默退化。

## 二、已裁定决策（用户 2026-07-27/29 裁定，本 spec 视为前提，不再重开）

| 维度 | 裁定 | 备注 |
|---|---|---|
| 驱动方式 | **无人值守循环** | 非手动起轮 |
| 循环载体 | **headless 每轮一个新进程**（systemd user timer + `claude -p`） | 从根上消除上下文累积 |
| 编排载体 | **内置 `Workflow` 工具** 承载模型侧编排 | 用户明确要求先建 agent workflow |
| 控制面 | **确定性可信控制器**承载全部副作用与状态迁移（评审 C-04） | Workflow 降权为「只产出结构化结果」 |
| 提案队列真值源 | **GitHub**（Issue/PR/branch/commit 联合对账），label 只作索引 | 手机/网页可见完整队列 |
| 提案文档落点 | **新建 `docs/proposals/`** 轻量提案卡；中大型再升级 `docs/plan/<topic>.md` | 不冲淡现有 plan/ |
| 节拍 | **30 分钟一轮，最多 5 个在飞 PR** | 满 5 后自动降为「只扫描写提案」 |
| 选题边界 | 已登记待办（ROADMAP/TRACKING/BACKLOG） + 允许发现新问题 + 允许卫生型改进；**不碰冻结红线** | 红线项只允许开 Issue 提请用户裁决 |
| GitHub Actions CI | **要建**，先跑 PoC 探清 runner 能力边界；且 CI 门是**开闸前提**而非并行子项（评审 I-09） | 无人值守 PR 的可信凭据 |
| 分支策略 | **merge-based**：主分支用 merge 回收，不做 rebase 纪律 | 允许大量 merge 提交 |
| GitHub 写身份 | **专用 fine-grained PAT**（属 `puxu-msft`，只授本仓库 Contents/Issues/Pull requests: write + Metadata: read），存仓库外，systemd 以 `GH_TOKEN` 注入 | 现有 `gh` 账号 `puxu_microsoft` 对本仓库仅 READ，跑不通状态机（评审 C-01，已实测确认） |
| 生产数据隔离 | **不做 OS 级隔离**（用户 2026-07-29 裁定「无需隔离」） | 评审 C-03 的专用 Unix 用户 / 独立 HOME / mount namespace 降为 optional 硬化项保留，见 §十四 |

## 三、架构总览

### 3.1 三层结构

评审 C-04 指出的核心问题：Workflow 的「确定性」只保证**调用顺序**，不保证**副作用正确**，也不保证 agent 的自述属实；而 v1 把最关键的状态盘点与模式路由放在 Workflow 之外由模型自由完成，自相矛盾。v2 拆成三层，信任逐层递减：

| 层 | 实体 | 可信度 | 职责 |
|---|---|---|---|
| **控制面** | 可信控制器（`scripts/harness/` 下的确定性程序，非模型） | 可信 | 单例锁、启动预检、GitHub 查询与对账、模式路由、凭据持有、worktree 生命周期、diff 路径白名单校验、commit/push/开 PR、label 原子迁移、预算与熔断、记账 |
| **编排面** | `.claude/workflows/scrollz-round.js` | 半可信（顺序确定，内容不可信） | 固定 agent 调用顺序与 barrier，收集结构化结果 |
| **执行面** | 各 agent（finder / judge / implementer / reviewer） | **不可信** | 只产出结构化主张与工作区改动；**不持有 GitHub 凭据，不执行状态迁移** |

**铁律：任何状态迁移只能由控制器在重新查证事实之后执行。** agent 说「我推了分支」，控制器去 `git ls-remote` 查；agent 说「测试绿了」，控制器查退出码、测试数、skip 数与被测 SHA。

### 3.2 组件与位置

| 组件 | 位置 | 依赖 |
|---|---|---|
| 定时器与单例闸 | `~/.config/systemd/user/scrollz-harness.{timer,service}` | systemd user（项目已在用同款） |
| 可信控制器 | `scripts/harness/`（随仓库版本化） | `gh` + `git` + `GH_TOKEN` |
| 轮次入口 skill | `.claude/skills/scrollz-round/SKILL.md` | Claude CLI 2.1.220 |
| Workflow 脚本 | `.claude/workflows/scrollz-round.js` | 内置 `Workflow` 工具 |
| 红线清单 | `docs/harness/redlines.yaml` | 控制器确定性判定用 |
| 队列与状态 | GitHub Issues/PR/branches + `docs/proposals/` | 专用 PAT |
| harness 工作区 | 专用 clone `~/src/scrollz-harness/`（**不复用用户开发目录**） | 评审 C-05 |
| 开发隔离 | `git worktree` at `~/src/scrollz-harness-wt/<issue>-<slug>` | 每提案一 worktree 一分支 |

轮次日志落 `~/.local/state/scrollz-harness/rounds/<round_id>.jsonl`，不进版本库；它是排障数据，不是真值。

**关于专用 clone**：用户的开发目录 `/home/xp/src/zipfs` 随时可能有未提交改动与并行会话（现场已实测存在）。harness 在那里 pull/commit/清 worktree 会误带他人变更或删掉别人的 worktree。专用 clone 提交的仍是同一个 `main` 分支、推同一个远端——「在主树留文档」的语义不变，只是换了个物理工作区。

## 四、状态：label 是索引，事实才是真值

### 4.1 label（索引层）

| label | 含义 |
|---|---|
| `harness:proposed` | 已通过对抗裁决、进入队列，未开工 |
| `harness:picked` | 已选中，开发中 |
| `harness:in-pr` | PR 已开，等待用户合并 |
| `harness:blocked` | 卡住，需用户裁决或外部条件 |
| `harness:needs-decision` | 触及冻结红线或架构决策，agent 不得自行实施 |
| `harness:rejected` | PR 被关闭未合并 / 用户否决，终态 |

辅助 label：`harness`（来源标记）、`T0`–`T4`（沿用 ROADMAP 优先级）、`size:S/M/L`、`lane:*`（见 §十一）。

### 4.2 对账（事实层）

每轮开头**先对账再路由**，不得直接按 label 分支（评审 I-07）。事实来源：Issue 状态 + PR 状态与 mergeable + 远端分支存在性与 SHA + 本地 worktree marker + Issue 上的迁移收据评论。对账规则表：

| 观察到的事实组合 | 判定 | 动作 |
|---|---|---|
| `picked` 且远端无分支 | 上轮在首次 push 前死亡 | 回落 `proposed`，清理残留 worktree |
| `picked` 且远端有分支、无 PR | 上轮中断于开发中 | 进入接续模式，从 `last_checkpoint` SHA 继续 |
| `in-pr` 且 PR 已合并 | 待收尾 | 进入收尾模式 |
| `in-pr` 且 PR 已关闭未合并 | 用户否决 | 迁 `harness:rejected`，记录关闭原因入拒绝记忆（§十一） |
| `in-pr` 且 PR 的 base 已漂移 | 并行 PR 先合并导致基线变化 | 把 main merge 进 feature 分支、重跑受影响测试；冲突则迁 `blocked` |
| Issue 被用户手动关闭 | 用户干预 | 视为终态，不再选中；记录原因 |
| label 缺失或双状态 | 上轮迁移中途失败 | 按事实重推，迁移幂等 |
| 无 marker 的 worktree/分支 | 不属于 harness | **不动**（禁止清理非自己创建的对象） |

### 4.3 迁移收据与幂等

每次状态迁移由控制器写一条 Issue 评论（固定 `HARNESS-RECEIPT` 结构）：`round_id` / `attempt_id` / 旧状态 / 新状态 / 已验证事实（branch SHA、PR 号、测试 receipt 摘要）。收据是对账的辅助证据，也是崩溃后判断「迁移是否发生过」的依据。所有迁移以 `(issue, attempt_id, target_state)` 幂等——重复执行不产生第二次副作用。

worktree 与分支带 owner marker：分支名固定前缀 `harness/<issue>-<slug>`，worktree 内放 `.harness-owner`（含 issue、attempt_id、base SHA）。

## 五、一轮的流程

### Phase A · 控制器：预检 → 对账 → 路由

**启动硬预检**（任一失败即 fail closed，不起模型，不烧钱）：

1. `GH_TOKEN` 存在且对本仓库 `viewerPermission >= WRITE`；能读 Issue/PR。
2. `git ls-remote` 可达；push 凭据可用。
3. 专用 clone 干净（无未提交改动、无冲突中状态）。
4. 预算未耗尽、熔断未触发、无 `harness:paused` 哨兵 Issue。
5. `claude` 可执行、认证有效。

预检通过后对账（§4.2），再路由到唯一模式：

| 优先级 | 模式 | 触发条件 | 本轮做什么 |
|---|---|---|---|
| 1 | **事实收尾** | 有已合并未收尾 PR | 对**所有**已合并 PR 做轻量幂等事实收尾（清分支/worktree、关 Issue、写收据），再限量做一份文档整理 |
| 2 | **接续** | 有 `picked` 且远端有分支、无 PR | 从 `last_checkpoint` 继续做完并开 PR |
| 3 | **只扫描** | 在飞 PR ≥ 5 或 `proposed` 队列已满上限 | 只跑扫描+裁决与队列治理，不开发 |
| 4 | **正常开发轮** | 其余 | 全流程 |

模式 1 拆成「必须立即做的事实收尾」与「可批处理的文档整理」，避免连续多个 PR 合并时把 `picked` 的中断恢复无限压后（评审 I-08）。

### Phase B · Workflow 编排（正常开发轮）

```
扫描（4 个并行 finder，各自视角）
  ├ lens-1 已登记待办：ROADMAP T0–T4 的 ☐/◐ 行、TRACKING 待推进、BACKLOG 成熟项
  ├ lens-2 代码与测试空白：未覆盖路径、TODO/FIXME、已知语义缺口
  ├ lens-3 实测信号：bench/results 里的未闭环结论、性能回归
  └ lens-4 文档-代码漂移与卫生：doc 与实现不一致、陈旧描述、小型重构
        ↓ 结构化候选：意图 / 证据（含文件行号）/ 触碰面 / 规模 / 风险 / 验收 oracle / fingerprint
去重（脚本内纯 JS + 控制器提供的现存 Issue 指纹表，含已 rejected / superseded）
        ↓
对抗裁决（3 个并行 judge，任一否决即淘汰，否决理由持久化）
  ├ judge-1 伪需求与已完成
  ├ judge-2 红线守卫：只负责发现 redlines.yaml **未覆盖**的新语义风险（确定性部分由控制器做）
  └ judge-3 冲突与可验收：触碰面 vs 在飞 PR；验收 oracle 是否可证伪
        ↓
排序选一（lane 配额 + aging + T 优先级，见 §十一）
        ↓
实现（worktree 内 TDD；**第一个可编译提交即 push**）
        ↓
独立评审（未参与实现的 reviewer，对抗视角）→ 据 Critical/Important 返工
        ↓
返回结构化结果给控制器（不自行开 PR、不自行改 label）
```

规模约束：单轮 agent ≤ 12（工具建议 <15；机器 20 核，并发上限 min(16, 18)）。

### Phase C · 控制器：验证 → 副作用 → 记账

1. **先建 Issue 再写提案卡**（评审 M-17）：Issue 号即提案卡编号 `docs/proposals/<issue>-<slug>.md`，消除编号分配冲突与占位改名的崩溃窗口。
2. 校验实现产出：diff 路径不越白名单、不触 redlines、分支 SHA 与 agent 主张一致、测试 receipt 有效（见 §十）。
3. 执行副作用：提交提案卡到 main、push 分支、`gh pr create`（正文含 `Closes #N`、被测 SHA、命令与退出码、测试/skip 计数、触碰面、评审结论）。
4. 迁移 label、写收据、记轮次账（`round_id`/mode/issue/attempt/duration/turns/cost/工具拒绝次数/退出码/last_checkpoint）。

## 六、收尾模式（用户合并之后）

1. 专用 clone `git pull`（**merge，不 rebase**）。
2. 更新 [ROADMAP.md](../ROADMAP.md) 对应行、[CHANGELOG.md](../CHANGELOG.md) 追加、[TRACKING.md](../TRACKING.md) 摘除已完成 WIP。
3. 提案卡标记完成并移入 `docs/proposals/archive/`。
4. 校验 owner marker 后删除远端分支与本地 worktree（marker 不符则跳过并告警）。
5. 高风险 PR（触及并发/崩溃安全/磁盘格式）派一次**合并态评审**；发现问题**开新 Issue**，不当场改。
6. **经验只产出候选文档或 Issue**：无人值守收尾**禁止**修改 `.claude/skills/`、`.claude/workflows/`、permissions、hooks、systemd 单元、全局 memory——这些是可执行策略，等于让系统按自己的输出重写自身控制逻辑；任何此类改动必须走独立 PR 由用户合并（评审 I-15）。

收尾产生的文档改动由控制器直接提交推送 main（路径白名单限定 `docs/`）。

## 七、中断韧性与上下文

| 失效场景 | 机制 |
|---|---|
| 进程中途被杀 / 网络断 | 下轮 Phase A 从 GitHub 事实重建；`picked` 按 §4.2 对账表分流。**要求：实现 agent 第一个可编译提交就 push**，并把 SHA 写进 Issue 收据作为 `last_checkpoint` |
| 一轮内上下文将满 | Workflow 重活全在 subagent；仍不够则把进度写成 `HARNESS-HANDOVER` 评论后主动退出，交给接续模式 |
| 两个轮次撞车 | `flock` 全局单例；systemd oneshot `Restart=no` |
| Workflow 跨进程续跑 | 明确不依赖 `resumeFromRunId`（同会话限定），一律靠事实重建 |
| 一轮跑太久 | 双层超时必须显式对齐（评审 I-06）：CLI 后台等待上限（`CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS`，Round 0 实测确认默认值）必须 **大于** systemd `TimeoutStartSec`（建议 25 分钟），否则 CLI 先截断而 systemd 还没超时 |
| 迁移做了一半 | 迁移幂等 + 收据；对账按事实重推 |
| 用户手动干预 | 用户改 label/关 Issue/关 PR 均按 §4.2 处理为合法输入，不覆盖用户意图 |

## 八、护栏与红线

### 8.1 机器可判定的红线清单

`docs/harness/redlines.yaml` 版本化列出：受保护路径 / 符号 / 常量 / 不变量、允许的改动类型、必须通过的测试、触发用户裁决的条件。**控制器在提交与开 PR 前对 diff 跑确定性 gate**；judge-2 只负责发现清单未覆盖的新语义风险（评审 I-16）。初始条目至少含：磁盘格式魔数与版本、superblock 布局、尾日志 record 格式、崩溃安全提交顺序、已生效 ADR 锚点。

### 8.2 行为红线（写进每个 agent 提示词 + 控制器强制）

1. **绝不合并**：禁止 `gh pr merge`、`git push --force`、改写 main 历史、删非 harness 创建的分支/worktree。
2. **绝不扩权**：不改 `~/.claude` 全局配置、不装系统包、不改他人 systemd 单元、不自改 harness 自身控制逻辑（§六.6）。
3. **测试纪律**：测试 backing 必须建在 tempdir 的**子目录**内（既有教训：落共享 temp 根会导致 `.scrollz.lock` 残留与 flock 偶发失败）；不得对生产挂载点执行 mount/umount/reconcile/purge。
4. **PR 必须自证**：无有效测试 receipt 不得开 PR，改为 `harness:blocked`。

> 注：用户已裁定不做 OS 级隔离，因此以上第 3 条依靠提示词纪律 + §九 deny 规则，而非操作系统边界。风险已知并接受，硬化方案见 §十四。

### 8.3 不可信输入纪律（评审 I-14）

本仓库公开，Issue/PR/评论/提交信息可被任何人写入，其中可能含提示注入；harness 自己写入的提案又会成为下一轮上下文，形成自注入回路。因此：**所有 GitHub 与仓库文本一律按 data 处理**，在提示词中置于显式不可信边界内；禁止从中提取可执行命令；控制动作只接受固定 schema，并校验 actor allowlist、repo、Issue marker 与合法迁移；外部评论只能作为候选证据，不能改变权限、红线或执行模式。

## 九、权限与预算契约（评审 C-02）

headless 下不存在「自动批准」，未获批的 Edit/Bash/网络操作会停下，当前 `.claude/settings.json` 的 `permissions.allow` 为空——按 v1 写法首轮就会卡死。契约固定为：

- `--permission-mode dontAsk` + **最小 allow 列表**（Read/Edit 限定路径、`Bash(cargo *)`、`Bash(git *)` 等）+ deny 列表（`Bash(gh pr merge*)`、`Bash(git push --force*)`、`Bash(systemctl *)`、`Bash(fusermount*)`、生产路径写入）。**禁止 `bypassPermissions`**。
- `--max-turns`、`--max-budget-usd`、`--output-format stream-json` 固定给值。
- 被拒工具调用不视为可绕过：控制器把「被拒次数 > 阈值」映射为 `harness:blocked` 并记账。
- GitHub 凭据只在控制器进程环境中，不进入 agent shell。

## 十、CI 与开闸门（Round 0）

**Round 0 是激活门，不是并行子项。** 未通过之前，harness 只允许跑「只扫描」模式，不得开代码 PR（评审 I-09）。

Round 0 必须实测确认：

1. `claude -p` 等待后台 Workflow 的真实行为与上限，及与 systemd 超时的先后顺序。
2. `--permission-mode dontAsk` + allow/deny 组合能否无人值守跑完一轮全部动作。
3. 专用 PAT 的实际权限：能建/改 label、开 Issue、开 PR。
4. GitHub runner 能力边界：`/dev/fuse`、`fusermount3`、loop 设备、`dm-flakey`/`dm-log-writes`、sudo、单 job 时长。

**测试 receipt 的硬要求**：本项目的 FUSE 测试在缺 `/dev/fuse` 或 `fusermount` 时会打印 SKIP 后**成功返回**——「cargo test 绿」不证明挂载路径跑过。因此 receipt 必须含被测 head SHA、命令、退出码、测试数、**skip 数**、以及「真实 FUSE 路径确实执行」的正向证据；skip 数超阈值即判定证据不足。

CI 分层预期：L0 `fmt`/`clippy`/`build`（required check）；L1 不需 FUSE 的测试（required）；L2 需 `/dev/fuse` 的挂载测试（视 PoC）；L3 systemd/dm-* 留本地，由控制器校验 receipt。边界结论写入 `docs/harness/ci-boundary.md`。

## 十一、队列治理

- **lane 配额 + aging**（评审 I-12）：候选分 `roadmap` / `defect` / `perf` / `hygiene` 四 lane，各有最低选中配额，防止「按低风险小规模排序」把大项与高风险正确修复永久饿死；排队越久权重越高。
- **否决可复议**：judge 否决必须持久化理由与「重新考虑的条件」，不得静默消失。
- **队列上限**：`proposed` 超上限后停止产出新提案，只做去重与治理。
- **fingerprint 而非标题去重**（评审 I-13）：指纹由规范化目标 + 不变量 + 主要位置 + 验收 oracle 构成，与 open/closed/rejected/superseded 一并比对，识别「换个措辞的同一提案」。
- **拒绝记忆**：用户关闭 Issue / 关闭 PR 的原因入库，供后续 finder 与 judge 使用。
- **质量指标**：持续统计合并率、拒绝率、重复率、revert 率、首次评审通过率、proposal→PR 周期、各 lens 有效率；低于门槛自动降级为只扫描或暂停请求复核。

## 十二、可观测性、失败预算与熔断（评审 I-11）

- 每轮记账字段见 §五 Phase C。
- 预算三档：per-round、per-day、rolling-24h；耗尽即暂停。
- 熔断：同类错误连续 N 次、或日预算耗尽、或质量指标跌破门槛 → 自动切 `paused`。
- 告警：通过专用哨兵 Issue + systemd `OnFailure` 单元；stale `picked`/`in-pr` 超时告警。
- 人工开关：`harness:paused` 哨兵 Issue 存在即暂停；提供只读诊断命令。

## 十三、验收判据

分两类，均需可证伪（评审 I-10）。

### 13.1 确定性验收（fixture repo + fake/recording GitHub adapter）

用受控 fixture 仓库注入确定性候选，在**每个副作用边界**注入崩溃并重启，断言恢复正确、无重复 Issue、无孤儿对象：

| 崩溃点 | 期望 |
|---|---|
| 建 Issue 前 / 后 | 无孤儿提案卡 / 不重复建 Issue |
| 提案卡提交前 / 后 | 提案卡与 Issue 一致 |
| 首次 push 前 / 后 | 前：回落 `proposed`；后：接续模式续做 |
| 开 PR 前 / 后 | 前：接续；后：不重复开 PR |
| label 迁移中途 | 幂等重推，不出现双状态/零状态 |

另需断言：PR 被关闭未合并 → `rejected`；用户手改 label → 按 §4.2 合法处理；PAT 失效 / GitHub 429/5xx → fail closed 且不烧钱；在飞 PR 达 5 时**第 6 个提案不产生任何分支/worktree/PR**。

### 13.2 真实环境验收

1. 关闭终端、断开会话，30 分钟后 `gh issue list --label harness` 出现新提案，提案卡已在 main。
2. 人为 `kill -9` 一轮进程后，下轮自动接续同一提案完成，无重复 Issue、无孤儿 worktree。
3. 用户合并一个 PR 后，后续轮次完成收尾：ROADMAP/CHANGELOG 已更新、分支与 worktree 已清理、Issue 已关。
4. 故意在 ROADMAP 塞一条触及磁盘格式的诱饵项 → 产出 `harness:needs-decision` Issue 而非 PR，且控制器的确定性 gate 也能独立拦下。
5. 一轮跑完后比对 mount table、systemd units、非 harness 分支与工作树 hash，均无变化。
6. 真实 GitHub smoke test 至少一轮，避免 fake adapter 自洽假绿。

## 十四、评审处置与未采纳记录

| 评审条目 | 处置 |
|---|---|
| C-01 凭据只读 | **采纳**，已实测确认；改用专用 PAT（§二） |
| C-02 headless 权限模型 | **采纳**（§九） |
| C-03 OS 级隔离 | **降档保留**：用户裁定「无需隔离」。保留提示词纪律 + deny 规则；专用 Unix 用户 / 独立 HOME / mount namespace 记为 optional 硬化项，触发条件——出现一次真实的生产数据事故或误触 |
| C-04 可信控制器 | **采纳**（§三、§五 Phase A/C） |
| C-05 专用 clone | **采纳**（§三.2） |
| I-06 双层超时 | **采纳**（§七），默认值待 Round 0 实测 |
| I-07 事实对账 | **采纳**（§四.2/4.3） |
| I-08 收尾饥饿与基线漂移 | **采纳**（§五 Phase A 模式 1、§四.2） |
| I-09 测试证据不可信 | **采纳**，CI 门升为激活门（§十） |
| I-10 验收判据弱 | **采纳**（§十三） |
| I-11 失败预算与熔断 | **采纳**（§十二） |
| I-12 大项饥饿 | **采纳**（§十一） |
| I-13 提案质量闭环 | **采纳**（§十一） |
| I-14 不可信输入 | **采纳**（§八.3） |
| I-15 自修改通道 | **采纳**（§六.6） |
| I-16 红线机器可判定 | **采纳**（§八.1） |
| M-17 编号顺序矛盾 | **采纳**（§五 Phase C.1） |

## 十五、开放项（实施期确认，不阻塞本 spec）

- `claude -p` 后台等待的真实上限与退出码形态——Round 0 实测（§十）。
- 每轮 token/美元预算、失败重试次数、熔断阈值 N、队列上限、skip 数阈值的**具体数值**——先给保守硬上限，实测后只调优不新建（§十二）。
- 控制器的实现语言：随仓库的 shell 脚本 vs 小型 Rust bin（后者可复用 workspace 与类型化 GitHub 客户端）——由 plan 阶段定。
