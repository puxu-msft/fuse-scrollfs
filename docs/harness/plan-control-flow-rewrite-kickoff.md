# 控制流重写（ADR-002 D0/D1/D2）Kick-off 提示词

> 复制以下整段作为新会话的第一条消息即可开工。

---

在 `/home/xp/src/zipfs`（crate 名 scrollz，远端 `puxu-msft/fuse-scrollfs`）实施 scrollz harness 的**控制流重写**：把「外层 `claude -p` 会话 → Skill → `Workflow` 工具后台起 7 个 agent → `TaskOutput` 阻塞回显」这条链，改为**控制器（Python）直接为每个 finder/judge 起一个独立顶层 `claude -p` 进程**。

**权威文档**（按此顺序读，冲突时以 ADR/计划为准）：
- 架构决策：`docs/harness/adr-002-control-flow-ownership.md`（已采纳，回答做什么/为什么）
- PoC 事实基础：`exp/stdio-driver/CONCLUSIONS.md`（四条结论全部 confirmed，是本计划的事实依据，不是推断）
- 现行规格：`docs/harness/spec.md`（§四三层信任模型、§六 outbox 事务协议、§七轮次流程仍然有效，本次不改，Phase 7 Task 7.5 会追加脚注说明 Workflow 段已被 ADR-002 取代）
- 现状与真机缺陷史：`docs/harness/HANDOVER.md`
- 最近一轮评审：`docs/harness/code-review-realmachine-fixes.md`（rmf-01/02/03/05/13 已修，其余仍开着；本计划顺手吸收 rmf-07/rmf-10/rmf-12/rmf-04/rmf-14/rmf-17 到新实现里，见「开放发现处置表」章节）
- **本次实施计划**：`docs/harness/plan-control-flow-rewrite.md`（v2，经跨模型对抗评审 `cfr-01`–`cfr-19` 处置后修订，9 个 Phase + 若干细分任务，回答怎么做；文首有「评审处置台账」与「开放发现处置表」，文末有执行状态表）
- **评审报告**：`docs/harness/plan-control-flow-rewrite-review.md`（GPT soul 对 v1 的对抗审查，10 Critical / 8 Important / 1 Minor，全部已处置——实施时若发现计划与此报告的处置描述有出入，以计划正文的最新版本为准，评审报告本身不再更新）

**执行方式**：用 `superpowers:subagent-driven-development`，一个任务派一个全新 subagent，任务之间由主会话评审。每个任务严格按计划里的 TDD 步骤走：写失败测试 → 跑到确认失败 → 写最小实现 → 跑到通过 → **正控**（临时还原实现确认测试真会红——**方向必须是"变红"，不是"仍然通过"**，这是 v2 修订专门订正过的一处常见误区）→ 提交。

**开工前必须知道的十件事**（v2 相对 v1 新增两条，第 9/10 项）：

