# 控制流重写计划 · 跨模型对抗性评审

> report_id: `cfr` · reviewed_at_rev: `2e8fbda2869c4c2b090db678faafbdd0aca32f71`
> 评审者：`gpt-souls:reviewer`（跨模型第二双眼睛——计划由 Claude soul 撰写）· 日期：2026-07-31
> **verdict: needs-fix** —— Critical 10 · Important 8 · Minor 1 · 合计 19

> **落盘说明**：评审者报称其运行时角色约束禁止创建文件，仅以内联 JSON 交付。本文件由协调者代为固化，内容逐条转录自该 JSON，未作删改；转录本身未经评审者复核。

## 三条最关键的发现

1. **所有 judge 调用都会被现有 `_extract_payload()` 拒绝**：它强制顶层含 `candidates:list`，而 judge 返回 `{verdict,reason,...}`。judge 会重试三次后全部降级否决，**完整扇出不可能成功发布**。
2. **Phase 5/6 的调用适配器与真实 `claude_runner.invoke()` 不匹配**：缺必需的 `cwd`/`timeout_s`，默认传空 `settings_path`，未传规范 `model` 与 `stream_log`，生产分支也没注入 `_role`。首个子调用必然失败。
3. **并发 finder 把同一个默认 SQLite connection 传进四个线程**并在线程内写账本。已独立复现：标准 `sqlite3` 立即抛 `ProgrammingError`。

## 逐条发现

### cfr-01 — Critical

```
finding_id: cfr-01
severity: Critical
primary_location: plan:487-491
related_locations: claude_runner.py:184-214
evidence_status: verified
```

**问题** —— 计划新增 finder/judge 专用 schema 校验，却没先让调用层接受 judge 的 JSON 形状。现有 `_extract_payload()` 只接受含 `candidates:list` 的对象。

**失败场景** —— `judge:redline` 正确返回 `{verdict:'pass',...}`，`parse_stream_json()` 仍记 `unparseable or malformed payload` 并令 `ok=False`；`validate_judge_output()` 根本没有执行机会。**所有 judge 调用都会重试三次后全部降级否决，完整扇出不可能成功发布。**

**修复建议** —— 让 `invoke()` 接受可注入的 payload parser，或先提供通用 JSON-object parser，再由 finder/judge 各自 schema 校验。必须加真实 `parse_stream_json → judge validator` 接缝测试。

### cfr-02 — Critical

```
finding_id: cfr-02
severity: Critical
primary_location: plan:1289-1337
related_locations: claude_runner.py:399-403
evidence_status: verified
```

**问题** —— Phase 5 草图调用真实 `invoke()` 时缺必需参数 `cwd`/`timeout_s`；`settings_path` 默认空串；没传 `model=DEFAULT_AGENT_MODEL` 和每子调用独立 `stream_log`。Phase 6 wrapper 强制 `pop('_role')`，但 `agents is not None` 的生产分支从未加入 `_role`。

**失败场景** —— 首个子调用必然失败：`_invoke_and_record` 因缺 `_role` 抛 `KeyError`；修掉后空 settings 触发 `UnsafeInvocationError`，或缺 `cwd`/`timeout_s` 触发 `TypeError`。

**修复建议** —— 定义完整单一的 `RoleInvocationRequest` 或显式 adapter。Phase 5 单测必须用**真实 `invoke` 签名**的 fake，而不是接受任意 `**kwargs` 的宽松替身。

### cfr-03 — Critical

```
finding_id: cfr-03
severity: Critical
primary_location: plan:1556-1560
related_locations: db.py:77-83
evidence_status: verified
```

**问题** —— 四个 finder 在线程池中共享 `deps.conn`，并由工作线程调用 `ledger.record_attempt_*()`。SQLite connection 用默认 `check_same_thread=True`。

**失败场景** —— **已独立复现**：主线程创建的 connection 在工作线程执行任意 SQL 立即抛 `sqlite3.ProgrammingError`。异常经 `future.result()` 穿透，整轮进 `unhandled-exception`，一个 finder 都不会真正启动。

