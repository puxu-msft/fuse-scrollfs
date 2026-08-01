# Phase 5 合并态评审（`fanout.py`）

> report_id: `cfr-p5-merged` · reviewed_at_rev: `9e9fdffc3dd1b517c2e99b8165145586ed9d0984`
> 评审者：`gpt-souls:reviewer`（跨模型）· 日期：2026-08-02
> **verdict: needs-fix** · blocker 0 · **major 2** · minor 1

> 落盘说明：评审者报称运行时禁止创建 Markdown，按约定回退为内联交付；协调者代为固化，未作删改，转录未经其复核。

## cfr-p5-merged-01 — major（契约本身的问题，**缺陷在 Phase 6 计划里，实施前被抓到**）

`fanout.py:453-466` · `plan:1349` · `budget.py:138-153`

**问题**：Phase 6 计划让 `invoke_fn` 闭包在调用返回后执行 `budget.record_invocation()`，但 `invoke_fn` **实际运行于 worker 线程**。该闭包会**间接捕获**主线程的 SQLite connection，违反「worker 不碰 SQLite」这条核心不变量。

**复现**：用真实 `sqlite3` connection 按计划形状构造 `invoke_and_record`，`run_wave_scheduled()` 立即抛 `ProgrammingError: SQLite objects created in a thread can only be used in that same thread`。

**修法**：worker 只执行模型调用并返回纯数据；由主线程在收集 `AttemptRecord` 后执行 `agent_attempts` 与 `invocations` **两类**账本写入。补一条**使用真实 sqlite3 connection** 的集成测试。

## cfr-p5-merged-02 — major（实现偏离契约）

`fanout.py:418-448`、`500-525`、`561-571`、`690-700` · `plan:1298-1301`

**问题**：调度器按 `single_call_cap_usd` **预留**预算，却**不把该值写入** `RoleInvocationRequest.grant_usd`；`run_fanout` 也没有暴露或贯穿该参数。Phase 6 要求的 `round_budget_usd / 7` 因而**无法成为真实的 CLI 单调用上限**。

**复现**：总池 0.2、`single_call_cap_usd=0.1` 调度两个请求，两次均成功预留，但实际请求仍各携带 `grant_usd=0.3`——**总请求上限 0.6，是预算池的 3 倍**。而真机 PoC 已证明 `--max-budget-usd` 是**滞后停止器**，不能靠事后 `settle` 补救并发双花。

**修法**：调度器构造最终 request 时同时 `replace(grant_usd=single_call_cap_usd)`，或强制 `make_request` 产出的 grant 与预留值相等；把该参数**贯穿** `run_fanout` / `run_finders` / `judge_candidate`，并新增「不相等即红」的测试。

## cfr-p5-merged-03 — minor（实现引入的新问题）

`fanout.py:318-337` · `tests/test_fanout.py:444-505`

**问题**：预算用二进制 float 做严格 `remaining < amount` 比较，**精确倍数会被提前拒绝**。

**复现**：`BudgetTracker(1.2)` 连续 `try_reserve(0.3)` 四次得 `[True, True, True, False]`，余额为 `0.2999999999999999`；`BudgetTracker(0.6)` 连续预留 `0.2` 三次也只成功两次。现有并发测试只断言「不超支」，**捕获不了少调度**。

**修法**：预算统一用**整数微美元**或 `Decimal`；至少增加精确倍数边界测试，**避免用任意 epsilon 掩盖真实超支**。

## 核实无误（评审说明了验证方式）

- 63 个 fanout 测试、429 harness 测试、13 额外测试全绿
- **rmf-03 场景实测**：finder 正常产出候选、redline 连续失败 3 次后，**顶层 `degraded` 非空**，未退化为干净的 `no-candidate`
- 预算超额扣减、fork `resumable` 门、judge degraded 顶层聚合**三条 mutation 均使目标测试变红**；mutation 在 `/tmp` 隔离副本执行，随后清理 `__pycache__`，被评审文件无工作区改动
- **直接插桩确认** `agent_attempts` 的 start/finish 写入均发生在主线程；`BudgetTracker` 的 reserve/settle 临界区由**同一把 Lock** 保护
- 动态 timeout **未使用 `max` 下限**；剩余窗口不足时不会启动调用
- judge fingerprint identity、`expected_tools`、`retryable`/`resumable`、全部 attempts、`skipped_judges`、`protocol_errors` 聚合**均已落地**

## 评审自陈最不确定的三条

1. `-01` 定为 major：缺陷会在 Phase 6 按当前计划接线时**确定触发**，但尚未进入当前生产路径。
2. `-03` 定为 minor：反例确定成立，但 Phase 6 采用 `grant / 7` 时，常见默认值未必触发该舍入方向。
3. 未把「单个 worker 抛编程异常后兄弟调用缺少 attempt 审计」另列发现——计划明确允许此类异常穿透整轮，是否仍要求完整兄弟审计**尚无明确契约**。
