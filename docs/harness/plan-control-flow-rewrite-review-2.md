# 控制流重写计划 · 第二轮复审（原评审者，跨模型）

> report_id: `cfr2` · reviewed_at_rev: `83362418ab81f10785741b263b97b28fedfeb569`
> 评审者：`gpt-souls:reviewer`（**同一位**，带上一轮上下文复审自己提的问题）· 日期：2026-08-01
> **verdict: needs-fix**

| 指标 | 数 |
|---|---|
| 上轮发现**已关闭** | **9** |
| 上轮发现**仍开 / 部分** | **10** |
| **本轮修复新引入**的缺陷类 | **4** |
| 当前 Critical / Important / Minor | 6 / 4 / 1 |

**转录核验**：`plan-control-flow-rewrite-review.md` 对上一轮报告的转录**未发现实质失真**；位置有所简写，但问题、证据、定级与建议均保持原意。

**范围保全核验**：Phase 7 Task 7.5 对 Stage 1b 的 B1–B8 **没有削减**——B1、B3–B8 未动；B2 保留远端对账、拒绝记忆、`possible_duplicate` 与统一 canonical-key 协议，只替换已消失的 JS/Workflow 接缝与跨语言 oracle。

> **落盘说明**：评审者报称运行时角色约束仍禁止创建文件，仅内联交付。本文件由协调者代为固化，未作删改；转录本身未经评审者复核。

## 三条最关键

1. **生产接线仍不会调用真实 `invoke()`**：`to_invoke_kwargs()` 虽已定义，但 Phase 6 仍执行 `deps.invoke(request)`，而 CLI 当前把裸 `claude_runner.invoke` 放进 `Deps`。finder/judge 请求仍硬编码 `settings_path=""`、`cwd="/tmp"`、`model=None`、`stream_log=None`。
2. **新调度器对多个候选重复使用同一 `(round_id, judge-role, attempt)`**，第二个候选复用同一 `--session-id` 与同一账本主键——**本轮修复新引入的身份碰撞**。
3. **`BudgetTracker` 与 `FanoutSettlement` 仍不能正确结算重试与超额**：实际成本高于预留时不扣差额，聚合又只收每角色最终 attempt，前几波成本从 round 结算中消失。

## 上轮 19 条的处置核验

| # | 状态 | 核验依据 |
|---|---|---|
| `cfr-01` | 已关闭 | Task 2.2 将通用 JSON-object parser 贯穿 `invoke → parse_stream_json → _parse_terminal_result`，judge 可不带 `candidates`；默认 finder parser 保持原契约。 |
| `cfr-02` | **仍开** | `RoleInvocationRequest` 已新增，但生产接线未使用 `to_invoke_kwargs()`（`plan:2819-2823` 仍直接 `deps.invoke(request)`）。请求工厂仍填空 settings、`/tmp` cwd，未设 model/stream log。 |
| `cfr-03` | 已关闭 | worker 只返回 `AttemptRecord`，SQLite 写移至 `future.result()` 后的主线程循环，跨线程 connection 缺陷消失。 |
| `cfr-04` | **仍开** | `FanoutSettlement` 已建立，但 `all_records` 只追加每角色**最终**记录；被后续 attempt 覆盖的失败尝试未进聚合，总成本/turns/denials/protocol errors 仍不完整。 |
| `cfr-05` | **仍开** | 调用前原子预留已实现，但 `BudgetTracker.settle()` 对 `actual > reserved` 不扣超额。**独立执行计划草图**：预留 0.3、实花 0.5，remaining 仍为 0.7，正确值应为 0.5。 |
| `cfr-06` | **仍开** | finder 传了 `expected_tools`，但 redline 与另两个 judge 的调用（`plan:2337-2341`、`2357-2361`）均未传，judge 能力漂移仍会被接受。finder 漂移还会因账本 status 不兼容而抛异常。 |
| `cfr-07` | 已关闭 | `judge_candidate()` 返回 `(verdicts, degraded)`，`run_fanout()` 在 `plan:2631-2636` 将 judge degraded 汇入顶层。 |
| `cfr-08` | 已关闭 | Phase 5 各任务独立提交新增模块，Phase 6 要求提交全部实际改动，并新增干净 worktree checkout 后复测步骤。 |
| `cfr-09` | **仍开** | 调度器虽先检查剩余时间不少于 10 秒，但请求仍硬编码 `timeout_s=60.0`。剩余 11 秒时会启动最长 60 秒的调用，仍可越过 round deadline。 |
| `cfr-10` | **仍开** | Task 8.4 改为真实创建 session，方向正确；但 attempt 2 要求模型直接报告 `XYZZY`，默认 `_extract_payload` 会拒绝纯文本，且 `InvocationResult` 没有 `result.result` 原文字段可供断言。该验收按当前接口无法完成。 |
| `cfr-11` | **仍开** | child session ID 已改为调用返回后记录，谱系方向修正；但计划宣称 ledger 写失败不阻断，实际 `plan:2040-2049` 没有任何 `try/except`。且 `capability_drift` 不是 ledger/DB 允许的 status，会直接抛错。 |
| `cfr-12` | **仍开** | 波次调度已真正实现，但 retryability 分类仍只有「异常穿透、capability drift 不重试、其余全部重试」。`error_max_budget_usd`、确定性 protocol error、无真实 session 的 timeout 仍全部进 fork 重试。 |
| `cfr-13` | 已关闭 | 顶层额外字段被拒，enum 前先验证 string；list/dict 等 unhashable 值不再触发 `TypeError`。 |
| `cfr-14` | 已关闭 | redline 短路与降级 verdict 均携带 `skipped_judges`，并有对应测试。 |
| `cfr-15` | 部分关闭，**仍计开放** | Task 0.2 的 stdio 开关与 Phase 6 正控方向均已订正；Task 8.4 的真实 session 方向也正确，但其结果 parser/oracle 与当前接口仍不匹配。 |
| `cfr-16` | **仍开** | 新增了处置表，但多项只是**自填声明**：rmf-04 的 protocol errors 未接入 round detail；rmf-08 的 per-call stream path 未在请求工厂设置；rmf-06/08/18 称登记 backlog 项 5、rmf-14 称项 6，**实际 backlog 只有 1–4**；rmf-16 被直接写成「无需处置」。 |
| `cfr-17` | 已关闭 | Task 7.5 保留 B1–B8 全部目标，只替换 B2 的 Workflow/跨语言接缝与验收对象，未发现范围削减。 |
| `cfr-18` | 已关闭 | 继任测试提前到 JS 删除前执行，删除与新测试同一提交；`\x1f` 断言已改为当前真实行为。 |
| `cfr-19` | 已关闭 | 主计划与 kickoff 均统一为 Phase 0/8 付费、Phase 1–7 假件零成本。 |