**修复建议** —— 不要跨线程传 connection：每 worker 独立 connection，或 worker 只返回事件、主线程串行落账。若选多连接，还要处理 WAL 写竞争与 `busy_timeout`。

### cfr-04 — Critical

```
finding_id: cfr-04
severity: Critical
primary_location: plan:1817-1855
related_locations: round.py:385-389,443-498
evidence_status: verified
```

**问题** —— Phase 6 删除了单一 `invocation`，却要求其后的结算与发布代码逐字保留。当前下游多处仍读 `invocation.cost_usd`/`turns`/`denials`/`exit_code`，`_settle_failed()` 也只接受一个 `InvocationResult`。

**失败场景** —— 扇出返回后，任何 `no-candidate`、非法候选、`duplicate` 或 `published` 分支首次访问 `invocation` 即抛 `NameError`。即便机械改成总成本，仍缺 turns/denials/exit code/未知成本/多子调用失败的聚合规则。

**修复建议** —— 为 `run_fanout()` 定义正式聚合结果（实际成本、未知成本预留、turns、denials、退出状态、protocol errors、capability drift）。**先改造所有结算分支消费该结果，再切换调用路径。**

### cfr-05 — Critical

```
finding_id: cfr-05
severity: Critical
primary_location: plan:1737-1740
related_locations: budget.py:137-166
evidence_status: verified
```

**问题** —— 没实现 spec 要求的 `outstanding_worst_case`。并发调用启动前不原子预留 grant，且 `_invoke_and_record()` 无论 `cost_known` 真假都只记 `cost_usd`。超时/断流通常 `cost_known=False,cost_usd=0`，剩余预算完全不缩减并继续重试。而 `--max-budget-usd` 已被 PoC 证明会滞后超额。

**失败场景** —— 四个 finder 同时读到相同剩余预算后各自启动；其一无终态 result、账本记 0，随后 fork 重试再次获 grant。实际首调用可能已花钱，控制器却按未花费处理，**可突破 per-round 预留**。

**修复建议** —— 调用前原子登记 grant 并计入 outstanding；终态成本已知后用实际值结算，未知时继续按 grant 占用。并发 grant 之和须在一个事务内受剩余预算约束。删除 `max(...,0.01)` 这种余额为 0 仍发调用的逻辑。

### cfr-06 — Critical

```
finding_id: cfr-06
severity: Critical
primary_location: plan:1841-1845
related_locations: round.py:182-199
evidence_status: verified
```

**问题** —— 计划声称 capability drift 检查随 `parse_stream_json()` 自动下沉，但解析器只收集 init 信息，**不比较**实际工具/MCP/插件/错误。真正的 fail-closed 比较在 `round._capability_drift_problems()`，Phase 6 删除了对它的调用。

**失败场景** —— 某子调用 init 意外出现 Bash、MCP 或插件时，只要终态 JSON 合法，`ok` 仍可为真并被 fanout 接受，**破坏现有权限成功谓词**。

**修复建议** —— 把 capability 检查移入 runner 的 Stage 1 调用入口，或让 fanout 对每个子结果调用同一检查并把漂移视为不可降级的整轮失败。加一个「子调用 init 多出 Bash」的端到端测试。

### cfr-07 — Critical

```
finding_id: cfr-07
severity: Critical
primary_location: plan:1593-1609
related_locations: round.py:432-450
evidence_status: verified
```

**问题** —— judge 降级只转成局部 reject verdict，没有写入 `run_fanout()` 顶层的 `degraded` 数组——该数组只来自 `run_finders()`。

**失败场景** —— finder 找到候选、redline judge 三次失败返回 `judge-unavailable`，候选进 rejected，但顶层 `degraded=[]`、`candidates=[]`。round 再次记成干净的 `no-candidate`——**精确复发 rmf-03**。

**修复建议** —— 让 judge 返回或写入统一 degraded sink，并加「finder 正常 + redline 全失败 → `no-candidate-degraded`」的 round 接缝测试。

