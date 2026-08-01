# Phase 1 + 2 合并态评审

> report_id: `cfr-p12-merged` · reviewed_at_rev: `d4001506110082e7b0ca721bdc11ec7bdde5fd23`
> 评审者：`gpt-souls:reviewer`（跨模型）· 日期：2026-08-02
> **verdict: needs-fix**（修复 Important 后可进入下一阶段）· Critical 0 · Important 2 · Minor 1

> 落盘说明：评审者报称运行时约束禁止创建 Markdown，仅内联交付；协调者代为固化，未作删改，转录未经其复核。

## cfr-p12-merged-01 — Important（契约本身的问题 + 测试假绿）

`role_invocation.py:10-18`

**问题**：`RequestContext` 不能产出或约束生产真值，只是接受任意值的容器；测试仅手工构造一组正确字面量，却声称验证了生产值。**实测 `cwd="/tmp"`、`settings_path=""`、`model=None`、`stream_log_dir="/tmp"` 可正常构造**，且全仓唯一构造点就是该测试。

**影响**：Phase 5/6 即使重新引入评审明确禁止的占位值，本阶段测试仍全绿——**cfr3-01 的核心要求尚未形成可执行地基**。

**修法**：在 Phase 2 提供唯一生产 factory（如 `build_request_context(cfg)`），直接绑定 `cfg.repo_root` / `SETTINGS_PATH` / `DEFAULT_AGENT_MODEL` / `cfg.state_db.parent`；测试调用该 factory，而不是自行填期望值。`__post_init__` 拒绝空值可以加，但**不能替代生产来源测试**。

## cfr-p12-merged-02 — Important（实现引入的新问题）

`claude_runner.py:418-439`

**问题**：`_persist_stream` 的 `0o600` **只在文件首次创建时生效**；`O_CREAT|O_TRUNC` 打开**既有**文件时会忽略 mode。复现：预建 0644 日志后调用 `_persist_stream`，写入完成后权限**仍为 0644**。现有测试只覆盖不存在的新文件，因此**假绿**。

**影响**：stream 路径按 round/role/attempt **确定性**生成，重跑、崩溃恢复或残留文件都可命中既有路径；新的敏感 stream 会写入全用户可读的 inode，违反本阶段明确的 0600 契约。

**修法**：用同目录 0600 临时文件写完后**原子 replace** 目标，或先以 `O_EXCL` 创建并显式处理冲突；补「目标已存在且为 0644」的回归测试。**仅加 `chmod` 会重新引入计划禁止的权限窗口。**

## cfr-p12-merged-03 — Minor（契约本身的问题）

`ledger.py:28-30`、`60-74`

**问题**：`ATTEMPT_STATUSES` 同时含运行态 `running` 与三个终态，而 `record_attempt_finished` 用**整个集合**校验终态。实测可把记录 finished 为 `running` 并同时写入 `ended_at`，形成**自相矛盾的审计行**。

**修法**：保留四值数据库词表，另定义 `ATTEMPT_TERMINAL_STATUSES = {success, failed_transport, capability_drift}`，`record_attempt_finished` 只接受终态；相应调整计划里「三处逐字同一集合」的措辞。

## 核实无误的部分（评审说明了验证方式）

- `agent_attempts` CHECK、`ledger.ATTEMPT_STATUSES` 与计划状态词一致，均**无 `degraded`**
- `build_stream_log_path` 的 identity 段与 `attempt_key` 当前**逐字一致**
- `InvocationResult.subtype` 保留终态原值；**parser 失败实测为 `ok=False` + `subtype="success"`，超时为 `None`**——Phase 5 分类表的区分依据成立
- **不传新参数时 argv 与改动前完全一致**；`STAGE1_ALLOWED_TOOLS` 仍为六项
- Phase 1/2 模块**未引入共享可变状态**；SQLite connection 仍线程绑定，**跨线程实测抛 `ProgrammingError`**——Phase 5 必须兑现「主线程串行写账本」
- 354 + 13 全绿；对 stream path 模板、`subtype` 透传、0600 创建模式各做一次 **mutation，三条目标测试均确实变红**，随后清 `__pycache__` 并确认被评审文件无工作区改动

## 评审自陈最不确定的三条

1. `-01` 定为 Important 而非 Critical：Phase 6 尚可补生产 factory，但当前所谓地基测试确实没有验证它声称的事情。
2. `-02` 的生产触发**频率**取决于确定性日志路径是否会被重用；**反例确定成立**，频率尚未实测。
3. 未把 init 与 result 的 `session_id` 不一致列为发现：代码会静默采用 init 值，但**缺少真实 CLI 会产生不一致值的证据**。
