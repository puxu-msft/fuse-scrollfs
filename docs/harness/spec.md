# scrollz 自主改进 harness / Spec

> 状态：**v7，Stage 1a 计划已按评审整改**。v1（9c5e1ab）/ v2（6831bda）/ v3（94048c3）/ v4（d0cda3a）/ v5（36f1a8b）经 gpt-souls:reviewer 四轮对抗评审；v6 按用户 2026-07-30 的仓库内布局偏好与 Web UI 规划调整配置。处置台账见 §十五。
> 撰写日期 2026-07-29，末次修订 2026-07-30。本文回答「做什么、为什么」；「怎么做」由后续 [plan.md](./plan.md) 承载。

## 零、交付分期（用户 2026-07-29 裁定）

| 阶段 | 范围 | 副作用面 | 需要的协议强度 |
|---|---|---|---|
| **Stage 1a · 发布回路** | 发现候选 → 对抗裁决 → 建 Issue + 提案卡发布到远端 main。**不开发、不开 PR、不建分支、不建 worktree**。节拍 **2 小时** | 建 Issue、设 label、提交 `docs/proposals/`、**push main（改远端 ref）** | 发布生命周期 + operation registry 与崩溃子矩阵 + 预算预留 + 权限隔离 |
| **Stage 1b · 治理与可观测** | 远端队列对账、拒绝记忆、机器红线 gate、质量指标、连续错误熔断、rolling-24h、OnFailure 告警。完成后节拍提到 **30 分钟** | 同 1a | 上述 + 跨语言指纹一致性 + 真实 API 契约 smoke |
| **Stage 2 · 开发轮** | 全流程：选题 → 实现 → 评审 → PR → 收尾 | 上述 + 分支 / worktree / PR / 多次状态迁移 / 删除清理 | 完整 outbox 事务 + 六维状态派生函数 + 全量崩溃矩阵 + 跨调用预算与截止 + CI 激活门 |

**Stage 1 的副作用面要如实描述**（评审 R3-03）：它没有 worktree、PR 和删除类操作，但**仍包含外部 create/update 与 main ref 变更**——push main 会改远端引用，设 label 会改 Issue，两者同样有并发、响应丢失与覆盖风险。把 Stage 1 说成「只建不改」会导致其 outbox 与崩溃矩阵被过度降级，这正是 v3 的错误。

Stage 1 先上线的理由不变：协议复杂度低一个量级，却能立刻暴露最不确定的东西——**选题质量**。若 finder/judge 产出的提案不值得做，后面所有工程都白搭。Stage 2 的设计在本 spec 中同样完整给出，不因分期而削减（§十五记录每条在哪个阶段生效）。

**Stage 1 的结束点**（评审 R3-01）：止于「提案卡已在**远端 main** 可见 + 发布收据写完」。建分支与 worktree 属 Stage 2，在 Stage 2 激活后才发生。

## 一、目标与问题陈述

让本项目在**无人值守**状态下持续自我推进：定时起一轮，agent 自己找到可行的下一步改进、在主分支留下提案文档、去独立 worktree 开发、提起 PR；用户只在 GitHub 上审阅并合并；合并后由后续轮次自动收尾。

要解决的五个真问题：

1. **选题质量**——没人拦住 agent 挑伪需求、重复已完成项、踩已冻结的设计红线。
2. **上下文耗尽**——长循环会话必然撑爆上下文，靠「写交接文档」只是缓解。
3. **中断韧性**——网络/进程中断随时可能发生，任何一轮都必须能被下一轮无损接续。
4. **状态可信**——模型的自我报告（「我推了分支」「测试绿了」）不能直接作为状态迁移依据。
5. **不失控**——失败要有预算和熔断，成本要有硬上限，队列不能无界膨胀，提案质量不能随时间静默退化。

## 二、已裁定决策

| 维度 | 裁定 | 备注 |
|---|---|---|
| 驱动方式 | **无人值守循环** | 非手动起轮 |
| 循环载体 | **headless 每轮一个新进程**（systemd user timer + `claude -p`） | 从根上消除上下文累积 |
| 编排载体 | **内置 `Workflow` 工具**（已核实存在；skill 指令调用它属合法 opt-in 路径） | 承载模型侧编排 |
| 控制面 | **确定性可信控制器**独占全部副作用与状态迁移 | Workflow 降权为「只产出结构化结果」 |
| 队列真值源 | **GitHub 事实**（Issue/PR/branch/commit 联合派生），label 只作索引 | 网页/手机可见 |
| 提案文档落点 | **新建 `docs/proposals/`**；中大型再升级 `docs/plan/<topic>.md` | 不冲淡现有 plan/ |
| 节拍 | **30 分钟一轮，最多 5 个在飞 PR** | 满 5 后降为只扫描 |
| 选题边界 | 已登记待办 + 允许发现新问题 + 允许卫生型改进；**不碰冻结红线** | 红线项只允许开 Issue 提请裁决 |
| CI | **要建**，先 PoC 探边界；CI 门是 **Stage 2 的激活门** | 无人值守 PR 的可信凭据 |
| 分支策略 | **merge-based**，不做 rebase 纪律 | 允许大量 merge 提交 |
| GitHub 写身份 | **专用 fine-grained PAT**（属 `puxu-msft`，仅本仓库 Contents/Issues/Pull requests: write + Metadata: read），存仓库外，systemd 注入 `GH_TOKEN` | 现 `gh` 账号 `puxu_microsoft` 对本仓库仅 READ（已实测），跑不通状态机 |
| 生产数据隔离 | **不做 OS 级隔离**（用户裁定「无需隔离」） | 改以 §九 确定性纵深防线替代；OS 隔离降为 optional，见 §十五 |
| 交付方式 | **分三阶段**：Stage 1a（发布回路，2h 节拍）→ Stage 1b（治理与可观测，提到 30min）→ Stage 2（开发轮）（§零） | 用户 2026-07-30 在计划评审后追加 1a/1b 拆分 |
| 目录布局 | **仓库内**：`.claude/{agents,rules,scripts,skills,workflows}`、`.worktree/<task>`、`docs/`（用户 2026-07-30 偏好） | 取代早期的「仓库外专用 clone」方案，隔离目标不变（§四） |
| 运行时状态 | **SQLite**（`.claude/state/harness.db`），控制器用 **Python 3** | 由 Web UI 规划倒推（§十六、§十七） |
| Web UI | **Stage 3，后续另开 spec**；现在只保证不堵路 | 用户 2026-07-30 提出 |