### cfr-08 — Critical

```
finding_id: cfr-08
severity: Critical
primary_location: plan:1861-1871
related_locations: —
evidence_status: verified
```

**问题** —— Task 6.1 明确修改 `claude_runner.py` 的工具集与 `fanout.py` 的四层函数签名，但提交命令**完全没有包含这两个文件**，也没包含相应测试。

**失败场景** —— 按计划提交后，checkout 出来的 commit 中 round 传入 `deadline_monotonic`/`single_call_cap_usd`，已提交的 fanout 却不接受；`STAGE1_ALLOWED_TOOLS` 仍是六项。工作区可能暂时全绿，但**提交本身不可运行**。

**修复建议** —— 提交路径必须包含所有实际修改文件，并在干净临时 worktree 上 checkout 该提交重跑全套测试。**不要用工作区绿替代提交态绿。**

### cfr-09 — Critical

```
finding_id: cfr-09
severity: Critical
primary_location: plan:1740
related_locations: round.py:24-29,347-357
evidence_status: verified
```

**问题** —— 用 `max(deadline-now-reserve, minimum_timeout)` 计算子调用超时，会在截止已耗尽时**反向放大**剩余时间——重新引入当前 round.py 已专门修掉的 deadline bug。

**失败场景** —— 剩余时间为负时 `max()` 仍返回正的最小超时并启动 Claude；systemd 可在账本与 outbox 清理前硬杀控制器。

**修复建议** —— 先判断剩余时间是否足够覆盖 cleanup reserve 与最小调用窗口，不足则不启动。只有通过该门后才把正的剩余值传给 invoke，**不能用下限修补负值**。

### cfr-10 — Critical

```
finding_id: cfr-10
severity: Critical
primary_location: plan:2030-2036
related_locations: exp/stdio-driver/CONCLUSIONS.md:63-81
evidence_status: verified
```

**问题** —— Phase 8 的 fault injection 在 attempt 1 不启动真实 Claude 而直接构造失败结果，却随后要求 `--resume` 该 attempt 的 session——**该 session 从未被 CLI 创建**。

**失败场景** —— attempt 2 对不存在的 session ID 执行 resume/fork，无法验证真实 fork 恢复；即便测试看到两个账本行，也只证明 fake 状态机能走，不证明 CLI 能恢复。

**修复建议** —— 先真实创建 attempt 1 session，再在控制器层把成功结果改判为需重试；更忠实的方案是复用 PoC 的 stdio interrupt。验收 oracle 必须核对新 session ID 与恢复的上下文。

### cfr-11 — Important

```
finding_id: cfr-11
severity: Important
primary_location: plan:1331-1361
related_locations: —
evidence_status: verified
```

**问题** —— fork attempt 的账本行在调用前写入，此时唯一已知的 `session_id` 是父 session；调用返回新 child ID 后没有更新该行。因此 attempt 2 的 `session_id` 与 `parent_session_id` 都是父 ID。另有「ledger 写失败不阻断」的要求，草图无任何容错。

**失败场景** —— Phase 8 要求的谱系断言无法成立；普通 SQLite 写错误也会让审计辅助表变成整轮硬依赖。

**修复建议** —— attempt 2 启动时只登记 parent 与 planned attempt，调用返回后原子补 child session ID；或延迟 insert。明确处理 ledger 写失败并保留可见告警。

### cfr-12 — Important

```
finding_id: cfr-12
severity: Important
primary_location: plan:901-906
related_locations: —
evidence_status: verified
```

**问题** —— 计划宣称按 wave 重试、用其它角色耗时形成自然退避，但每个 worker 在 `run_role_with_retry()` 内**立即连续**执行 attempt 1/2/3。`_is_transport_like()` 恒真且未被调用，所有返回型失败都会重试。

**失败场景** —— 四个角色各自在同一故障窗口连续重试三次；`error_max_budget_usd`、确定性协议错误等也会被当成传输故障继续消耗。