1. **主工作树可能有他人未提交改动**。提交一律用 `git commit -m <msg> -- <本任务的路径>` 限定路径，绝不 `git add -A`。
2. **零第三方依赖**。只用 Python 3 标准库（`unittest`/`sqlite3`/`subprocess`/`concurrent.futures`/`uuid`/`re`/`threading`/`dataclasses`），不建 venv、不装 pip 包。测试跑法：`cd /home/xp/src/zipfs/.claude/scripts && /home/linuxbrew/.linuxbrew/bin/python3 -m unittest discover -s harness/tests -t .`（当前基线 304 个测试全绿，任何时候都不得让它变红）。
3. **绝对路径**（systemd 的 user PATH 不含这些目录）：`python3`=`/home/linuxbrew/.linuxbrew/bin/python3`、`claude`=`/home/xp/.local/bin/claude`、`gh`=`/usr/bin/gh`、`git`=`/usr/bin/git`。
4. **Phase 1–7 不产生任何外部副作用**，全部用假件（fake `invoke_fn`，且**必须接受真实 `RoleInvocationRequest` 类型**，不是宽松 `**kwargs`——这是 v2 修订的核心要求之一，测试替身用宽松签名会让生产代码与测试代码之间的参数不匹配被完全掩盖）测试，可以放手做。**Phase 0（会话原语真机验证）与 Phase 8（真机切换验收）是仅有的两个付费真机阶段**，必须逐步执行、每步之间停下确认，不要连跑。
5. **不改动的模块白名单**：`outbox.py`、`budget.py`、`queue.py`（只能新增不能改现有实现）、`publish.py`、`lifecycle.py`、`gitops.py`、`precheck.py`、`db.py`（只能新增 `CREATE TABLE IF NOT EXISTS`）。发现"必须改这些才能完成任务"，先停下来问，不要默默改。
6. **候选 DTO 契约不变**——`round.py` 现有的 `_REQUIRED_CANDIDATE_FIELDS` 等校验常量、`Publisher.publish()` 的调用方式全部不动。扇出重写只改"候选怎么产生"，不改"候选长什么样"和"候选之后发生什么"。
7. **Phase 6（`round.py` 接线）是唯一一次性改动 `round.py` 与工具集的阶段**。Phase 0–5 只新增文件（`session_identity.py`/`ledger.py`/`fanout_schema.py`/`prompts.py`/`fanout.py`/`role_invocation.py`），不碰 `round.py`；`STAGE1_ALLOWED_TOOLS` 收窄（从六项到 `{Read,Grep,Glob}`）与 `round.py` 接入新扇出逻辑必须在同一个提交里原子完成，中间不允许出现"工具集已改但调用路径未改"的过渡态。Phase 6 同时要求把 `round.py` 现有的结算分支（`_settle_failed`/`_capability_drift_problems`）从"读单一 `InvocationResult`"改为"读 `fanout.FanoutSettlement` 聚合结果"——**这是本计划里改动最集中、最容易出接缝错误的一步**，实施顺序要求"先想清楚结算分支怎么改，再写调用段"，不要先切换调用路径再回头发现下游读的是一个已经不存在的变量。
8. **两个待决项已给出推荐方案，需要 Phase 0 实测验证而非直接采信**：(A) 单发 `-p` 模式是否支持 `--resume --fork-session`（若不支持需转向 dual-pipe，计划里已写好备选路径）；(B) Stage 1 只读工具是否触发 `can_use_tool`（预期不会，Phase 0 Task 0.2 给出实测结论——**注意验证时必须真正打开 `--permission-prompt-tool stdio` 开关**，不打开开关就断言"没看到 control_request"是无效证据，这是 v1 曾经犯过的错误）。这两项完成前，Phase 2 之后的具体实现细节可能需要调整，不要在 Phase 0 结论出来之前就假设推荐方案成立并跳着做后面的 Phase。
9. **Phase 5 的并发实现有严格的线程安全要求**：worker 线程（`ThreadPoolExecutor` 里跑的那些）**只允许返回纯数据的 `AttemptRecord`**，绝不允许直接访问 SQLite `conn`（`db.connect()` 用默认 `check_same_thread=True`，跨线程访问会直接抛异常）——账本写入必须延后到主线程收集完一波结果之后再做。预算也必须通过 `fanout.BudgetTracker`（`threading.Lock` 保护的原子预留）分配，不能用"读一个可能过期的剩余值再决定是否发起调用"这种存在竞态的写法。
10. **Phase 8 的 fork 重试真机验证必须基于真实创建的 CLI session**：不能用一个从未真正调用过 `claude` 的假失败结果去验证"fork 能否恢复"——`--resume` 一个从未被 CLI 创建过的 session ID 行为未定义，这样的验证不能证明 ADR D2 要求的能力。Task 8.4 已给出具体做法（借用 PoC `driver.py` 的 `interrupt()` 方法真实中断一个在跑的会话）。

**完成的标志**：Phase 8 全部走完，`agent_attempts`/`invocations` 表在真机验证轮次里有对应记录，probe 负向验证工具集恰为三项，至少一次完整扇出真机跑通并正确判定结果，fork 重试路径至少一次真机复现（基于真实创建的 session）。systemd timer **仍保持 disabled**（是否启用是用户裁决范围外的下一步，本计划不做）。

**每完成一个任务**：更新 `plan-control-flow-rewrite.md` 文末执行状态表的该行（状态 + 验证证据 + 偏差），与代码一起提交。**注意 v2 的任务编号相对 v1 有变化**（Phase 2 新增 Task 2.2/2.3，原工具收窄任务改号为 2.4；Phase 5 从 5 个任务扩为 6 个；Phase 7 新增 Task 7.5，且 Task 7.1/7.2 的执行顺序对调），实施前请先读一遍执行状态表的开头说明，确认对照的是当前编号。

**遇到分叉停下来问，而非自行决定的情形**：
- Phase 0 的两个待决项实测结果与推荐方案不符。
- 任何一个"不改动模块白名单"里的文件被发现必须改动才能完成任务。
- 真机验证（Phase 8）中出现任何计划未预见的行为（例如 fork 后上下文丢失、成本远超预期）。
- Phase 6 接线时发现 `round.py` 还有本计划未列出的 `invocation.*` 引用点需要改成 `settlement.*`（`cfr-04` 要求逐一核对，若发现计划遗漏了某个引用点，先按同样模式改掉并在执行状态表的「偏差」列记录，不必停下来问，但要如实记录不能悄悄绕过）。

