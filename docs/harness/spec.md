# scrollz 自主改进 harness / Spec

> 状态：**草案 v3，待用户复核**。v1（9c5e1ab）与 v2（6831bda）分别经 gpt-souls:reviewer 两轮对抗评审判定 needs-rework；本版按第二轮 Critical 4 / Important 7 / Minor 1 逐条处置，并加入实测环境事实。处置台账见 §十五。
> 撰写日期 2026-07-29。本文回答「做什么、为什么」；「怎么做」由后续 [plan.md](./plan.md) 承载。

## 零、交付分期（用户 2026-07-29 裁定）

| 阶段 | 范围 | 副作用面 | 需要的协议强度 |
|---|---|---|---|
| **Stage 1 · 只扫描** | 发现候选 → 对抗裁决 → 建 Issue + 提案卡推 main。**不开发、不开 PR、不建 worktree** | 建 Issue、加 label、提交 `docs/proposals/`、push main | 建 Issue 幂等 + 预算预留 + 权限隔离 + 队列治理 |
| **Stage 2 · 开发轮** | 全流程：选题 → 实现 → 评审 → PR → 收尾 | 上述 + 分支 / worktree / PR / 多次状态迁移 / 删除清理 | 完整 outbox 事务 + 状态派生函数 + 崩溃点矩阵 + CI 激活门 |

Stage 1 先上线的理由：它的副作用只有「建对象」没有「改与删」，协议复杂度低一个量级，却能立刻暴露最不确定的东西——**选题质量**。若 finder/judge 产出的提案不值得做，后面所有工程都白搭。Stage 2 的设计在本 spec 中同样完整给出，不因分期而削减（§十五记录哪些条目属 Stage 2 才生效）。

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
| 交付方式 | **分两阶段**（§零） | |

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
| **控制面** | 可信控制器（`scripts/harness/`，非模型） | 可信 | 单例锁、启动预检、预算预留、GitHub 查询与状态派生、模式路由、凭据持有、Issue/分支/worktree 生命周期、diff 白名单与红线 gate、commit/push/开 PR、label 迁移、outbox 事务、熔断与记账 |
| **编排面** | `.claude/workflows/scrollz-*.js` | 半可信（顺序确定，内容不可信） | 固定 agent 调用顺序与 barrier，收集结构化结果 |
| **执行面** | agent（finder / judge / implementer / reviewer） | **不可信** | 只产出结构化主张与工作区改动；**不持凭据、不 push、不改 label、不开 PR** |

**铁律：任何状态迁移只能由控制器在重新查证事实之后执行。**

组件位置：

| 组件 | 位置 |
|---|---|
| 定时器与单例闸 | `~/.config/systemd/user/scrollz-harness.{timer,service}`（固定 PATH，绝对路径） |
| 可信控制器 | `scripts/harness/`（随仓库版本化） |
| 轮次入口 skill | `.claude/skills/scrollz-round/SKILL.md` |
| Workflow 脚本 | `.claude/workflows/scrollz-{propose,implement,review}.js` |
| 红线清单 | `docs/harness/redlines.yaml` |
| 专用 settings | `.claude/harness-settings.json`（只给 harness 会话用） |
| 测试 launcher | 控制器持有、**仓库外**、agent 不可修改 |
| harness 工作区 | 专用 clone `~/src/scrollz-harness/`（不复用用户开发目录） |
| 开发 worktree | `~/src/scrollz-harness-wt/<issue>-<slug>`（Stage 2） |
| outbox / ledger | `~/.local/state/scrollz-harness/`（durable，见 §六） |

**关于专用 clone**：用户开发目录 `/home/xp/src/zipfs` 随时有未提交改动与并行会话（现场已实测存在）。harness 在那里 pull/commit/清 worktree 会误带他人变更或删掉别人的对象。专用 clone 提交的仍是同一 `main`、推同一远端——「在主树留文档」的语义不变，只是换了物理工作区。

## 五、状态：事实派生函数

label 是索引不是真值。每轮**先派生状态再路由**。