**修复建议** —— 把 attempt 循环提升到 fanout wave 层：每波每角色最多一次，收集失败后再进下一波。定义并测试 retryability 分类，保留「模型 schema 输出随机失败可重试」这一例外。

### cfr-13 — Important

```
finding_id: cfr-13
severity: Important
primary_location: plan:620-657
related_locations: —
evidence_status: verified
```

**问题** —— finder validator 没拒绝顶层额外字段，未逐字迁移 JS 的 `additionalProperties:false`。且在确认 size/priority 是字符串前就做 set membership。

**失败场景** —— `{'candidates':[{'size':[],...}]}` 使 validator 抛 `TypeError` 并终止整轮，无法进入计划声称的 schema 失败重试路径。

**修复建议** —— 所有不可信字段先类型检查再枚举检查；顶层字段集合须恰为 `{'candidates'}`。为 unhashable enum 值与顶层额外字段加负测。

### cfr-14 — Important

```
finding_id: cfr-14
severity: Important
primary_location: plan:1477-1496
related_locations: code-review-realmachine-fixes.md:420-440
evidence_status: verified
```

**问题** —— rmf-12 的修复只迁了专有字段占位与 `degraded:true`，**没有迁移评审明确要求的 `skipped_judges`**。redline 短路后的拒绝仍无法表达判据面不完整。

**失败场景** —— Stage 1b 接入拒绝记忆时，会把仅有 redline 一票的记录当成完整裁决，错误推导永久拒绝或 reconsider 条件。

**修复建议** —— 在 rejected item 上记录 `skipped_judges`，并让 Stage 1b 明确禁止从不完整裁决推导永久拒绝。

### cfr-15 — Important

```
finding_id: cfr-15
severity: Important
primary_location: plan:110-120
related_locations: —
evidence_status: verified
```

**问题** —— 至少三处测试 oracle 与断言不匹配：Task 0.2 未给进程加 `--permission-prompt-tool stdio` 却要据 stdout 无 control_request 断言工具不触发；Phase 6 Step 7 明确要求 mutation 后测试**仍通过**（那不是正控）；Phase 8 fake fault 不创建可 resume session。

**失败场景** —— 三个测试都可能得到绿色或预期输出，但分别没有验证 can_use_tool 行为、短路接线和真实 fork。

**修复建议** —— Task 0.2 必须带 stdio permission flag 并提供 control response；Phase 6 应使用**会杀死目标集成测试**的 mutation；Phase 8 使用真实已创建 session。

### cfr-16 — Important

```
finding_id: cfr-16
severity: Important
primary_location: plan:893-906
related_locations: code-review-realmachine-fixes.md 多处
evidence_status: verified
```

**问题** —— 计划只声明吸收 rmf-07/10/12；对**仍开放且位于本次修改模块上**的 rmf-04、06、08、14、16、17、18 没有逐条处置。JS/TaskOutput 退役只能关闭其专属触发点，不能自动关闭 env allowlist、stream 权限/脱敏/轮转、协议错误持久化、CLI 版本漂移与 systemd 日志问题。

**失败场景** —— 新架构一轮产生最多七个 stream，却没有 per-call stream path；父环境的非 `CLAUDE_`/`ANTHROPIC_` 控制变量仍穿透；`_describe_degraded()` 读 `agentType/label`，而新记录只有 `role`，日志会显示 `?×N`。

**修复建议** —— 计划新增「开放发现处置表」，逐条写 adopted / superseded / deferred / 仍开放及理由。**本次直接修改的 runner/round 问题应在切换前修复，而不是靠旧资产删除推定消失。**

### cfr-17 — Important

```
finding_id: cfr-17
severity: Important
primary_location: plan:4
related_locations: plan-stage1b.md:21-27, spec.md:31-39
evidence_status: verified
```

