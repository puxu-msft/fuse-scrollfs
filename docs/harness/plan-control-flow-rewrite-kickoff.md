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
- **本次实施计划**：`docs/harness/plan-control-flow-rewrite.md`（v4，经三轮跨模型对抗评审 `cfr-01`–`cfr-19`/`cfr2-01`–`cfr2-10`/`cfr3-01`–`cfr3-03` 处置后修订，9 个 Phase + 若干细分任务，回答怎么做；文首有三份「评审处置台账」（第一/二/三轮）与「开放发现处置表」，文末有执行状态表。**v3 起全篇代码块降级为「接口契约+不变量+测试清单」形式，不再提供完整可执行函数体**——实施者需要按 TDD 自行写出最小实现，不能从文档里复制代码；**第三轮评审确认这一降级方向正确**，新引入缺陷数从上一轮的 4 降为 0）
- **评审报告**：`docs/harness/plan-control-flow-rewrite-review.md`（第一轮，10 Critical / 8 Important / 1 Minor）、`docs/harness/plan-control-flow-rewrite-review-2.md`（第二轮，关闭 9 条、新引入 4 类设计缺陷、指出处置表存在自填假声明）、`docs/harness/plan-control-flow-rewrite-review-3.md`（第三轮，关闭 7 条、因契约降级丢失 3 条细节、新引入缺陷归零）——全部已处置；实施时若发现计划与报告的处置描述有出入，以计划正文的最新版本为准，评审报告本身不再更新

**执行方式**：用 `superpowers:subagent-driven-development`，一个任务派一个全新 subagent，任务之间由主会话评审。每个任务严格按计划里的 TDD 步骤走：写失败测试 → 跑到确认失败 → 写最小实现 → 跑到通过 → **正控**（临时还原实现确认测试真会红——**方向必须是"变红"，不是"仍然通过"**，这是 v2 修订专门订正过的一处常见误区）→ 提交。

**开工前必须知道的十五件事**（v4 相对 v3 新增三条，第 13/14/15 项）：