### 5.1 正交事实维度

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

1. `closed-by-linked-merge` 或 PR `merged` → **待收尾**
2. Issue `closed-by-user` → **用户终态**，记录原因，不再选中
3. PR `closed-unmerged` → **rejected**，原因入拒绝记忆
4. PR `open` 且 base `stale` → merge main 进 feature 分支、重跑受影响测试；冲突则 `blocked`
5. PR `open` 且 `fresh` → **等待用户**，不动
6. PR `none` 且 branch `present` 且 worktree `owned` → **接续**，从收据中的 `last_checkpoint` 继续
7. PR `none` 且 branch `present` 且 worktree `absent`/`marker-mismatch` → 重建 worktree 后接续
8. PR `none` 且 branch `absent` → 回落 `proposed`
9. 其余任何组合（含 PR `multiple`、`proposed` 却有分支、`blocked` 却有 open PR、收据与远端事实不符、label 缺失或双状态） → **`needs-human-reconciliation`**，开哨兵评论并告警，**不猜**

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
3. natural key 约定：
   - Issue：标题/正文内嵌机器 marker `HARNESS-OP:<operation_id>`，建前建后均按 marker 搜索。
   - PR：`repo + head branch + base branch`，重试前按 head 查现存 PR。
   - receipt 评论：隐藏 marker / 固定首行。
4. **label 迁移**：单次 replace-all（保留 `T*`/`size:*`/`lane:*` 辅助 label）或带期望旧值的 compare-and-set；不可 CAS 时把人工并发修改视为**冲突**而非覆盖，转 `needs-human-reconciliation`。
5. 存储要求是「durable intent + 原子更新 + 崩溃恢复」；SQLite WAL 是直接选择，严格实现的 append-only journal + fsync + checksum 亦可（实现由 plan 定）。

## 七、一轮的流程

### Phase A · 控制器：预检 → 预算预留 → 派生 → 路由

**启动硬预检**（任一失败 fail closed，不起模型、不烧钱）：`GH_TOKEN` 对本仓库 `viewerPermission >= WRITE`；`git ls-remote` 可达；专用 clone 干净；无 `harness:paused` 哨兵；`claude`/`cargo` 绝对路径可执行；outbox 无未决且无法判定的 operation。

**预算预留**（评审 R2-07）：调用 `claude` **之前**原子预留本轮最大预算并落盘，事后按实际成本结算；结果未知时按最坏上限计费直到对账成功。熔断计数同样在尝试开始前落盘。否则「崩溃 → 重启 → 再花一次」可无限越过日预算。

**派生与路由**（§五）→ 唯一模式：

| 优先级 | 模式 | 触发 | 本轮做什么 |
|---|---|---|---|
| 1 | 事实收尾 | 有已合并未收尾 PR | 对**所有**已合并 PR 做轻量幂等事实收尾，再限量做一份文档整理 |
| 2 | 接续 | §5.2 第 6/7 条 | 从 `last_checkpoint` 继续 |
| 3 | 只扫描 | 在飞 PR ≥ 5 或队列满 或 **Stage 1** | 只扫描+裁决+队列治理 |
| 4 | 正常开发轮 | 其余（Stage 2） | 全流程 |

### Phase B · 分段执行（评审 R2-01 修正）

v2 的致命矛盾：既要求「agent 第一个可编译提交即 push」保证中断韧性，又要求「只有控制器能 push」——**不可能同时成立**，因为控制器在 Workflow 返回前无法介入。修正为**多次 Workflow 调用，控制器在段间接管**：