## 三、实测环境事实（影响设计，非假设）

| 事实 | 影响 |
|---|---|
| `gh` API 身份 `puxu_microsoft` 对本仓库 `viewerPermission=READ`；git SSH 身份是 `puxu-msft`（可推） | 两条身份链分裂，必须用专用 PAT 统一 API 写权限 |
| 用户级 `~/.claude/settings.json` 含 **332 条 `Bash(...)` 授权**，并允许 `mcp__plugin_serena_serena__execute_shell_command` 等可执行 shell 的 MCP 工具 | permission 作用域会合并；「在项目里写最小 allow」**不成立**，必须隔离 setting sources 并禁用 MCP |
| systemd user `PATH` = 系统默认，**不含** `~/.local/bin`（`claude`）与 `~/.cache/cargo/bin`（`cargo`/`rustc`） | service 必须固定 `Environment=PATH=` 或用绝对路径，否则永久卡在预检 |
| systemd user 环境含 `SSH_AUTH_SOCK`（gpg-agent） | agent 只要能跑 `git` 就能用你的 SSH 身份直接 push，绕过「凭据只在控制器」 |
| `Workflow` 脚本内有 `budget` 全局（`total`/`spent()`/`remaining()`），超限时 `agent()` 抛错 | 每轮预算可在脚本内硬收敛，优于只靠外部 `--max-budget-usd` |
| `Workflow` 的 `agent()` 支持 `agentType` | 裁决/评审阶段可点名 `gpt-souls:*` 做跨模型对抗，避免同模型自查盲区 |
| `Workflow` 并发上限 `min(16, cores-2)`，本机 20 核 → 16；建议单轮 <15 agent | §六 的 agent 数上限依此 |
| 交互式认证的 MCP server 在 headless/cron 下可能不可用 | 设计不得依赖任何 MCP |
| 远端 main **无 branch protection**，且专用 PAT 无 Administration 权限 | 「required check」需 owner 一次性配置并留激活收据，否则术语要改口径（§十一） |

## 四、架构：三层信任

| 层 | 实体 | 可信度 | 职责 |
|---|---|---|---|
| **控制面** | 可信控制器（`.claude/scripts/harness/`，非模型） | 可信 | 单例锁、启动预检、预算预留、GitHub 查询与状态派生、模式路由、凭据持有、Issue/分支/worktree 生命周期、diff 白名单与红线 gate、commit/push/开 PR、label 迁移、outbox 事务、熔断与记账 |
| **编排面** | `.claude/workflows/scrollz-*.js` | 半可信（顺序确定，内容不可信） | 固定 agent 调用顺序与 barrier，收集结构化结果 |
| **执行面** | agent（finder / judge / implementer / reviewer） | **不可信** | 只产出结构化主张与工作区改动；**不持凭据、不 push、不改 label、不开 PR** |

**铁律：任何状态迁移只能由控制器在重新查证事实之后执行。**

组件位置（**用户偏好的仓库内布局**）：

| 组件 | 位置 |
|---|---|
| 定时器与单例闸 | `~/.config/systemd/user/scrollz-harness.{timer,service}`（固定 PATH，绝对路径） |
| 可信控制器 | `.claude/scripts/harness/`（随仓库版本化） |
| 轮次入口 skill | `.claude/skills/scrollz-round/SKILL.md` |
| Workflow 脚本 | `.claude/workflows/scrollz-{propose,implement,review}.js` |
| harness 专用 agent 定义 | `.claude/agents/`（finder-* / judge-* / implementer / reviewer） |
| harness 专用规则 | `.claude/rules/`（注入 agent 的不可信输入纪律、红线纪律等） |
| harness 会话 settings | `.claude/harness-settings.json` |
| 红线清单 | `docs/harness/redlines.yaml` |
| 运行时状态（outbox / ledger） | `.claude/state/harness.db`（SQLite，**gitignore**） |
| 发布工作区 | `.worktree/_publish`（detached at `origin/main`） |
| 开发 worktree | `.worktree/<issue>-<slug>`（Stage 2，**gitignore**） |
| 测试 launcher | 控制器持有、**仓库外**、agent 不可修改（Stage 2） |
| PAT | `~/.config/scrollz-harness/env`（仓库外，`chmod 600`） |

`.gitignore` 需追加：`.worktree/`、`.claude/state/`。

**发布工作区为何是 detached**：git 不允许两个 worktree 同时检出同一分支，而用户主工作区已占用 `main`。故 `.worktree/_publish` 停在 detached `origin/main`，提交后以 `git push origin HEAD:main` 发布。这既满足仓库内布局偏好，又拿到独立 index——**不会碰主工作区的未提交改动**（评审 C-05 的目标不变，实现手段由「专用 clone」改为「detached worktree」）。控制器每轮开始前 `git fetch` 并把 `_publish` 重置到最新 `origin/main`。

**运行时状态存 SQLite** 而非纯日志文件：一方面满足 §六 durable intent 的原子更新与崩溃恢复要求，另一方面为未来的 Web UI（§十七）提供可直接只读查询的数据源，避免届时再补一层导出。

## 五、状态：事实派生函数

label 是索引不是真值。每轮**先派生状态再路由**。

### 5.0 Stage 1 发布生命周期（评审 R3-02 / R4）