1. **主工作树可能有他人未提交改动**。提交一律用 `git commit -m <msg> -- <本任务的路径>` 限定路径，绝不 `git add -A`。
2. **零第三方依赖**。只用 Python 3 标准库（`unittest`/`sqlite3`/`subprocess`/`concurrent.futures`/`uuid`/`re`/`threading`/`dataclasses`），不建 venv、不装 pip 包。测试跑法：`cd /home/xp/src/zipfs/.claude/scripts && /home/linuxbrew/.linuxbrew/bin/python3 -m unittest discover -s harness/tests -t .`（当前基线 304 个测试全绿，任何时候都不得让它变红）。
3. **绝对路径**（systemd 的 user PATH 不含这些目录）：`python3`=`/home/linuxbrew/.linuxbrew/bin/python3`、`claude`=`/home/xp/.local/bin/claude`、`gh`=`/usr/bin/gh`、`git`=`/usr/bin/git`。
4. **Phase 1–7 不产生任何外部副作用**，全部用假件（fake `invoke_fn`，且**必须接受真实 `RoleInvocationRequest` 类型**，不是宽松 `**kwargs`——这是 v2 修订的核心要求之一，测试替身用宽松签名会让生产代码与测试代码之间的参数不匹配被完全掩盖）测试，可以放手做。**Phase 0（会话原语真机验证）与 Phase 8（真机切换验收）是仅有的两个付费真机阶段**，必须逐步执行、每步之间停下确认，不要连跑。
5. **不改动的模块白名单**：`outbox.py`、`budget.py`、`queue.py`（只能新增不能改现有实现）、`publish.py`、`lifecycle.py`、`gitops.py`、`precheck.py`、`db.py`（只能新增 `CREATE TABLE IF NOT EXISTS`）。发现"必须改这些才能完成任务"，先停下来问，不要默默改。
6. **候选 DTO 契约不变**——`round.py` 现有的 `_REQUIRED_CANDIDATE_FIELDS` 等校验常量、`Publisher.publish()` 的调用方式全部不动。扇出重写只改"候选怎么产生"，不改"候选长什么样"和"候选之后发生什么"。
7. **Phase 6（`round.py` 接线）是唯一一次性改动 `round.py`/`cli.py` 与工具集的阶段**。Phase 0–5 只新增文件（`session_identity.py`/`ledger.py`/`fanout_schema.py`/`prompts.py`/`fanout.py`/`role_invocation.py`），不碰 `round.py`/`cli.py`；`STAGE1_ALLOWED_TOOLS` 收窄（从六项到 `{Read,Grep,Glob}`）、`round.py` 接入新扇出逻辑、`cli.py` 构造真正执行 `to_invoke_kwargs()` 展开的调用适配闭包，三者必须在同一个提交里原子完成，中间不允许出现"工具集已改但调用路径未改"或"`RoleInvocationRequest` 定义了但生产代码仍把它整体塞进 `invoke()` 第一个形参"的过渡态（后者正是第二轮评审 cfr2-01 指出、第一轮 v2 修订未能真正闭合的缺陷）。Phase 6 同时要求把 `round.py` 现有的结算分支（`_settle_failed`/`_capability_drift_problems`）从"读单一 `InvocationResult`"改为"读 `fanout.FanoutSettlement` 聚合结果"——**这是本计划里改动最集中、最容易出接缝错误的一步**，实施顺序要求"先想清楚结算分支怎么改，再写调用段"，不要先切换调用路径再回头发现下游读的是一个已经不存在的变量。
8. **两个待决项已给出推荐方案，需要 Phase 0 实测验证而非直接采信**：(A) 单发 `-p` 模式是否支持 `--resume --fork-session`（若不支持需转向 dual-pipe，计划里已写好备选路径）；(B) Stage 1 只读工具是否触发 `can_use_tool`（预期不会，Phase 0 Task 0.2 给出实测结论——**注意验证时必须真正打开 `--permission-prompt-tool stdio` 开关**，不打开开关就断言"没看到 control_request"是无效证据，这是 v1 曾经犯过的错误）。这两项完成前，Phase 2 之后的具体实现细节可能需要调整，不要在 Phase 0 结论出来之前就假设推荐方案成立并跳着做后面的 Phase。
9. **Phase 5 的并发实现有严格的线程安全要求**：worker 线程（`ThreadPoolExecutor` 里跑的那些）**只允许返回纯数据的 `AttemptRecord`**，绝不允许直接访问 SQLite `conn`（`db.connect()` 用默认 `check_same_thread=True`，跨线程访问会直接抛异常）——账本写入必须延后到主线程收集完一波结果之后再做，且调用点须显式 `try/except` 包裹（写失败不阻断本轮，这个容错职责在调用方，`ledger.py` 本身不吞异常）。预算也必须通过 `fanout.BudgetTracker`（`threading.Lock` 保护的原子预留）分配，不能用"读一个可能过期的剩余值再决定是否发起调用"这种存在竞态的写法；`settle()` 必须允许结算后余额变负（超额成本要真扣，不能用 `max(...,0)` 把超支抹平）。
10. **`AttemptRecord` 有 `retryable`/`resumable` 两个独立布尔位，不能混为一谈**：`retryable` 回答"值得再试一次吗"，`resumable` 回答"能安全 fork 续接吗"——后者只有在 `InvocationResult.session_id` 真正被 CLI 报告过时才为真，绝不能用预分配的派生 session_id 冒充"已确认存在的会话"去发起 `--resume`（第二轮评审 cfr2-07 指出的具体缺陷：超时或进程未及 `init` 就被杀时，这个会话可能从未被 CLI 创建过）。`retryable=True` 但 `resumable=False` 时必须发起全新（非 fork）尝试。
11. **judge 的 task identity 必须携带 candidate 的 fingerprint，不能用静态角色字符串**：`judge:redline`/`judge:completed`/`judge:oracle` 这类静态字符串在"一轮扇出裁决多个候选"的场景下会导致第二个候选的账本主键与会话身份撞上第一个候选（第二轮评审 cfr2-02 指出的具体缺陷）。正确形式是 `f"judge:<type>:<fingerprint>"`（`fingerprint` 来自 `queue.fingerprint()`），且这个 task identity 必须在 session identity 派生、账本主键、`fanout.py` 内部的角色 dict key 四处统一使用同一个字符串，不能各自拼接。
12. **Phase 8 的 fork 重试真机验证必须基于真实创建的 CLI session**：不能用一个从未真正调用过 `claude` 的假失败结果去验证"fork 能否恢复"——`--resume` 一个从未被 CLI 创建过的 session ID 行为未定义，这样的验证不能证明 ADR D2 要求的能力。Task 8.4 已给出具体做法（借用 PoC `driver.py` 的 `interrupt()` 方法真实中断一个在跑的会话）。**探针必须注入专用的 `payload_parser`**（第二轮评审 cfr2-06 指出：默认 parser 都要求 JSON 结构，而探针要求模型输出纯文本暗号，会被默认 parser 直接拒绝；断言点是探针专用 parser 产出的 `payload["raw_text"]`，不是不存在的 `result.result` 字段）。
13. **`RequestContext` 是生产环境 `cwd`/`settings_path`/`model`/`stream_log` 取值的唯一来源**（第三轮评审 cfr3-01 指出的具体缺陷）：Phase 6 Task 6.1 的 `_build_request_context(cfg)` 是整个计划里"这些值生产环境该等于什么"的唯一构造点，`fanout.py` 的 `_make_request` 只消费它，不允许重新硬编码一份。judge task identity（含 fingerprint）必须同时贯穿 session ID、账本 `attempt_key`、`stream_log` 路径三处，且三者共用同一个拼接模板（`role_invocation.build_stream_log_path` 与 `ledger` 的 `attempt_key` 逐字一致）——只验证 session ID 不同是不够的，`attempt_key`/`stream_log` 完全可能因为各自独立实现而遗漏 fingerprint。
14. **`AttemptRecord.retryable` 的赋值必须来自可测试的终态分类表，不是"是否等于 capability_drift"这种二元判断**（第三轮评审 cfr3-02 指出：三轮评审反复指出这个问题，直到本轮才要求真正的分类表）：`error_max_budget_usd`（预算耗尽）与确定性协议异常（同一次调用内出现重复 `init`/重复 `result` 事件）**不可重试**——前者重试大概率再次撞线，后者是这次调用产出的 stream 结构本身已经损坏，不是随机的网络抖动。只有真正的传输抖动（超时、`API Error: Server error` 一类）与 schema 校验随机失败才可重试。`InvocationResult.subtype`（Task 2.1 新增）是做这个判断的依据，务必透传到 `AttemptRecord`。
15. **开放发现处置表宣称的修复必须真正体现在对应任务的不变量与测试清单里，不能只停留在处置表的文字描述**（第三轮评审 cfr3-03 指出的具体缺陷）：`record_degraded` 双写 `agentType`（Task 5.2）、`FanoutSettlement.protocol_errors` 聚合（Task 5.6）、`round.py` 消费 `_format_detail` 与显式设 `model=DEFAULT_AGENT_MODEL`（Task 6.1）——这三项此前都只在处置表里"宣称"，本轮已补齐对应任务的测试断言，实施时必须真正写出这些测试，不能假设处置表说过就等于任务已覆盖。

