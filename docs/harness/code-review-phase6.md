# Phase 6 合并态评审（`round.py` 接线 + 生产路径切换）

> report_id: `cfr-p6-merged` · reviewed_at_rev: `602e2c6f2ff9017d510ec267ef6dfa34e747b723`
> 评审者：`gpt-souls:reviewer`（跨模型）· 日期：2026-08-02
> **verdict: needs-fix** · critical 0 · **important 4** · minor 1

> 落盘说明：评审者按运行时约束内联交付；协调者代为固化，未作删改，转录未经其复核。

## cfr-p6-merged-01 — important（实现引入的新问题）

`round.py:374-375`、`fanout.py:69-78,339-355`

默认 $1.50 预算除以 7 后**逐项量化**为 214286 微美元，**7 份合计 1,500,002 微美元，超出总池 1,500,000 两微美元**。若前 6 次调用均花满 grant，第 7 个 oracle judge 会被**错误标为 budget-exhausted**，整轮变成 `no-candidate-degraded`。

（成因：整数微美元化解决了浮点少调度，但除法的舍入方向没同步处理。）

## cfr-p6-merged-02 — important（实现偏离契约）

`.claude/harness-settings.json:3-10`

Phase 6 要求 `permissions.allow` 收窄为 `Read`/`Grep`/`Glob`，但该文件**仍允许** `Skill`、`Workflow`、`TaskOutput`、`TodoWrite`，且提交**未包含该文件**。

**诚实定级**：`--tools` 当前仍限制实际 init 工具集，因此**不是已证实的权限升级**；但明确的配置收窄义务没有落地——一旦将来某处不传 `--tools`，主防线就只剩这份没收窄的 allow 列表。

## cfr-p6-merged-03 — important（合并接缝问题）

`budget.py:109-123,181-208`、`round.py:561-570`

实际总成本超过 round 预留时，`Budget.settle` 先写 `result=budget_breach`，随后 `record_outcome` **又覆盖为** `published` 等业务结果。

**实测**：预留 $1.50、结算 $1.70 后仍返回 `published`，账本也记 `published`，**CLI 可退出 0**——超支被静默抹掉。两个写者争同一个字段。

## cfr-p6-merged-04 — important（测试缺口）

`tests/test_cli.py:32-92`

**没有 `no-candidate-degraded` 的退出码用例**。把它加入成功映射后，整个 `TestCliRoundExitCode` **仍全绿**——协调者指定的这条 mutation 验证**失败**。

即：当前行为正确（该值确实映射为非零），但**没有任何测试守着它**，下一次改动可以自由破坏它。

## cfr-p6-merged-05 — minor（既有测试被削弱）

`tests/test_round.py:471-498`

`known_canonical_keys` 测试从「`json.loads` 验证合法 JSON」降为「只查两个子串」。**实测**：把序列化器改成包含相同子串的**非法 JSON**，测试仍绿。

（该测试原本的用意正是防「`repr()` + 引号替换产出非法 JSON」这一类缺陷。）

## 核实无误（评审说明了验证方式）

- 440 + 13 个 Python 测试及 legacy Node 测试全绿
- **能力漂移**新增 `Bash`/MCP 时整轮返回 `capability-drift`；对应 mutation 会使测试变红
- **`record_invocation` 在主线程**使用真实 `sqlite3` connection；对应 mutation 会使测试变红
- `no-candidate-degraded` 当前**确实**由 CLI 映射为非零退出码（只是无测试保护，见 `-04`）
- **所有现有 return 分支已改为消费 `FanoutSettlement`**，`round.py` 无残留 `invocation.*` 引用
- `single_call_cap_usd` 会写入请求 `grant_usd`；截止不足时不会启动调用，**未用 `max()` 修补负值**