分期新增了「Issue 与提案卡联合发布」这一状态机，六维（§5.1）表达不了它——「只有 Issue」「本地已 commit 未 push」「已 push 但无收据」「全部完成」在六维里坍缩成同一个 tuple，Stage 1 的中断因此无法恢复。故 Stage 1 单列。

判定依据为 **outbox + 本地发布工作区 + 远端 GitHub/Git 事实**三者；**完成态必须以远端事实确认**。

必须写成**有序派生函数**而非条件表——发布完成后 `issue-created` / `labels-set` / `proposal-published` / `publication-receipt-complete` 会同时为真，无序求值会误落 `inconsistent`：

```
按优先级求值，命中即停：

0. Issue 已被用户关闭                                    → closed-by-user
1. 发布收据存在，且其 operation_id、proposal path、
   远端 main 的 blob/commit 三者全部一致                 → publication-receipt-complete
2. 远端 main 已含绑定同一 operation_id 的提案卡          → proposal-published
3. `.worktree/_publish` 有绑定同一 operation_id 的本地 proposal
   commit，且远端 main 尚无                              → proposal-committed-local
4. Issue 的 label 集合与预期全集一致                     → labels-set
5. 存在绑定 operation_id 的 Issue                        → issue-created
6. outbox 有 prepared 的 operation，远端无对应 Issue      → candidate-selected
7. 其余组合，或任何绑定冲突                              → inconsistent
```

两处精度要求：

- **`proposal-published` 必须查远端** `docs/proposals/<issue>-<slug>.md` 的存在与 operation 绑定，不能只看本地 commit。
- **`publication-receipt-complete` 不能只验收据 marker 存在**，必须同时校验收据绑定的 `operation_id`、proposal path 与已观察到的远端 main commit/blob 一致；否则一条过早写入或陈旧的收据会把「尚未发布」误判为完成。

### 5.1 正交事实维度（Stage 2）

先把观察归一化到互相独立的维度，再做判定（不把 label 写进判定条件）：

| 维度 | 取值 |
|---|---|
| Issue lifecycle | `open` / `closed-by-user` / `closed-by-linked-merge` |
| PR lifecycle | `none` / `open` / `closed-unmerged` / `merged` / `multiple` |
| branch lifecycle | `absent` / `present-at-receipted-SHA` / `present-diverged` |
| worktree lifecycle | `absent` / `owned` / `foreign` / `marker-mismatch` |
| attempt lifecycle | `none` / `active` / `superseded` |
| base 漂移 | `fresh` / `stale`（PR base SHA ≠ 当前 main） |

### 5.2 有序判定（优先级从高到低，命中即停）

**判定条件只允许使用 §5.1 的事实维度，不得出现 label**（label 不一致本身由第 10 条兜底）。

0. attempt `superseded` → **不复活**：该 attempt 的分支/worktree 交给清理流程，Issue 回到队列由新 attempt 处理。此条必须先于第 6/7/8 条求值，否则 `superseded + branch absent` 会被误判为回落 proposed、`superseded + branch present` 会被误判为接续（评审 R3 指认）
1. `closed-by-linked-merge` 或 PR `merged` → **待收尾**
2. Issue `closed-by-user` → **用户终态**，记录原因，不再选中
3. PR `closed-unmerged` → **rejected**，原因入拒绝记忆
4. PR `open` 且 base `stale` → merge main 进 feature 分支、重跑受影响测试；冲突则 `blocked`
5. PR `open` 且 `fresh` → **等待用户**，不动
6. PR `none` 且 branch `present-at-receipted-SHA` 且 worktree `owned` → **接续**，从收据中的 `last_checkpoint` 继续
7. PR `none` 且 branch `present`（含 `diverged`）且 worktree `absent`/`marker-mismatch` → 重建 worktree 后接续；`diverged` 需先核对收据 SHA，无法解释的分歧转第 10 条
8. PR `none` 且 branch `absent` 且 attempt `none`/`active` → 回落 `proposed`
9. PR `multiple` → **`needs-human-reconciliation`**
10. 其余任何组合（含收据与远端事实不符、label 缺失或双状态、worktree `foreign`） → **`needs-human-reconciliation`**，开哨兵评论并告警，**不猜**

**互斥完备性必须被机械证明**：以 property-based test 穷举六维度全组合，断言每个组合恰好落到一个结果，无重叠、无遗漏（评审 R2-02）。

`foreign` / 无 marker 的分支与 worktree **一律不动**——禁止清理非 harness 创建的对象。

### 5.3 标识与收据

- 分支名固定 `harness/<issue>-<slug>`；worktree 内 `.harness-owner`（issue、attempt_id、base SHA）。
- 每次迁移由控制器写一条 Issue 评论（固定首行 marker `HARNESS-RECEIPT`）：`round_id` / `attempt_id` / 旧态 / 新态 / 已验证事实（branch SHA、PR 号、测试 receipt 摘要）。
- 收据是对账辅助证据，不是唯一真值；控制器崩溃后必须**双向 reconcile**（outbox ↔ 远端事实），不得单信任一方。

## 六、副作用：durable outbox 事务协议

GitHub 的建 Issue / 开 PR / 写评论**没有统一幂等接口**，「响应丢失」会造成重复对象或丢账（评审 R2-03）。因此控制器必须持久化 intent，而不是事后记账：

1. **任何副作用前**先落盘并 `fsync`：`operation_id`、natural key、payload hash、`phase=prepared`。
2. 调用返回后写 `observed`；**响应不确定时禁止盲重试**，先按 natural key 查询远端。
3. **唯一执行入口**（评审 R2-06 残留）：所有外部副作用**只能**经统一的 operation registry / outbox executor 发出；崩溃矩阵从该 registry **自动生成**。否则实现者随手加一处直连 `gh`/`git` 调用，矩阵永远不知道它存在。绕过 registry 的直接调用列入红线 gate。

