# 控制流重写计划 · 第三轮复审（原评审者）

> reviewed_at_rev: `24b6e49b7f882bdcc7c62b28ce6f95775609b1e0` · 日期：2026-08-02
> **verdict: needs-fix**，但**形态判定为「不是 wrong-shape」**——契约规格是正确方向，只是仍有 3 个不会被当前测试清单自然暴露的阻塞缺口。

> 落盘说明：评审者运行时角色约束仍禁止创建文件，内联交付，由协调者代为固化，未作删改；转录未经评审者复核。

## 收敛趋势

| 轮次 | 关闭 | 仍开/部分 | 新引入 | 因降级丢失 |
|---|---|---|---|---|
| 第 2 轮（`8336241`）| 9 | 10 | **4** | — |
| 第 3 轮（`24b6e49`）| 7 | 3 + 旧 2 | **0** | **3** |

**新引入归零**是本轮最重要的信号。**因降级丢失 3 条**是形态改变的代价，也正是本轮评审被要求重点查的风险。

**转录核验**：`plan-control-flow-rewrite-review-2.md` 未发现实质转录失真。

## 三个阻塞项

### cfr3-01 — 因降级而丢失（承接 cfr2-01/cfr2-02）

位置：`plan:457-489`、`1025-1065`、`1147-1208`

**问题**：契约没有定义生产请求从哪里取得并验证 `cwd`、`settings_path`、`model`、per-attempt `stream_log`。Phase 6 只验证 adapter **展开了哪些键**，不验证**值正确**；Task 5.5 虽要求 task identity 贯穿 session、ledger 与路由，但测试只比较 session ID，**没有验证 attempt key、ledger key、stream path 共用同一 identity**。

**必须的修复**：新增显式 `RequestContext` 或 request factory 契约；测试**两个候选**的 judge 请求，逐项断言 session ID、attempt key、ledger role/key、stream path 均由同一含 fingerprint 的 task identity 派生，并断言 cwd/settings/model 为**生产值**。

### cfr3-02 — 遗留，cfr-12 三轮未关闭

位置：`plan:877-914`、`973-1003`

**问题**：契约仍规定所有 `invocation.ok=False` 都是 `failed_transport, retryable=True`。`error_max_budget_usd`、确定性 protocol corruption、非法请求仍会重试。**新增 `retryable` 字段只把策略显式化，没有把分类做对。**

**必须的修复**：列出可测试的终态分类表——传输中断、overload、随机 schema 输出失败**可重试**；预算耗尽、参数错误、确定性协议异常**不可重试**。每类至少一条测试。

### cfr3-03 — 因降级而丢失（承接 cfr2-09/cfr-16）

位置：`plan:121-130`、`763-860`、`1190-1208`

**问题**：开放发现表仍宣称 rmf-04 的 `protocol_errors` 会进 round detail、`record_degraded` 会双写 `agentType`、rmf-17 会显式设置规范 model，**但对应任务的不变量与测试清单没有这些断言**。Phase 5.2 保留的草图仍只写 `role`；Phase 6 没有消费 `settlement.protocol_errors`。

**必须的修复**：把三项写进**实际任务**的不变量与测试清单。**不能只留在处置表。**

## 非阻塞但需明确处置

- **rmf-08 的延后理由站不住**：本计划把每轮日志从 1 份扩大到多份，属于**直接放大**，不是无关任务。至少在 Phase 6 前落实 0600、脱敏与保留策略，或取得明确的延后裁决。
- rmf-06、rmf-14 的 CLI 版本部分、rmf-16、rmf-18 可作独立后续项，但当前只写「仍开放」而**没有 owner 或重议触发点**；这避免了假关闭，却仍不是可执行追踪。

## 八项设计要求的留存核验（本轮首要问题）

| 设计要求 | 结论 |
|---|---|
| judge identity 含 fingerprint | **保留**，但 stream-path/ledger 联合测试**缺失** |
| `WaveResult` 携带全部 attempts | 保留，字段、不变量、成本求和测试均明确 |
| `BudgetTracker` 允许负余额 | 保留，明确 `remaining += reserved - actual`，有超额与负余额测试 |
| 状态词三处统一 | 保留，DB CHECK / ledger 校验 / `AttemptRecord` 一致 |
| 所有 judge 传 `expected_tools` | 保留，Task 5.5 明确要求每次调度传入 |
| `retryable`/`resumable` 分离 | 保留，且**只有 CLI 实报 session ID 才允许 fork**；测试充分 |
| 能力漂移不可降级 | 保留，由全部 attempts 聚合后在 round 层 fail closed |
| 动态 `timeout_s` | 保留，明确按 deadline 动态收缩，有不同 deadline 的对照测试 |

## 逐条关闭状态

| # | 状态 |
|---|---|
| cfr2-01 | 部分关闭 |
| cfr2-02 | 部分关闭 |
| cfr2-03 | 已关闭 |
| cfr2-04 | 已关闭 |
| cfr2-05 | 已关闭 |
| cfr2-06 | 已关闭 |
| cfr2-07 | 已关闭 |
| cfr2-08 | 已关闭 |
| cfr2-09 | **仍开** |
| cfr2-10 | 已关闭 |
| cfr-12（旧）| **仍开** |
| cfr-16（旧）| **仍开** |