```
段 1  Workflow scrollz-propose
        扫描（4 lens 并行 finder）→ JS 去重 → 3 judge 对抗裁决（可跨模型 agentType）→ 排序选一
        ↓ 返回结构化候选，不产生任何外部副作用
控制器  建 Issue（natural key 幂等）→ 冻结 attempt_id → 提案卡 docs/proposals/<issue>-<slug>.md
        → 提交并推 main → 建分支 + worktree + .harness-owner → 写 intent receipt
        ↓                                    【Stage 1 到此结束】
段 2  Workflow scrollz-implement（在既有 worktree 内，TDD）
        只允许 commit，**不允许 push**；到达 checkpoint 即返回
        ↓
控制器  校验 commit（diff 路径白名单 + 红线 gate + 属主）→ push → 更新 last_checkpoint 收据
        ↓ 未完成则回到段 2 下一段（多次调用，而非一次后台 Workflow 内部回调）
段 3  Workflow scrollz-review（未参与实现的 reviewer，可跨模型）
        ↓
控制器  验证测试 receipt → gh pr create（含 Closes #N、被测 SHA、命令与退出码、测试数/skip 数、触碰面、评审结论）
        → label 迁移 → 写收据 → 结算预算与记账
```

单轮 agent ≤ 12（工具建议 <15，并发上限 16）。

### Phase C · 记账退出

轮次账字段：`round_id` / mode / issue / attempt / workflow run id / duration / turns / cost / 工具拒绝次数 / result / exit code / last_checkpoint。

## 八、收尾模式

1. 专用 clone `git pull`（merge，不 rebase）。
2. 更新 [ROADMAP.md](../ROADMAP.md) / [CHANGELOG.md](../CHANGELOG.md) / [TRACKING.md](../TRACKING.md)。
3. 提案卡移入 `docs/proposals/archive/`。
4. **校验 owner marker 后**删远端分支与 worktree；marker 不符则跳过并告警。
5. 高风险 PR（并发/崩溃安全/磁盘格式）派一次合并态评审；发现问题**开新 Issue**，不当场改。
6. **经验只产出候选文档或 Issue**：无人值守收尾**禁止**修改 `.claude/skills/`、`.claude/workflows/`、permissions、hooks、systemd 单元、全局 memory——这些是可执行策略，等于让系统按自己的输出重写自身控制逻辑；此类改动必须走独立 PR 由用户合并。

## 九、权限、凭据与确定性纵深防线

### 9.1 会话隔离（对抗作用域合并，评审 R2-04）

- 固定 `--setting-sources` 或专用 `--settings .claude/harness-settings.json`，**阻断用户级 332 条 Bash 授权、hooks 与 plugin 能力**进入 harness 会话。
- `--disallowedTools "mcp__*"`：设计不依赖任何 MCP，且 headless 下 MCP 本就可能不可用。
- `--permission-mode dontAsk` + 最小 allow；**禁止 `bypassPermissions`**。
- deny 至少覆盖：`git push*`、`git remote *`、`git config *`、`gh *`、`systemctl *`、`fusermount*`、生产路径写入。仅禁 force push 远远不够。
- **凭据清场**：agent 进程显式清除 `GH_TOKEN`/`GITHUB_TOKEN`/`SSH_AUTH_SOCK`/git credential helper 环境，设 `GIT_TERMINAL_PROMPT=0`。否则 systemd user 环境里现存的 `SSH_AUTH_SOCK` 足以让 agent 直接推 main。
- `--max-turns`、`--max-budget-usd`、`--output-format stream-json` 固定给值；脚本内再用 `budget.remaining()` 二次收敛。
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
5. **branch protection 激活收据**（评审 R2-11）：远端 main 现无保护，且专用 PAT 无 Administration 权限，无法自建或自查。需仓库 owner 一次性配置并留收据（保护已启用、required checks 精确名称、是否要求 up-to-date、是否禁 force push 与删除）；控制器每轮只读校验仍符合收据，不符即暂停自动开发。**若用户决定不用 branch protection，文档必须把「required」改称「控制器的 merge-readiness 条件」，不得混用术语。**

**测试 receipt 硬要求**：本项目 FUSE 测试在缺 `/dev/fuse` 或 `fusermount` 时会打印 SKIP 后**成功返回**——「cargo test 绿」不证明挂载路径跑过。receipt 必须含被测 head SHA、命令、退出码、测试数、**skip 数**、以及「真实 FUSE 路径确实执行」的正向证据；skip 超阈值即判证据不足。