### 6.1 Stage 1 的 operation registry（上线前必须全覆盖）

| operation | natural key / 恢复查询 |
|---|---|
| 创建 Issue | 正文内嵌 `HARNESS-OP:<operation_id>`，建前建后均按 marker 搜索 |
| 设置初始 label | **优先随 Issue create 一次提交**；若单独发出，重试前先读当前 label 集 |
| 写提案卡 + 本地 commit | 固定 proposal path + commit trailer `HARNESS-OP:<operation_id>` |
| push main | 查询**远端 main** 是否已含该 operation 的 commit 或相同 proposal blob |
| 写发布收据 | 固定 comment marker + operation ID |
| 本地 ledger / 预算结算 | outbox operation ID |

**main 并发更新**：push 遇 non-fast-forward 时**不得**另建第二张卡或第二个 Issue；应 fetch/merge 当前 main，把同一 operation 的 proposal commit 重放或合并后再 push，保持同一 operation lineage。

### 6.2 Stage 2 追加的 natural key

PR：`repo + head branch + base branch`，重试前按 head 查现存 PR；receipt 评论：隐藏 marker / 固定首行。

### 6.3 label 迁移

GitHub labels API **没有通用 compare-and-set**，replace-all 在读后写之间仍可能覆盖用户的并发修改。因此：迁移前先读当前 label 集并与预期旧值比对，不符即判**冲突**转 `needs-human-reconciliation`，**不覆盖**；replace-all 时必须原样保留 `T*`/`size:*`/`lane:*` 等辅助 label。

### 6.4 存储

要求是「durable intent + 原子更新 + 崩溃恢复」；SQLite WAL 是直接选择，严格实现的 append-only journal + fsync + checksum 亦可（技术选型由 plan 定，不是规格要求）。控制器崩溃后必须**双向 reconcile**（outbox ↔ 远端事实），不得单信任一方。

## 七、一轮的流程

### Phase A · 控制器：预检 → 预算预留 → 派生 → 路由

**启动硬预检**（任一失败 fail closed，不起模型、不烧钱）：`GH_TOKEN` 对本仓库 `viewerPermission >= WRITE`；`git ls-remote` 可达；`.worktree/_publish` 干净且已重置到最新 `origin/main`；无 `harness:paused` 哨兵；`claude`/`cargo` 绝对路径可执行；outbox 无未决且无法判定的 operation。

**预算预留**（评审 R2-07）：调用 `claude` **之前**原子预留本轮最大预算并落盘，事后按实际成本结算；结果未知时按最坏上限计费直到对账成功。熔断计数同样在尝试开始前落盘。否则「崩溃 → 重启 → 再花一次」可无限越过日预算。

**派生与路由**（§五）→ 唯一模式：

| 优先级 | 模式 | 触发 | 本轮做什么 |
|---|---|---|---|
| 1 | 事实收尾 | 有已合并未收尾 PR | 对**所有**已合并 PR 做轻量幂等事实收尾，再限量做一份文档整理 |
| 2 | 接续 | §5.2 第 6/7 条 | 从 `last_checkpoint` 继续 |
| 3 | 只扫描 | 在飞 PR ≥ 5 或队列满 或 **Stage 1** | 只扫描+裁决+队列治理 |
| 4 | 正常开发轮 | 其余（Stage 2） | 全流程 |

### Phase B · 分段执行（评审 R2-01 修正）

v2 的致命矛盾：既要求「agent 第一个可编译提交即 push」保证中断韧性，又要求「只有控制器能 push」——**不可能同时成立**，因为控制器在 Workflow 返回前无法介入。修正为**多次 `claude -p` 调用，控制器在段间接管**：

```
段 1  Workflow scrollz-propose
        扫描（4 lens 并行 finder）→ JS 去重 → 3 judge 对抗裁决（可跨模型 agentType）→ 排序选一
        ↓ 返回结构化候选，不产生任何外部副作用
控制器  建 Issue（natural key 幂等，label 随建一次提交）→ 冻结 attempt_id
        → 写提案卡 docs/proposals/<issue>-<slug>.md（commit trailer 绑定 operation）
        → push main（non-fast-forward 时 fetch/merge 后重放同一 operation）
        → 写发布收据
        ↓                      【★ Stage 1 到此结束 ★】
        ↓                      【以下为 Stage 2，需 §十一 激活门通过】
控制器  建分支 + worktree + .harness-owner → 写 intent receipt
        ↓
段 2  Workflow scrollz-implement（在既有 worktree 内，TDD）
        只允许 commit，**不允许 push**；到达 checkpoint 即返回
        ↓
控制器  校验 commit（diff 路径白名单 + 红线 gate + 属主）→ push → 更新 last_checkpoint 收据
        ↓ 未完成则再起一次段 2 调用（多次 invocation，而非一次后台 Workflow 内部回调）
段 3  Workflow scrollz-review（fresh session，未参与实现的 reviewer，可跨模型）
        ↓
控制器  验证测试 receipt → gh pr create（含 Closes #N、被测 SHA、命令与退出码、测试数/skip 数、触碰面、评审结论）
        → label 迁移 → 写收据 → 结算预算与记账
```

#### B.1 上下文续接契约

- propose / implement / review 使用**独立 session**；review **必须** fresh session 以保证独立性。
- implement 的后续 segment 采用 **fresh invocation + durable checkpoint**，而非依赖 `--resume`：输入固定为 Issue 规格、base/head SHA、上一 checkpoint、剩余验收项、工作树状态。`--resume` 可作为优化，但 session **不是真值**——控制器或 session 丢失都不得影响恢复。
- 明确不依赖 `resumeFromRunId`（同会话限定）。

#### B.2 跨调用预算 grant（评审 R3）

`--max-budget-usd` 是**每次 `claude -p` 调用**的独立上限，Workflow 的 `budget` 也只属于当前 run——多次 implement 会各自拿到完整 cap，不会自动继承「本轮剩余」。故控制器维护：