## 本轮发现

### cfr2-01 — Critical（遗留：cfr-02 修复未闭合）

```
finding_id: cfr2-01
severity: Critical
origin: 遗留：cfr-02 修复未闭合
primary_location: plan:2819-2823
evidence_status: verified
```

**失败场景** —— `Deps.invoke` 仍是裸 `claude_runner.invoke`。传一个 `RoleInvocationRequest` 位置参数只会占据 `prompt`，其余必需参数缺失而 `TypeError`。即使改成 `to_invoke_kwargs()`，空 settings 也会触发 `UnsafeInvocationError`。

**修复建议** —— 由 round 构造完整 request context，或给 fanout 传 `make_request_context`；生产 adapter 必须 `invoke(**to_invoke_kwargs(request))`。测试应断言真实 repo cwd、settings、model、stream path 与**动态** timeout。

### cfr2-02 — Critical（**本轮修复引入**）

```
finding_id: cfr2-02
severity: Critical
origin: **本轮修复引入**
primary_location: plan:2312-2316
evidence_status: verified
```

**失败场景** —— 候选 A 被 redline 否决后，候选 B 再跑 `judge:redline`，两次都用 `derive_session_id(round_id,'judge:redline',1)`，账本键也都是 `round_id:judge:redline:1`。**第二次会话 ID 与账本主键冲突。**

**修复建议** —— judge task identity 必须含稳定 candidate key（如 `judge:redline:<fingerprint>`）；session identity、attempt key、ledger role/task key 与 stream path 共用同一 task identity。

### cfr2-03 — Critical（**本轮修复引入**）

```
finding_id: cfr2-03
severity: Critical
origin: **本轮修复引入**
primary_location: plan:1946-1955
evidence_status: verified
```

**失败场景** —— 预留 0.3、实花 0.5 后 tracker 不扣额外 0.2；同时 `run_wave_scheduled()` 只返回最终 attempt，前两次失败成本不会进入 `all_records` 与 `FanoutSettlement`。round 最终可明显少计真实成本。

**修复建议** —— tracker 按 `remaining += reserved - actual` 结算，允许变负并阻止后续调用；调度器须返回**全部** attempt records，另行提供每角色 final record，settlement 与 ledger 均基于全部 attempts。

### cfr2-04 — Critical（**本轮修复引入**）

```
finding_id: cfr2-04
severity: Critical
origin: **本轮修复引入**
primary_location: plan:2337-2361
evidence_status: verified
```