CI 分层预期：L0 `fmt`/`clippy`/`build`；L1 不需 FUSE 的测试；L2 需 `/dev/fuse` 的挂载测试（视 PoC）；L3 systemd/dm-* 留本地由 launcher 产出 receipt。边界结论写入 `docs/harness/ci-boundary.md`。

## 十二、队列治理

- **软配额而非硬配额**（评审 R2-09）：候选分 `roadmap`/`defect`/`perf`/`hygiene` 四 lane，按 rolling N 次**实际开发选择**计算，且只对**当前 eligible** 候选生效（blocked、needs-decision、与在飞 PR 冲突、oracle 不成立者不参与）。欠额只**提高权重**不强制选择；只扫描期间只累计 deficit，不虚构选中。
- **aging**：排队越久权重越高；队列治理必须能把 stale 项迁 `superseded`/`blocked` 释放容量，避免旧而不适用的候选永久占位。
- **去重分两级**（评审 R2-08）：精确 operation/proposal ID 可硬去重；**fingerprint 只能产出 `possible_duplicate`**，交确定性字段与 judge 复核——fingerprint 的「规范化目标/不变量」由不可信模型生成，既可能误碰撞也可能被轻微改写绕过。
- **拒绝可复议**：rejected 记录必须带 `reconsider_when` 与决定版本；条件满足、关联代码 SHA 变化或用户裁定变化后**自动失效**，不得成为永久去重键。保留人工 override 与 `supersedes` 关系。
- **质量指标**：合并率、拒绝率、重复率、revert 率、首次评审通过率、proposal→PR 周期、各 lens 有效率；跌破门槛自动降级只扫描或暂停请求复核。

## 十三、可观测性、失败预算与熔断

- 预算三档：per-round、per-day、rolling-24h，均带 §七 的**事前预留**。
- 熔断：同类错误连续 N 次、日预算耗尽、质量指标跌破门槛 → 切 `paused`。
- 告警：专用哨兵 Issue + systemd `OnFailure`；stale `picked`/`in-pr` 超时告警；预检失败必须列明缺失项（如 PATH 缺 `claude`）并触发告警，而非静默重试。
- 人工开关：`harness:paused` 哨兵 Issue 存在即暂停；提供只读诊断命令。

## 十四、验收判据

### 14.1 状态派生函数

property-based test 穷举 §5.1 六维度全组合，断言每组合恰好命中一条判定、无重叠无遗漏，非规范组合唯一落到 `needs-human-reconciliation`。

### 14.2 崩溃点矩阵（从控制器的 operation 清单**自动生成**，非手写）

每个 operation 至少四个崩溃点：`before-call` / `server-applied-response-lost` / `after-response-before-ledger` / `after-ledger`。覆盖对象必须包含：建 Issue、提案卡 commit、push main、建 worktree、写 `.harness-owner`、实现 commit、feature push、开 PR、label 迁移（含 replace-all 响应未知）、写 receipt、写 outbox/ledger，以及**收尾流程的独立矩阵**（关 Issue、删远端分支、删 worktree、文档 commit、push main、receipt）。断言不止「不重复」，还要「重启后最终状态一致」。

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
| R1 C-05 专用 clone | 采纳（§四） | Stage 1 |
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

## 十六、开放项（实施期确认，不阻塞本 spec）

- `claude -p` 后台等待的真实上限与退出码形态——Round 0 实测。
- 具体数值：每轮 token/美元预算、重试次数、熔断阈值 N、队列上限、skip 数阈值、lane 配额窗口 N——先给保守硬上限，实测后只调优不新建。
- 控制器实现语言：shell 脚本 vs 小型 Rust bin（后者可复用 workspace 与类型化 GitHub 客户端）——由 plan 定。
- outbox 存储：SQLite WAL vs append-only journal + fsync + checksum——需求是 durable intent + 原子更新 + 崩溃恢复，技术选型由 plan 定。