```
remaining_grant = round_reserved - settled_cost - outstanding_worst_case
```

每次调用前发放 `invocation_grant <= remaining_grant`，同时设置该次的 `--max-budget-usd` 与脚本内 `budget.total`；无最终 result 时按 grant 全额占用直到可对账。**agent 数（≤12）、turns、重试次数同样跨调用累计**，不是每个 Workflow 各自 ≤12。

#### B.3 单调截止（评审 R3）

`TimeoutStartSec`（建议 25 分钟）约束的是**整个控制器轮次**，不是每个子进程。控制器用单调时钟维护 `remaining_time = round_deadline - now`，每次 Workflow 的子进程超时与后台等待上限都不得超过剩余时间，并**预留** checkpoint、outbox 结算与 SIGTERM 清理的时间窗。否则第一次 implement 就能吃光 25 分钟，后面的 push、评审与记账永远没有执行窗口。

### Phase C · 记账退出

轮次账字段：`round_id` / mode / issue / attempt / workflow run id / duration / turns / cost / 工具拒绝次数 / result / exit code / last_checkpoint。

## 八、收尾模式

1. `.worktree/_publish` fetch 后重置到最新 `origin/main`（merge 语义，不 rebase）。
2. 更新 [ROADMAP.md](../ROADMAP.md) / [CHANGELOG.md](../CHANGELOG.md) / [TRACKING.md](../TRACKING.md)。
3. 提案卡移入 `docs/proposals/archive/`。
4. **校验 owner marker 后**删远端分支与 worktree；marker 不符则跳过并告警。
5. 高风险 PR（并发/崩溃安全/磁盘格式）派一次合并态评审；发现问题**开新 Issue**，不当场改。
6. **经验只产出候选文档或 Issue**：无人值守收尾**禁止**修改 `.claude/skills/`、`.claude/workflows/`、permissions、hooks、systemd 单元、全局 memory——这些是可执行策略，等于让系统按自己的输出重写自身控制逻辑；此类改动必须走独立 PR 由用户合并。

## 九、权限、凭据与确定性纵深防线

### 9.1 会话隔离（对抗作用域合并，评审 R2-04 / R3）

v3 写的「固定 `--setting-sources` **或** 专用 `--settings`」是伪协议：`--settings` 只覆盖同名键，未覆盖的 user/project/local permissions、hooks、plugins 仍然生效。必须给出**一个精确、经实测确认的启动组合**（flag 语义已核实）：

```
claude -p "/scrollz-round" \
  --setting-sources project \            # 只加载项目作用域，屏蔽用户级 332 条 Bash 授权与 hooks/plugins
  --settings .claude/harness-settings.json \
  --strict-mcp-config \                  # 不提供 --mcp-config，等价于零 MCP（含 serena 的 execute_shell_command）
  --tools "<按阶段收敛的内置工具集>" \    # Stage 1 不含 Bash/Edit/Write
  --permission-mode dontAsk \
  --max-turns <N> --max-budget-usd <grant> \
  --output-format stream-json
```

- **Stage 1 的 `--tools` 不含 `Bash`/`Edit`/`Write`**——finder/judge 只需 `Read`/`Grep`/`Glob` 读仓库，全部外部动作由控制器执行。这使 §9.2 的多数防线在 Stage 1 直接不适用（评审 R2-05 前提）。
- **禁止 `bypassPermissions` 与 `--dangerously-skip-permissions`**。
- Stage 2 放开 Bash 时，deny 至少覆盖：`git push*`、`git remote *`、`git config *`、`gh *`、`systemctl *`、`fusermount*`、生产路径写入。仅禁 force push 远远不够。
- **凭据清场**：agent 进程显式清除 `GH_TOKEN`/`GITHUB_TOKEN`/`SSH_AUTH_SOCK`/git credential helper 环境，设 `GIT_TERMINAL_PROMPT=0`。否则 systemd user 环境里现存的 `SSH_AUTH_SOCK` 足以让 agent 直接推 main。
- Round 0 必须**负向验证**：`system/init` 输出中无插件与 MCP 工具；agent 无法 `push main`、无法 `gh issue create`、无法经 MCP 间接执行 shell。只验 happy path 不算通过。
- 被拒工具调用超阈值 → `harness:blocked` 并记账，不视为可绕过。

### 9.2 不做 OS 隔离前提下的确定性防线（评审 R2-05）

用户已裁定不做 OS 级隔离。但有一条防御必须补：**Bash deny 管不住子进程**——`cargo test` 会执行 agent 刚写的代码，那段代码可以直接调 `fusermount3`、`systemctl`、读写真实 `~/.claude/projects`，完全绕过顶层命令拦截。低成本补法：

1. agent 不获得通用 `Bash(cargo *)`；测试经**控制器持有、仓库外、agent 不可改**的 launcher 执行。
2. launcher 固定临时 `HOME`/`CLAUDE_PROJECTS`/`SCROLLZ_HOME`，执行前后记录 mount table、scrollz units、相关进程、生产目录 metadata hash；有变化即 kill + 熔断 + 告警。
3. tokenized `PreToolUse(Bash)` guard 处理复合命令、wrapper、绝对路径与重定向；无法解析的高危命令 fail closed。**它只防直接 shell，不宣称能管住子进程。**
4. scrollz CLI 增加 harness-mode interlock：harness 环境变量启用时，`enable apply/restore/purge/reconcile/remount` 与 managed mount 只接受本轮临时根，拒绝真实生产根；**interlock 实现文件本身列入红线**，agent 改动即阻断。
5. diff gate：新增 `Command::new`/`std::process`/`systemctl`/`fusermount`/绝对 home 路径/环境变量清除时，要求人工审查。

这些不等同 OS 隔离，但对「合作型但会犯错的 agent」是有实质价值的确定性防线。