**失败场景** —— judge 调度没传 `expected_tools`，Bash/MCP 漂移不会被发现。finder 若检测到漂移，主线程会把 status `capability_drift` 写给只允许 `success/degraded/failed_transport` 的 ledger 与数据库，抛 `ValueError` 而不是产生结构化 `capability-drift` round 结果。

**修复建议** —— 所有 judge 调用传同一 expected-tools；统一 AttemptRecord、ledger 函数与 DB CHECK 的状态词，或把 capability drift 在写账前映射到合法审计状态并单独保存原因。

### cfr2-05 — Critical（遗留：cfr-09 修复不完整）

```
finding_id: cfr2-05
severity: Critical
origin: 遗留：cfr-09 修复不完整
primary_location: plan:1990-2007
evidence_status: verified
```

**失败场景** —— deadline 还剩 11 秒时最小窗口检查通过，但 request 的 `timeout_s` 是固定 60 秒。调用仍可能侵占 cleanup reserve 并被 systemd 硬杀。

**修复建议** —— 每波每项发起前设 `timeout_s = deadline - now` 的正值并预留清理窗口；不足最小可用 timeout 则不启动。

### cfr2-06 — Critical（遗留：cfr-10/cfr-15 验收修复未闭合）

```
finding_id: cfr2-06
severity: Critical
origin: 遗留：cfr-10/cfr-15 验收修复未闭合
primary_location: plan:3092-3094
evidence_status: verified
```

**失败场景** —— attempt 2 被要求输出 `XYZZY` 纯文本，但默认 parser 只接受含 `candidates` 的对象；`InvocationResult` 也不保存原始 `result.result` 字符串，计划无法执行其断言。

**修复建议** —— 让 probe 返回固定 JSON 对象并注入 `_extract_json_object`，或为探针提供专用 parser/原始 result 字段；验收读取实际存在的 `InvocationResult.payload`。

### cfr2-07 — Important（**本轮修复引入**）

```
finding_id: cfr2-07
severity: Important
origin: **本轮修复引入**
primary_location: plan:1688-1722
evidence_status: verified
```

**失败场景** —— 超时或进程在 init 前失败时，`run_one_attempt` 用**预分配的** `request.session_id` 冒充实际 session ID；下一波据此 fork。该 session 可能从未被 CLI 创建——**这是 cfr-10 指出的无效 resume，从 Phase 8 的 fake 挪进了生产重试路径**。

**修复建议** —— 只有观察到 init/result 中的**真实** session ID 才允许 fork；否则下一波用新 attempt session 或按不可恢复传输失败处理。

### cfr2-08 — Important（遗留：cfr-11/cfr-12 修复不完整）

```
finding_id: cfr2-08
severity: Important
origin: 遗留：cfr-11/cfr-12 修复不完整
primary_location: plan:2040-2059
evidence_status: verified
```

**失败场景** —— ledger 写没有声明中的容错；任一审计写失败仍终止整轮。所有 `failed_transport` 又无条件重试，包括预算终态、确定性协议损坏与无真实 session 的失败。

**修复建议** —— 为 ledger 写增加可见但非阻断的错误处理；让 AttemptRecord 携带**明确的 retryable 与 resumable 两个位**，调度器只在两者满足时 fork。

### cfr2-09 — Important（遗留：cfr-16 仅写处置声明）

```
finding_id: cfr2-09
severity: Important
origin: 遗留：cfr-16 仅写处置声明
primary_location: plan:65-80
evidence_status: verified
```

**失败场景** —— 处置表称 `record_degraded` 双写 `agentType`，实际代码草图仍只写 `role`；称 rmf-04 会进 round detail，实际调用段未消费 `settlement.protocol_errors`；称 rmf-06/08/18 进 backlog 5、rmf-14 进 backlog 6，**实际 backlog 只有 1–4**；per-call stream/model 也未接线。

**修复建议** —— 把开放发现表改成真实状态，不得写「新增处置」代替任务。若决定延后，应新增真实 backlog 条目、owner 与触发点；本计划直接放大的 rmf-08 至少应在切换前落实 0600、脱敏与保留策略。

### cfr2-10 — Minor（**本轮修复引入**）

```
finding_id: cfr2-10
severity: Minor
origin: **本轮修复引入**
primary_location: plan:3041
evidence_status: verified
```

**失败场景** —— Task 7.5 要求 `rg Workflow spec.md plan-stage1b.md` 只命中新增脚注，但当前 spec 中有大量需保留的历史 Workflow 说明，按 Task 7.5 自己的「不删除原图」策略也必然继续命中。**该验收命令必失败**，不能证明文档同步完整。

**修复建议** —— 改为对明确的现行断言做定点检查，或建立允许保留的历史锚点清单逐条分类，不用全文件零/少匹配作为 oracle。