**开工前须知的一项非阻塞提醒**：`claude_runner._persist_stream()` 的 stream 落盘权限已在 Task 2.1 收紧为 `0o600`（rmf-08 部分处置），但脱敏与轮转/保留策略仍明确延后（暴露面因扇出改动放大 7 倍，已量化说明），不在本次实施范围内，不要顺手展开这两项设计。

**完成的标志**：Phase 8 全部走完，`agent_attempts`/`invocations` 表在真机验证轮次里有对应记录，probe 负向验证工具集恰为三项，至少一次完整扇出真机跑通并正确判定结果，fork 重试路径至少一次真机复现（基于真实创建的 session）。systemd timer **仍保持 disabled**（是否启用是用户裁决范围外的下一步，本计划不做）。

**每完成一个任务**：更新 `plan-control-flow-rewrite.md` 文末执行状态表的该行（状态 + 验证证据 + 偏差），与代码一起提交。**注意 v4 的任务编号相对 v3 不变，但 Task 2.1/2.3/5.2/5.3/5.5/5.6/6.1 补充了新的不变量与测试清单条目**（见执行状态表开头说明与「评审处置台账（第三轮）」），实施前请先读一遍执行状态表的开头说明与三份评审处置台账，确认理解每个任务具体改了什么，不要假设 v3 的契约（更不用说 v2 的完整代码草图）已经足够或可以原样照抄。

**遇到分叉停下来问，而非自行决定的情形**：
- Phase 0 的两个待决项实测结果与推荐方案不符。
- 任何一个"不改动模块白名单"里的文件被发现必须改动才能完成任务。
- 真机验证（Phase 8）中出现任何计划未预见的行为（例如 fork 后上下文丢失、成本远超预期）。
- Phase 6 接线时发现 `round.py` 还有本计划未列出的 `invocation.*` 引用点需要改成 `settlement.*`（`cfr-04` 要求逐一核对，若发现计划遗漏了某个引用点，先按同样模式改掉并在执行状态表的「偏差」列记录，不必停下来问，但要如实记录不能悄悄绕过）。