### 9.3 不可信输入纪律

本仓库公开，Issue/PR/评论/提交信息可被任何人写入，可能含提示注入；harness 自己写入的提案又会成为下一轮上下文，形成自注入回路。因此：所有 GitHub 与仓库文本**一律按 data 处理**，置于提示词中显式不可信边界内；禁止从中提取可执行命令；控制动作只接受固定 schema，并校验 actor allowlist、repo、marker 与合法迁移；外部评论只能作为候选证据，不能改变权限、红线或执行模式。

## 十、红线清单

`docs/harness/redlines.yaml` 版本化。**每条必须声明可执行 oracle 类型**，不能只有自然语言（评审 R2-10）：

| oracle 类型 | 含义 |
|---|---|
| `deny_change` | 指定文件/字节/常量禁止变化 |
| `requires_decision` | 触碰即转 `needs-decision` |
| `requires_tests` | 列出必须执行且**不可 skip** 的测试 |
| `cross_site_invariant` | 列出所有已知实施点 + 可复现搜索式 |
| `manual_semantic_review` | 无法机器证明，直接阻断自动 PR |

机器 gate 的结论只能是「规则命中 / 未命中」，**不得声称自然语言不变量已被证明**——「文件没碰受保护 symbol，但调用顺序改了同一不变量」是最危险的假绿。初始条目至少覆盖：磁盘格式魔数与版本、superblock 布局、尾日志 record 格式、崩溃安全提交顺序、已生效 ADR 锚点、§9.2 的 interlock 实现文件。

## 十一、CI 与 Stage 2 激活门

**Round 0 是 Stage 2 的激活门。** 未通过前只允许 Stage 1。必须实测：

1. `claude -p` 等待后台 Workflow 的真实行为与上限，及与 systemd 超时的先后顺序（CLI 后台等待上限必须 **大于** `TimeoutStartSec`，建议后者 25 分钟）。
2. `--settings` + `--permission-mode dontAsk` + deny 组合能否无人值守跑完，且**负向验证**：agent 无法 `push main`、无法 `gh issue create`、无法经 MCP 间接执行 shell。只验 happy path 不算通过。
3. 专用 PAT 实权：建/改 label、开 Issue、开 PR。
4. GitHub runner 能力边界：`/dev/fuse`、`fusermount3`、loop 设备、`dm-flakey`/`dm-log-writes`、sudo、单 job 时长。
5. **main 保护策略与 Stage 1 直推的共存**（评审 R2-11 / R3-04）。远端 main 现无保护，专用 PAT 无 Administration 权限，无法自建或自查。且存在真实冲突：Stage 2 若启用「必须走 PR」的 ruleset，Stage 1 每轮向 main 直推提案卡就不再成立。三条出路，**本 spec 推荐第一条**：
   - **推荐：不启用阻断直推的保护**，把 spec 中的「required check」一律改称**控制器的 merge-readiness 条件**——CI 仍在 PR 上跑并可见，合并决定权本来就只在用户（单维护者仓库，保护带来的边际收益小于协议复杂度）。术语不得与 GitHub 的 required status checks 混用。
   - 提案卡也走轻量 docs PR：保护可启用，但每张提案卡都要用户点一次合并，无人值守价值被削弱。
   - ruleset 允许受限 bot bypass：权限面更大，必须有独立激活测试与审计，且控制器须严格校验提交只含单个 `docs/proposals/<issue>-<slug>.md`。

   无论选哪条，都需 owner 一次性配置并留**激活收据**（保护是否启用、required checks 精确名称、是否要求 up-to-date、是否禁 force push 与删除）；控制器每轮只读校验仍符合收据，不符即暂停自动开发。

**测试 receipt 硬要求**：本项目 FUSE 测试在缺 `/dev/fuse` 或 `fusermount` 时会打印 SKIP 后**成功返回**——「cargo test 绿」不证明挂载路径跑过。receipt 必须含被测 head SHA、命令、退出码、测试数、**skip 数**、以及「真实 FUSE 路径确实执行」的正向证据；skip 超阈值即判证据不足。

CI 分层预期：L0 `fmt`/`clippy`/`build`；L1 不需 FUSE 的测试；L2 需 `/dev/fuse` 的挂载测试（视 PoC）；L3 systemd/dm-* 留本地由 launcher 产出 receipt。边界结论写入 `docs/harness/ci-boundary.md`。

## 十二、队列治理

### 12.1 Stage 1（只扫描期）

Stage 1 没有「实际开发选择」，按开发选择计算的软配额在此**不适用**，只会无限累计 deficit（评审 R2-09/R3-05）。Stage 1 用：

- `proposed` **总上限 + per-lane 上限**（约束每轮发布什么、发布多少）；
- 精确 operation/proposal ID 硬去重；
- `possible_duplicate` 复核；
- user-closed Issue 的拒绝记忆；
- typed `reconsider_when`（见 12.3）；
- stale / superseded 清理。

### 12.2 Stage 2（开发期）

- **软配额而非硬配额**：候选分 `roadmap`/`defect`/`perf`/`hygiene` 四 lane，按 rolling N 次**实际开发选择**计算，且只对**当前 eligible** 候选生效（blocked、needs-decision、与在飞 PR 冲突、oracle 不成立者不参与）。欠额只**提高权重**不强制选择；只扫描期间只累计 deficit，不虚构选中。
- **aging**：排队越久权重越高；治理必须能把 stale 项迁 `superseded`/`blocked` 释放容量，避免旧而不适用的候选永久占位。

### 12.3 去重与复议（两级）