**问题** —— 计划声称 Stage 1b 与现行 spec 不受影响，但删除 Workflow 与跨语言 canonical-key seam 直接改变了 Stage 1b B2 的数据流与「跨语言一致性测试」验收；现行 spec 仍把 Workflow 写成已裁定的编排载体与 Phase B 主路径。

**失败场景** —— 重写完成后，未来实施者按冻结的 Stage 1b 计划会继续尝试把 known keys 传给 Workflow 并要求 Node/Python 比对，而生产代码中已无该对象。

**修复建议** —— 保持 B1–B8 功能范围不削减，但同步修订 Stage 1b 的实现接缝与验收 oracle；同时更新 spec 的当前架构视图，明确 ADR-002 已取代旧 Workflow 决策。

### cfr-18 — Important

```
finding_id: cfr-18
severity: Important
primary_location: plan:1880-1960
related_locations: queue.py:17-42
evidence_status: verified
```

**问题** —— Phase 7 顺序自相矛盾：Task 7.1 已删 `scrollz-propose.js` 并提交，Task 7.2 却称新旧测试可同时为绿（因为 JS 到其 Step 3 才删）。另一新测试断言 `_norm('a\x1fb') == 'a b'`，而当前实现刻意按 ECMAScript 语义保留该控制字符。

**失败场景** —— Task 7.2 开始时旧跨语言测试已因 JS 不存在而红；新 normalization 测试也立即红。**独立实跑确认**当前 `_norm('a\x1fb')` 返回 `'a\x1fb'`。

**修复建议** —— 先在 JS 尚存在时写并验证继任测试，再在同一原子提交中删除 JS 接缝与旧测试。继任测试应**冻结当前 Python 规范化真值**，而不是悄悄改变 canonical-key 语义。

### cfr-19 — Minor

```
finding_id: cfr-19
severity: Minor
primary_location: plan:28
related_locations: kickoff:23
evidence_status: verified
```

**问题** —— Global Constraints 写「Phase 0–6 全部只用假件、不花真钱」，Phase 0 标题与任务却明确是真机付费探针；kickoff 又写 Phase 0 与 Phase 8 都花钱。

**失败场景** —— 实施者无法从计划正文判断 Phase 0 是否需要逐步确认与费用授权。

**修复建议** —— 改成「Phase 1–7 不花钱；Phase 0 与 Phase 8 为付费真机阶段」，并让主计划与 kickoff 使用同一句约束。

## 核实无误的部分（评审者说明了验证方式）

- PoC 的「一子任务一顶层 process/session」硬约束已在总体架构中正确吸收；计划没有重新采用 `Task` 做扇出。
- 计划文字明确承认 `--max-budget-usd` 是滞后停止触发器，也没把 `can_use_tool` 当完整权限边界；问题出在 Phase 6 实现没有兑现对应预算谓词。
- `queue.canonical_key` 实际是模块级函数，计划按该真实签名调用；`remember_canonical_key(fp,key)` 与 `known_canonical_keys()` 的参数形状也匹配当前代码。
- 基线测试已实跑：13 + 304 全部通过。**但这些绿色不能证明新路径已集成**——Phase 3–5 尚未接线时旧生产路径仍可运行。
- JS 侧四条核心语义在计划中均有明确迁移意图（最多三次尝试、同类 degraded 折叠、redline 先跑并短路、降级 judge 按否决处理）。但 judge degraded 的顶层可观测性与 wave 退避尚未正确迁移。

## 评审者自陈最不确定的三条判断

1. `cfr-10` 的 Critical 定级依赖对「构造失败结果时完全跳过首次 invoke」的字面理解；若实施者原意是先真实建立 session 再改判失败，可降级——但计划当前没写出这一步。
2. Read/Grep/Glob 在开启 stdio permission tool 后是否真实产生 `can_use_tool` **本次未付费验证**；能确认的是现有 Task 0.2 的 oracle 无法判定该断言。
3. Stage 1b 的跨语言测试应视为正式冻结验收还是随 ADR-002 自动失效，属文档治理判断；但无论如何，「完全不受影响」这一陈述已被具体的文件删除计划证伪。