- 精确 operation/proposal ID 可硬去重；**fingerprint 只能产出 `possible_duplicate`**，交确定性字段与 judge 复核——fingerprint 的「规范化目标/不变量」由不可信模型生成，既可能误碰撞也可能被轻微改写绕过。
- **`reconsider_when` 必须是可执行谓词**，不能是自然语言，否则「自动失效」无从实现（评审 R2-08 残留）。允许的谓词类型：`main_sha_changed`、`dependency_issue_closed(#N)`、`decision_version_gt(v)`、`not_before(date)`。无法机器判断的条件只能转**人工复议**，不得伪装成自动。
- rejected 记录必须带 `reconsider_when` 与决定版本；条件满足后自动失效，**不得成为永久去重键**。保留人工 override 与 `supersedes` 关系。

## 十三、可观测性、失败预算与熔断

### 13.1 分阶段的质量指标（评审 R3-05）

指标 schema、阈值与熔断规则**按阶段分离**；切到 Stage 2 才开始累计开发类指标。**未定义值不得按 0 处理**，否则上线即误熔断。

| 阶段 | 可观测指标 |
|---|---|
| Stage 1 | 提案被用户保留 / 关闭的比例、重复率、`needs-decision` 率、各 lens 采纳率 |
| Stage 2 | 追加：合并率、拒绝率、revert 率、首次评审通过率、proposal→PR 周期 |

### 13.2 预算与熔断

- 预算三档：per-round、per-day、rolling-24h，均带 §七 的**事前预留**与跨调用 grant。
- 熔断：同类错误连续 N 次、日预算耗尽、当前阶段的质量指标跌破门槛 → 切 `paused`。
- 告警：专用哨兵 Issue + systemd `OnFailure`；stale `picked`/`in-pr` 超时告警；预检失败必须列明缺失项（如 PATH 缺 `claude`）并触发告警，而非静默重试。
- 人工开关：`harness:paused` 哨兵 Issue 存在即暂停；提供只读诊断命令。

## 十四、验收判据

### 14.1 状态派生函数

- **Stage 1**：穷举 §5.0 的**底层布尔事实**（而非直接穷举八个已命名状态）的全组合，断言该有序函数对每个组合恰好产生一个结果；重点覆盖「只有 Issue」「本地已 commit 未 push」「已 push 未写收据」「收据存在但绑定不一致」「已完成」的可区分性。
- **Stage 2**：property-based test 穷举 §5.1 六维度全组合，断言每组合恰好命中一条判定、无重叠无遗漏，非规范组合唯一落到 `needs-human-reconciliation`。

### 14.2 崩溃点矩阵（从 §六 的 operation registry **自动生成**，非手写）

每个 operation 至少四个崩溃点：`before-call` / `server-applied-response-lost` / `after-response-before-ledger` / `after-ledger`。断言不止「不重复」，还要「重启后最终状态一致」。

- **Stage 1 子矩阵（上线前必须完成，不得延到 Stage 2）**：建 Issue、设 label、提案卡 commit、push main（含 non-fast-forward 重放）、写发布收据、写 outbox/ledger。
- **Stage 2 追加**：建 worktree、写 `.harness-owner`、实现 commit、feature push、开 PR、label 迁移（含 replace-all 响应未知），以及**收尾流程的独立矩阵**（关 Issue、删远端分支、删 worktree、文档 commit、push main、receipt）。

### 14.3 真实环境验收

1. 关闭终端、断开会话，30 分钟后 `gh issue list --label harness` 出现新提案，提案卡已在 main（Stage 1 即可验）。
2. `kill -9` 一轮进程后，下轮自动接续同一提案完成，无重复 Issue、无孤儿 worktree。
3. 用户合并一个 PR 后，后续轮次完成收尾：文档已更新、分支与 worktree 已清理、Issue 已关。
4. 诱饵项：ROADMAP 塞一条触及磁盘格式的候选 → 产出 `harness:needs-decision` 而非 PR，且控制器确定性 gate 独立拦下。
5. **负向权限验收**：agent 尝试 `git push origin HEAD:main`、`gh issue create`、经 MCP 执行 shell，全部失败。
6. 一轮跑完后比对 mount table、systemd units、非 harness 分支与工作树 hash、生产目录 metadata hash，均无变化。
7. PAT 失效 / GitHub 429/5xx → fail closed 且不烧钱。
8. 在飞 PR 达 5 时第 6 个提案**不产生任何分支/worktree/PR**。
9. 真实 GitHub smoke test 至少一轮，避免 fake adapter 自洽假绿。

## 十五、评审处置台账

| 条目 | 处置 | 生效阶段 |
|---|---|---|
| R1 C-01 凭据只读 | 采纳，专用 PAT（§二、§三） | Stage 1 |
| R1 C-02 headless 权限 | 采纳并在 R2-04 后加强（§9.1） | Stage 1 |
| R1 C-03 OS 隔离 | **降档保留**：用户裁定不做；改以 §9.2 确定性防线替代。触发重议的条件——出现一次真实生产数据事故或误触 | — |
| R1 C-04 可信控制器 | 采纳（§四、§七） | Stage 1 |
| R1 C-05 不污染用户工作区 | 采纳（§四）；实现手段按用户布局偏好由「专用 clone」改为 `.worktree/_publish` detached worktree | Stage 1 |
| R1 I-06 双层超时 | 采纳（§十一.1） | Stage 1 |
| R1 I-07 事实对账 | 采纳并按 R2-02 重做为派生函数（§五） | Stage 2（Stage 1 只需退化版） |
| R1 I-08 收尾饥饿与基线漂移 | 采纳（§5.2 第 4 条、§七模式 1） | Stage 2 |
| R1 I-09 测试证据 | 采纳，CI 升为 Stage 2 激活门（§十一） | Stage 2 |
| R1 I-10 验收弱 | 采纳并按 R2-06 自动生成矩阵（§十四） | Stage 2 |
| R1 I-11 预算与熔断 | 采纳并按 R2-07 补事前预留（§七、§十三） | Stage 1 |
| R1 I-12 大项饥饿 | 采纳并按 R2-09 改软配额（§十二） | Stage 1 |
| R1 I-13 提案质量闭环 | 采纳并按 R2-08 拆两级去重（§十二） | Stage 1 |
| R1 I-14 不可信输入 | 采纳（§9.3） | Stage 1 |
| R1 I-15 自修改通道 | 采纳（§八.6） | Stage 2 |
| R1 I-16 机器红线 | 采纳并按 R2-10 加 oracle 类型（§十） | Stage 2 |
| R1 M-17 编号顺序 | 采纳，且 R2-01 修正后真正成立（§七 Phase B） | Stage 1 |
| R2-01 Phase 因果矛盾 | 采纳，改为多次 Workflow 调用 + 段间控制器接管（§七 Phase B） | Stage 2 |
| R2-02 派生函数不互斥完备 | 采纳（§五、§14.1） | Stage 2 |
| R2-03 outbox 事务协议 | 采纳（§六） | Stage 1 |
| R2-04 凭据与作用域合并 | 采纳（§9.1、§三 实测证据） | Stage 1 |
| R2-05 无隔离下的纵深防线 | 采纳（§9.2） | Stage 2（launcher 在 Stage 1 无测试执行，不需要） |
| R2-06 崩溃矩阵不全 | 采纳，改自动生成（§14.2） | Stage 2 |
| R2-07 预算事前预留 | 采纳（§七） | Stage 1 |
| R2-08 fingerprint 永久压制 | 采纳（§十二） | Stage 1 |
| R2-09 配额语义 | 采纳（§十二） | Stage 1 |
| R2-10 红线 oracle 类型 | 采纳（§十） | Stage 2 |
| R2-11 branch protection 收据 | 采纳（§十一.5） | Stage 2 |
| R2-12 systemd PATH | 采纳（§三、§四、§十三） | Stage 1 |
| R3-01 Stage 1 结束点越界 | 采纳，结束点前移至发布收据完成（§零、§七 Phase B） | Stage 1 |
| R3-02 Stage 1 发布生命周期缺失 | 采纳（§5.0、§6.1、§14.1、§14.2） | Stage 1 |
| R3-03 「只建不改」描述失准 | 采纳（§零） | Stage 1 |
| R3-04 保护策略与直推冲突 | 采纳，推荐「不启用阻断直推的保护 + 改口径为 merge-readiness」（§十一.5），**待用户确认** | Stage 2 交界前 |
| R3-05 指标混用与误熔断 | 采纳，指标按阶段分离、未定义值不补 0（§13.1） | Stage 1 |
| R3 attempt 维度未参与判定 | 采纳，新增第 0 条 superseded 优先（§5.2） | Stage 2 |
| R3 判定条件混入 label | 采纳，判定只用事实维度，label 不一致由兜底条处理（§5.2） | Stage 2 |
| R3 registry 唯一入口 | 采纳（§六.3 唯一执行入口 + 红线 gate） | Stage 1 |
| R3 label 无 CAS | 采纳，改为读-比对-冲突转人工（§6.3） | Stage 1 |
| R3 跨调用预算 grant | 采纳（§七 B.2） | Stage 2 |
| R3 单调截止 | 采纳（§七 B.3） | Stage 2 |
| R3 上下文续接契约 | 采纳，fresh invocation + durable checkpoint（§七 B.1） | Stage 2 |
| R3 `--settings` 伪协议 | 采纳，给出精确启动组合并已核实 flag 语义（§9.1） | Stage 1 |
| R3 `reconsider_when` 需 typed | 采纳（§12.3） | Stage 1 |

## 十六、开放项（实施期确认，不阻塞本 spec）

- `claude -p` 后台等待的真实上限与退出码形态——Round 0 实测。
- 具体数值：每轮 token/美元预算、重试次数、熔断阈值 N、队列上限、skip 数阈值、lane 配额窗口 N——先给保守硬上限，实测后只调优不新建。

**已从开放项转为裁定**（因 §十七 Web UI 与仓库内布局偏好）：

- **outbox / ledger 存储 = SQLite**（WAL）。它同时满足 durable intent 的原子更新与崩溃恢复，以及 Web UI 的只读查询；落 `.claude/state/harness.db`，gitignore。
- **控制器实现语言 = Python 3**（标准库 `sqlite3`，GitHub 访问经 `gh`）。理由：无构建步骤、状态机与 SQL 直写、未来 Web UI 可复用同一进程与数据层；相对 Rust bin 少一层编译与 workspace 耦合，相对 shell 多了可测试性与结构化数据处理能力。此裁定可在 plan 阶段以实测理由推翻。

## 十七、未来阶段：Web UI（Stage 3，用户 2026-07-30 提出）

用户希望后续配套 Web UI。本 spec 不展开其设计，但**现在就要保证不把路堵死**，故已产生两条前置约束（见 §十六）：状态存 SQLite、控制器用 Python。

预期形态与边界：

- **只读优先**：仪表盘展示轮次流水（round_id / mode / 耗时 / 成本 / turns / 结果）、提案队列与 lane 分布、状态派生结果、outbox 未决 operation、预算与熔断状态、质量指标趋势。数据源就是 `.claude/state/harness.db` + GitHub 事实缓存，不新增真值源。
- **写操作一律回流 GitHub**：即使将来加「暂停」「否决提案」「立即起一轮」等按钮，其效果也必须落成 GitHub 上的对象（`harness:paused` 哨兵 Issue、关闭 Issue、label 变更）或控制器的 outbox operation，**不得**在 Web 层另建一份状态。理由与 §四 铁律一致——真值只能有一处，且用户在网页与手机上的手动干预必须与 Web UI 看到的是同一份事实。
- **不暴露凭据**：PAT 只在控制器进程环境，Web 层不读取、不转发。
- **本地绑定**：默认只监听 loopback；如需远程访问由用户自行决定方式，不在 harness 内置认证。

触发条件：Stage 1 稳定运行、积累了足够轮次数据之后再启动设计（届时另开 spec）。
