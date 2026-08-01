# scrollz harness · 控制流重写实施计划（ADR-002 D0/D1/D2 落地）

> 状态：**草稿 v5，已处置四轮跨模型对抗评审（cfr-01–19、cfr2-01–10、cfr3-01–03、cfr4-01–02），撰写中，尚未提交第五轮审查**。
> 撰写日期 2026-07-31，v3 修订日期 2026-08-02，v4 修订日期 2026-08-02，v5 修订日期 2026-08-02。回答「怎么做」；「做什么/为什么」见 [adr-002-control-flow-ownership.md](./adr-002-control-flow-ownership.md)、PoC 结论见 [exp/stdio-driver/CONCLUSIONS.md](../../exp/stdio-driver/CONCLUSIONS.md)、现行不变量见 [spec.md](./spec.md)、真机现状见 [HANDOVER.md](./HANDOVER.md)、最近一轮评审见 [code-review-realmachine-fixes.md](./code-review-realmachine-fixes.md)、第一轮评审见 [plan-control-flow-rewrite-review.md](./plan-control-flow-rewrite-review.md)、第二轮评审见 [plan-control-flow-rewrite-review-2.md](./plan-control-flow-rewrite-review-2.md)、第三轮评审见 [plan-control-flow-rewrite-review-3.md](./plan-control-flow-rewrite-review-3.md)、第四轮评审见 [plan-control-flow-rewrite-review-4.md](./plan-control-flow-rewrite-review-4.md)。
> 关联但**冻结不动**：[plan-stage1a.md](./plan-stage1a.md)（Task 1–12 已完成，是本计划的起点代码）、[plan-stage1b.md](./plan-stage1b.md)（治理范围，不受本次重写影响，仍在其冻结范围内）。

> **For agentic workers:** REQUIRED SUB-SKILL: 用 `superpowers:subagent-driven-development`（推荐）或 `superpowers:executing-plans` 逐任务实施。任务用 checkbox（`- [ ] `）追踪。

**Goal**：废弃「外层 `claude -p` 会话 → Skill → `Workflow` 工具后台起 7 个 agent → `TaskOutput` 阻塞回显」这条链，改为**控制器（Python）直接为每个 finder/judge 起一个独立顶层 `claude -p` 进程**，编排（去重、排序、judge 短路、降级即否决、失败后 fork 续跑）全部落在可单元测试的 Python 代码里。产出给下游（`validate_candidate` → `_derive_labels` → `Publisher.publish`）的候选 DTO 形状**不变**，因此 `outbox.py`/`budget.py`/`queue.py`/`publish.py`/`lifecycle.py`/`gitops.py`/`precheck.py`/`db.py` **不改动**（除 `db.py` 新增一张纯追加表）。

**核心架构变化（区别于 plan-stage1a 起点代码，务必先读懂再动手）**：

1. **外层会话本身消失。** 现在的形态是「一个外层 `claude -p` 会话调 `Skill(scrollz-round)` → 该 skill 指示模型调 `Workflow` 工具 → workflow 内部再 `agent()` 起 7 个子 agent」——这是一层三级嵌套。重写后，Python 直接 `subprocess` 起 7 个**顶层独立**的 `claude -p` 进程（4 finder + 最多 3 judge），彼此之间没有任何「外层模型」。`.claude/skills/scrollz-round/`、`.claude/workflows/scrollz-propose.js` 整个不再被调用，Phase 7 予以删除。
2. **`STAGE1_ALLOWED_TOOLS` 大幅收窄。** 现在的集合是 `{Read, Grep, Glob, Skill, Workflow, TaskOutput}`——`Skill`/`Workflow`/`TaskOutput` 三项存在的唯一理由是「外层会话要能调用 Workflow 并阻塞等待」。外层消失后，每个 finder/judge 进程只需要 `{Read, Grep, Glob}`。这是本次改动最大的攻击面收窄，不是可选项。
3. **`--agents <json>` + `Task` 工具明确不用于扇出。** PoC Q6 已实测：`--agents`+`Task` 路径会触发「一次输入产生第二个顶层 `result`」的真实反例（后台任务通知机制），这正是 ADR 判定要规避的坑。因此每个 finder/judge 的「persona」不通过 `--agents` 注入，而是把 `.claude/agents/harness-*.md` 的 frontmatter（`tools`）与正文（persona 指令）在 Python 侧读出、拼进该顶层进程的 `-p "<prompt>"` 参数里。`--agents` 本身没有被否定（Q7 通用化仍可能用到它承载 persona），只是**不用它做扇出**——见文末「未采纳方案」。
4. **会话身份与 fork 重试是新增的正交能力**，不影响 1–3 的候选/裁决产出契约。

**Tech Stack**：与 plan-stage1a 一致——Python 3 标准库（`sqlite3`/`unittest`/`subprocess`/`concurrent.futures`/`uuid`/`re`），`gh` CLI，`git`，systemd user timer，`claude` CLI。**新增**：`concurrent.futures.ThreadPoolExecutor`（并发原语，仍是标准库）。**不引入**任何第三方包，不建 venv。

---

## Global Constraints（每个任务隐含适用）

延续 `plan-stage1a.md` 的全部 Global Constraints（绝对路径、零依赖、仓库根、凭据隔离、副作用唯一入口、提交纪律），额外补充：

- **不改动的模块（白名单式声明，任何任务都不得触碰）**：`outbox.py`、`budget.py`、`queue.py`（除了在其现有 `_norm`/`canonical_key`/`fingerprint` 之外新增，不修改既有实现）、`publish.py`、`lifecycle.py`、`gitops.py`、`precheck.py`（除新增一项只读检查外不改现有检查）、`db.py`（只允许新增 `CREATE TABLE IF NOT EXISTS`，不改任何既有表定义/索引）。任何任务如果发现「必须改这些文件才能完成」，先停下来在 Plan 里补一节说明理由，不得默默改。
- **候选 DTO 契约不变**：最终交给 `round.py` 现有校验/发布链的 candidate 对象字段集合、类型、约束**逐字复用** `round.py` 现有的 `_REQUIRED_CANDIDATE_FIELDS`/`_OPTIONAL_CANDIDATE_FIELDS`/`_LANES`/`_PRIORITIES`/`_SIZES`/`_SLUG_RE`/`_MAX_*` 常量。judge 裁决产出的 `verdicts` 字段形状延续 `pickVerdictFields` 现有的三种专有字段（`evidence`/`invariant_at_risk`/`suggested_oracle`）。
- **测试基线**：改动前 304 个测试全绿（`cd .claude/scripts && python3 -m unittest discover -s harness/tests -t .`）。改动过程中允许**净增**测试数；`test_canonical_key_cross_language.py` 是本计划唯一计划**删除**的既有测试文件（Phase 7，理由见该阶段），删除时必须同时补一条不依赖 JS 的等价不变量测试，不是净减覆盖。
- **付费阶段仅两处，其余全部零成本**：Phase 1–7（除下述例外）全部只用假件（fake `invoke_fn`）跑，不花真钱、不碰公开仓库。**Phase 0（会话原语真机验证）与 Phase 8（真机切换验收）是仅有的两个付费真机阶段**，且必须逐步执行、每步之间停下确认，延续 `plan-stage1a.md` Task 13 的纪律。（评审 cfr-19：此前措辞误写「Phase 0–6 全部只用假件」，与 Phase 0 标题本身矛盾，已订正；kickoff 文档同步使用同一句表述。）
- **正控纪律**（本项目反复验证有效，见 HANDOVER「方法论沉淀」）：每个任务写完实现后，**临时还原到实现前的状态跑一次测试，确认测试真的会红**，再恢复实现。计划里每个任务的「正控」小节写明具体还原动作。**正控的方向是单向的**：mutation（临时破坏实现）必须让相关测试从绿变红，绝不能反过来要求「mutation 后测试仍然通过」——那不是正控，是验证了错误的方向（评审 cfr-15，Phase 6 Step 7 已订正，见该阶段）。
- **提交态必须独立可运行，不得只验工作区**：每个任务的 `git commit` 命令列出的文件集合，必须包含该任务**实际修改或新增**的**全部**文件（含被牵连修改的既有测试文件）。任何任务提交后，若怀疑提交不完整，应在**干净的临时 worktree**（`git worktree add /tmp/<name> <commit-sha>`）上 checkout 该提交并独立重跑全套测试，而不是依赖当前工作区（工作区可能混有下一个任务已经开始的改动，会掩盖「这次提交本身缺文件」的问题）（评审 cfr-08）。
- **调用适配层必须使用真实签名，不得用宽松 `**kwargs` 掩盖漂移**：Phase 5 起，任何测试专用替身（fake `invoke_fn`）必须接受与生产 `RoleInvocationRequest`（Phase 2 定义）完全一致的参数形状，不允许用「接受任意关键字参数」的宽松签名或测试专用的旁路字段（如 `_test_role`）。测试专用路由信息（"这次调用对应哪个角色"）通过 `RoleInvocationRequest.role` 这个**生产代码本身就有的字段**读取，不新造一个只有测试认识的键（评审 cfr-02，详见 Phase 2/5 的重新设计）。
- **代码草图降级为「接口契约 + 不变量 + 测试清单」，不写完整可执行函数体**（第二轮评审 cfr2 系列指出的根因修正）：第一轮修订里写的"看起来可运行"的完整 Python 代码，被评审**独立执行后**发现多处签名不匹配、状态词不统一、结算逻辑有竞态与算错——这些本该是 TDD 几分钟内就能抓到的问题，却因为写成了详尽但未验证的伪代码，耗费了一整轮评审去发现。因此本次修订（v3）起，**全篇的代码块只保留**：(a) 数据类的字段定义（这是契约，字段名/类型/默认值需要跨模块一致）；(b) 函数签名（参数名、类型标注、返回类型）；(c) 用列表形式写明的不变量（前置条件、后置条件、边界行为）。**不再给出完整函数体实现**——那是 TDD 的 Step 3（写最小实现）应该由实施者亲手写、亲手跑红绿的部分，写在计划里反而会被误当作"已验证的正确代码"直接复制。每个任务的「测试清单」改为列出「测试要断言什么」（断言点，不是完整测试源码），实施者按此清单逐条写出可执行的测试。

---

## 评审处置台账（跨模型对抗评审 `cfr-01`–`cfr-19`，`docs/harness/plan-control-flow-rewrite-review.md`）

> 本台账逐条记录 GPT soul 评审报告的处置结果，是本次修订（v2）相对 v1 的完整变更索引。**全部 19 条均已处置，无遗留未回应项**。

| 编号 | 严重级别 | 处置 | 落点（本次修订后的章节） |
|---|---|---|---|
| cfr-01 | Critical | **采纳**：judge 输出 `{verdict,...}` 会被现有 `_extract_payload()` 的 `candidates` 强制要求拒收。新增可注入 `payload_parser` 参数贯穿 `invoke()`→`parse_stream_json()`→`_parse_terminal_result()`，finder 用现有 `_extract_payload`（默认值，向后兼容），judge 用新增的 `_extract_json_object`（只要求顶层是 dict，不要求 `candidates`） | Phase 2 Task 2.2（新任务） |
| cfr-02 | Critical | **采纳**：定义 `RoleInvocationRequest` 数据类作为唯一调用契约，字段与 `claude_runner.invoke()` 真实签名逐一对应；测试 fake 必须接受同一类型，不再用宽松 `**kwargs` | Phase 2 Task 2.3 + Phase 5 全面重写 |
| cfr-03 | Critical | **采纳**：`agent_attempts` 账本写入从"worker 线程内即时写"改为"主线程在 `future.result()` 汇总之后串行写"，worker 线程只返回数据，从不触碰 SQLite connection | Phase 5 Task 5.3/5.4 重写 |
| cfr-04 | Critical | **采纳**：定义 `FanoutSettlement` 聚合结果（总成本/成本是否全部已知/turns/denials/退出状态/protocol errors），Phase 6 的全部结算分支改为消费 `FanoutSettlement`，不再引用不存在的单一 `invocation` 变量。第二轮评审 cfr2-03 进一步指出聚合必须基于全部 attempts（含重试失败的记录），非仅每角色最终记录，已在 Phase 5 Task 5.4/5.6 修正 | Phase 5 Task 5.4/5.6、Phase 6 全面重写 |
| cfr-05 | Critical | **采纳**：新增进程内 `_BudgetTracker`（`threading.Lock` 保护的原子预留/结算），并发调用前先原子 `try_reserve()`，避免 SQLite 跨线程读写、避免多个 finder 读到同一份"剩余预算"后重复占用 | Phase 5 Task 5.3 重写 + Phase 6 |
| cfr-06 | Critical | **采纳**：capability drift 检查（沿用 `round.py` 现有 `_capability_drift_problems` 语义）在 Phase 6 的调用适配器里对**每次**子调用结果执行，命中即抛 `CapabilityDriftError`（不可降级，穿透整个扇出直接使整轮失败），不再假设"随解析器自动下沉" | Phase 6 重写 |
| cfr-07 | Critical | **采纳**：judge 降级结果现在同时写入两处——局部 `reject` verdict（保留原语义）与顶层 `degraded` 列表（新汇入 `judge_candidate`/`run_fanout` 的返回值），避免 rmf-03 复发 | Phase 5 Task 5.4 重写 |
| cfr-08 | Critical | **采纳**：Task 6.1 的 `git commit` 命令补全 `claude_runner.py`/`fanout.py`/`session_identity.py` 等实际改动文件；新增"提交态干净 worktree 复核"步骤 | Phase 6 Task 6.1 + Global Constraints |
| cfr-09 | Critical | **采纳**：截止时间计算改为"先判断剩余时间是否覆盖最小调用窗口，不足则该角色直接判 `deadline-exhausted` 降级，不发起调用"，删除 `max(负值, 下限)` 这种会反向放大剩余时间的写法 | Phase 5 Task 5.3/5.4 重写 |
| cfr-10 | Critical | **采纳**：Task 8.4 改为先用真实 `invoke()` 建立 attempt 1 的真实会话（CLI 真正返回 session_id），再通过 stdio 控制通道对该真实进程发 `interrupt`（借用 PoC `driver.py` 的中断手法，专供本任务使用，不进生产 `invoke()`），验收断言比对真实新旧 session_id 与恢复内容 | Phase 8 Task 8.4 重写 |
| cfr-11 | Important | **采纳**：账本写入延后到"调用已返回、真实 `session_id` 已知"之后才发生（cfr-03 修复的直接推论），fork attempt 的 `parent_session_id` 现在正确指向**上一次的真实 CLI session_id**，不再是恒定的父 ID 占位；同时明确"ledger 写失败不阻断本轮"的容错语义在设计里落实（try/except 由调用方包裹） | Phase 5 Task 5.4 重写 |
| cfr-12 | Important | **采纳**：重试循环从"单角色内部连续 3 次尝试"改为"波次调度"——所有角色的 attempt N 在同一波并发发起，只有失败的角色进入 attempt N+1；新增显式 retryability 分类（配置错误穿透不重试；传输失败/schema 校验失败可重试） | Phase 5 Task 5.3/5.4 重写 |
| cfr-13 | Important | **采纳**：`fanout_schema.py` 的字段校验改为"先类型检查、通过后才做枚举/集合成员检查"，顶层字段集合严格等于 `{"candidates"}`（迁移 JS `additionalProperties:false` 语义） | Phase 3 重写 |
| cfr-14 | Important | **采纳**：rejected 记录新增 `skipped_judges` 字段（redline 短路或降级导致未跑到的 judge 类型列表），供 Stage 1b 拒绝记忆判断裁决面是否完整 | Phase 5 Task 5.4 重写 |
| cfr-15 | Important | **采纳**（三处分别处置）：Task 0.2 补 `--permission-prompt-tool stdio` + control_response 处理，使"无 `control_request`"这个断言真正有意义；Phase 6 Step 7 的 mutation 测试方向订正为"必须让测试失败"；Phase 8 Task 8.4 改为真实创建 session（同 cfr-10） | Phase 0 Task 0.2、Phase 6 Task 6.1、Phase 8 Task 8.4 |
| cfr-16 | Important | **采纳**：新增「开放发现处置表」章节，逐条处置 `code-review-realmachine-fixes.md` 里仍开放、且落在本次改动模块上的 rmf-04/06/08/14/16/17/18 | 新增「开放发现处置表」章节 |
| cfr-17 | Important | **采纳**：新增 Phase 7 Task 7.5，同步修订 `spec.md`（Workflow 相关段落标注已被 ADR-002 取代）与 `plan-stage1b.md`（B2 的实现接缝改为"Python 侧 `known_canonical_keys` 直接供 `fanout.dedupe_and_rank` 消费，不再有 Workflow/Node 对象"）。**功能范围不削减**：B1–B8 全部条目原样保留，只改实现接缝描述与验收 oracle 的具体指代对象 | Phase 7 Task 7.5（新增） |
| cfr-18 | Important | **采纳**（两处）：(a) Phase 7 任务顺序订正为"先在 JS 仍存在时写好并验证继任测试，与旧测试短暂共存都验证通过，再在同一个原子提交里同时删除 JS 与旧测试"；(b) 继任测试的断言从计划撰写时臆造的期望值改为**冻结当前 Python 实际行为**（`_norm('a\x1fb') == 'a\x1fb'`，已实测确认，不是 `'a b'`） | Phase 7 Task 7.1/7.2 重排 |
| cfr-19 | Minor | **采纳**：Global Constraints 措辞订正，Phase 0/Phase 8 为付费真机阶段，其余零成本；kickoff 文档同步使用同一句表述 | Global Constraints（已生效，见上）+ kickoff 文档 |

**未被反驳的条目**：本轮评审的 19 条经复核（含独立实跑复现 cfr-01/cfr-03/cfr-18 三条最关键发现）**全部成立，无一条被驳回**。评审者自陈的三处不确定判断——(1) cfr-10 是否可通过"先建真实会话再判失败"降级；(2) Read/Grep/Glob 是否触发 `can_use_tool` 未付费验证；(3) Stage 1b 跨语言测试冻结与否属文档治理判断——本次修订采纳评审者给出的更严格路线（(1) 按"先真实建立会话"处理，不降级；(2) 保留在 Phase 0 Task 0.2 由本计划的真机步骤验证；(3) 明确写入 Task 7.5，视为需要同步修订而非自动失效）。

---

## 第一轮 19 条核验结果（第二轮评审复核，`docs/harness/plan-control-flow-rewrite-review-2.md`）

> 第二轮评审对 v2 修订逐条独立核验，结果如下——本表格是对上表「处置」列的**事后验证**，不是新的处置决定。

| 编号 | 核验结果 | 备注 |
|---|---|---|
| cfr-01 | 已关闭 | `_extract_json_object`/可注入 `payload_parser` 已落地并验证 |
| cfr-02 | **仍开**（第三轮起再修） | `RoleInvocationRequest` 已新增，但第二轮评审发现生产接线（Phase 6）仍直接执行 `deps.invoke(request)` 而非 `deps.invoke(**to_invoke_kwargs(request))`——本轮（v3）已在 Phase 6 Task 6.1 修正为"`cli.py` 构造真正执行 `to_invoke_kwargs` 展开的适配闭包" |
| cfr-03 | 已关闭 | worker 线程只产出纯数据 `AttemptRecord`，账本写入延后到主线程串行执行，已用现有 `db.py` 实测复现原缺陷并验证修复 |
| cfr-04 | **仍开**（本轮已修） | 第二轮评审指出 `all_records` 只追加每角色最终记录，聚合成本漏算失败重试的花费——本轮（v3）在 Phase 5 Task 5.4/5.6 改为 `WaveResult.all_attempts` 携带全部尝试（cfr2-03 同源问题，已合并修正） |
| cfr-05 | **仍开**（本轮已修） | `BudgetTracker.settle()` 对 `actual > reserved` 不扣超额——本轮（v3）改为 `remaining += reserved - actual`（不夹 `max`），允许变负（cfr2-03 同源问题） |
| cfr-06 | **仍开**（本轮已修） | redline 与另两个 judge 的调用均未传 `expected_tools`——本轮（v3）在 Phase 5 Task 5.5 补齐（cfr2-04 同源问题） |
| cfr-07 | 已关闭 | judge 降级已同步写入局部 verdict 与顶层 `degraded` 数组，已用正控验证 |
| cfr-08 | 已关闭 | Task 6.1 的提交文件列表已补全，新增干净 worktree 独立复核步骤 |
| cfr-09 | **仍开**（本轮已修） | 请求仍硬编码 `timeout_s=60.0`——本轮（v3）在 Phase 5 Task 5.4 改为按 `deadline_monotonic - time.monotonic() - _CALL_TEARDOWN_MARGIN_S` 动态计算 |
| cfr-10 | **仍开**（本轮已修） | Task 8.4 要求模型直接报告 `XYZZY`，默认 `_extract_payload`/`_extract_json_object` 都会拒绝纯文本，`InvocationResult` 没有 `result.result` 原文字段——本轮（v3）在 Phase 8 Task 8.4 改为注入探针专用 `_extract_probe_echo` parser，断言点改为 `payload["raw_text"]`（cfr2-06 同源问题） |
| cfr-11 | **仍开**（本轮已修） | ledger 写入声明中的容错在代码里未真正落实（无 `try/except`）；`capability_drift` 曾不是 ledger/DB 允许的状态词——本轮（v3）在 Phase 5 Task 5.4 的不变量 8 明确要求调用点显式 `try/except Exception`，状态词已在 Phase 1 Task 1.2/Phase 5 Task 5.3 统一 |
| cfr-12 | **仍开**（本轮已修） | retryability 分类此前仍只有"异常穿透、capability drift 不重试、其余全部重试"——本轮（v3）在 Phase 5 Task 5.3/5.4 引入显式 `retryable`/`resumable` 两个布尔位（cfr2-07 同源问题），判定逻辑更细致 |
| cfr-13 | 已关闭 | `fanout_schema.py` 已改为"先类型检查、再枚举检查"，顶层字段集合严格匹配 |
| cfr-14 | 已关闭 | `skipped_judges` 字段已落地并被测试覆盖 |
| cfr-15 | 部分关闭，仍计开放（本轮已修剩余部分） | Task 0.2 的 stdio 开关与 Phase 6 正控方向均已订正；Task 8.4 的真实 session 方向也正确，但其结果 parser/oracle 与当时接口仍不匹配——本轮（v3）随 cfr-10/cfr2-06 一并修正 |
| cfr-16 | **仍开**（本轮已修） | 处置表存在与代码不符的自填声明（"backlog 项 5/6"实际不存在于 backlog 列表）——本轮（v3）已删除这些不存在的编号引用，改为"仍开放，理由是 X"的如实表述（cfr2-09 同源问题） |
| cfr-17 | 已关闭 | `spec.md`/`plan-stage1b.md` 同步修订已落地，B1–B8 范围确认无削减 |
| cfr-18 | 已关闭 | Phase 7 任务顺序已对调，继任测试断言值已用实测值冻结 |
| cfr-19 | 已关闭 | Global Constraints 措辞已订正 |

---

## 评审处置台账（第二轮，`cfr2-01`–`cfr2-10`，`docs/harness/plan-control-flow-rewrite-review-2.md`）

> 本台账记录第二轮跨模型对抗评审的处置结果，是 v3 修订相对 v2 的完整变更索引。v3 的核心格式变化：**代码草图降级为「接口契约 + 不变量 + 测试清单」**（不再提供完整可执行函数体），理由见 Global Constraints 最后一条——第二轮评审能够独立执行 v2 的完整代码并发现 `BudgetTracker.settle()` 的算术错误，恰恰证明"写得像能跑"反而拖慢了缺陷发现，本该是 TDD 几分钟内抓到的问题却耗费了一整轮评审。

| 编号 | 严重级别 | 处置 | 落点（v3 修订后的章节） |
|---|---|---|---|
| cfr2-01 | Critical | **采纳**：`RoleInvocationRequest`/`to_invoke_kwargs()` 已定义但生产接线（Phase 6）仍直接 `deps.invoke(request)`——本轮明确要求 `cli.py` 构造真正执行 `to_invoke_kwargs()` 展开的适配闭包，`Deps.invoke` 类型标注同步更新，新增测试清单条目直接断言展开后的关键字参数集合覆盖 `invoke()` 全部必需参数 | Phase 6 Task 6.1 重写 |
| cfr2-02 | Critical | **采纳**：judge task identity 必须携带 candidate 的稳定标识（`queue.fingerprint()`），否则同一轮内两个候选依次裁决时，第二个候选的账本主键与会话身份都会与第一个候选撞车。修正：judge 角色格式从静态字符串改为 `f"judge:<type>:<fingerprint>"`，session identity/`agent_attempts.attempt_key`/`AttemptRecord.role`/账本记录四处统一使用这个 task identity | Phase 1 Task 1.1（角色枚举+正则校验）、Phase 5 Task 5.5（`judge_candidate` 消费） |
| cfr2-03 | Critical | **采纳**（两处）：(a) `BudgetTracker.settle()` 对超额成本（`actual > reserved`）不扣减，修正为 `remaining += reserved - actual`（去掉 `max(...,0.0)`），允许变负并借此自然阻止后续预留；(b) `run_wave_scheduled` 只返回每角色最终记录，聚合结算漏算失败重试已产生的真实成本，修正为返回 `WaveResult`（同时携带 `final` 与 `all_attempts`），`FanoutSettlement` 聚合改为基于全部 attempts | Phase 5 Task 5.4（`BudgetTracker`/`WaveResult`）、Task 5.6（聚合消费） |
| cfr2-04 | Critical | **采纳**：judge 调度此前未传 `expected_tools`，Bash/MCP 等能力漂移在 judge 侧不会被发现；同时状态词 `capability_drift` 曾未被 `agent_attempts` 表的 CHECK 约束接受。修正：(a) `judge_candidate` 的全部 `run_wave_scheduled` 调用补传 `expected_tools`；(b) Phase 1 Task 1.2 的状态词枚举、`ledger.record_attempt_finished` 校验、`AttemptRecord.status` 字面量三处统一为 `{'running','success','failed_transport','capability_drift'}`（去掉 v2 的 `'degraded'`，因为"降级"是编排层结论，不是单次 attempt 状态） | Phase 1 Task 1.2（状态词统一）、Phase 5 Task 5.5（`expected_tools` 透传） |
| cfr2-05 | Important | **采纳**：v2 的完整代码草图使评审需要独立执行才能发现 cfr2-03 的算术错误——这是"写得太详尽但未验证"的结构性问题，不是某一处孤立笔误。修正：全篇代码块降级为「接口契约（数据类字段+函数签名）+ 不变量（前置/后置条件列表）+ 测试清单（断言点）」，不再提供完整函数体，把"最小实现"的编写还给 TDD 的 Step 3 由实施者亲手写、亲手跑红绿 | Global Constraints（新增段落）+ 全篇 Phase 1–8 任务描述格式统一 |
| cfr2-06 | Critical（cfr-10 遗留未闭合） | **采纳**：Task 8.4 探针要求模型输出纯文本 `XYZZY`，但默认 `payload_parser` 都要求 JSON 结构，`InvocationResult` 无 `result.result` 原文字段可断言。修正：探针注入专用 `_extract_probe_echo` parser（把原始文本包成 `{"raw_text": text}`，不要求任何 JSON 结构），断言点改为 `payload["raw_text"]` 是否含 `XYZZY` | Phase 8 Task 8.4 重写 |
| cfr2-07 | Critical | **采纳**：`run_one_attempt`/波次调度器此前用 `request.session_id`（预分配、未必被 CLI 确认过的值）冒充"真实 session_id"去 fork，超时或进程未及 `init` 即被杀时该会话可能从未被 CLI 创建，`--resume` 行为未定义。修正：`AttemptRecord` 新增 `retryable`/`resumable` 两个显式布尔位——`resumable` 只在 `InvocationResult.session_id` 非空（CLI 通过 `init`/`result` 事件真正报告过）时为真；只有 `retryable and resumable` 同时成立才发起 fork 续跑，否则发起全新（非 fork）尝试 | Phase 5 Task 5.3（`AttemptRecord` 新增字段）、Task 5.4（波次调度器消费两个布尔位） |
| cfr2-08 | Important | **采纳**：Phase 1 Task 1.2 的 `ledger.py` 此前只在文字里声明"写失败不阻断本轮"，代码里没有对应 `try/except`——本轮明确该容错职责在**调用方**（Phase 5 波次调度器），`ledger.py` 函数本身不吞异常；Phase 5 Task 5.4 的不变量 8 与测试清单第 9 条要求波次调度器对账本写入调用点显式 `try/except Exception` | Phase 1 Task 1.2（职责边界声明）、Phase 5 Task 5.4（调用方容错落实） |
| cfr2-09 | Important | **采纳**：开放发现处置表（cfr-16 产出）对 rmf-06/rmf-08/rmf-14/rmf-18 四条引用了"backlog 项 5"/"backlog 项 6"，但「通用化接缝」章节的 backlog 列表实际只有 4 项（1–4），这两个编号是自填的假引用。修正：**删除**这些不存在的编号引用，改为"仍开放，理由是 X"的如实表述，不预先编造追踪编号——如需正式排期由主会话另行登记 | 开放发现处置表（本节，rmf-06/08/14/18 四行已更新） |
| cfr2-10 | Important | **采纳**：Phase 7 Task 7.5 Step 4 的验收 oracle `rg -n "Workflow" docs/harness/spec.md` 会因为 spec.md 刻意保留的大量历史 Workflow 段落而必然产生大量非预期匹配，不能用"全文件零/少匹配"做验收判据。修正：改为对 Step 1–3 明确写下的具体文本做定点存在性检查（`rg` 命中数固定为 1 的窄范围断言），不再对整个文件做宽泛扫描 | Phase 7 Task 7.5 Step 4 重写 |

**未被反驳的条目**：本轮 10 条经复核**全部成立，无一条被驳回**。cfr2-05（格式问题）与 cfr2-09（处置表自填声明问题）虽然分级为 Important 而非 Critical，但被认为是本轮修订里影响面最广的两条——前者改变了全篇文档的呈现形式，后者要求对已写下的处置结论做诚实性复核而非只看形式完整。

---

## 评审处置台账（第三轮，`cfr3-01`–`cfr3-03`，`docs/harness/plan-control-flow-rewrite-review-3.md`）

> 第三轮评审明确判定 v3（第二轮修订）"形态不是 wrong-shape"——契约化降级方向正确，八项核心设计要求中七项完整保留（`WaveResult` 全部 attempts、`BudgetTracker` 允许负余额、状态词三处统一、judge 传 `expected_tools`、`retryable`/`resumable` 分离、能力漂移不可降级、动态 `timeout_s`），新引入缺陷数从上一轮的 4 降为 0。仅剩三个阻塞项，均属"因降级丢失细节"或"三轮未真正关闭"，本表记录 v4 修订的处置。

| 编号 | 严重级别 | 处置 | 落点（v4 修订后的章节） |
|---|---|---|---|
| cfr3-01 | Critical | **采纳**：契约此前未定义生产请求的 `cwd`/`settings_path`/`model`/per-attempt `stream_log` 从哪里取得并验证——Phase 6 测试只验证 adapter 展开了哪些键，不验证值是否正确；judge task identity 虽要求贯穿 session/ledger/路由，但测试只比较了 session ID，未验证 `attempt_key`/`stream_log` 是否同样由同一 task identity 派生。修正：新增 `RequestContext`（Task 2.3）与 `build_stream_log_path`（与账本 `attempt_key` 共用拼接模板），Task 5.5 新增两个候选的联合测试（逐项断言 session ID/attempt key/stream path 均由同一含 fingerprint 的 task identity 派生），Task 6.1 新增 `_build_request_context(cfg)` 作为生产值的唯一构造点并断言其产出值确为生产真值 | Phase 2 Task 2.3（`RequestContext`/`build_stream_log_path` 定义）、Phase 5 Task 5.5（联合测试+消费）、Phase 6 Task 6.1（生产值唯一构造点） |
| cfr3-02 | Critical | **采纳**：`retryable` 字段自 cfr2-07 引入以来，赋值规则始终是"`failed_transport` 恒真、`capability_drift` 恒假"——三轮评审反复指出这只是把旧的"全部重试"策略换了个字段名字，没有做终态分类本身；`error_max_budget_usd`（预算耗尽）与确定性协议异常（重复 init/result 事件）不应被无差别当作可重试的传输故障。修正：Task 2.1 让 `InvocationResult` 暴露 `subtype` 字段（透传终态事件原始值），Task 5.3 新增可测试的终态分类表（区分预算耗尽/确定性协议异常"不可重试" vs 真实传输抖动/schema 校验失败"可重试"），`_classify_retryable` 函数体现分类表全部行，不是"看 status 字面量做二元判断" | Phase 2 Task 2.1（`subtype` 透传）、Phase 5 Task 5.3（终态分类表 + `_classify_retryable`） |
| cfr3-03 | Important | **采纳**：开放发现处置表宣称三项修复（rmf-04 的 `protocol_errors` 进 `detail`、`record_degraded` 双写 `agentType`、rmf-17 显式设规范 `model`），但对应任务的不变量与测试清单从未真正覆盖这三点——处置表记的是作者相信会做的事，不是任务清单里真正执行的断言。修正：三项全部补进**实际任务**的不变量与测试清单：`record_degraded` 双写测试补进 Task 5.2；`protocol_errors` 聚合测试补进 Task 5.6；`_format_detail` 消费聚合结果与 `model=DEFAULT_AGENT_MODEL` 显式传入的测试补进 Task 6.1 | Phase 5 Task 5.2（`agentType` 双写测试）、Task 5.6（`protocol_errors` 聚合测试）、Phase 6 Task 6.1（`_format_detail`/`model` 显式传入测试） |

**非阻塞但需明确处置的一项（rmf-08）**：第三轮评审明确指出"本计划把每轮日志从 1 份扩大到多份，属于直接放大，不是无关任务"——不能再简单写"与本次改动无关"。已处置：Phase 2 Task 2.1 把 stream 落盘改为创建时即以显式 `0o600` 模式打开（第四轮评审订正：不是"落盘后 chmod"，那样会有短暂的默认权限窗口）；脱敏与轮转/保留策略仍明确延后（需要新的设计决策，非本计划范围），但已量化说明暴露面**最坏情形放大 39 倍**（第四轮评审订正：原"7 倍"未计入 fork 重试的多次 attempt 与 judge 侧对多个候选的持续裁决，正确算法见下方 Task 2.1 补充说明），供用户择期裁决，不再是模糊或错误的表述。

**未被反驳的条目**：本轮 3 条经复核**全部成立，无一条被驳回**。cfr3-01/cfr3-03 是"因降级丢失"的细节缺口（第二轮修订在把代码草图收窄为契约时，一并收窄掉了原本存在于详尽代码里的交叉验证），cfr3-02 是三轮评审持续指出、直到本轮才真正给出可测试终态分类表的设计缺陷——三者均非评审误判，予以全部采纳。

---

## 评审处置台账（第四轮，`cfr4-01`–`cfr4-02`，`docs/harness/plan-control-flow-rewrite-review-4.md`）

> 第四轮是对第三轮三个阻塞项 + rmf-08 的**定点核验**（未做全面复评）。结果：cfr3-03 已关闭，rmf-08 部分关闭（权限位测试可证伪成立，但倍数计算与创建方式有误），**cfr3-01/cfr3-02 仍开且阻塞**。评审明确指出"只改这四条"的约束本身造成了两处接缝不一致——本表沿用 cfr4 编号记录 v5 修订的贯通处置，不再局部打补丁。

| 编号 | 严重级别 | 处置 | 落点（v5 修订后的章节） |
|---|---|---|---|
| cfr4-01 | Critical | **采纳**（两处，贯通修复）：(a) Task 5.5 v4 的联合测试 oracle 写错——要求 `session_id`（UUIDv5）与 `attempt_key`/`stream_log` 里的明文 `round_id:task_role:attempt` 段"逐字一致"，但两者类型不同不可能字面相等；订正为"三者分别与各自独立计算的预期值比对"（同源验证，不是字面相等）。(b) 契约未要求 fork 续跑时重建 `stream_log`——`build_continuation_request` 只覆盖 `prompt`/`session_id`/`resume`/`fork_session` 四个字段，`stream_log` 原样保留自上一次请求，导致 attempt 2 覆盖 attempt 1（恰是最需要判因的失败记录）的日志；订正为 `make_request` 签名改为 `(role, attempt) -> RoleInvocationRequest`，每一波都先取得该 attempt 对应的骨架请求（含随 attempt 变化的 `stream_log`），fork 只在此骨架上覆盖四个字段，不复用上一次的请求对象 | Phase 5 Task 5.4（`make_request` 签名+骨架请求+新不变量 9+测试 10）、Task 5.5（`_make_request` 签名同步+联合测试 oracle 订正+新测试 10） |
| cfr4-02 | Critical | **采纳**：v4 的终态分类表仍有两处未定义区域——(a) parser 层模型输出失败（`InvocationResult.ok=False`、**`subtype="success"`**、`protocol_errors` 含 `"unparseable or malformed payload"`）进不了表中任何一行，因为它既不是 `subtype is None`（超时）也不是 `error_max_budget_usd`，`validate()` 更不会被调用（`invocation.ok` 已经是 `False`）；(b) `subtype=None` 被一律判可重试，但 CLI 启动/认证失败同样不产生终态事件、同样是 `subtype=None`，重试无意义。修正：v5 分类表补齐两行——`subtype="success"` 但 payload 解析失败→可重试（与 schema 校验失败同源）；`protocol_errors` 含 `"missing init event"`（CLI 从未完成协议握手的特征）→ 不可重试，与"真正超时但 `protocol_errors` 为空"的既有可重试分支明确区分；补充优先级顺序不变量与对应测试 | Phase 5 Task 5.3（终态分类表 v5，新增不变量 9，新增测试 14/15/16） |

**非阻塞项复核（rmf-08）**：第四轮指出 v4 的处置有两处需订正——(a) `_persist_stream()` 描述为"落盘后 chmod"，存在从默认权限创建到 `chmod` 生效之间的短暂窗口；已订正为直接以 `os.open(..., 0o600)` 创建，不再有事后 `chmod` 步骤（见 Task 2.1）。(b) 放大倍数"7 倍"计算有误——遗漏了 fork 重试（每个 attempt 各写一次 stream_log）与 judge 侧对多个候选的持续裁决（redline 否决一个候选后仍会裁决下一个候选，不是恒定角色数）；已订正为**最坏情形 39 个文件**（4 finder × 最多 3 次尝试 = 12 + 最多 3 个候选 × 最多 3 个 judge × 最多 3 次尝试 = 27），倍数按 39 计（见 Task 2.1 与开放发现处置表 rmf-08 行）。**这处订正是本计划已转述给用户的一处错误数字，属于必须修正而非润色的类别**。

**未被反驳的条目**：本轮 2 条经复核**全部成立，无一条被驳回**。评审对协调者"只改四条是否造成接缝不一致"的提问给出了肯定回答，两条阻塞项确实分别源于"局部改了一处、遗漏了贯通到相邻任务"——cfr4-01 的联合测试 oracle 错误与 stream_log 未随 attempt 更新是同一个契约缺口的两个表现（`build_continuation_request` 只知道覆盖哪些字段，不知道"骨架请求应该随 attempt 变化"这件事此前完全没写进契约）；cfr4-02 的两处遗漏都是"分类表只覆盖了此前想到的情形，没有系统性枚举 `InvocationResult` 全部可能的字段组合"。本轮采用"贯通修复"而非继续局部打补丁，正是针对这个根因。

---

## 开放发现处置表（cfr-16：`code-review-realmachine-fixes.md` 中仍开放且落在本次改动模块上的条目）

> 退役 JS/`TaskOutput`/`Skill`/`Workflow` 只能关闭这些发现里"由外层会话/Workflow 编排导致"的那部分触发路径，**不会自动关闭**与 `claude_runner.invoke()`/`round.py` 本身相关的部分。以下逐条核查：本次改动是否让该发现的根因消失、还是仍需在新代码里显式处理。

| rmf 编号 | 原发现 | 本次改动后的状态 | 处置 |
|---|---|---|---|
| rmf-04 | `invocation-failed`/`capability-drift` 分支只带 `raw_tail`（末 5 行），`protocol_errors` 结论被丢弃 | **仍开放，且扇出后影响面扩大**——每轮最多 7 次独立子调用，每次失败都可能需要这条判因链路，而不是过去"一次顶层调用" | **新增处置**：Phase 5 的 `AttemptRecord`（见 Task 5.3）已含字段 `protocol_errors: list[str]`，直接透传 `InvocationResult.protocol_errors`；Phase 6 的 `FanoutSettlement` 聚合时保留每个失败角色的 `protocol_errors`，写入 `round.py` 返回的 `detail` 字段（沿用 rmf-04 建议的 `"; ".join(protocol_errors) or raw_tail` 拼接方式）。落点：Phase 5 Task 5.3/5.6、Phase 6 Task 6.1 |
| rmf-05 | 成本已知时仍按预留满额计费 | **已被 `round.py` 现状代码修复**（`_settle_failed()` 按 `cost_known` 分支），本次改动不改这段逻辑，且 Phase 5/6 的新增 `_BudgetTracker`（cfr-05 修复）与其正交——`_BudgetTracker` 管"本轮扇出内部的并发预留"，`_settle_failed`/`budget.settle`/`budget.abandon` 管"整轮对 `budget_days` 的最终结算"，两层不冲突 | 无需处置，确认不回归 |
| rmf-06 | env "deny-by-default" 是前缀级黑名单而非白名单，`CLAUDECODE` 等变量穿透 | **不在本次改动范围**——`_sanitize_env()` 完全不因扇出重写而改变，七个子调用复用同一份 `_sanitize_env()` 逻辑（每次调用各自构造 argv 与 env，但函数本身不变） | **仍开放，理由是**：修复 `_sanitize_env` 的白名单化是独立的安全加固任务，与"扇出架构从 JS 迁到 Python"无逻辑依赖，混在本计划里会扩大变更面而不利于审查。**不落 backlog 编号**（第二轮评审 cfr2-09 指出此前登记的"backlog 项 5"并不存在于下方「通用化接缝」章节的 backlog 列表里，是自填的假引用）——如需正式排期，应在本计划提交合入后由主会话另开一个独立 issue/backlog 条目登记，本计划只如实记录"仍开放、未处置、原因如上"这个状态，不预先编造一个不存在的追踪编号 |
| rmf-08 | stream 落盘默认权限 0644、无脱敏、无轮转、无限增长 | **本次改动扩大暴露面**：原来一轮一个 stream 文件，扇出后一轮**最坏情形 39 个**（4 finder × 最多 3 次尝试 = 12 个，最多 3 个候选 × 最多 3 个 judge × 最多 3 次尝试 = 27 个，第四轮评审订正：v4 曾错误估算为"7 个/7 倍"，遗漏了 fork 重试的多次 attempt 与 judge 侧对多个候选的持续裁决——redline 否决一个候选后扇出仍会继续裁决下一个候选，不是恒定 5 个） | **权限部分已处置**：Phase 2 Task 2.1 已把 `_persist_stream()` 改为创建时即以显式 `0o600` 模式打开（不是"落盘后 chmod"，避免中间权限窗口，第四轮评审订正），并有测试断言权限位与创建方式（第三/四轮评审 cfr3/cfr4 附带处置，成本最低、可在本计划内直接完成）。**脱敏与轮转/保留策略仍明确延后，非"与本次改动无关"**：这两项需要新的设计决策（脱敏规则、保留窗口长度），不是本计划可以顺手做的一行修改；已量化说明放大倍数（**最坏情形 39 倍**，非此前错误估算的 7 倍）供用户择期裁决是否值得单独立项，不越权替用户决定优先级，不落 backlog 编号（同一处 cfr2-09 反例，见上） |
| rmf-14 | `TaskOutput` 是官方标记 `[Deprecated]` 的工具，CLI 无版本钉死 | **本次改动直接消灭该发现的触发对象**——`TaskOutput` 随 Phase 6/7 完全退出 `STAGE1_ALLOWED_TOOLS` 与 `harness-settings.json`，新架构不再依赖任何后台任务通知机制 | **确认关闭（部分）**：`TaskOutput` 依赖本身随本计划核心设计目标消灭（ADR D1 就是为了消灭对 `TaskOutput`/`Workflow` 的依赖）。**CLI 版本钉死这条建议仍开放，理由是**：与本次架构重写无直接关联，是通用的可用性加固（`precheck` 里加 `claude --version` 断言），**不落 backlog 编号**（同上 cfr2-09 反例，此前的"backlog 项 6"同样是不存在的假引用） |
| rmf-16 | `.claude/systemd/` 需调整三处（`flock -E`、日志轮转、`OnFailure`） | **不在本次改动范围**——systemd 单元文件本身不受扇出架构影响，`round.py` 对外的 CLI 接口（`python3 -m harness.cli round`）签名不变 | 无需处置，非本次改动触及的文件；`.claude/systemd/` 不在本计划「文件结构」清单内 |
| rmf-17 | 内层 13 个 agent 用别名 `'sonnet'` 而非规范模型 ID | **本次改动直接消灭该发现的触发对象**——旧架构的"内层 agent" 概念本身消失（不再有 Workflow 内部的 `agent(prompt, {model:'sonnet'})` 调用），新架构里每个 finder/judge 都是顶层 `invoke()` 调用，Phase 2 的 `RoleInvocationRequest.model` 字段要求显式传入 | **确认关闭并加固**：Phase 6 `round.py` 接线时为每个角色的 `RoleInvocationRequest` 显式设置 `model=DEFAULT_AGENT_MODEL`（复用 `claude_runner.DEFAULT_AGENT_MODEL` 规范 ID 常量，与 rmf-17 建议一致），新增测试断言"七个角色的调用请求 `model` 字段均等于 `DEFAULT_AGENT_MODEL`"。落点：Phase 6 Task 6.1 |
| rmf-18 | `_HARNESS_OWNED_CLAUDE_ENV` 语义与名字相反（惰性集合） | **不在本次改动范围**——该集合与 env 消毒逻辑本身不因扇出重写而改变 | **仍开放，理由同 rmf-06**——与 `_sanitize_env` 周边的独立加固同批次，不落 backlog 编号（同一处 cfr2-09 反例） |

**评审 cfr-16 指出的具体反例复核**：「`_describe_degraded()` 读 `agentType/label`，而新记录只有 `role`，日志会显示 `?×N`」——**已确认属实**（`round.py` 现有 `_describe_degraded()` 读 `d.get('agentType') or d.get('label')`，Phase 5 v1 草图的 `record_degraded()` 只写 `role` 字段）。处置：Phase 5 Task 5.4 重写时，`record_degraded()` 写入的字典**同时**含 `role`（新字段，供 Python 内部消费）与 `agentType`（等于 `role`，供 `round.py` 现有 `_describe_degraded()` 不加修改就能正确读取），避免因为字段改名产生 `?×N` 的可观测性回归；或者反过来修改 `round._describe_degraded()` 读 `role` 字段——**本计划选择前者（双写字段）**，因为 `_describe_degraded()` 不在改动白名单之外，但既然 Phase 6 本来就要碰 `round.py`，改这一行属于同一提交的自然范围,选哪种都可行,取双写是因为改动面更小、不需要同时确认没有其它调用点读 `agentType`。

---



## 待决项（写给主会话/实施者，推荐方案已给出，非阻塞式）

以下两点是本计划内部的技术路线选择，**不改变外部行为契约**（发布产物、候选 DTO 形状、崩溃恢复语义均不变），因此不构成需要转交裁决的架构分叉；但因为它们决定了后续所有实现细节的形状，在此明示，供实施者/评审在 Phase 0 完成后据实测结果确认或调整。

### 待决 A：会话原语用「扩展现有单发 `invoke()`」还是「采用 PoC 的 dual-pipe 驱动」

**推荐：扩展现有单发 `invoke()`**（`claude_runner.py` 现有的 `subprocess.run(capture_output=True)` 模式），只新增 `session_id`/`resume`/`fork_session` 三个可选参数传进 `build_argv`。

- 理由：Stage 1 的每个 finder/judge 调用逻辑上是**单轮问答**（一个 prompt，等一个 `result`），不需要 PoC driver.py 那种长命进程 + 多轮 stdin 喂入的能力。PoC 的 Q1–Q3（双 pipe 多轮）、Q4（`can_use_tool` 拦截）在 Stage 1 均用不上（Stage 1 只读工具不触发 `can_use_tool`，见待决问题 5 的正式结论）。Q5（fork 重试）是本计划**唯一**要用的能力，而 Q5 的 PoC 复现虽然用了 dual-pipe 传输，但 `--resume`/`--fork-session`/`--session-id` 三个标志本身是会话解析层的语义，与「输入是通过 stdin 流式喂入还是通过 `-p` 单发参数传入」是正交的两件事——**这是推断，非 PoC 直接实测的组合**，因此设为 Phase 0 的第一个 go/no-go 验证任务，用一次真机探针（约 $0.05）验证「`-p` 单发 + `--session-id` 首次调用」→「`-p` 单发 + `--resume <sid> --fork-session` 二次调用」的组合确实可行。
- **备选（未采纳）**：把 PoC 的 `exp/stdio-driver/driver.py` 的 `Invocation` 类整体产品化为新会话运行模块。**不采纳理由**：引入线程化的 stdin/stdout 双向管道读写，代码复杂度与故障面（死锁、部分行、背压）显著高于现有单发模式，而 Stage 1 用不上它解决的任何问题（多轮、`can_use_tool` 拦截）。若 Phase 0 的验证失败（单发模式不支持 resume/fork），**才**转向此备选，届时把 driver.py 的读写线程模型移植进 `claude_runner.py`（保留现有 env 消毒/argv 校验逻辑，只替换 `subprocess.run` 为线程化 `Popen`）。

### 待决 B：迁移策略——一次性替换 vs 新旧并存

**推荐：一次性替换，不做并存。**

- 理由：`.claude/systemd/scrollz-harness.timer` 目前是 `disabled`/`inactive`（ADR 头部已注明「2 小时定时器在重写完成前不启用」），没有实时流量需要与旧路径并行验证。维护两条编排路径（旧 JS workflow + 新 Python 扇出）会重复付出「两套降级/短路/去重逻辑保持一致」的成本，且没有对应收益——旧路径唯一验证过的价值（真机跑通、发布 Issue #1）已经被记录在 HANDOVER，不会因为删除代码而丢失。
- **备选（未采纳）**：保留旧 JS 路径一段时间、用 feature flag 切换。**不采纳理由**：`round.py` 里两条路径共存意味着 `_run_round_body` 需要分叉成两套预算/截止时间计算逻辑，这类分叉正是历史上多次被评审抓到「两份实现互相漂移」的形态（`STAGE1_TOOLS`、canonical key 跨语言）；用户可随时 `git revert` 到重写前的提交回退，不需要代码层面的双轨制。

---

## 文件结构（新增/修改一览）

| 路径 | 变化 | 职责 |
|---|---|---|
| `.claude/scripts/harness/claude_runner.py` | **改** | 新增 `session_id`/`resume`/`fork_session` 参数；`STAGE1_ALLOWED_TOOLS` 收窄为 `{Read,Grep,Glob}` |
| `.claude/scripts/harness/session_identity.py` | **新** | `derive_session_id(round_id, role, attempt)` 纯函数 |
| `.claude/scripts/harness/fanout_schema.py` | **新** | finder/judge 输出 JSON 的 Python 侧结构校验（原 JS `CANDIDATE_SCHEMA`/`JUDGE_SCHEMAS`） |
| `.claude/scripts/harness/fanout.py` | **新** | `run_finders()`/`run_judges()`：并发调用、批次重试、降级归类、redline 短路 |
| `.claude/scripts/harness/prompts.py` | **新** | 读取 `.claude/agents/harness-*.md`，拼装完整 prompt（frontmatter 解析 + persona 正文 + 任务指令） |
| `.claude/scripts/harness/ledger.py` | **新** | `agent_attempts` 表的读写：记录每次子调用的谱系、成本、结果（纯审计，非崩溃恢复关口） |
| `.claude/scripts/harness/db.py` | **改（仅新增表）** | `CREATE TABLE IF NOT EXISTS agent_attempts (...)` |
| `.claude/scripts/harness/round.py` | **改** | `_run_round_body` 的调用段替换为 `fanout.run_finders`/`run_judges`；`STAGE1_TOOLS` 随 `claude_runner` 收窄；截止时间分配改为「每次子调用前重算剩余时间」 |
| `.claude/harness-settings.json` | **改** | `permissions.allow` 收窄，删除 `Skill`/`Workflow`/`TaskOutput` |
| `.claude/scripts/harness/tests/test_session_identity.py` | 新 | |
| `.claude/scripts/harness/tests/test_fanout_schema.py` | 新 | |
| `.claude/scripts/harness/tests/test_fanout.py` | 新 | |
| `.claude/scripts/harness/tests/test_prompts.py` | 新 | |
| `.claude/scripts/harness/tests/test_ledger.py` | 新 | |
| `.claude/scripts/harness/tests/test_claude_runner.py` | 改（追加用例） | |
| `.claude/scripts/harness/tests/test_round.py` | 改（重写扇出相关用例） | |
| `.claude/scripts/harness/tests/test_canonical_key_cross_language.py` | **删** | 见 Phase 7 |
| `.claude/workflows/scrollz-propose.js` | **删** | 见 Phase 7 |
| `.claude/skills/scrollz-round/` | **删** | 见 Phase 7 |
| `.claude/workflows/tests/degraded-dedup.test.mjs` | **删**，逻辑迁入 `test_fanout.py` | 见 Phase 7 |
| `docs/harness/redlines.yaml` | **改（仅补充 reason 说明）** | 保留 `.claude/workflows/`/`.claude/skills/` 路径条目不删（防止未来重新引入） |

---

## Phase 0 · 会话原语真机验证（go/no-go，花真钱但零外部写入）

> ### ✅ 已执行（2026-08-02，提交 `77a0237`，实花约 $0.10 / 预算 $2，零外部写入）
>
> 结论文档：[`exp/control-flow-rewrite-probe/CONCLUSIONS.md`](../../exp/control-flow-rewrite-probe/CONCLUSIONS.md)
>
> - **Task 0.1 / 待决 A：`confirmed`** —— 单发 `-p` 下 `--session-id` 被原样接受、`--resume --fork-session` 产生新 ID 且完整保留上文。**Phase 2 按推荐路线扩展现有 `invoke()`，不转 dual-pipe，接口形状不变。**
> - **Task 0.2 / 设计问题 5：`confirmed`，但本节下文原本的预设被推翻。**
>
> **⚠️ 下文「理论上只读工具不应触发」这句是错的，实施时不要采信。** 实测：开启 `--permission-prompt-tool stdio` 后，一次 `Read /etc/hostname` **确实产生了 `control_request`**。
>
> **最终决策不变（Stage 1 不启用 stdio），但理由改为：** 启用它会给每次 `Read`/`Grep`/`Glob` 都带来一个必须应答的 `control_request`——每轮最多 39 次子调用，等于凭空引入一个必须正确实现的 control 循环，而收益为零（工具集已收窄到只读三件，`permissions.allow` 是主防线，没有需要拦截的写操作）。
>
> **准确表述**：只读工具**并不豁免**权限门；它们在生产配置（`--permission-mode dontAsk` + `permissions.allow`，不带 stdio 标志）下不触发，是因为 allow 列表**预先放行**了，不是因为工具只读。`can_use_tool` 的触发面取决于「工具 × 权限模式 × allow 列表」的组合，**不能凭工具的读写属性推断**。


**目标**：在写一行生产代码之前，验证「待决 A」的推荐路线（单发 `-p` + `session_id`/`resume`/`fork-session`）在真机上成立，并对设计问题 5（Stage 1 只读工具是否需要 `--permission-prompt-tool stdio`）给出实测结论而非推断。这一阶段**不属于 TDD 五步**（它是可行性探针，不是产品代码），产物是一份实测结论文档 + 复现脚本，仿照 `exp/stdio-driver/` 的证据纪律（`report_id`/`finding_id`/`conclusion_strength`/字节级证据）。

**为什么要单列一个 Phase 而不是在 Phase 2 里顺带验证**：如果推断被证伪（`-p` 单发模式不支持 `--resume --fork-session`，必须走 dual-pipe），Phase 2 及以后所有任务的 `claude_runner.py` 接口形状都要改。提前在 Phase 0 花 $1 以内验证清楚，比在 Phase 5 发现推断有误再返工便宜。

**预算**：预留 $2（5 次调用，每次 ≤ $0.30，参考 PoC 实测单价）。

### Task 0.1：验证「`-p` 单发 + session_id 首次 + resume/fork-session 续跑」的组合

- [ ] **Step 1**：写探针脚本 `exp/control-flow-rewrite-probe/probe_resume.py`（只用标准库 `subprocess`），复用 `claude_runner.py` 现有 `build_argv`/`invoke` 的 env 消毒与 argv 校验逻辑（`import` 现有模块，不复制代码），执行：
  1. `invoke(prompt="Remember the codeword PLUM. Reply exactly OK.", tools="", grant_usd=0.15, max_turns=3, settings_path=..., cwd=..., timeout_s=60, session_id=<uuid4>)`（新参数，Phase 2 才实现——此处先用一个不依赖生产代码改动的最小裸调用，直接拼 argv，不导入未实现的接口）。
  2. 断言 `result.session_id == <传入的uuid>`（若 `InvocationResult` 尚无该字段，探针脚本直接解析 stdout 的 `init` 事件取 `session_id`，不依赖生产代码）。
  3. `invoke(prompt="What was the codeword? Reply exactly CODE:<word>.", tools="", grant_usd=0.15, max_turns=3, resume=<同一 session_id>, fork_session=True, ...)`（单发 `-p` 模式，非 dual-pipe）。
  4. 断言第二次调用的 stdout 里 `result.result == "CODE:PLUM"` 且返回的 `session_id` **不等于**第一次的（fork 产生新 ID，与 PoC Q5 一致）。
- [ ] **Step 2**：跑脚本，记录退出码、`total_cost_usd`、完整 stdout 落盘到 `exp/control-flow-rewrite-probe/artifacts/`。
- [ ] **Step 3**：写结论文档 `exp/control-flow-rewrite-probe/CONCLUSIONS.md`，仿 PoC 格式给出 `conclusion_strength: confirmed|refuted`。
  - **若 confirmed**：Phase 2 按待决 A 推荐路线（扩展 `invoke()`）实施，不做变更。
  - **若 refuted**（例如单发模式下 `--resume --fork-session` 报错、或 fork 后无法读到第一轮上下文）：在此记一条「待决 A 推翻，转 dual-pipe」的决定，Phase 2 改为把 `exp/stdio-driver/driver.py` 的 `Invocation` 读写线程模型移植进 `claude_runner.py`（作为新函数 `invoke_stateful()`，与现有单发 `invoke()` 并存，因为普通单轮调用仍用单发省资源）。此时 Phase 2 的任务描述需相应展开为两个子任务（本计划到时候由实施者在 plan 里追加，不在这里预先写死不确定的代码）。
- [ ] **Step 4**：提交 `exp/control-flow-rewrite-probe/`（探针代码 + 结论文档 + 落盘证据，不含 `wire.*.bin` 之外的大文件）。

```bash
cd /home/xp/src/zipfs
git add exp/control-flow-rewrite-probe/
git commit -m "docs(harness): Phase 0 会话原语真机验证（session_id/resume/fork-session 单发模式）" -- exp/control-flow-rewrite-probe/
```

**正控**：本任务无生产实现，正控不适用（它本身就是可行性验证）。

### Task 0.2：验证 Stage 1 只读工具集是否触发 `can_use_tool`（设计问题 5 的实测结论）

**背景**：spec.md §9.1 已明确「Stage 1 的 `--tools` 不含 `Bash`/`Edit`/`Write`」，本计划进一步把工具集收窄到 `{Read,Grep,Glob}`。ADR 反例表明「本地分类器自动放行的安全 Bash 不产生 `can_use_tool`」，但**没有实测过 `Read`/`Grep`/`Glob` 本身是否会触发权限请求**（理论上只读工具不应触发，但本计划遵守「不猜、做便宜的 PoC」的项目纪律）。

**cfr-15 修正**：v1 草图的 Step 1 只用现有 `invoke()`（不带 `--permission-prompt-tool stdio`）跑一次 `Read` 调用，然后断言"stdout 无 `control_request`"——**这个 oracle 与断言的对象不是同一件事**：不带该标志时，CLI 根本不会产出任何 `control_request` 事件，不论被调用的工具是否本该触发权限请求。"没看见 `control_request`"在这个配置下永远为真，不能说明"Read 不需要权限确认"，只能说明"没打开能观察到它的开关"。必须先打开开关才能做出有意义的判断（正控原则的另一种表现——先确认负控本身有能力探测到目标现象，再采信它的阴性结果）。

- [ ] **Step 1（修正）**：用 PoC `driver.py` 现有的 `Invocation` 类（或参照其 `run_safe_bash`/`run_permission_deny` 的构造方式），起一个**带** `--permission-prompt-tool stdio` 的进程：`tools="Read"`，`extra_args=["--permission-prompt-tool", "stdio"]`，prompt 要求"读取 `/etc/hostname` 并报告内容"。用一个 `on_control` 回调**主动监听并记录**所有 `control_request` 事件（不论收到什么都先记下再回 `allow`，确保即便真的触发也不会卡住探针）。
- [ ] **Step 2**：跑脚本，记录 `control_request` 事件列表（可能为空，也可能非空）。
- [ ] **Step 3**：记录结论：
  - 若确认整个会话过程 `control_request` 事件列表为空（预期结果，且**这次是在打开了拦截开关的前提下观察到的空**，结论才有效）：**正式回答设计问题 5** ——Stage 1（本次重写覆盖的 finder/judge 只读扫描）**不使用** `--permission-prompt-tool stdio`，因为没有需要拦截的工具调用；主防线仍是 `--tools` allowlist + `harness-settings.json` 的 `permissions.allow`（与现状一致，只是集合收窄）。**Stage 2（开发轮，要写代码）时才需要它**：届时 agent 会拿到 `Bash`/`Edit`/`Write`，`--permission-prompt-tool stdio` 提供的「拦截—校验—回填」是控制器审查每一次写操作参数的手段，届时在 Stage 2 的独立计划里设计其 `control_request` 处理循环（本计划不展开，登记进 backlog）。
  - 若任一只读工具意外触发（不预期，需追查）：记为 `needs_decision`，本计划的 Phase 2/5 需追加处理该 `control_request` 的最小回填逻辑（一律 allow，因为只读工具无害），并把这条从「结论」降级为「已知例外」写入 Phase 2 任务说明。
- [ ] **Step 4**：结论并入 Task 0.1 的同一份 `CONCLUSIONS.md`，一并提交。

**验收判据（Phase 0 整体）**：`CONCLUSIONS.md` 对待决 A 与设计问题 5 均给出 `confirmed` 或 `refuted` 结论，不遗留「假设」；设计问题 5 的结论必须基于"确实打开了拦截开关后观察到的结果"，不得基于"未打开开关时的沉默"。

---

## Phase 1 · 会话身份派生 + 谱系账本（纯函数 + 新表，零 IO 依赖之外）

**目标**：实现 ADR D1「按 `(round_id, role, attempt)` 确定性派生 session_id」与 D2「fork 重试谱系可审计」的数据层基础。本阶段产出两个独立、可单测的模块，不涉及真实 `claude` 调用。

**设计回答（问题 2：session 身份怎么定）**

```python
def derive_session_id(round_id: str, role: str, attempt: int) -> str:
    """确定性派生 UUID v5（namespace + name），同输入必产同输出。

    role ∈ {"finder:roadmap","finder:code","finder:bench","finder:hygiene",
            "judge:redline","judge:completed","judge:oracle"}
    attempt 从 1 开始（首次尝试）；fork 重试产生 attempt=2,3,...
    """
    name = f"{round_id}:{role}:{attempt}"
    return str(uuid.uuid5(_HARNESS_SESSION_NAMESPACE, name))
```

- **与幂等键的关系**：`derive_session_id` 的输出**只用作 `claude --session-id` 参数**，不是 outbox 的 natural key。outbox 幂等键的定义（`round.py` 现有的 `fingerprint()` / `Outbox` 的 `(kind, natural_key)` 唯一索引）完全不变——candidate 一旦产出，走的还是现有的 `publish_proposal`/`commit_proposal`/`push_main`/`publication_receipt` 四个 operation，与本次扇出改动无关。session_id 解决的是**同一个逻辑角色在同一轮内的会话身份稳定性**（用于 attempt 1 失败后 attempt 2 能 `--resume` 到正确的会话），是编排层内部状态，不进 outbox。
- 为什么用 `uuid5` 而非哈希截断字符串：`--session-id` 要求合法 UUID 格式（PoC 已用 `str(uuid.uuid4())` 验证格式接受），`uuid5(namespace, name)` 天然产出合法 UUID 且确定性——同一 `(round_id, role, attempt)` 任何时候调用都得到同一 ID，无需持久化「本轮用过哪些 ID」这件事本身（虽然仍会持久化到账本用于审计，见下）。

**设计回答（问题 3：fork 重试谱系记录）**——**新增一张纯追加表**，不改任何既有表（延续本库「只追加表」不变量，与 `proposal_keys` 表先例一致）。

> **schema 的权威定义在 Task 1.2，不在这里。** 本节此前重复了一份 `CREATE TABLE` 全文，v2→v3 修正状态词（cfr2-04）时只改了 Task 1.2 那份，这里留下了含 `'degraded'`、缺 `'capability_drift'` 的旧版——**照旧版实现出的 CHECK 会让能力漂移在写账本时抛 `sqlite3.IntegrityError`，正是 cfr2-04 要修的那个缺陷**。这是本项目反复出现的「第二份硬编码真相」形态（`STAGE1_TOOLS` 同源），故本节改为只描述**意图**、不再复制 DDL，实施时以 Task 1.2 为准。
>
> 意图：以 `attempt_key = f"{round_id}:{role}:{attempt}"` 为主键；记录 `session_id`（`derive_session_id` 输出）与 `parent_session_id`（`attempt>1` 时指向 fork 源，`attempt=1` 为 `NULL`），使「第 N 次尝试」这条谱系可审计；带 `status`/`cost_usd`/`turns`/`created_at`/`ended_at` 供结算与判因；按 `round_id` 建索引。状态词枚举见 Task 1.2 的「状态词统一」小节。

`ledger.py` 提供三个函数：`record_attempt_started(conn, round_id, role, attempt, session_id, parent_session_id)`、`record_attempt_finished(conn, attempt_key, status, cost_usd, turns)`、`attempts_for_round(conn, round_id) -> list[dict]`（供审计/`status` CLI 命令未来展示谱系用；本计划不新增 CLI 命令，只留查询函数，CLI 展示留 backlog）。这张表是**纯审计**，不是崩溃恢复的判定依据——`fanout.py`（Phase 5）的重试判定只依赖内存中本轮的执行状态，账本写失败不得阻断本轮（与 `_persist_stream` 的「落盘失败不影响结论」纪律一致）。

### Task 1.1：`session_identity.py`

**接口契约**：

```python
ROLES: frozenset[str]  # 见下方"角色枚举"
def derive_session_id(round_id: str, role: str, attempt: int) -> str: ...
```

**角色枚举（v3 修正 cfr2-02：judge 角色不再是静态的 7 个字符串，而是要能携带 candidate 身份）**：

- finder 角色维持静态：`finder:roadmap`、`finder:code`、`finder:bench`、`finder:hygiene`。
- **judge 角色不能再是静态的 `judge:redline`/`judge:completed`/`judge:oracle`**——第二轮评审 cfr2-02（Critical，已核实）指出：一轮扇出可能对多个候选依次裁决（`run_fanout` 对 `ranked` 列表逐个调用 `judge_candidate`），若两个候选都跑 `judge:redline`，`derive_session_id(round_id, "judge:redline", 1)` 对两个候选算出**同一个** session_id，`agent_attempts` 的主键 `attempt_key = f"{round_id}:{role}:{attempt}"` 也撞在一起——第二个候选的账本写入会因主键冲突失败，且实际上是在用第一个候选的 session 去发第二个候选的裁决请求。
- **修正**：judge 的 task identity 必须包含**candidate 的稳定标识**（候选的 `fingerprint`，`queue.fingerprint()` 已经产出的确定性摘要，不是运行时生成的随机值）。`derive_session_id` 的 `role` 参数对 judge 场景传入 `f"judge:redline:{fingerprint}"` 这种形式（`fingerprint` 是 `queue.fingerprint(goal, invariant, primary_path, oracle)` 的输出，32 位十六进制字符串）。`ROLES` 集合的校验方式相应从"精确匹配 7 个字符串"改为"finder 角色精确匹配 4 个字符串之一；judge 角色匹配 `^judge:(redline|completed|oracle):[0-9a-f]{32}$` 正则"。
- **同一 task identity 贯穿四处**（cfr2-02 要求）：session identity（本函数输出）、`agent_attempts.attempt_key`（Task 1.2）、`fanout.py` 内部路由角色的 dict key（Phase 5）、Phase 2 `RoleInvocationRequest.stream_log` 的路径构造，**全部使用同一个"role 字符串"作为唯一标识源**，不允许四处各自拼接出可能不一致的字符串。

**不变量**：
- 同一 `(round_id, role, attempt)` 任何时候求值得到同一 UUID（确定性，供 attempt 1 预先算出 `--session-id`）。
- 不同 `round_id`/`role`/`attempt` 中任一项不同，输出必须不同（无理论碰撞保证需求，`uuid5` 的抗碰撞性质已足够）。
- 输出必须是合法 UUID 格式字符串（`uuid.UUID(output)` 不抛异常）。
- 非法角色格式（finder 角色不在 4 个字符串内、judge 角色不匹配上述正则）抛 `ValueError`；`attempt < 1` 或非整数同样抛 `ValueError`。

**测试清单**（断言点，不是完整测试源码）：
1. 确定性：同输入两次求值相等。
2. 合法 UUID 格式。
3. `round_id`/角色/`attempt` 三者任一不同则输出不同（三条独立用例）。
4. 未知 finder 角色字符串抛 `ValueError`。
5. **新增**（cfr2-02）：两个不同 `fingerprint` 的 `judge:redline:<fp1>` 与 `judge:redline:<fp2>` 产生不同 session_id——这是本次修正要防止的回归的直接验证。
6. **新增**：judge 角色字符串格式不合法（如 `judge:redline:short`，fingerprint 长度不对）抛 `ValueError`。
7. `attempt < 1` 或非 `int` 抛 `ValueError`。

- [ ] **Step 1**：按上方测试清单写 `.claude/scripts/harness/tests/test_session_identity.py`，跑至因模块不存在而红。
- [ ] **Step 2**：按接口契约实现 `.claude/scripts/harness/session_identity.py`（`uuid.uuid5(命名空间UUID, f"{round_id}:{role}:{attempt}")`，命名空间 UUID 任取一个固定值，全仓库唯一即可，不承载业务语义）。
- [ ] **Step 3**：跑通全部用例；正控——临时把 `uuid5` 换成 `uuid4`（每次随机），确认确定性用例变红；改回。
- [ ] **Step 4**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/session_identity.py .claude/scripts/harness/tests/test_session_identity.py
git commit -m "feat(harness): 会话身份确定性派生 derive_session_id（含 judge task identity 携带 candidate fingerprint，cfr2-02）" -- .claude/scripts/harness/session_identity.py .claude/scripts/harness/tests/test_session_identity.py
```

### Task 1.2：`agent_attempts` 表 + `ledger.py`

**接口契约**：

```sql
CREATE TABLE IF NOT EXISTS agent_attempts (
    attempt_key       TEXT PRIMARY KEY,   -- f"{round_id}:{role}:{attempt}"，role 用 Task 1.1 的统一 task identity
    round_id          TEXT NOT NULL,
    role              TEXT NOT NULL,      -- finder 角色或 "judge:<type>:<fingerprint>"
    attempt           INTEGER NOT NULL,   -- 1 起
    session_id        TEXT NOT NULL,      -- attempt=1 为 derive_session_id 的输出；attempt>=2 为 CLI 实际返回的新 session id
    parent_session_id TEXT,               -- attempt>1 时指向上一次的**真实** session_id（fork 源）；attempt=1 为 NULL
    status            TEXT NOT NULL CHECK (status IN (
                          'running', 'success', 'failed_transport',
                          'capability_drift')),  -- 见下方"状态词统一"
    cost_usd          REAL,
    turns             INTEGER,
    created_at        REAL NOT NULL,
    ended_at          REAL
);
CREATE INDEX IF NOT EXISTS idx_agent_attempts_round ON agent_attempts(round_id);
```

```python
def record_attempt_started(conn, *, round_id: str, role: str, attempt: int,
                           session_id: str, parent_session_id: str | None) -> None: ...
def record_attempt_finished(conn, *, attempt_key: str, status: str,
                            cost_usd: float, turns: int) -> None: ...
def attempts_for_round(conn, round_id: str) -> list[dict]: ...
```

**状态词统一（v3 修正 cfr2-04，Critical，已核实）**：第二轮评审发现 Phase 5（Task 5.3）的 `AttemptRecord.status` 用了字面量 `"capability_drift"`（下划线），而本表 v2 版本的 `CHECK` 约束只允许 `('running','success','degraded','failed_transport')`——写入 `capability_drift` 状态会直接抛 `sqlite3.IntegrityError`，而不是产出结构化的 `capability-drift` round 结果，能力漂移这个"本该被显式判定为整轮失败"的场景反而在写账本这一步意外崩溃。**修正**：本表的 `status` 枚举、`ledger.record_attempt_finished` 的合法值校验、`AttemptRecord.status` 的字面量（Phase 5 Task 5.3）三处**必须逐字相同**——本任务把统一后的枚举值定为 `{'running', 'success', 'failed_transport', 'capability_drift'}`（**去掉了 v2 的 `'degraded'`**，因为"降级"是 `round.py`/`fanout.py` 编排层的结论，不是单次 attempt 的状态；单次 attempt 只有"成功""传输失败""能力漂移"三种终态，"该角色本轮整体降级"是波次调度器耗尽重试后的**外部**结论，不需要在 `agent_attempts` 表里对应一个状态值）。Phase 5 Task 5.3 的 `AttemptRecord` 定义与本表引用同一个概念性状态词表，实施时须交叉核对。

**不变量**：
- `attempt_key` 全局唯一（主键），构造方式与 Task 1.1 的 task identity 完全一致。
- `record_attempt_finished` 若传入不在允许集合内的 `status`，在 Python 侧（不依赖 DB 报错）就地拒绝，抛 `ValueError`——比让 SQLite 抛 `IntegrityError` 更早发现、错误信息更明确。
- 表是纯追加+更新（`started`/`finished` 两段式），不参与跨进程崩溃恢复判定（那是 outbox 的职责），写失败不得阻断本轮扇出——**这个"不阻断"的容错点由调用方（Phase 5 波次调度器）的 `try/except` 实现，`ledger.py` 本身不吞异常**（cfr2-08 指出 v2 只在文字里声明容错、代码里没有对应 `try/except`；本任务的 `ledger.py` 函数本身不加 `try/except`——**容错职责在调用方**，Phase 5 波次调度器写账本处需要显式 `try/except Exception` 包裹并记日志，这是本任务与 Phase 5 之间需要交叉核对的一处接缝，Phase 5 任务描述会重申）。

**测试清单**：
1. `record_attempt_started` → `record_attempt_finished` 往返，`attempts_for_round` 能读回正确的 `status`/`cost_usd`。
2. fork 重试场景：attempt 2 的 `parent_session_id` 指向 attempt 1 的 session_id，两条记录都在。
3. 重复 `attempt_key` 触发主键冲突（`sqlite3.IntegrityError`）——固化"调用方需自行保证幂等"的契约边界。
4. `attempts_for_round` 对不存在的 `round_id` 返回空列表。
5. **新增**（cfr2-04）：`record_attempt_finished(status="capability_drift", ...)` 写入成功，不抛异常（验证 DB CHECK 与函数校验都认得这个状态词）。
6. **新增**：`record_attempt_finished(status="degraded", ...)` 或任何不在枚举内的值，在 Python 侧抛 `ValueError`（而不是让它传到 SQL 层才失败）。

- [ ] **Step 1**：按测试清单写 `.claude/scripts/harness/tests/test_ledger.py`，跑至因模块不存在而红。
- [ ] **Step 2**：在 `db.py` 的 `SCHEMA` 字符串末尾追加上方表定义（只追加，不改动前面任何一行）；按接口契约实现 `.claude/scripts/harness/ledger.py`。
- [ ] **Step 3**：跑通全部用例，并重跑 `test_db.py` 确认既有 schema 测试未受影响。
- [ ] **Step 4（正控）**：临时把 `agent_attempts` 表定义从 `SCHEMA` 里删掉，跑 `test_ledger.py`，确认 `sqlite3.OperationalError: no such table`；恢复。另临时把 Python 侧状态词校验删掉，用一个非法状态词调用 `record_attempt_finished`，确认它能绕过 Python 校验直接触发 SQL 层 `IntegrityError`（验证"Python 侧校验确实先于 SQL 层生效"这条断言的必要性）；恢复。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/db.py .claude/scripts/harness/ledger.py .claude/scripts/harness/tests/test_ledger.py
git commit -m "feat(harness): agent_attempts 谱系账本（纯追加表，状态词统一为 running/success/failed_transport/capability_drift，cfr2-04）" -- .claude/scripts/harness/db.py .claude/scripts/harness/ledger.py .claude/scripts/harness/tests/test_ledger.py
```

---
## Phase 2 · `claude_runner.py` 扩展：会话身份参数 + 工具集收窄

**目标**：`build_argv`/`invoke` 新增 `session_id`/`resume`/`fork_session` 三个可选参数（假设 Phase 0 confirmed 待决 A；若 refuted 则按 Phase 0 记录的替代方案展开，此处按 confirmed 路径写）；`STAGE1_ALLOWED_TOOLS` 从 `{Read,Grep,Glob,Skill,Workflow,TaskOutput}` 收窄为 `{Read,Grep,Glob}`。

**为什么工具集收窄是本阶段而非 Phase 5 才做**：`_validate_tools` 是 `build_argv` 内部的强制校验（`UnsafeInvocationError`），一旦改了 `STAGE1_ALLOWED_TOOLS` 常量，`round.py` 现有引用它的 `STAGE1_TOOLS = ",".join(sorted(STAGE1_ALLOWED_TOOLS))` 会立即联动变化，因此收窄工具集与新增会话参数是同一处代码的同一次编辑，放在一个任务里做，避免中间态。**但实际收窄动作挪至 Phase 6 执行**（见 Task 2.4 说明），本阶段只新增能力，不改变既有行为。

### Task 2.1（含 rmf-08 权限收紧）：`build_argv`/`invoke` 新增会话身份参数（含 cfr3-02 所需的 `subtype` 暴露）

**rmf-08 补充（第三轮评审明确指出"延后理由站不住"，本任务落实其中可低成本处理的一项；第四轮评审订正放大倍数与创建方式）**：本计划把每轮 stream 落盘从 1 份扩大到多份（Phase 2 Task 2.3 的 `RoleInvocationRequest.stream_log`，Phase 6 接线时每个子调用各自一个文件，Phase 5 fork 重试后每个 attempt 各自一个文件，见下方 cfr4-01 的贯通修复），这是**直接放大**暴露面，不是无关任务，不能一概推给"独立安全加固、非本计划引入"。本任务借 Task 2.1 已经在改 `claude_runner.py` 的机会处理权限收紧。**第四轮评审指出两处问题**：(1) v4 用"落盘后 `os.chmod`"，存在从"文件创建"到"chmod 生效"之间的短暂窗口，期间文件按系统 umask 的默认权限（通常 0644，全用户可读）存在，这个窗口本身就是暴露；(2) `os.chmod` 与"创建时直接指定权限"相比是多余的一步。**修正**：改为用 `os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)` 直接以 `0o600` 模式创建文件（`os.open()` 传入的 `mode` 参数是创建新文件时的权限位，只会被 umask "清除"其中出现的位，而 `0o600` 只含 owner 读写位，常见 umask 配置不会清除 owner 位，因此这个调用能保证文件从创建的第一个字节起就是 `0o600`，不存在任何中间窗口），不再需要事后 `chmod`。

**放大倍数订正（第四轮评审指出 v4 的"7 倍"不诚实）**：v4 的表述"每轮最多 7 个文件"只考虑了"一个角色一个文件"，没有考虑 Phase 5 的 fork 重试——**每个 attempt 都会写一次 `stream_log`**（Phase 5 Task 5.4 的 `run_wave_scheduled` 每一波都可能重新调用 `invoke()`），且 judge 侧的短路规则不保证角色数恒定（redline 否决一个候选后，扇出仍会对**下一个候选**继续裁决，不是"最多裁决一次就结束"）。按最坏情形重新计算：4 个 finder × 最多 3 次尝试 = 12 个文件；`ranked` 候选最多 3 个（`fanout.dedupe_and_rank` 的 `_MAX_RANKED_CANDIDATES` 上限），每个候选最多裁决 3 个 judge × 最多 3 次尝试 = 9 个文件，3 个候选最多 27 个 judge 文件——**最坏情形总计 12 + 27 = 39 个文件**，不是 7 个。磁盘增长速度同比例按 39 倍估算，不是 7 倍。这是本计划已经转述给用户的一处错误数字，本次订正修正。

**脱敏与轮转/保留策略明确仍延后，量化说明放大倍数（rmf-08 不能再模糊处置的部分，第四轮评审订正倍数计算）**：脱敏（落盘前对 prompt/输出做敏感信息过滤）与轮转/保留（防止 `.claude/state/rounds/` 无限增长）需要新的设计决策（脱敏规则、保留窗口长度），不是本计划可以顺手做的一行修改，继续登记为**明确的延后项，而非"与本次改动无关"**：暴露面从"每轮 1 个文件"变为"每轮最坏情形 39 个文件"——**订正计算**（第四轮评审指出 v4 的"7 倍"遗漏了两个维度）：(a) fork 重试维度，Phase 5 波次调度每个 attempt 都各自写一次 `stream_log`（不是一个角色只写一次），4 个 finder 各自最多 3 次尝试 = 12 个文件；(b) judge 侧持续裁决维度，`fanout.dedupe_and_rank` 的候选上限是 3（`_MAX_RANKED_CANDIDATES`），redline 否决一个候选后扇出会继续裁决**下一个候选**而非结束，因此 judge 侧最坏情形是 3 个候选 × 最多 3 个 judge × 最多 3 次尝试 = 27 个文件——**总计最坏情形 12+27=39 个文件，放大倍数 39 倍**，不是此前算错的 7 倍。磁盘增长速度同比例按 39 倍估算。这两条需要用户在本计划实施前后择期裁决是否值得单独立项，本计划只负责把"权限"这一项在低成本范围内做掉、把"脱敏/轮转"的延后决策讲清楚量化后果，不越权替用户决定优先级。

**接口契约**：

```python
def build_argv(prompt: str, tools: str, grant_usd: float, max_turns: int,
               settings_path: str, model: str | None = None,
               session_id: str | None = None, resume: str | None = None,
               fork_session: bool = False) -> list[str]: ...
```

**不变量**：
- `session_id` 与 `resume` 互斥——同时提供两者，`build_argv` 必须拒绝（`UnsafeInvocationError`），因为攻两个矛盾的会话身份标志同时传给 `claude` 是调用方编排错误，不应静默取其一。
- `fork_session=True` 时必须提供 `resume`（fork 语义依附于续接一个已有会话），否则拒绝。
- `session_id`/`resume` 若提供，必须是合法 UUID 格式字符串，否则拒绝（`UnsafeInvocationError`，与既有 `_validate_tools` 等校验同一层级、同一异常类型）。
- `invoke()` 同步新增三个透传参数；`InvocationResult` 新增字段 `session_id: str | None = None`，取自 stream 的 `init`/`result` 事件里的 `session_id` 字段（若存在）。
- **`InvocationResult` 同时新增字段 `subtype: str | None = None`**（第三轮评审 cfr3-02 要求：`_parse_terminal_result` 内部已经读取了终态 `result` 事件的 `subtype` 字段用于判断是否 `success`，但此前这个值读完即弃，`AttemptRecord`/Phase 5 的重试判定完全看不到它，只能靠"`ok` 是否为真"这一个粗粒度信号）。`subtype` 取值直接透传终态事件的原始字符串（`"success"`/`"error_max_turns"`/`"error_max_budget_usd"`/`"error_during_execution"` 等，不做枚举校验——本层只负责如实传递，分类判断是 Phase 5 Task 5.3 的职责）；超时（未见任何终态事件）时 `subtype` 为 `None`。
- **`_persist_stream()` 改为创建时即以 `0o600` 模式打开，不事后 `chmod`**（rmf-08，第四轮评审订正）：用 `os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)` 拿到文件描述符，再 `os.fdopen(fd, "w", encoding="utf-8")` 包装成文件对象写入——不存在"先以默认权限创建、再收紧"的中间窗口。不改变函数签名、不改变调用方式，落盘失败时的容错语义（吞 `OSError`、写 stderr）不变。

**测试清单**：
1. 提供 `session_id` 时 argv 含 `--session-id <值>`。
2. 提供 `resume`+`fork_session=True` 时 argv 含 `--resume <值>` 与 `--fork-session`。
3. 同时提供 `session_id`+`resume` 抛 `UnsafeInvocationError`。
4. `fork_session=True` 但无 `resume` 抛 `UnsafeInvocationError`。
5. `session_id`/`resume` 格式非法 UUID 抛 `UnsafeInvocationError`。
6. `invoke()` 的返回值 `InvocationResult.session_id` 能正确带出 stream 里 `init` 事件的 `session_id`（复用文件内既有的 fake stream 构造 fixture 模式）。
7. **cfr3-02 新增**：终态事件 `subtype="error_max_budget_usd"` 时，`InvocationResult.subtype == "error_max_budget_usd"`（`ok` 仍为 `False`，与现有行为一致，只是新增字段可读）。
8. **cfr3-02 新增**：超时场景（`subprocess.TimeoutExpired`，无终态事件）时，`InvocationResult.subtype is None`。
9. **rmf-08 新增**：调用 `invoke(..., stream_log=<临时文件路径>)` 后，该文件的权限位恰为 `0o600`（用 `stat.S_IMODE(path.stat().st_mode)` 断言，不依赖测试环境的 umask）——测试还需验证**没有中间窗口**：用一个记录每次 `os.open`/`os.chmod` 调用的假 `os` 模块替身（或直接检查落盘函数源码不含 `chmod` 调用，只含一次 `os.open(..., 0o600)`），断言权限收紧发生在创建调用本身，不是创建后的第二步。

- [ ] **Step 1**：按测试清单写测试（追加到 `test_claude_runner.py`），跑至因 `build_argv`/`invoke` 不接受这些参数、或落盘权限不是 `0o600` 而红。
- [ ] **Step 2**：实现改动（`build_argv` 新增参数与校验；`invoke()` 透传；`InvocationResult` 新增 `session_id`/`subtype` 字段；`parse_stream_json`/`_parse_terminal_result` 解析 `init`/`result` 事件时带出这两个字段——`subtype` 只是把已经读到的值透传出去，不新增解析逻辑；`_persist_stream()` 改为 `os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)` + `os.fdopen(fd, "w", encoding="utf-8")`，不再有事后 `chmod` 步骤）。
- [ ] **Step 3**：跑通，重跑既有全部用例确认无回归。
- [ ] **Step 4（正控）**：临时删除互斥校验，确认对应用例变红；恢复。另临时把 `_persist_stream()` 的 `os.open` 调用改回 `path.open("w", encoding="utf-8")`（即恢复到"用系统默认权限创建"），跑用例 9，确认变红；恢复。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/claude_runner.py .claude/scripts/harness/tests/test_claude_runner.py
git commit -m "feat(harness): claude_runner 支持 session_id/resume/fork_session，InvocationResult 暴露 subtype（cfr3-02 前置），stream 落盘收紧为 0600（rmf-08 部分处置）" -- .claude/scripts/harness/claude_runner.py .claude/scripts/harness/tests/test_claude_runner.py
```

### Task 2.2（处置 cfr-01）：`invoke()` 接受可注入的 payload parser

**背景（评审 cfr-01，已独立复现，Critical）**：现有 `_extract_payload()` 硬编码要求顶层 JSON 含 `candidates: list`。这是为"外层会话原样回显 workflow 结果"这个旧架构写的契约。新架构下，judge 的顶层输出是 `{"verdict": "pass", "reason": "...", ...}`，**没有** `candidates` 字段——用现有 `_extract_payload()` 解析 judge 的输出会被判定为不可解析。**每一次 judge 调用都会被这样拒绝**，重试耗尽后必然全部降级，任何候选都无法通过全部三个 judge。

**接口契约**：

```python
def _strip_fence_and_parse(text: str) -> object | None: ...  # 剥壳，不做顶层类型校验
def _extract_payload(text: str) -> dict | None: ...          # finder 场景：顶层须含 candidates:list（原行为不变）
def _extract_json_object(text: str) -> dict | None: ...      # judge 场景：顶层只需是 dict，无字段要求
def parse_stream_json(lines, *, payload_parser=_extract_payload) -> InvocationResult: ...
def invoke(..., payload_parser=_extract_payload) -> InvocationResult: ...
```

**不变量**：
- `_extract_payload`/`_extract_json_object` 共享同一段"剥壳"逻辑（`_strip_fence_and_parse`），不允许两份可能漂移的独立实现——fence 处理、trailing 内容检查只写一次。
- `_extract_payload` 原有行为逐字不变（向后兼容，默认值不传时全部既有测试保持绿）。
- `_extract_json_object` 只要求顶层是 `dict`，不检查任何具体字段——字段级校验是 `fanout_schema.validate_judge_output`（Phase 3）的职责，两层不重复。
- `parse_stream_json`/`invoke` 的 `payload_parser` 参数透传到 `_parse_terminal_result`，替换掉原来硬编码调用 `_extract_payload` 的那一行。

**测试清单**：
1. 不传 `payload_parser` 时，`parse_stream_json` 对不含 `candidates` 的 payload 仍判失败（向后兼容）。
2. 传 `_extract_json_object` 时，`{"verdict":"pass","reason":"r"}` 这类无 `candidates` 的 payload 被正确解析、`ok=True`。
3. `_extract_json_object` 拒绝非 dict 顶层（数组、字符串）。
4. `_extract_json_object` 同样拒绝"闭合 fence 后有多余文字"（验证剥壳逻辑共享）。
5. `invoke()` 接受 `payload_parser` 关键字参数并生效（端到端）。

- [ ] **Step 1**：按测试清单写测试，跑至因 `_extract_json_object` 不存在、`parse_stream_json`/`invoke` 不接受该关键字而红。
- [ ] **Step 2**：实现改动（拆出 `_strip_fence_and_parse`；新增 `_extract_json_object`；`_parse_terminal_result`/`parse_stream_json`/`invoke` 依次新增 `payload_parser` 透传参数，默认值 `_extract_payload` 保持向后兼容）。
- [ ] **Step 3**：跑通全部新增用例；重跑既有全部用例，确认零回归。
- [ ] **Step 4（正控）**：临时放宽 `_extract_json_object` 的顶层类型检查（允许 list），确认"拒绝非 dict"用例变红；恢复。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/claude_runner.py .claude/scripts/harness/tests/test_claude_runner.py
git commit -m "feat(harness): claude_runner 支持可注入 payload_parser，修复 judge 输出被强制要求 candidates 字段（cfr-01）" -- .claude/scripts/harness/claude_runner.py .claude/scripts/harness/tests/test_claude_runner.py
```

### Task 2.3（处置 cfr-02/cfr2-01/cfr3-01）：`RoleInvocationRequest` + `RequestContext`——扇出调用的唯一契约

**背景（评审 cfr-02，已独立复现，Critical；第二轮 cfr2-01 指出仍未闭合）**：v1 草图用宽松 `**kwargs` 调 `invoke_fn`，缺 `cwd`/`timeout_s` 等必需参数、未传 `model`/`stream_log`。v2 定义了 `RoleInvocationRequest` 与 `to_invoke_kwargs()`，但**第二轮评审发现生产接线（Phase 6）仍在直接执行 `deps.invoke(request)`**——把整个 `RoleInvocationRequest` 对象当成 `invoke()` 的第一个位置参数传入，而不是先用 `to_invoke_kwargs()` 展开成关键字参数。**本任务的契约必须在 Phase 6 被真正使用，不能定义了却不接线**——Phase 6 Task 6.1 的实现步骤需要显式包含 `deps.invoke(**to_invoke_kwargs(request))` 这一调用形式，且 `Deps.invoke` 字段的类型标注需要相应更新为接受 `RoleInvocationRequest` 或者是一个包装了 `to_invoke_kwargs` 展开的适配函数——两种选择都可以，但必须明确写死一种，不能两边"看起来都行"地留白。

**cfr3-01（Critical，因降级丢失，第三轮评审指出）**：`RoleInvocationRequest` 定义了 `cwd`/`settings_path`/`model`/`stream_log` 四个字段，但契约里**从未明确这四个值在生产路径上从哪里取得**——此前的测试清单只用 `_make_request` 局部测试替身里的占位值（`cwd="/tmp"`、`settings_path=""`、`model=None`）验证过字段"存在"，Phase 6 的测试清单也只验证了 adapter **展开了哪些键**（键集合是 `invoke()` 参数名的子集），从未验证这些键对应的**值是不是生产环境该有的值**（例如 `cwd` 应该等于 `cfg.repo_root`，不是测试替身里随手写的 `/tmp`）。同时 Task 5.5 的不变量 2 要求 judge task identity（含 fingerprint）贯穿 session ID、账本 `attempt_key`、`AttemptRecord.role` 三处，但测试清单第 6 条**只比较了两次调用的 `request.session_id` 是否不同**——没有验证 `attempt_key`（账本主键）与 `stream_log` 路径是否**同样**由这个 task identity 派生、且与 session ID 用的是同一个源字符串（三者各自独立拼接出"看起来都对但可能不一致"的字符串，是完全可能发生的实现错误，例如 `attempt_key` 不小心用了 `role` 原始值而 `session_id` 用了 `task_role`）。

**修复：新增显式 `RequestContext` 契约**，把"生产环境下这些值应该等于什么"从"分散在 Phase 5/6 各处的口头约定"收敛为一个可传递、可测试的数据对象，同时补齐 task identity 四处统一的联合测试。

**接口契约**：

```python
@dataclass(frozen=True)
class RequestContext:
    """扇出调用的生产环境上下文——把 cwd/settings_path/model/stream_log_dir
    这四个"应该等于什么"的问题收敛到一处，供 run_finders/judge_candidate
    的 _make_request 工厂消费，不再让这四个值在 Phase 5/6 之间只靠口头约定
    传递（cfr3-01 修复）。
    """
    cwd: str               # 生产值：str(cfg.repo_root)，不是测试替身的 "/tmp"
    settings_path: str     # 生产值：round.SETTINGS_PATH（现有常量，".claude/harness-settings.json"）
    model: str             # 生产值：claude_runner.DEFAULT_AGENT_MODEL，不是 None
    stream_log_dir: str    # 生产值：str(cfg.state_db.parent / "rounds")，per-attempt 文件名由调用方按 task identity 拼接

def build_stream_log_path(stream_log_dir: str, round_id: str, task_role: str,
                          attempt: int) -> str: ...
    # 返回 f"{stream_log_dir}/{round_id}:{task_role}:{attempt}.jsonl"（与
    # attempt_key 用同一个 f-string 模板拼接，两者除文件后缀外逐字相同，
    # 供测试机械比对字符串前缀一致）

@dataclass(frozen=True)
class RoleInvocationRequest:
    role: str
    prompt: str
    tools: str
    grant_usd: float
    max_turns: int
    settings_path: str
    cwd: str
    timeout_s: float
    model: str | None = None
    session_id: str | None = None
    resume: str | None = None
    fork_session: bool = False
    stream_log: object = None
    payload_parser: object = _extract_payload  # 默认 finder 场景

def to_invoke_kwargs(request: RoleInvocationRequest) -> dict: ...  # 展开为 invoke(**kwargs) 可用的字典，不含 role
def for_judge(**kwargs) -> RoleInvocationRequest: ...  # payload_parser 默认改为 _extract_json_object
```

**不变量**：
- 字段集合与 `claude_runner.invoke()` 的真实参数名（除 `role`）逐一对应——`to_invoke_kwargs()` 返回的每个键都必须是 `invoke()` 签名里真实存在的参数名，用 `inspect.signature` 机械核对，不凭记忆维护对应关系。
- `role` 字段是路由信息，不出现在 `to_invoke_kwargs()` 的返回值里（不传给 `invoke()`）。
- **生产代码必须调用 `invoke(**to_invoke_kwargs(request))` 或等价的完整展开**，不允许把 `RoleInvocationRequest` 对象整体当位置/关键字参数传给 `invoke()`（那样只会填满第一个形参 `prompt`，其余必需参数全部缺失，`TypeError`）——这条不变量在 Phase 6 Task 6.1 的实现里必须体现，本任务只定义类型，接线正确性由 Phase 6 负责，但**两处必须交叉核对一致**。
- `for_judge()` 的默认 `payload_parser` 是 `_extract_json_object`，其余字段与基类构造方式相同。
- **`RequestContext` 的四个字段值必须是生产真值，不是占位符**（cfr3-01）：`cwd` 非空且不等于测试常用的占位值（如硬编码的 `/tmp`，除非该值恰好就是被测环境的真实 `repo_root`）；`model` 非 `None`；`settings_path` 等于 `round.SETTINGS_PATH` 常量本身（不是重新拼接的字符串字面量，避免两处"看起来一样"实则不同步）。
- **`build_stream_log_path` 与账本 `attempt_key` 共用同一个 task identity 拼接模板**（cfr3-01）：两者除文件后缀外的字符串前缀逐字相同——`Task 1.2` 的 `attempt_key = f"{round_id}:{role}:{attempt}"`，`build_stream_log_path` 返回 `f"{stream_log_dir}/{round_id}:{task_role}:{attempt}.jsonl"`，`round_id:task_role:attempt` 这一段必须逐字等于 `attempt_key`——测试用字符串前缀比对机械核对，不凭肉眼判断"看起来像"。

**测试清单**：
1. 缺任一必需字段（如只传 `role`）时 `TypeError`（dataclass 天然行为）。
2. `to_invoke_kwargs()` 的返回值键集合是 `invoke()` 真实参数名的子集（`inspect.signature` 核对），且不含 `role`。
3. 默认（非 `for_judge`）构造的 `payload_parser` 是 `_extract_payload`。
4. `for_judge()` 构造的 `payload_parser` 是 `_extract_json_object`。
5. **cfr3-01 核心之一**：`build_stream_log_path(stream_log_dir, round_id, task_role, attempt)` 返回值的 `round_id:task_role:attempt` 段，与 `ledger` 模块用同样三个入参拼出的 `attempt_key` 逐字相等（字符串前缀比对）。
6. **cfr3-01 核心之二**：`RequestContext` 的 `model` 字段类型标注为 `str`（非 `str | None`）——dataclass 层面就不允许构造出 `model=None` 的 `RequestContext`，把"必须显式设置"从运行时断言提升为类型系统能表达的约束（不完全依赖测试兜底）。

- [ ] **Step 1**：按测试清单写测试（新建 `test_role_invocation.py`），跑至因模块不存在而红。
- [ ] **Step 2**：按接口契约实现 `.claude/scripts/harness/role_invocation.py`（含 `RequestContext`/`build_stream_log_path`）。
- [ ] **Step 3**：跑通全部用例。
- [ ] **Step 4（正控）**：临时在 `claude_runner.invoke()` 里改名一个参数（模拟签名漂移），确认 `to_invoke_kwargs` 一致性测试变红；恢复。另临时把 `build_stream_log_path` 的拼接模板改成不含 `task_role`（只用 `round_id:attempt`），跑用例 5，确认变红（复现"stream path 与 attempt_key 各自拼接、可能不一致"这个 cfr3-01 指出的缺口）；恢复。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/role_invocation.py .claude/scripts/harness/tests/test_role_invocation.py
git commit -m "feat(harness): RoleInvocationRequest + RequestContext —— 扇出调用的唯一契约，生产值来源显式化，stream path 与账本 attempt_key 共用拼接模板（cfr-02, cfr3-01）" -- .claude/scripts/harness/role_invocation.py .claude/scripts/harness/tests/test_role_invocation.py
```

### Task 2.4（原挪至 Phase 6 执行，此处只登记设计，不在本阶段实施）

**为什么不在 Phase 2 就收窄工具集**：`STAGE1_ALLOWED_TOOLS` 被 `round.py` 当前仍在使用的 Workflow 调用路径依赖。若在 Phase 2 就收窄，`round.py` 在 Phase 6 完成接线之前的每一次真实调用都会因为工具集不含 `Skill`/`Workflow` 而必然 `capability-drift`，且大量既有测试会在 Phase 2 到 Phase 6 之间持续报红，违反「每个任务完成后测试变绿」的纪律。

**处置**：`STAGE1_ALLOWED_TOOLS` 的收窄与 `round.py` 接入 `fanout.py`（Phase 5 产出）**在 Phase 6 同一个任务（Task 6.1）里原子完成**，中间不存在「工具集已改但调用路径未改」的过渡态。

---

## Phase 3 · `fanout_schema.py`：候选/裁决 JSON 的 Python 侧结构校验

**目标**：把现在活在 `scrollz-propose.js` 里的 `CANDIDATE_SCHEMA`/`JUDGE_SCHEMAS`（JSON Schema 字面量，靠 Workflow 工具的 `schema` 参数由 **Claude 侧**在模型输出后立即结构化校验）迁移为 Python 侧的显式校验函数——因为扇出后每个 finder/judge 是独立顶层进程，**不再有 Workflow 的 `schema` 参数可用**，模型输出必须原样吐到 stdout 的 `result.result` 文本里，由控制器自己解析 JSON 并校验形状。

**与 `round.py` 现有 `validate_candidate`/`_ALLOWED_CANDIDATE_FIELDS` 的关系**：`round.py` 现有的 candidate DTO 校验是**第二道闸**，校验的是「四个 finder 的产出经过去重/排序/裁决后，最终选中的那一个候选」是否满足发布前置条件。`fanout_schema.py` 是**第一道闸**，校验的是「单个 finder/judge 进程的原始输出」是否满足其自身的 schema。两道闸校验的字段集合不同，**不合并、不删除任何一道**，`round.py` 的现有校验逻辑完全不改。

### Task 3.1：`CANDIDATE_SCHEMA` 校验函数

**cfr-13 修正（Important，已核实）**：v1 草图的 `validate_finder_output` 未拒绝顶层额外字段（未迁移 JS 版 `additionalProperties:false` 语义），且枚举成员检查（如 `c["size"] not in _SIZES`）在**没有先做类型检查**的情况下直接对可能是 list/dict 的值做集合成员测试，会抛 `TypeError: unhashable type` 而不是被收集进错误列表、终止整个校验调用。

**接口契约**：

```python
_CANDIDATE_REQUIRED: frozenset[str]  # title/goal/invariant/primary_path/oracle/evidence/
                                     # touched_paths/size/priority/needs_decision/body_md/slug
_SIZES: frozenset[str]      # {"S","M","L"}
_PRIORITIES: frozenset[str] # {"T0".."T4"}
_MAX_CANDIDATES: int = 3

def _check_enum(value, allowed: frozenset, field_label: str, errors: list[str]) -> None: ...
def _validate_one_candidate(c: dict, errors: list[str], idx: int) -> None: ...
def validate_finder_output(payload: dict) -> list[str]: ...

_JUDGE_SCHEMAS: dict  # judge_type -> {"required": frozenset, "verdicts": frozenset}
def validate_judge_output(judge_type: str, payload: dict) -> list[str]: ...
```

**不变量**：
- `validate_finder_output` 顶层字段集合严格等于 `{"candidates"}`（迁移 JS `additionalProperties:false`）——额外顶层字段判失败。
- `_check_enum` 必须先检查 `isinstance(value, str)`，再做 `value in allowed` 成员测试——不允许 unhashable 值（list/dict）让检查本身抛异常，这类值必须被收集为一条 `errors` 字符串。
- `_validate_one_candidate` 对每个候选：未知字段收集为错误（不 raise）；缺必需字段收集为错误并提前返回（避免后续解引用 KeyError）；`touched_paths` 须为字符串列表；`size`/`priority` 走 `_check_enum`；`needs_decision` 须为 bool。
- `validate_judge_output(judge_type, payload)`：`judge_type` 不在 `_JUDGE_SCHEMAS` 内时 `KeyError`（这是编排层 bug，不是数据错误，允许异常穿透）；字段集合与 `_JUDGE_SCHEMAS[judge_type]["required"]` 严格相等；`verdict` 走 `_check_enum`。
- 三种 judge schema 的必需字段互不相同（`harness-judge-completed`→`evidence`；`harness-judge-redline`→`invariant_at_risk`，且 `verdicts` 多一个 `needs_decision`；`harness-judge-oracle`→`suggested_oracle`），互相之间的专有字段不能混用。

**测试清单**：
1. 合法单候选/空候选列表通过。
2. 缺 `candidates` 字段、`candidates` 非 list、超过 3 条、缺必需字段、含未知字段——均判失败（不抛异常）。
3. 枚举值非法（如 `size="XL"`）判失败。
4. `touched_paths`/`needs_decision` 类型错误判失败。
5. **cfr-13**：顶层含额外字段判失败。
6. **cfr-13**：`size`/`priority`/`verdict` 传 list/dict（unhashable）**不抛异常**，被收集为错误字符串。
7. 三种 judge schema 各自的合法/非法用例（专有字段互换判失败、缺字段判失败、`verdict` 枚举非法判失败）。
8. 未知 `judge_type` 抛 `KeyError`。

- [ ] **Step 1**：按测试清单写 `.claude/scripts/harness/tests/test_fanout_schema.py`，跑至因模块不存在而红。
- [ ] **Step 2**：按接口契约实现 `.claude/scripts/harness/fanout_schema.py`（字段集合逐字迁自 `scrollz-propose.js` 的 `CANDIDATE_SCHEMA`/`JUDGE_SCHEMAS`，不新增/不删减字段）。
- [ ] **Step 3**：跑通全部用例。
- [ ] **Step 4（正控）**：临时禁用未知字段检测，确认对应用例变红；临时把 `_check_enum` 的类型前置检查删掉，确认 unhashable 值用例变为未捕获 `TypeError` 而非正常错误列表；两处均恢复后重跑确认绿。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/fanout_schema.py .claude/scripts/harness/tests/test_fanout_schema.py
git commit -m "feat(harness): fanout_schema —— finder/judge 输出的 Python 侧结构校验（含 cfr-13 修复：顶层字段封闭 + 类型前置检查）" -- .claude/scripts/harness/fanout_schema.py .claude/scripts/harness/tests/test_fanout_schema.py
```

**长度上限追加（吸收评审 rmf-15）**：原 JS `CANDIDATE_SCHEMA` 对字符串字段无 `maxLength`。新架构下 TaskOutput 已不存在（消灭了 rmf-15 的触发路径本身），但为了与 `round.py` 现有 `_MAX_SHORT_TEXT=300`/`_MAX_LONG_TEXT=20000` 保持一致，在 `_validate_one_candidate` 里追加对应长度上限校验，补 2 条测试（超长 `title`、超长 `body_md`）。**顺手补齐，不得无限期搁置**。

---

## Phase 4 · `prompts.py`：agent 定义装配（读 `.claude/agents/harness-*.md`）

**目标**：现有 `.claude/agents/harness-{finder,judge}-*.md` 七个文件（frontmatter 含 `name`/`description`/`tools`，正文是 persona 指令）**保留原样，不改一个字**。新增 `prompts.py` 负责在 Python 侧把这些文件解析出来，与「不可信数据边界包裹」「候选 JSON 契约」拼成每次顶层 `claude -p` 调用的完整 prompt 字符串。

**为什么不用 `--agents <json>`**：ADR 与 PoC Q6 已明确「`--agents` 完全可行，但用于 `Task` 工具扇出会触发反例」。本计划**不使用 `Task` 工具**，而是把 agent 定义的内容直接拼进顶层进程自己的 `-p` prompt 参数，不依赖任何未在 PoC 中验证过的 CLI 标志。

### Task 4.1：agent 定义解析

**接口契约**：

```python
@dataclass(frozen=True)
class AgentDef:
    name: str
    description: str
    tools: tuple[str, ...]
    body: str

def parse_agent_file(path: Path) -> AgentDef: ...
def build_finder_prompt(agent: AgentDef, *, blocked_lanes: list[str],
                        known_canonical_keys: list[str]) -> str: ...
def build_judge_prompt(agent: AgentDef, candidate: dict, *,
                       inflight_paths: list[str]) -> str: ...
```

**不变量**：
- `parse_agent_file`：文件须以 YAML frontmatter（`---`包裹）开头，缺失或格式不对抛 `ValueError`；`name`/`description`/`tools` 三个 frontmatter 键缺任一抛 `ValueError`；`tools` 值按逗号分割为 tuple；`body` 是 frontmatter 之后的正文，去除首尾空白。
- `build_finder_prompt`：把 `agent.body`（persona 正文）、不可信数据边界提示语、`blocked_lanes`/`known_canonical_keys` 上下文（JSON 序列化）、输出 schema 说明（`{"candidates":[...]}`）拼接成完整 prompt。
- `build_judge_prompt`：把 `agent.body`、"BEGIN/END UNTRUSTED CANDIDATE" 包裹的候选原文（含 `inflight_paths`）、裁决指令拼接成完整 prompt——候选内容必须**完整原样**出现在提示词里（即便候选文本包含疑似指令性文字，也只作为待核验数据传入，不做任何过滤/转义之外的改写）。

**测试清单**：
1. 解析样例 agent 文件，`name`/`description`/`tools`/`body` 均正确。
2. 缺 frontmatter、缺必需键各自抛 `ValueError`。
3. `build_finder_prompt` 输出包含 persona 正文与 `candidates` schema 说明。
4. `build_judge_prompt` 输出包含 "BEGIN/END UNTRUSTED CANDIDATE" 边界标记，且候选原文（含疑似指令性文字）完整出现在提示词里、不被过滤。
5. **集成性测试**：对仓库内全部 7 个真实 `.claude/agents/harness-*.md` 文件跑 `parse_agent_file`，断言全部无异常抛出，且 `tools == ("Read", "Grep", "Glob")`——这条同时验证"Phase 2/6 工具集收窄"与"agent 文件 frontmatter"两者一致。

- [ ] **Step 1**：按测试清单写 `.claude/scripts/harness/tests/test_prompts.py`，跑至因模块不存在而红。
- [ ] **Step 2**：按接口契约实现 `.claude/scripts/harness/prompts.py`。
- [ ] **Step 3**：跑通全部用例。
- [ ] **Step 4（正控）**：临时让 frontmatter 正则总不匹配，确认解析用例变红；恢复。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/prompts.py .claude/scripts/harness/tests/test_prompts.py
git commit -m "feat(harness): prompts —— 从 agent 定义文件装配顶层调用 prompt" -- .claude/scripts/harness/prompts.py .claude/scripts/harness/tests/test_prompts.py
```

---


## Phase 5 · `fanout.py`：并发扇出、降级、redline 短路、fork 重试

**这是本计划最核心的模块**，把现在活在 `scrollz-propose.js` 里的全部编排语义（去重、排序、`safeAgent` 重试、`normalizeError`/`recordDegraded` 折叠、redline 先跑短路、降级按否决处理）迁到 Python，**同时吸收 `code-review-realmachine-fixes.md` 已登记但尚未落地的三条改进**（rmf-07 预算感知重试 + 批次退避、rmf-10 ID 折叠正则补全、rmf-12 降级 verdict 补齐专有字段占位）——这些是已被评审接受、只是此前受限于 JS 实现而未做的修复，本次重写没有理由不带上，属于「反-YAGNI」范畴，不是新增范围。

**本节经跨模型对抗评审（`cfr-01`–`cfr-19`）后结构性重写，采用统一契约与波次调度架构，处置 cfr-02/03/05/06/07/09/12/13/14，详见文首「评审处置台账」。**

**设计回答（问题 1：`degraded`/重试/短路语义迁移后的形状，v2 订正）**

| JS 原语义 | Python 新形态 | 变化说明 |
|---|---|---|
| `safeAgent`：同一 agent 最多重试 `MAX_AGENT_ATTEMPTS=3` 次，每次都是全新 `agent()` 调用（无上下文延续） | **波次调度**（cfr-12 修正）：所有角色的 attempt 1 在同一波并发发起；只有失败的角色进入 attempt 2（用 `--resume <上次真实 session_id> --fork-session` 续接），第三波同理。**不是**单角色内部连续发起 3 次尝试——那样会让同一角色在同一个故障窗口内背靠背连打 3 枪，浪费重试机会且不构成任何退避 | 语义增强，非削减；同时是 cfr-12 修复本身 |
| `normalizeError` 折叠同类传输故障 | `normalize_error`：迁移全部逻辑，并按 rmf-10 补齐 UUID/ULID/`req_`/`trace` 前缀四种 ID 格式的正则，且**必须放在裸 hex 规则之前**；截断策略改为「保留头 200+尾 100」而非纯截头 | 修复 rmf-10 的两个真实漏检（UUID 不折叠、zod 长样板前缀误折叠） |
| `recordDegraded` 折叠计数 | `record_degraded`：同 role + 同规范化错误 → 折叠计数；`occurrences`/`attempts` 字段语义不变；字典同时写 `role`（Python 内部消费）与 `agentType`（等于 `role`，供 `round.py` 现有 `_describe_degraded()` 不改代码即可正确读取，见开放发现处置表对 cfr-16 反例的处置） | 逐字迁移 + cfr-16 反例修复 |
| redline judge 先跑、reject 即短路 | `judge_candidate`：先跑 `harness-judge-redline`，`reject` 即返回，不跑另外两个 judge；短路时 `skipped_judges` 字段记录未跑到的 judge 类型列表（cfr-14 修复） | 逐字迁移 + cfr-14 修复 |
| 降级 judge 按否决处理 | 降级时构造 `{judge, verdict:'reject', reason:'judge-unavailable', <judge专有字段>:None, degraded:True, skipped_judges:[...]}`，**补齐该 judge 的专有字段占位**（rmf-12 修复：形状与真实否决恒定，供 Stage 1b 拒绝记忆消费时可靠区分「降级导致的否决」与「真实否决」），且**降级同时写入顶层 `degraded` 数组**（cfr-07 修复：此前只写局部 reject verdict，顶层 `degraded` 仍是空的，导致"finder 找到候选但 redline 全部降级"这个场景在 `round.py` 侧被误判为干净的 `no-candidate`，精确复发 rmf-03） | 修复 rmf-12 + cfr-07 + cfr-14 |
| 无预算感知、无退避（rmf-07） | 波次调度天然获得退避（同一波内所有角色并发发出，不同波之间天然有其它角色调用耗时作为间隔）；预算感知由**进程内线程安全的 `_BudgetTracker`** 提供（cfr-05 修复，见下），每次发起调用前先原子 `try_reserve()`，不足则该角色直接进入本波的"预算不足，提前降级"分支，不进入下一波 | 修复 rmf-07 + cfr-05（预算感知从"读一个可能过期的剩余值"改为"原子预留，杜绝并发重复占用"） |

**设计回答（问题 4：并发原语与失败隔离，v2 订正）**——`concurrent.futures.ThreadPoolExecutor`（标准库）。每个 finder/judge 调用都是「起子进程 + 阻塞等待其退出」，是 IO 密集型等待（GIL 在 `subprocess.run` 阻塞期间会释放），线程池是标准库里最直接的选择，不需要 `multiprocessing`（没有 CPU 密集型计算，`subprocess.run` 本身已经把实际工作转移到独立操作系统进程）。

**并发安全的三层修正（cfr-03/cfr-05/cfr-06，本轮评审后新增）**：

1. **worker 线程绝不直接访问 SQLite connection**（cfr-05 前身 cfr-03 修复，Critical，已独立复现）：v1 草图让每个 worker 线程在 `run_role_with_retry` 内部直接调用 `ledger.record_attempt_started/finished(conn, ...)`，而 `db.connect()` 用的是 SQLite 默认 `check_same_thread=True`，跨线程调用会立即抛 `sqlite3.ProgrammingError`（已用现有 `db.py` 实测复现，见文首处置台账 cfr-03）。修正：worker 线程的 `_run_one`/`_run_judge` 只返回一个纯数据的 `AttemptRecord`（角色/attempt/session_id/parent_session_id/status/cost/turns，不含任何 IO），主线程在 `future.result()` 汇总之后**串行**遍历这些记录并调用 `ledger.record_*`。这样 `conn` 从始至终只在主线程被访问。
2. **并发预算原子性**（cfr-05，Critical）：v1 草图里每个 worker 独立调用 `remaining_budget_usd()` 读取剩余额度，四个 finder 几乎同时读到同一个值后各自决定是否重试——这是一个经典的"读-判断-写"竞态：若四者都读到"还剩 $1"，都可能各自发起一次调用，实际占用远超 $1。修正：新增 `_BudgetTracker`（`threading.Lock` 保护的进程内计数器），提供 `try_reserve(amount) -> bool`（原子地"若剩余额度足够则立即扣减并返回 True，否则返回 False"）与 `settle(amount, actual)`（调用返回后，把预留额度与实际成本的差额刷正——`cost_known=False` 时不刷正，因为成本真的未知，必须继续按已扣减的预留额度占用，不能当作 0 处理，这与 `budget.py` 现有 `abandon()` 语义一致）。所有 worker 在真正发起 `invoke()` 之前，都必须先拿到 `_BudgetTracker.try_reserve()` 返回 `True`，拿不到就直接判该角色本波预算不足。
3. **capability drift 检查显式安排，不假设自动下沉**（cfr-06，Critical）：v1 草图的文字曾暗示"能力漂移检测随 `parse_stream_json()` 自动下沉"，但 `parse_stream_json()`/`InvocationResult` 只**收集** `init_tools`/`init_mcp_servers`/`init_plugins`/`init_errors` 这些原始数据，真正做"是否漂移"比较判断的是 `round.py` 现有的 `_capability_drift_problems()` 函数（对比 `STAGE1_TOOLS` 期望集合）。扇出后每次子调用都是独立顶层进程，都有自己的 `init` 事件，**必须对每一次子调用结果都执行这个比较**，而不是像旧架构那样只在外层会话执行一次。修正：`fanout.py` 新增 `_check_capability_drift(invocation, expected_tools) -> list[str]`（提取 `round._capability_drift_problems` 的核心逻辑为独立可复用函数，不删除 `round.py` 现有函数，两者共享同一段判定逻辑，避免出现"两份漂移判定，可能不一致"的新形态——具体实现见 Task 5.4）。任何一次子调用检测到漂移，**该角色本次尝试判定为不可重试的失败**（不同于传输故障可以重试——工具集漂移可能是配置本身的问题，重试大概率复现同样的漂移，且这是一个比传输抖动更需要人工关注的信号），整轮扇出对此**不可降级**：`run_fanout()` 检测到任一子调用漂移即让整轮直接判定失败（穿透式，不静默吞掉，与 `round.py` 现有把"能力漂移"视为整轮失败的语义一致，只是判定点从"一次顶层调用"下放到"每次子调用各自判定，任一漂移则整轮失败"）。

失败隔离通过 worker 函数**不允许任何预期失败模式（子进程超时/非零退出/协议错误/schema 校验失败/能力漂移）以异常形式传出 `Future`**——全部收敛为一个结构化的 `AttemptRecord` 返回值；只有真正的编程缺陷（如 `RoleInvocationRequest` 构造时的 `TypeError`、`UnsafeInvocationError` 因调用方传参错误触发）才允许异常穿透，这类错误**不重试、不降级**，直接让整轮失败并原样向上抛出（与 `round.py` 现有的「单一 finalize 边界」`except Exception` 兜底一致——不新增一层吞错误的 `except`）。

**待决 C（本阶段内部，非阻塞主线）：`session_id` 的确定性派生只覆盖每个角色的 attempt 1，attempt≥2（fork 产生）的会话身份由 CLI 返回，不强行对齐派生值。** 这是对 ADR 原文「每个会话 `--session-id` 由控制器按 `(round_id, role, attempt)` 确定性派生」的具体化，而非削弱：`--resume <sid> --fork-session` 语义上产生一个**新的、由 CLI 分配**的会话 ID（PoC Q5 实测：`aace…7adc` fork 出 `d4a0…966d`，二者不同），无法通过任何标志强制其等于某个预先算好的值（PoC 未测试过 `--session-id` 与 `--resume --fork-session` 同时传入的行为，属未验证组合，不假设其存在）。因此 `derive_session_id(round_id, role, attempt)` 的确定性价值在于：(a) attempt 1 的会话身份可预先算出并传给 `--session-id`；(b) `agent_attempts.attempt_key` 作为该角色本轮第 N 次尝试的**审计主键**始终可确定性地算出，不依赖任何运行时返回值；(c) `agent_attempts.session_id` 列对 attempt≥2 记录的是 CLI 实际返回的会话 ID（不等于派生值），`parent_session_id` 指向上一次 attempt 的**实际** session_id（cfr-11 修正：账本写入延后到调用真正返回、真实 session_id 已知之后才发生，见 Task 5.4 的主线程串行写账本设计），链路依然可审计追溯，只是「实际会话 ID」与「派生 attempt_key」是两个独立但都可查的坐标系。

**待决 D（本阶段内部，非阻塞主线）：本轮扇出不参与跨进程崩溃恢复。** 若 `harness.cli round` 进程本身在扇出阶段（finder/judge 调用期间）被杀，下一次 `round` 调用会生成全新 `round_id` 从头扫描——这与当前实现完全一致（`Outbox.open_roots()` 只追踪 `publish_proposal` 及其子 operation 的谱系，从未追踪「扫描进行到几个 finder」这类状态）。`agent_attempts` 表和 `derive_session_id` 因此不是**跨进程崩溃恢复**的判定依据（那仍是 outbox 独占的职责，本计划不改），而是**单次 round 进程执行期间**的会话身份来源与审计记录，让「同一轮内因传输故障重试」使用 fork 续接而非从零新开对话——这个价值发生且仅发生在一次 `run_round()` 调用的生命周期内。若未来需要「扇出阶段本身也可跨进程崩溃恢复」，那是一个新的架构决策（需要给扫描阶段引入类似 outbox 的持久化 intent），本计划不做，记入文末 backlog。

### Task 5.1：`canonical_key` 去重 + 排序（不再需要 JS 实现）

**这是一个隐含的简化**：现在 `queue.py` 的 `canonical_key`/`fingerprint` 只被 Python 侧（`round.py`）消费，`scrollz-propose.js` 里独立维护了一份 `canonicalKey()` 做「本轮内跨 finder 去重」，两份实现的一致性靠 `test_canonical_key_cross_language.py` 硬钉。**扇出迁入 Python 后，本轮内去重与跨轮去重可以共用同一个 Python 函数**（`queue.canonical_key`，Phase 5 不新增函数，直接复用现有 `queue.py` 的 `canonical_key`/`_norm`），JS 版本不再有存在理由——这正是 Phase 7 删除跨语言测试的依据。

- [ ] **Step 1: 写失败测试**（追加到新文件 `.claude/scripts/harness/tests/test_fanout.py`）

```python
import unittest
from harness.fanout import dedupe_and_rank

_C1 = {"title": "a", "goal": "g1", "invariant": "i1", "primary_path": "p1",
       "oracle": "o1", "priority": "T0", "size": "S", "lane": "roadmap"}
_C2 = {"title": "b", "goal": "g2", "invariant": "i2", "primary_path": "p2",
       "oracle": "o2", "priority": "T2", "size": "M", "lane": "defect"}
_C1_DUP = dict(_C1, title="a-dup")  # 同 goal/invariant/primary_path/oracle


class TestDedupeAndRank(unittest.TestCase):
    def test_dedupes_by_canonical_key_within_batch(self):
        result = dedupe_and_rank([_C1, _C1_DUP, _C2], known_canonical_keys=set())
        self.assertEqual(len(result), 2)

    def test_known_keys_from_previous_rounds_are_excluded(self):
        from harness.queue import canonical_key
        known = {canonical_key(_C1["goal"], _C1["invariant"],
                               _C1["primary_path"], _C1["oracle"])}
        result = dedupe_and_rank([_C1, _C2], known_canonical_keys=known)
        self.assertEqual([c["title"] for c in result], ["b"])

    def test_ranks_by_priority_then_size(self):
        low_priority_small = dict(_C2, priority="T3", size="S")
        high_priority_large = dict(_C1, priority="T0", size="L")
        result = dedupe_and_rank([low_priority_small, high_priority_large],
                                 known_canonical_keys=set())
        self.assertEqual(result[0]["priority"], "T0")

    def test_blocked_lanes_excluded_before_ranking(self):
        result = dedupe_and_rank([_C1, _C2], known_canonical_keys=set(),
                                 blocked_lanes=["roadmap"])
        self.assertEqual([c["title"] for c in result], ["b"])

    def test_missing_title_or_oracle_dropped(self):
        bad = dict(_C2, title="")
        result = dedupe_and_rank([_C1, bad], known_canonical_keys=set())
        self.assertEqual(len(result), 1)
```

- [ ] **Step 2**：跑测试，确认 `ModuleNotFoundError: harness.fanout`（红）。
- [ ] **Step 3**：写最小实现 `.claude/scripts/harness/fanout.py`（本任务只写 `dedupe_and_rank`，其余函数在 Task 5.2–5.4 陆续追加到同一文件）：

```python
"""控制器驱动扇出（ADR-002 D1/D2）。取代 scrollz-propose.js + Skill(scrollz-round)
+ TaskOutput 的三级嵌套编排——每个 finder/judge 现在是控制器直接起的独立顶层
`claude -p` 进程，编排逻辑（去重/排序/短路/降级/重试）全部是可单测的 Python 代码。
"""
from __future__ import annotations

from .queue import canonical_key

_PRIORITY_ORDER = {"T0": 0, "T1": 1, "T2": 2, "T3": 3, "T4": 4}
_SIZE_ORDER = {"S": 0, "M": 1, "L": 2}
_MAX_RANKED_CANDIDATES = 3


def dedupe_and_rank(candidates: list[dict], *, known_canonical_keys: set[str],
                    blocked_lanes: list[str] | None = None) -> list[dict]:
    """本轮内跨 finder 去重（含跨轮已知 key）+ 按 priority/size 排序。

    与 queue.py 的 canonical_key 是**同一个**函数（不再有 JS 独立实现，见
    Phase 5 Task 5.1 说明）：本轮内的 seen 集合与跨轮传入的
    known_canonical_keys 现在共享同一套规范化逻辑，天然消除了曾经需要
    `test_canonical_key_cross_language.py` 钉住的漂移风险。
    """
    blocked = set(blocked_lanes or ())
    seen = set(known_canonical_keys)
    deduped: list[dict] = []
    for c in candidates:
        if not c.get("title") or not c.get("oracle"):
            continue
        if c.get("lane") in blocked:
            continue
        key = canonical_key(c.get("goal", ""), c.get("invariant", ""),
                            c.get("primary_path", ""), c.get("oracle", ""))
        if key in seen:
            continue
        seen.add(key)
        deduped.append(dict(c, canonical_key=key))
    deduped.sort(key=lambda c: (
        _PRIORITY_ORDER.get(c.get("priority"), 9),
        _SIZE_ORDER.get(c.get("size"), 9)))
    return deduped[:_MAX_RANKED_CANDIDATES]
```

- [ ] **Step 4**：跑通全部 5 个用例（绿）。
- [ ] **Step 5（正控）**：临时把排序 key 改成恒定值 `lambda c: 0`（禁用排序），跑 `test_ranks_by_priority_then_size`，确认变红；恢复。
- [ ] **Step 6**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
git commit -m "feat(harness): fanout —— 候选去重与排序（复用 Python 侧 canonical_key，不再需要 JS 实现）" -- .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
```

### Task 5.2（cfr3-03 补充：`agentType` 双写不变量与测试）：错误规范化折叠（`normalize_error`/`record_degraded`，含 rmf-10 修复）

**cfr3-03 背景（第三轮评审指出，承接 cfr2-09/cfr-16）**：开放发现处置表（本文档「评审 cfr-16 指出的具体反例复核」一节）宣称 `record_degraded()` 会同时写 `role` 与 `agentType` 两个字段，但本任务此前的测试清单与实现代码**只写了 `role`**——处置表记的是作者相信会做的事，不是任务清单里真正要求执行的断言，第三轮评审明确指出这个缺口必须补进**实际任务**，不能只留在处置表里。本任务补上缺失的测试与实现要求。

- [ ] **Step 1: 写失败测试**（追加到 `test_fanout.py`）

```python
class TestNormalizeError(unittest.TestCase):
    def test_folds_hex_request_id(self):
        from harness.fanout import normalize_error
        a = normalize_error("API Error: Server error mid-response. req_9f3a2b7c1d")
        b = normalize_error("API Error: Server error mid-response. req_11ee44aa99")
        self.assertEqual(a, b)

    def test_folds_uuid_trace_id(self):
        # rmf-10 的真实漏检：UUID 中间分组长度不足 8，原裸 hex 正则不折叠
        from harness.fanout import normalize_error
        a = normalize_error("...(trace 9f3a2b7c-1d4e-4f8a-9b2c-1234567890ab)")
        b = normalize_error("...(trace 0c8d51ea-7b62-4a19-8e30-0987654321fe)")
        self.assertEqual(a, b)

    def test_does_not_fold_different_error_kinds(self):
        from harness.fanout import normalize_error
        a = normalize_error("schema validation failed: candidates")
        b = normalize_error("API Error: Server error mid-response. req_9f3a2b7c1d")
        self.assertNotEqual(a, b)

    def test_preserves_tail_difference_after_shared_prefix(self):
        # rmf-10 的另一个真实漏检：纯截头会把共享前缀之后的差异部分丢掉，
        # 导致两个不同的 zod 校验错误被误判为同一条。
        from harness.fanout import normalize_error
        shared_prefix = "x" * 250
        a = normalize_error(shared_prefix + " MISSING body_md on candidate 1")
        b = normalize_error(shared_prefix + " MISSING slug on candidate 2")
        self.assertNotEqual(a, b)


class TestRecordDegraded(unittest.TestCase):
    def test_folds_same_role_same_error(self):
        from harness.fanout import record_degraded
        degraded = []
        record_degraded(degraded, role="finder:roadmap", error="e1", attempts=3)
        record_degraded(degraded, role="finder:roadmap", error="e1", attempts=3)
        self.assertEqual(len(degraded), 1)
        self.assertEqual(degraded[0]["occurrences"], 2)
        self.assertEqual(degraded[0]["attempts"], 6)

    def test_does_not_fold_different_roles(self):
        from harness.fanout import record_degraded
        degraded = []
        record_degraded(degraded, role="finder:roadmap", error="e1", attempts=3)
        record_degraded(degraded, role="judge:redline", error="e1", attempts=3)
        self.assertEqual(len(degraded), 2)

    def test_writes_agent_type_alias_for_round_describe_degraded(self):
        # cfr3-03 新增：round.py 现有 _describe_degraded() 读的是
        # d.get('agentType')，不是 d.get('role')——处置表宣称的"双写"必须
        # 有一条测试真正断言这件事，否则字段名漂移会在 Phase 6 才被发现
        # （届时 round.py 的日志会显示 "?×N" 而不报错，属于静默的可观测性
        # 回归，不会被任何测试自然捕获）。
        from harness.fanout import record_degraded
        degraded = []
        record_degraded(degraded, role="finder:roadmap", error="e1", attempts=3)
        self.assertEqual(degraded[0]["agentType"], "finder:roadmap")
        self.assertEqual(degraded[0]["agentType"], degraded[0]["role"])
```

- [ ] **Step 2**：跑测试，确认因函数不存在、或 `agentType` 键缺失而红。
- [ ] **Step 3**：在 `fanout.py` 追加：

```python
import re

# 折叠顺序敏感：UUID 规则必须在裸 hex 规则之前，否则 UUID 第一段会先被
# 裸 hex 规则吃掉，UUID 规则就匹配不上剩余部分了（rmf-10 修复）。
_ID_PATTERNS = (
    re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", re.I),
    re.compile(r"\b[0-9A-HJKMNP-TV-Z]{26}\b"),          # ULID
    re.compile(r"req_\S+"),
    re.compile(r"trace[-_]?id[=: ]\S+", re.I),
    re.compile(r"\d{10,}"),                              # 时间戳
    re.compile(r"[0-9a-f]{8,}", re.I),                   # 裸 hex（放最后）
)


def normalize_error(err: object) -> str:
    """折叠传输层故障的样板差异部分，保留真正有区别的错误文本。

    与原 JS normalizeError 的两处修复（rmf-10）：
    1. 补齐 UUID/ULID/req_/trace-id 四种常见 ID 格式的折叠规则，且 UUID
       规则必须先于裸 hex 规则匹配，否则 UUID 首段被裸 hex 规则先吃掉。
    2. 截断策略从纯截头改为「保留头 200 + 尾 100」，避免共享长前缀、
       尾部才有差异的错误（典型如 zod 多字段校验报错）被误判为同一条。
    """
    text = str(getattr(err, "message", None) or err)
    for pattern in _ID_PATTERNS:
        text = pattern.sub("<id>", text)
    text = re.sub(r"\s+", " ", text).strip()
    if len(text) > 300:
        text = text[:200] + "…" + text[-100:]
    return text


def record_degraded(degraded: list[dict], *, role: str, error: str,
                    attempts: int) -> None:
    """折叠同 role + 同规范化错误的降级记录。

    `agentType` 字段（cfr3-03 修复）：与 `role` 值完全相同，纯粹是为了让
    `round.py` 现有 `_describe_degraded()`（读 `d.get('agentType') or
    d.get('label')`）不改代码即可正确读取——处置表此前只是"宣称"这个双写，
    本任务把它变成真正执行且被测试断言的行为（第三轮评审 cfr3-03）。
    """
    for d in degraded:
        if d["role"] == role and d["error"] == error:
            d["occurrences"] += 1
            d["attempts"] += attempts
            return
    degraded.append({"role": role, "agentType": role, "error": error,
                     "occurrences": 1, "attempts": attempts})
```


- [ ] **Step 4**：跑通全部用例（绿）；重跑 `test_fanout.py` 全部（Task 5.1 用例应不受影响）。
- [ ] **Step 5（正控）**：临时把 `_ID_PATTERNS` 里的 UUID 正则移到裸 hex 正则**之后**，跑 `test_folds_uuid_trace_id`，确认失败（复现 rmf-10 指出的顺序敏感问题）；恢复到 UUID 在前。另临时把 `record_degraded` 的 `degraded.append(...)` 里 `"agentType": role` 这一项删掉（复现 cfr3-03 指出的"处置表宣称但代码未做"缺口），跑 `test_writes_agent_type_alias_for_round_describe_degraded`，确认失败（`agentType` 键缺失）；恢复。
- [ ] **Step 6**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
git commit -m "feat(harness): fanout —— 错误规范化折叠（修复 rmf-10 的 UUID/尾部差异漏检；record_degraded 真正双写 agentType，cfr3-03）" -- .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
```

### Task 5.3（处置 cfr-02/cfr-03/cfr-06/cfr-12/cfr2-07/cfr3-02/cfr4-02）：单次尝试原语 `run_one_attempt`（不含重试循环、不含并发、不含账本 IO）

**v5 重写说明**：v1 草图的 `run_role_with_retry` 把"单次调用"与"最多 3 次重试循环"揉在一个函数里，且直接用宽松 `**kwargs` 调 `invoke_fn`、直接访问 `conn` 写账本。cfr-12 指出重试循环应提升到跨角色的"波次"层（Task 5.4），本任务因此只保留一个不做重试、不做并发、不碰账本的最小原语——它只负责"用给定的 `RoleInvocationRequest` 发起一次调用，判定结果"。

**cfr2-07（Critical，本轮修复引入）**：`AttemptRecord` 新增两个显式布尔位 `retryable`/`resumable`，把"值得再试一次"与"能不能 fork 续接"拆成两个独立问题。

**cfr3-02/cfr4-02（Critical，四轮未真正关闭，本轮补齐两处遗漏分支）**：第二轮修订新增了 `retryable` 字段，但赋值规则仍是"`failed_transport` 恒 `True`、`capability_drift` 恒 `False`"——只是把"全部重试"策略换了名字。第三轮修订加了预算耗尽/确定性协议异常两条分类，但**第四轮评审指出这张表本身仍有未定义区域**：

1. **parser 层模型输出失败未落表**：模型返回无法解析或缺 `candidates` 的 payload 时（`claude_runner._parse_terminal_result` 里 `event.get("subtype") == "success"` 但 `_extract_payload`/`_extract_json_object` 返回 `None`），产出的 `InvocationResult` 是 `ok=False`、**`subtype="success"`**（终态事件本身没有报错，只是 payload 解不出来）、`protocol_errors=["unparseable or malformed payload in success result"]`。这个组合**进不了**第三轮表格任何一行——它不是 `subtype is None`（超时），不是 `error_max_budget_usd`，也不是 `validate()` 拒绝（那是 `invocation.ok=True` 时才会调用 `validate`，这里 `invocation.ok` 已经是 `False`，`validate` 根本不会被调用到）。`_classify_retryable` 对这个真实且高频的情形（真机实测中模型偶发返回非 JSON 文本是常见故障模式）此前结果**未定义**。修正：这类情形与"schema 校验失败"同源——都是模型输出的随机波动，可重试（沿用 rmf-07 精神）。
2. **`subtype=None` 不能一律判可重试**：v4 的表格把"`subtype` 为 `None`"整体归为"真正的传输抖动，可重试"，但 CLI 进程启动失败或认证失败同样不会产生任何终态事件，`subtype` 同样是 `None`——这类失败重试多少次都没用（配置/凭据问题，不是网络抖动）。**区分依据**：`claude_runner.invoke()` 里唯二两条不经过 `parse_stream_json()` 就直接返回的路径是 `subprocess.TimeoutExpired`（真正的执行中超时，`protocol_errors` 恒为空列表，因为这条路径根本不调用 `parse_stream_json`）；而 CLI 启动失败/认证失败会让进程带着空的或非法的 stdout 退出，仍然会走到 `parse_stream_json(空或非法 lines)`，产出 `init_count == 0` → `protocol_errors` 含 `"missing init event"`。**`protocol_errors` 是否含 `"missing init event"` 是可机械核对的区分点**：为空（超时路径）→ 可重试；含 `"missing init event"`（CLI 从未建立协议握手）→ 不可重试。

**可测试的终态分类表（v5，贯通覆盖 parser 层与 validate 层，按下方优先级顺序判断，命中即返回，不再继续检查后续行）**：

| 优先级 | 终态特征 | `status` | `retryable` | 理由 |
|---|---|---|---|---|
| 1 | `invocation.ok=True`，`validate(payload)` 无错误 | `success` | `False` | 已成功 |
| 2 | 能力漂移（`init_tools` 等与 `expected_tools` 不符） | `capability_drift` | `False` | 配置信号，重试大概率复现同样问题（cfr-06 既有结论） |
| 3 | `invocation.protocol_errors` 含 `"duplicate init events"` 或 `"duplicate terminal result events"`（同一次调用内部出现结构性重复） | `failed_transport` | `False` | 确定性协议异常——损坏的是这次调用产生的 stream 结构本身，重试大概率产生同样结构的 stream |
| 4 | `invocation.protocol_errors` 含 `"missing init event"`（CLI 进程退出但从未产生任何 init 事件——启动失败/认证失败/参数错误的特征，第四轮评审 cfr4-02 新增） | `failed_transport` | `False` | 配置/环境/凭据问题，重试多少次都没用，与"传输抖动"不同源 |
| 5 | `invocation.subtype == "error_max_budget_usd"` | `failed_transport` | `False` | 预算耗尽是确定性结果——同一 `grant_usd` 重试大概率再次撞线 |
| 6 | `invocation.ok=False` 且 `invocation.subtype == "success"` 且 `protocol_errors` 含 `"unparseable or malformed payload"`（终态事件本身没报错，是模型输出解不出 JSON，第四轮评审 cfr4-02 新增） | `failed_transport` | `True` | parser 层模型输出随机失败——与下一行 schema 校验失败同源，都是模型输出的随机波动，重试可能恢复（rmf-07 精神） |
| 7 | `validate(payload)` 返回非空错误列表（schema 校验失败，`invocation.ok=True`） | `failed_transport` | `True` | schema 校验失败在真机场景下多为模型输出随机波动，重试可能恢复（沿用 rmf-07 既有结论） |
| 8（默认兜底） | 以上均不命中——典型是 `invocation.subtype is None` 且 `protocol_errors` 为空（`subprocess.TimeoutExpired` 路径，真正的执行中超时），或 `subtype` 是其它非上述值且 `protocol_errors` 干净（如 `"error_during_execution"` 搭配传输层错误文本） | `failed_transport` | `True` | 真正的传输抖动/进程被中途杀死，值得重试 |
| — | `UnsafeInvocationError`（或任何编程/配置类异常）从 `invoke_fn` 抛出 | 不产生 `AttemptRecord`，异常穿透 | 不适用 | 配置错误不重试、不降级（既有规则，本任务不改） |

**接口契约**：

```python
@dataclass(frozen=True)
class AttemptRecord:
    role: str
    attempt: int
    status: str  # "success" | "failed_transport" | "capability_drift"（与 Task 1.2 状态词逐字一致）
    session_id: str | None = None
    parent_session_id: str | None = None
    cost_usd: float = 0.0
    cost_known: bool = True
    turns: int = 0
    denials: int = 0
    protocol_errors: list = field(default_factory=list)
    payload: dict | None = None
    last_error: str | None = None
    retryable: bool = False   # cfr2-07 新增，cfr3-02/cfr4-02 修正赋值规则（见上方分类表）
    resumable: bool = False   # cfr2-07 新增
    subtype: str | None = None  # cfr3-02 新增：原样透传 InvocationResult.subtype，供分类表判断依据可审计

def _classify_retryable(invocation: InvocationResult, status: str,
                        validation_errors: list[str]) -> bool: ...
    # 按上方 v5 终态分类表逐行实现，按优先级顺序判断、命中即返回；
    # status="success"/"capability_drift" 两行照旧恒定返回；
    # status="failed_transport" 时依次检查 protocol_errors 的三种特征
    # （duplicate/missing init/unparseable payload）与 subtype
    # （error_max_budget_usd），均不命中才落到默认可重试兜底
def _check_capability_drift(invocation: InvocationResult, expected_tools: frozenset[str]) -> list[str]: ...
def run_one_attempt(*, role: str, attempt: int, request: RoleInvocationRequest,
                    invoke_fn, validate,
                    expected_tools: frozenset[str] | None = None) -> AttemptRecord: ...
def build_continuation_request(previous: RoleInvocationRequest, resume_session_id: str) -> RoleInvocationRequest: ...
```

**不变量**：
1. `invoke_fn` 必须接受**唯一一个位置参数** `RoleInvocationRequest`（cfr-02）——不用宽松 `**kwargs`，测试替身与生产代码共用同一个类型。
2. `AttemptRecord` 是纯数据（`dataclasses.asdict()` 递归取值不含任何 `Connection`/`Lock` 类型实例），可安全跨线程通过 `future.result()` 传回主线程（cfr-03 的前提）。
3. `expected_tools` 非空且 `invocation.init_seen` 为真时才做能力漂移检查（cfr-06）；命中漂移 → `status="capability_drift"`，`retryable=False`（分类表优先级 2），`resumable` 按下方规则从 `invocation.session_id` 是否非空决定。
4. **`retryable` 的赋值必须调用 `_classify_retryable`（v5 分类表），按优先级顺序逐行判断，不得用"是否等于 `capability_drift`"这种二元判断代替，也不得只覆盖 validate 层而遗漏 parser 层的同类失败**（cfr3-02/cfr4-02 核心：`_classify_retryable` 必须能正确处理"`invocation.ok=False` 但 `subtype="success"`"这个 parser 层失败的具体组合，不能假设"`ok=False` 就必然是传输失败"）。
5. `resumable` 的计算**只看** `invocation.session_id`（`InvocationResult` 字段），不回退到 `request.session_id`/`request.resume`——`session_id`（`AttemptRecord` 字段，供审计与下一波构造 `parent_session_id` 用）允许在 `invocation.session_id` 为空时回退到 `request.session_id or request.resume`（这是"记录我们认为这次尝试用的是哪个身份"，与"能否安全 fork"是两件事，`resumable` 只回答后者）。
6. 编程/配置类异常（如 `UnsafeInvocationError`）不被本函数捕获，原样穿透——不属于"传输故障"，不重试、不降级。
7. `build_continuation_request`：`resume`+`fork_session=True`，`session_id` 置空（与原有互斥校验一致），`prompt` 换成续接指令（不是重发原始任务）。
8. `AttemptRecord.subtype` 原样保留 `InvocationResult.subtype`（不做转换），供下游（账本、故障排查）审计"这次判定 `retryable` 的依据具体是什么"，不是只留一个布尔结论看不出理由。
9. **优先级顺序不可颠倒**（cfr4-02）：`protocol_errors` 的三种检查（duplicate/missing init/unparseable payload）必须先于"默认兜底可重试"判断；`error_max_budget_usd` 检查必须先于"默认兜底"；这保证"表面上都是 `ok=False`"的多种情形被正确分流，不会被更靠后的宽泛规则提前捕获。

**测试清单**（断言点，不是完整测试源码）：
1. 成功调用 → `status="success"`，`session_id` 取自 `InvocationResult.session_id`，`payload` 原样带出，`retryable=False`。
2. 传输失败（`invocation.ok=False`，`subtype=None`，`protocol_errors=[]`，模拟真正的执行中超时）→ `status="failed_transport"`，`retryable=True`，`last_error` 非空。
3. schema 校验失败（`invocation.ok=True` 但 `validate()` 返回错误）→ `status="failed_transport"`，`retryable=True`（不是致命错误，沿用 rmf-07 结论）。
4. `UnsafeInvocationError` 从 `invoke_fn` 抛出 → 原样穿透，不被本函数捕获或转换。
5. 能力漂移（`init_tools` 多出未预期工具）→ `status="capability_drift"`，`retryable=False`，`last_error` 含多出的工具名。
6. 工具集与期望一致 → 不触发漂移分支，走正常成功/失败判定。
7. `AttemptRecord` 是 dataclass，且其字段值集合里不含 `Connection`/`Lock` 类型实例（纯数据验证）。
8. **cfr2-07 核心之一**：`InvocationResult(session_id=None, ...)`（模拟超时/进程未及 `init` 即被杀）+ 调用本身失败 → `AttemptRecord.resumable == False`，即便 `request.session_id` 非空。
9. **cfr2-07 核心之二**：`InvocationResult(session_id="real-sid", ...)`（CLI 真实报告过）→ `AttemptRecord.resumable == True`。
10. `build_continuation_request` 产出的请求 `resume == 传入的 session_id`、`fork_session is True`、`session_id is None`、`prompt` 与原始 prompt 不同。
11. **cfr3-02 核心之一（不可重试）**：`InvocationResult(ok=False, subtype="error_max_budget_usd", ...)` → `status="failed_transport"` 但 `retryable=False`（区别于用例 2 的普通传输故障）。
12. **cfr3-02 核心之二（不可重试）**：`InvocationResult(ok=False, protocol_errors=["duplicate terminal result events: 2"], ...)` → `status="failed_transport"` 但 `retryable=False`（确定性协议异常，不是随机传输抖动）。
13. **cfr3-02 核心之三（可重试，对照组）**：`InvocationResult(ok=False, subtype="error_during_execution", protocol_errors=[], raw_tail="API Error: Server error mid-response", ...)`（无 `protocol_errors`，非预算耗尽）→ `status="failed_transport"` 且 `retryable=True`（真正的传输抖动分支仍然可重试，不能因为新增分类而误伤这条既有路径）。
14. **cfr4-02 核心之一（parser 层失败，可重试）**：`InvocationResult(ok=False, subtype="success", protocol_errors=["unparseable or malformed payload in success result"], payload=None, ...)` → `status="failed_transport"` 且 `retryable=True`（终态事件本身报的是 `success`，只是 payload 解不出来——第四轮评审指出的、v4 分类表未定义的具体组合，现在有明确归属）。
15. **cfr4-02 核心之二（CLI 启动/认证失败，不可重试）**：`InvocationResult(ok=False, subtype=None, protocol_errors=["missing init event", "missing terminal result event"], ...)`（模拟 CLI 进程退出但从未产生 init 事件）→ `status="failed_transport"` 但 `retryable=False`（区别于用例 2 的"真正超时"——两者都是 `subtype=None`，但 `protocol_errors` 是否含 `"missing init event"` 是区分依据）。
16. **优先级顺序验证（cfr4-02）**：构造一个同时满足"`protocol_errors` 含 `missing init event`"与"`subtype` 恰好是 `error_max_budget_usd`"的病态输入（人为构造，验证代码鲁棒性而非真实场景）→ 断言 `_classify_retryable` 按分类表优先级顺序（`missing init event` 检查在前）返回，不因为检查顺序写反而产生歧义结果——这条测试的价值是把"优先级顺序"从注释里的口头约定变成可回归的断言。

- [ ] **Step 1**：按测试清单写测试（追加到 `test_fanout.py`），跑至因 `AttemptRecord.retryable`/`resumable`/`subtype` 字段或 `run_one_attempt`/`_classify_retryable` 不存在而红。
- [ ] **Step 2**：按接口契约实现（`fanout.py` 追加 `AttemptRecord`/`_classify_retryable`/`_check_capability_drift`/`run_one_attempt`/`build_continuation_request`；`_classify_retryable` 内部按分类表优先级顺序写 `if`/`elif` 链，不用字典查表这种不保证顺序的写法）。
- [ ] **Step 3**：跑通全部用例；重跑既有 `test_fanout.py`（Task 5.1/5.2）确认无回归。
- [ ] **Step 4（正控）**：临时把 `resumable` 的计算改为直接读 `bool(session_id)`（即回退到 `request.session_id` 也算数），跑用例 8，确认变红（复现 cfr2-07 指出的"用预分配值冒充真实 ID"）；恢复。另临时把 `_classify_retryable` 简化回"`status != 'capability_drift'` 就返回 `True`"（复现四轮未关闭的 cfr-12/cfr3-02/cfr4-02 缺陷），跑用例 11/12/15，确认变红（预算耗尽、确定性协议异常、CLI 启动失败均被误判为可重试）；恢复。**再做一次针对性正控（cfr4-02 要求）**：临时把 `_classify_retryable` 里"`protocol_errors` 含 `missing init event`"这一行判断删掉（但保留其余分支不变），跑用例 15，确认变红（CLI 启动失败被误判为可重试，落到默认兜底）；恢复。这一步专门验证"CLI 启动失败"与"真正超时"两个同为 `subtype=None` 的分支确实被分开处理，不是共用同一段默认逻辑侥幸算对。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
git commit -m "feat(harness): fanout —— run_one_attempt 单次尝试原语，retryable 分类表贯通覆盖 parser 层与 validate 层失败、区分 CLI 启动失败与真正超时（cfr-02/03/06/12, cfr2-07, cfr3-02, cfr4-02）" -- .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
```


### Task 5.4（处置 cfr-03/cfr-05/cfr-09/cfr-12/cfr2-03/cfr2-07/cfr4-01）：`BudgetTracker`（线程安全原子预留，允许变负）+ 波次调度器 `run_wave_scheduled`（返回全部 attempts，per-attempt stream_log 贯穿）

**v4 重写说明（在 v3 基础上修正第四轮评审新发现的一处设计缺陷）**：

- **cfr2-03（Critical，本轮修复引入）第一处**：v2 的 `BudgetTracker.settle()` 用 `remaining += max(reserved - actual, 0.0)` 计算刷正差额——`actual > reserved`（超额）时这个表达式恒为 0，超支部分从未从 `remaining` 里真正扣除（评审实测：预留 0.3、实花 0.5，`remaining` 仍是刷正前的值 + 0.3，等同于超支被系统免单）。修正：`settle()` 无条件执行 `remaining += reserved - actual`（不夹 `max`），允许 `remaining` 变负；一旦变负，任何后续 `try_reserve(amount>0)` 因 `remaining < amount` 恒为 `False`，自然阻止后续调用，不需要额外的"熔断"分支。
- **cfr2-03 第二处**：v2 的 `run_wave_scheduled` 只返回 `{role: 最终 AttemptRecord}`，前几波失败的尝试（连同它们已花费的真实成本）在返回值里完全消失，Task 5.6 的 `FanoutSettlement` 据此聚合的总成本会漏掉这部分真实支出。修正：返回类型改为 `WaveResult`（见下），同时携带 `final`（供路由/裁决消费，行为与 v2 一致）与 `all_attempts`（供 Task 5.6 聚合全部真实花费，不止最后一条）。
- **cfr2-07（Critical，本轮修复引入）**：v2 用 `request.session_id`（预分配、派生值）冒充"上一次的真实 session_id"去构造 fork 续跑请求；但超时或进程在 `init` 事件之前被杀时，这个会话可能从未在 CLI 侧真正创建，`--resume <伪造 ID>` 大概率失败或续接到错误上下文。修正：`AttemptRecord`（Task 5.3 同步新增两个字段，见该任务）携带 `resumable: bool`（`True` 当且仅当 `session_id` 取自 `InvocationResult.session_id`——即 CLI 通过 `init`/`result` 事件真正报告过的值，而不是回退用的 `request.session_id`/`request.resume`）。本任务的波次调度器只在 `record.retryable and record.resumable` 同时成立时才发起 fork 续跑；`retryable=True` 但 `resumable=False` 时发起一次**全新**尝试。
- **cfr-09（沿用 v2 判断逻辑，新增动态 `timeout_s`）**：v2 只判断"截止时间是否足够开始新的一波"，但请求本身的 `timeout_s` 从未随之收缩（仍固定为 60 秒）。修正：每一波构造请求时，`timeout_s` 取 `min(原始 timeout_s 上限, deadline_monotonic - time.monotonic() - _CALL_TEARDOWN_MARGIN_S)`，随实际剩余时间收缩，不小于 0；不使用 `max()` 垫底（那正是 cfr-09 指出的、`round.py` 历史上已修过一次的反模式）。
- **cfr-12**：波次调度"是否进入下一波"的判定，从"看 `status` 字面量做 if/elif"改为直接读 `AttemptRecord.retryable` 布尔位（Task 5.3 产出）——状态判定与重试策略解耦，以后新增状态词不需要同时改这里的分支逻辑。
- **cfr4-01（Critical，第四轮评审指出，因契约未贯穿波次调度而暴露的具体 bug）**：v4 的 `make_request` 工厂签名是 `make_request(role: str) -> RoleInvocationRequest`——**不接受 `attempt` 参数**。`build_continuation_request(previous, resume_session_id)`（Task 5.3 定义）用 `dataclasses.replace(previous, prompt=..., session_id=None, resume=..., fork_session=True)` 从**上一次的请求对象**派生新请求，只覆盖了 `prompt`/`session_id`/`resume`/`fork_session` 四个字段——**`stream_log` 字段原样保留自 `previous`，从未随 attempt 编号更新**。真实后果：attempt 1 失败后 fork 出 attempt 2，attempt 2 会把输出写到与 attempt 1 完全相同的 `stream_log` 路径，**覆盖掉 attempt 1 的日志**——而 attempt 1 恰恰是失败的那一次，是最需要事后判因的一次。**修正**：`make_request` 工厂签名改为 `make_request(role: str, attempt: int) -> RoleInvocationRequest`（Task 5.5 的 `_make_request` 内部据此调用 `role_invocation.build_stream_log_path(context.stream_log_dir, round_id, task_role, attempt)`，`attempt` 参数直接决定 stream_log 路径的一部分，见 Task 5.5）；`run_wave_scheduled` 在每一波（包括 fork 续跑）都先调用 `make_request(role, attempt)` 取得这个 attempt 编号对应的"骨架请求"（含正确的 per-attempt `stream_log`/`session_id` 默认值），再在 fork 场景下用 `build_continuation_request(base_request, resume_session_id)` 只覆盖 `prompt`/`session_id`/`resume`/`fork_session` 四个字段（`stream_log` 取自这个"骨架请求"，不再取自 `previous`）——`build_continuation_request` 的第一个参数含义因此从"上一次的请求"变为"本次 attempt 的骨架请求"，两者字段集合相同，唯一区别是调用点传入哪个对象。

**接口契约**：

```python
@dataclass(frozen=True)
class WaveResult:
    final: dict[str, AttemptRecord]        # 每角色最终（或最后已知）记录
    all_attempts: list[AttemptRecord]      # 全部角色 × 全部波次的每一次尝试，顺序=发生顺序

class BudgetTracker:
    def __init__(self, total_usd: float) -> None: ...
    def try_reserve(self, amount: float) -> bool: ...
    def settle(self, *, reserved: float, actual: float, cost_known: bool) -> None: ...
    def remaining(self) -> float: ...  # 可为负

def run_wave_scheduled(*, roles: tuple[str, ...], make_request, invoke_fn, validate,
                       budget: BudgetTracker, deadline_monotonic: float,
                       single_call_cap_usd: float = _DEFAULT_SINGLE_CALL_CAP_USD,
                       expected_tools: frozenset[str] | None = None,
                       conn=None, round_id: str = "") -> WaveResult: ...
    # make_request 签名变化（cfr4-01）：Callable[[str, int], RoleInvocationRequest]
    # ——第二个参数是本次调用对应的 attempt 编号（从 1 起），供调用方构造
    # per-attempt 的 stream_log 路径；run_wave_scheduled 每一波都传入当前
    # attempt 编号调用它，不止 attempt 1 才调用
```

**不变量**：
1. `BudgetTracker.try_reserve`/`settle` 由同一把 `threading.Lock` 保护，"读剩余-判断-扣减"是单个原子临界区。
2. `settle()` 的刷正表达式是 `remaining += reserved - actual`，**不夹 `max(...,0.0)`**——`actual > reserved` 时 `remaining` 可以变负；`cost_known=False` 时整个 `settle()` 提前返回，不做任何刷正（预留额度按最坏情形继续占用，与 `budget.py` 现有 `abandon()` 语义一致）。
3. `remaining() < 0` 之后，任何正数 `try_reserve()` 调用返回 `False`——这是"变负值"的自然推论，不是额外分支，不需要单独维护"是否已耗尽"标志位。
4. `run_wave_scheduled` 返回的 `all_attempts` 长度 = 全部角色在全部波次里**实际发起调用**的次数总和（因预算不足/截止时间不足而未发起调用的角色不产生 `AttemptRecord`，只体现在 `final` 里）。
5. 每一波开始前，若 `deadline_monotonic - time.monotonic() < _MIN_CALL_WINDOW_S`，尚未成功的角色直接标记 `failed_transport`（`last_error` 含 `"deadline-exhausted"`）并结束循环，不构造请求、不发起调用、不计入 `all_attempts`。
6. 每次实际构造的请求，其 `timeout_s` 字段等于 `min(request 原 timeout_s, deadline_monotonic - time.monotonic() - _CALL_TEARDOWN_MARGIN_S)`，不小于 0；不使用 `max(x, 下限)` 这种会在剩余为负时把负值垫成正值的写法。
7. 决定"该角色是否进入下一波"只读 `AttemptRecord.retryable`（`True` 才留在 pending）；每一波无论 fork 与否都先调用 `make_request(role, attempt)` 取得该 attempt 编号对应的骨架请求（含正确的 per-attempt `stream_log`，cfr4-01）；`retryable=True` 且 `resumable=False` 时直接使用这个骨架请求发起**全新**（非 fork）尝试；`retryable=True` 且 `resumable=True` 时用 `build_continuation_request(骨架请求, resume_session_id)` 在骨架请求基础上覆盖 `prompt`/`session_id`/`resume`/`fork_session` 四个字段后发起 fork。
8. 主线程串行写账本（继承 v2）；`ledger.record_attempt_started/finished` 的调用点显式 `try/except Exception`（cfr-11 修复：v2 只在文字里声明"写账本失败不阻断本轮"，代码里从未真正包裹）——捕获后仅记录到 stderr（或调用方注入的 logger），不重新抛出，本轮流程继续。
9. **每个 attempt 的 `stream_log` 路径必须随 attempt 编号变化，不得复用上一次 attempt 的路径**（cfr4-01）：`all_attempts` 里任意两条记录，若角色相同但 `attempt` 不同，两者对应发出的请求 `stream_log` 字段必须不同——这是 fork 场景下"attempt 1 的失败日志不被 attempt 2 覆盖"这个具体后果的直接不变量。

**测试清单**（断言点，不是完整测试源码）：

`BudgetTracker`：
1. 充足预算下 `try_reserve` 成功并正确扣减。
2. 预算不足时 `try_reserve` 失败，`remaining` 不变。
3. 10 线程并发 `try_reserve(0.3)`、总额 1.0 → 成功次数 × 0.3 ≤ 1.0（无超支）。
4. `settle(reserved=0.3, actual=0.1, cost_known=True)` → `remaining` 相比 settle 前增加 0.2（正确退回未用完的预留）。
5. **cfr2-03 核心**：`settle(reserved=0.3, actual=0.5, cost_known=True)` → `remaining` 相比 settle 前**减少** 0.2（超额部分被扣除，不是恒为 0 的 `max` 表达式）。
6. `settle(..., cost_known=False)` → `remaining` 相比 settle 前不变（保留原预留额度）。
7. 连续两次超额 `settle` 后 `remaining` 变负；此后任何 `try_reserve(amount>0)` 返回 `False`。

`run_wave_scheduled`／`WaveResult`：
1. 全部角色首波成功 → `final` 含全部角色、状态皆 `success`；`all_attempts` 长度等于角色数（每角色恰一次尝试）；`make_request` 收到的 `attempt` 参数恒为 `1`。
2. 一角色首波失败（`retryable=True, resumable=True`，即返回了真实 `session_id`）、次波经 fork 成功 → `final[该角色].attempt == 2`；`all_attempts` 含该角色两条记录（首波失败 + 次波成功）；`make_request` 第二次被以 `attempt=2` 调用（不是只在 attempt 1 调用一次）。
3. **cfr2-07 核心**：一角色首波因超时失败（从未观察到真实 `session_id`，`resumable=False`，但 `retryable=True`）→ 次波必须是全新尝试（断言传给 `invoke_fn` 的 `request.resume is None` 且 `request.fork_session is False`，`request.session_id` 等于该角色 attempt=2 的新派生值，不等于首波使用的值）。
4. **cfr2-03 核心**：一角色连续两波失败，每波 `reserved=0.3, actual=0.5`（超额）→ 调用结束后 `budget.remaining()` 体现两次超额的累计扣减（不是只扣一次），且 `all_attempts` 长度为 2（两次尝试都被记录，即便都失败）。
5. 截止时间在波次开始前已耗尽 → `final` 标记 `failed_transport` 且 `last_error` 含 `"deadline-exhausted"`；`invoke_fn` 全程未被调用；`all_attempts` 为空。
6. 预算不足以覆盖第二波 → 第二波不发起调用（`invoke_fn` 调用次数与预算允许的波次数一致）。
7. 能力漂移（`expected_tools` 与返回 `init_tools` 不等）→ `AttemptRecord.retryable` 为 `False`（不重试），只调用一次，`final` 状态为 `capability_drift`。
8. **cfr-09 核心**：用两个不同的 `deadline_monotonic`（一个宽松、一个紧张）各跑一次，断言两次传给 `invoke_fn` 的 `request.timeout_s` 不相等，且都不等于任何硬编码常量（如 `60.0`）——验证 timeout 随截止时间动态收缩。
9. **cfr-11 核心**：注入一个在 `record_attempt_started`/`finished` 调用时抛异常的假 `conn`（或 monkeypatch `ledger` 模块函数）→ `run_wave_scheduled` 正常返回预期的 `WaveResult`，不因账本写入异常而中断或向上抛出。
10. **cfr4-01 核心（fork 后 stream_log 不覆盖）**：一角色首波失败（`resumable=True`）、次波经 fork 成功——构造 `make_request(role, attempt)` 返回的骨架请求，其 `stream_log` 字段按 `attempt` 参数变化（如 `f"log-attempt-{attempt}.jsonl"` 这种测试替身）→ 断言传给 `invoke_fn` 的 attempt 1 请求与 attempt 2 请求的 `stream_log` 字段**不相等**（验证 fork 场景下确实调用了 `make_request(role, 2)` 取得新骨架、而不是直接复用 attempt 1 请求对象的 `stream_log`）。

- [ ] **Step 1**：按上方测试清单写测试（追加到 `test_fanout.py`），跑至因 `WaveResult`/新签名/新字段不存在而红。
- [ ] **Step 2**：按接口契约与不变量实现 `BudgetTracker`、`WaveResult`、`run_wave_scheduled`（依赖 Task 5.3 同步产出的 `AttemptRecord.retryable`/`resumable` 字段与 `session_identity.derive_session_id`，两个任务需交叉核对字段名一致；`make_request` 每一波都调用，不止 attempt 1）。
- [ ] **Step 3**：跑通全部用例；重跑既有 `test_fanout.py` 用例（Task 5.1–5.3）确认无回归。
- [ ] **Step 4（正控）**：临时把 `settle()` 的刷正表达式改回 `remaining += max(reserved - actual, 0.0)`，跑用例 5 与 7，确认变红（复现 cfr2-03 指出的缺陷）；恢复。另临时把"是否 fork"的判断从 `record.retryable and record.resumable` 改回只看 `record.retryable`，跑用例 3，确认变红（复现 cfr2-07 指出的"用预分配 ID 冒充真实 ID 去 fork"）；恢复。再临时把 fork 分支改回"直接对 attempt 1 的请求对象调用 `build_continuation_request`"（不先调用 `make_request(role, 2)` 取新骨架），跑用例 10，确认变红（两次 attempt 的 `stream_log` 相同，复现 cfr4-01 指出的"fork 后覆盖上一次日志"缺陷）；恢复。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
git commit -m "feat(harness): fanout —— BudgetTracker 允许变负结算 + 波次调度器返回全部 attempts，make_request 携带 attempt 编号确保 fork 后 stream_log 不覆盖上一次日志（cfr-03/05/09/12, cfr2-03/07, cfr4-01）" -- .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
```


### Task 5.5（处置 cfr-06/cfr-07/cfr-14/cfr2-02/cfr2-04/cfr3-01/cfr4-01）：finder 并发扇出 + judge 短路裁决（组合编排，消费 `WaveResult`）

**v5 重写说明**：本任务把 `run_finders`/`judge_candidate` 改为调用 Task 5.4 产出的 `run_wave_scheduled`（返回 `WaveResult`，而非 v2 的裸 `dict[str, AttemptRecord]`），并在 v2 已修复的三处（`skipped_judges` cfr-14、judge 降级同步顶层 `degraded` cfr-07、`expected_tools` 透传 cfr-06）之上，补齐四处评审指出的设计缺陷：

- **cfr2-02（Critical，已核实）**：`judge_candidate` 每次调用都必须为该候选构造**携带 fingerprint 的 task identity**（Task 1.1 的新设计），不能再对 `judge:redline` 这类静态角色字符串重复派生 session_id——否则同一轮内两个候选依次裁决时，第二个候选的账本写入会与第一个候选撞主键，且实际发出的是第一个候选的会话。本任务的 `_make_request` 必须先算出 `task_role = f"{role}:{queue.fingerprint(candidate['goal'], candidate['invariant'], candidate['primary_path'], candidate['oracle'])}"`，把 `task_role`（而不是裸的 `judge:redline`）传给 `session_identity.derive_session_id`、`run_wave_scheduled` 的 `roles=`、以及 `AttemptRecord.role`/`all_records`/`degraded` 里记录的 `role` 字段——四处统一（Task 1.1 已定的不变量），`_JUDGE_ROLE_TO_TYPE` 等按类型分派的查表逻辑改为对 `task_role` 做前缀匹配（`task_role.split(":", 2)[:2]` 取出 `("judge", "redline")` 这一级用于查表），不能整串相等匹配。
- **cfr2-04（Critical，已核实）**：`judge_candidate` 的三次 `run_wave_scheduled` 调用（redline 一次、`completed`+`oracle` 一次）此前均未传 `expected_tools`，Bash/MCP 等能力漂移在 judge 侧完全不会被发现。修正：`judge_candidate` 与 `run_finders` 一样，把 `_STAGE1_EXPECTED_TOOLS` 透传给每一次 `run_wave_scheduled` 调用。
- **cfr3-01（Critical，因降级丢失，第三轮评审指出）**：此前的测试清单第 6 条只断言"两个候选调用的 `request.session_id` 不相等"，没有验证 `attempt_key`（账本主键）与 `stream_log` 路径是否同样由这个含 fingerprint 的 task identity 派生。本任务补上跨三处的联合测试，并要求 `_make_request` 显式使用 `role_invocation.build_stream_log_path`（Task 2.3 新增）而不是自行拼接 stream 路径字符串；`cwd`/`settings_path`/`model` 三个字段的值改为从调用方传入的 `RequestContext`（Task 2.3 新增）读取，不再是本函数内部硬编码的占位字符串。
- **cfr4-01（Critical，第四轮评审指出，两处修正）**：
  1. **联合测试的 oracle 本身写错了**——v4 的测试清单第 8 条要求"`request.session_id` 与 `attempt_key`/`stream_log` 路径里的 `round_id:task_role:attempt` 段**逐字一致**"，但 `session_id` 是 `session_identity.derive_session_id()` 产出的 **UUIDv5**（一段确定性哈希值，形如 `d4a0c718-d6eb-43c7-ab6d-aaabadc9966d`），**不可能**与 `round_id:task_role:attempt` 这种明文拼接字符串相等——这条测试若按字面实现，一写出来就必然失败，任何按此验收的实施者都会卡在这一步。**正确的 oracle**：三者不是"字面相等"关系，而是"**同源**"关系——分别独立计算出预期的 `session_id`（调用 `session_identity.derive_session_id(round_id, task_role, attempt)`）、预期的 `attempt_key`（`f"{round_id}:{task_role}:{attempt}"`）、预期的 `stream_log` 路径（调用 `role_invocation.build_stream_log_path(...)`），再断言被测函数产出的三个值分别等于这三个**独立计算的预期值**——这才是能验证"三处用了同一个 `task_role`"的正确断言方式，而不是要求三个不同类型的值互相字面相等。
  2. **`_make_request` 签名需要接受 `attempt` 参数**（承接 Task 5.4 cfr4-01 的贯通修复）：Task 5.4 的 `make_request` 工厂签名已改为 `(role: str, attempt: int) -> RoleInvocationRequest`，本任务的 `_make_request`（`run_finders`/`judge_candidate` 内部构造后传给 `run_wave_scheduled` 的那个闭包）必须实现这个新签名，`attempt` 参数直接传给 `role_invocation.build_stream_log_path(context.stream_log_dir, round_id, task_role, attempt)`——这保证了同一角色的不同 attempt 产出不同的 `stream_log` 路径，fork 续跑不会覆盖上一次失败尝试的日志。

**接口契约**：

```python
_STAGE1_EXPECTED_TOOLS: frozenset[str]  # {"Read", "Grep", "Glob"}，finder/judge 共用

def run_finders(*, round_id: str, invoke_fn, budget: BudgetTracker,
                deadline_monotonic: float, blocked_lanes: list[str],
                known_canonical_keys: set[str],
                context: "role_invocation.RequestContext",
                agents: dict[str, AgentDef] | None = None,
                conn=None, all_records: list[AttemptRecord] | None = None
                ) -> tuple[list[dict], list[dict]]: ...  # (ranked_candidates, degraded)

def judge_candidate(*, round_id: str, candidate: dict, invoke_fn,
                    budget: BudgetTracker, deadline_monotonic: float,
                    inflight_paths: list[str],
                    context: "role_invocation.RequestContext",
                    agents: dict[str, AgentDef] | None = None,
                    conn=None, all_records: list[AttemptRecord] | None = None
                    ) -> tuple[list[dict], list[dict]]: ...  # (verdicts, degraded)
```

**不变量**：
1. `run_finders`/`judge_candidate` 都从 `run_wave_scheduled` 返回的 `WaveResult.final` 读取每角色最终结果（供路由/裁决），并把 `WaveResult.all_attempts` 的全部记录追加进调用方传入的 `all_records`（而不是像 v2 那样只追加 `final` 里的每角色一条）——这是 Task 5.4 的 `WaveResult` 存在的直接原因：Task 5.6 的结算聚合需要看到全部尝试的真实花费，不止最后一次。
2. `judge_candidate` 每次构造请求前，先算出携带 fingerprint 的 `task_role`（cfr2-02），并在**全部**下游标识（`derive_session_id` 的 `role` 参数、`run_wave_scheduled` 的 `roles=`、`AttemptRecord.role`、账本 `attempt_key`、`role_invocation.build_stream_log_path` 的 `task_role` 参数）里统一使用它；不同候选（不同 `fingerprint`）算出的 `task_role` 必须不同（cfr2-02/cfr3-01）。
3. `judge_candidate` 的每一次 `run_wave_scheduled` 调用都传 `expected_tools=_STAGE1_EXPECTED_TOOLS`（cfr2-04）；`run_finders` 同理（沿用 v2）。
4. redline judge 返回 `reject`（或降级/失败后按"降级即拒绝"规则解析出 `reject`）→ 短路，不调用另外两个 judge；此时 `verdicts` 只含 redline 一条，`skipped_judges` 字段列出被跳过的另外两个 judge 类型（`harness-judge-completed`、`harness-judge-oracle`）。
5. redline 通过 → 继续裁决另外两个 judge，三条 verdict 的 `skipped_judges` 均为空列表。
6. 任一 judge 降级（耗尽重试仍失败）→ 该 judge 的局部 verdict 是 `{"judge": ..., "verdict": "reject", "reason": "judge-unavailable", <该 judge 专有字段>: None, "degraded": True, "skipped_judges": [...]}`（rmf-12 占位字段 + cfr-14 skipped_judges），**同时**该 judge 被 `record_degraded` 记入本函数返回的顶层 `degraded` 列表（cfr-07：不能只在局部 verdict 体现，顶层数组必须同步非空，否则 round.py 侧会把这种场景误判为干净的 no-candidate，精确复发 rmf-03）。
7. `run_finders` 里任一 finder 非成功 → 记入 `degraded`（`record_degraded` 折叠），不产出候选；`known_canonical_keys`/`blocked_lanes` 的过滤逻辑复用 Task 5.1 的 `dedupe_and_rank`，不重复实现。
8. **`_make_request` 的 `cwd`/`settings_path`/`model` 三个字段值取自入参 `context: RequestContext`**（cfr3-01），不是函数内部硬编码的字面量——生产路径（Phase 6 接线）传入携带真实值的 `RequestContext`，测试路径传入携带测试替身值的 `RequestContext`，两条路径共用同一个"从哪里取值"的结构，不会出现"生产该传什么值"只停留在文档说明、代码里各自硬编码一份的情况。
9. **`_make_request` 签名是 `(role: str, attempt: int) -> RoleInvocationRequest`**（cfr4-01，与 Task 5.4 `run_wave_scheduled` 要求的 `make_request` 签名一致），其 `stream_log` 字段通过 `role_invocation.build_stream_log_path(context.stream_log_dir, round_id, task_role, attempt)` 构造，不自行拼接路径字符串——`task_role` 与传给 `derive_session_id`/账本的值完全相同（同一个变量，不重新计算），`attempt` 参数保证同一角色不同尝试产出不同路径。

**测试清单**（断言点，不是完整测试源码）：

`run_finders`：
1. 四个 finder 都成功、一个产出一条候选 → 返回 1 条候选、`degraded` 为空。
2. 一个 finder 持续失败耗尽重试、其余成功 → 该 finder 不影响其余候选产出，`degraded` 含且仅含该 finder 一条。

`judge_candidate`：
3. redline 返回 `reject` → 只调用 redline 一次（其余两个 judge 的 `invoke_fn` 分支不应被触发），`verdicts` 长度为 1，`skipped_judges` 含另外两个 judge 类型。
4. redline 返回 `pass` → 三个 judge 都被调用，三条 verdict 的 `skipped_judges` 均为空。
5. 全部 judge 持续失败降级 → redline 局部 verdict 的 `verdict/reason/degraded/skipped_judges` 字段符合 6 号不变量；**顶层 `degraded` 长度为 1 且 `role` 字段等于该次调用实际使用的 `task_role`**（不是裸 `judge:redline`）。
6. **cfr2-02 核心**：对两个不同 `candidate`（不同 fingerprint）分别调用 `judge_candidate`，断言两次调用中传给 `invoke_fn` 的 `request.session_id` 不相等（验证 task identity 确实携带了 fingerprint，不会在同一轮内撞车）。
7. **cfr2-04 核心**：任一 judge 调用返回能力漂移（`init_tools` 含未预期工具）→ 该 judge 的 `AttemptRecord.status == "capability_drift"` 被正确传导（通过 mock/monkeypatch 断言传给 `run_wave_scheduled` 的 `expected_tools` 参数非 `None`，或直接构造漂移场景断言最终 verdict 走"judge-unavailable"降级分支而非误判为成功）。
8. **cfr3-01/cfr4-01 核心（联合测试，oracle 已订正为"同源"而非"字面相等"）**：对**两个不同候选** `candidate_a`/`candidate_b`（不同 fingerprint）分别调用 `judge_candidate`，各自捕获传给 `invoke_fn` 的 `request.session_id` 与 `request.stream_log`，以及写入账本的 `attempt_key`（通过传入的假 `conn`/`ledger` 替身捕获实际写入的主键值）。对每个候选**独立计算**三个预期值：`expected_session_id = session_identity.derive_session_id(round_id, task_role, 1)`、`expected_attempt_key = f"{round_id}:{task_role}:1"`、`expected_stream_log = role_invocation.build_stream_log_path(context.stream_log_dir, round_id, task_role, 1)`（`task_role` 由测试自己按 cfr2-02 的公式算出）。断言：(a) 被测函数产出的 `request.session_id`/账本 `attempt_key`/`request.stream_log` **分别等于**这三个独立计算的预期值（不是三者互相字面相等）；(b) 两个候选之间，这三组实际值**分别互不相同**（不会出现"session_id 变了但 stream_log 路径没变"这种部分更新的漂移）。
9. **cfr3-01 核心（生产值验证）**：传入一个 `RequestContext(cwd="/real/repo", settings_path=".claude/harness-settings.json", model="claude-sonnet-5", stream_log_dir="/real/repo/.claude/state/rounds")` → 断言传给 `invoke_fn` 的**全部**请求（4 finder + 最多 3 judge）的 `cwd`/`settings_path`/`model` 字段都等于这个 `RequestContext` 里的值，不是函数内部残留的硬编码占位符（如 `"/tmp"`、`""`、`None`）。
10. **cfr4-01 核心（fork 后 stream_log 不覆盖，端到端）**：构造 redline judge 首波失败（`resumable=True`）、次波 fork 成功的场景 → 断言 attempt 1 与 attempt 2 两次调用 `invoke_fn` 收到的 `request.stream_log` 不相等（`build_stream_log_path` 的 `attempt` 参数从 1 变为 2），这条测试验证 `_make_request(role, attempt)` 签名确实按 attempt 变化产出不同路径，是 Task 5.4 用例 10 在 `judge_candidate` 这一层的端到端复现。

- [ ] **Step 1**：按测试清单写测试（追加到 `test_fanout.py`），跑至因签名/字段不匹配而红。
- [ ] **Step 2**：按接口契约与不变量实现 `run_finders`/`judge_candidate`（复用 Task 1.1 的 `session_identity.derive_session_id`、Task 2.3 的 `role_invocation.RequestContext`/`build_stream_log_path`、Task 5.4 的 `run_wave_scheduled`/`WaveResult`；`_make_request` 内部签名与 `run_wave_scheduled` 要求的 `make_request(role, attempt)` 一致）。
- [ ] **Step 3**：跑通全部用例；重跑既有 `test_fanout.py`（Task 5.1–5.4）确认无回归。
- [ ] **Step 4（正控）**：临时把短路判断注释掉（改成永不短路），跑用例 3，确认失败（`invoke_fn` 对其余 judge 分支的 `AssertionError` 被触发）；恢复。另临时删除"降级同步顶层 `degraded`"这一步，跑用例 5，确认失败（顶层 `degraded` 为空，复现 cfr-07）；恢复。再临时把 `judge_candidate` 的 `task_role` 构造改回裸角色字符串（去掉 fingerprint 拼接），跑用例 6 与用例 8，确认均变红（用例 6：两次调用 session_id 相同，复现 cfr2-02；用例 8：两个候选的 `session_id`/`attempt_key`/`stream_log` 都不等于各自独立计算的预期值，因为预期值本身按 fingerprint 算出而实现没有加）；恢复。最后临时把 `_make_request` 的 `attempt` 参数忽略（`build_stream_log_path` 调用硬编码传 `1`），跑用例 10，确认变红（两次 attempt 的 `stream_log` 相同，复现 cfr4-01 指出的"fork 后覆盖上一次日志"缺陷）；恢复。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
git commit -m "feat(harness): fanout —— finder/judge 组合编排消费 WaveResult 与 RequestContext，judge task identity 携带 fingerprint，联合测试 oracle 订正为同源比对，_make_request 携带 attempt 编号（cfr-06/07/14, cfr2-02/04, cfr3-01, cfr4-01）" -- .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
```


### Task 5.6（处置 cfr-04/cfr-06/cfr2-03/cfr3-03）：顶层 `run_fanout()`——组合入口，聚合结算基于全部尝试

**v4 重写说明**：本任务的 `_aggregate_settlement` 现在消费 Task 5.4/5.5 传出的 `all_records`——它已包含**每一波、每次实际发起的调用**（不止每角色最后一条，cfr2-03 修复的直接消费点），因此 `FanoutSettlement` 的总成本/turns/denials 现在真实反映本轮全部尝试的花费，包括失败重试所消耗的部分。

**cfr3-03 补充（第三轮评审指出，承接 rmf-04）**：开放发现处置表宣称 `AttemptRecord.protocol_errors` 会被 `FanoutSettlement` 聚合、供 Phase 6 写入 `round.py` 返回的 `detail` 字段（rmf-04 的判因链路修复），但本任务此前的测试清单没有一条**真正断言** `FanoutSettlement.protocol_errors` 聚合了多个角色/多次尝试的具体错误内容——`FanoutSettlement` 字段契约里虽然列了 `protocol_errors: list`，测试清单却完全没有覆盖它的聚合行为，只覆盖了 `capability_drift`/`cost_known`。本任务补上这条测试。

**接口契约**：

```python
@dataclass(frozen=True)
class FanoutSettlement:
    total_cost_usd: float = 0.0
    cost_known: bool = True
    total_turns: int = 0
    total_denials: int = 0
    protocol_errors: list = field(default_factory=list)
    capability_drift: list = field(default_factory=list)
    exit_code: int = 0  # 扇出无单一顶层进程退出码概念，恒为 0（占位，兼容 round.py 现有签名）

def _aggregate_settlement(records: list[AttemptRecord]) -> FanoutSettlement: ...

def run_fanout(*, round_id: str, invoke_fn, budget: BudgetTracker,
              deadline_monotonic: float, blocked_lanes: list[str],
              known_canonical_keys: set[str], inflight_paths: list[str],
              agents: dict[str, AgentDef] | None = None,
              conn=None) -> dict: ...
    # 返回 {"candidates": [...], "rejected": [...], "degraded": [...],
    #       "settlement": FanoutSettlement}
```

**不变量**：
1. `run_fanout` 内部维护一个贯穿本轮的 `all_records: list[AttemptRecord]`，传给 `run_finders` 与每一次 `judge_candidate` 调用的 `all_records=` 参数（同一个列表对象，各函数原地 `append`/`extend`）——`_aggregate_settlement` 最终读的是这一个列表，覆盖本轮全部角色 × 全部波次的全部尝试。
2. `_aggregate_settlement` 对 `all_records` 里的**每一条**记录累加 `cost_usd`/`turns`/`denials`（不是只累加每角色最后一条）——`total_cost_usd` 因此等于本轮真实花费的总和，包括耗尽重试但最终失败的角色在失败之前已经产生的成本（cfr2-03）。
3. 任一记录 `cost_known=False` → 整体 `FanoutSettlement.cost_known=False`（一票否决，成本不确定就是不确定，不能因为其余记录成本已知而"平均"或忽略）。
4. 任一记录 `status="capability_drift"` → 该记录被计入 `capability_drift` 列表；`run_fanout` 本身不对此做"整轮失败"的判断——那是调用方（Phase 6 的 `round.py` 接线）的职责，`run_fanout` 只负责如实透传 `settlement.capability_drift` 是否非空（cfr-06：检查点在 Phase 6，`fanout.py` 只产出可供检查的信号）。
5. `degraded` 是 `run_finders` 与每次 `judge_candidate` 调用返回的 `degraded` 列表的合并（cfr-07 沿用），顶层 `degraded` 非空是"finder 有候选但 judge 全部降级"这个场景与"真正无候选"场景的唯一区分信号。
6. `selected`（顶层 `candidates`）取自 `ranked` 列表中第一个全部 judge 通过（无 `reject`）的候选；一旦选中即停止裁决后续候选（不为已选中候选之外的候选继续花钱）。
7. **`_aggregate_settlement` 对 `all_records` 里每条记录非空的 `protocol_errors` 做拼接**（cfr3-03/rmf-04）：格式为 `f"{record.role}:{record.attempt}: {e}"`（角色+attempt 编号作为前缀，区分是哪次尝试产生的协议错误），汇总进 `FanoutSettlement.protocol_errors` 列表——这条聚合逻辑此前只在 Task 5.3 的 `AttemptRecord.protocol_errors` 字段注释与本任务的接口契约里提及，但从未被测试清单真正断言过，第三轮评审指出这是"处置表宣称、任务清单遗漏"的典型缺口，本任务补齐。

**测试清单**（断言点，不是完整测试源码）：
1. 全部 finder 返回空候选 → `candidates=[]`，`rejected=[]`，返回值含 `degraded` 键（即便为空）。
2. 一个候选通过全部三个 judge → `candidates` 长度 1，该候选的 `verdicts` 长度 3。
3. redline judge 持续失败降级、finder 侧有候选 → `candidates=[]`，顶层 `degraded` 非空且含一条 `role` 前缀为 `judge:redline` 的记录（cfr-07 端到端验证）。
4. **cfr2-03 核心**：构造一个角色首波失败（产生成本）、次波成功的场景 → `settlement.total_cost_usd` 等于首波成本+次波成本之和，**不等于**仅次波成本（验证聚合基于全部 attempts，不是只取最终记录）。
5. 任一子调用 `cost_known=False` → `settlement.cost_known` 为 `False`（即便其余子调用成本均已知）。
6. 任一子调用检测到能力漂移 → `settlement.capability_drift` 非空。
7. **cfr3-03 核心（rmf-04 端到端）**：构造两个不同角色，各自的 `InvocationResult.protocol_errors` 含不同的协议错误文本（如 `["duplicate init events: 2"]` 与 `["unparseable stream line: ..."]`）→ `settlement.protocol_errors` 长度为 2，且每条都能通过角色名前缀区分来自哪个角色（不是把两条错误文本合并成一条丢失来源信息）。

- [ ] **Step 1**：按测试清单写测试（追加到 `test_fanout.py`），跑至因 `FanoutSettlement`/`run_fanout` 签名或聚合逻辑不匹配而红。
- [ ] **Step 2**：按接口契约与不变量实现 `FanoutSettlement`/`_aggregate_settlement`/`run_fanout`。
- [ ] **Step 3**：跑通全部用例；跑整个 `test_fanout.py` 确认无回归；跑全量测试套件确认其余模块未受影响。
- [ ] **Step 4（正控）**：临时把 `_aggregate_settlement` 改回只遍历"每角色最后一条记录"（模拟 v2 的缺陷），跑用例 4，确认失败（聚合成本漏掉了失败重试的花费，复现 cfr2-03）；恢复。另临时删除 `degraded.extend(judge_degraded)` 这一步，跑用例 3，确认失败（顶层 `degraded` 为空，复现 cfr-07）；恢复。再临时把 `protocol_errors` 聚合逻辑删掉（`_aggregate_settlement` 里跳过该字段拼接），跑用例 7，确认失败（`settlement.protocol_errors` 为空，复现 cfr3-03 指出的"宣称但未测试"缺口）；恢复。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
git commit -m "feat(harness): fanout —— run_fanout 组合入口，FanoutSettlement 基于全部 attempts 聚合含 protocol_errors 判因链路（cfr-04/cfr-06, cfr2-03, cfr3-03/rmf-04）" -- .claude/scripts/harness/fanout.py .claude/scripts/harness/tests/test_fanout.py
```

**Phase 5 收尾检查**：跑全量测试套件，确认 Phase 0–4 基线 + 本阶段新增用例全部绿，且既有测试无一因本阶段改动而回归。若 Task 3.1 末尾提到的"长度上限"补充尚未做，此时是最后合适时机。**另需核对**：`session_identity`/`ledger`/`role_invocation`/`prompts`/`fanout_schema`/`queue` 六个模块在 `fanout.py` 顶部的 import 语句齐全（Task 5.1–5.6 分散追加的 import 需要在此阶段汇总核对一次，避免遗漏；`queue.fingerprint` 是 Task 5.5 cfr2-02 修复新增的依赖，需确认已加入 import 列表）。


---

## Phase 6 · `round.py` 接线：工具集收窄 + 扇出接入 + 结算聚合消费 + 截止时间修正

**目标**：把 `_run_round_body` 里现有的单次 `deps.invoke(...)` 调用替换为一次 `fanout.run_fanout(...)` 调用，同时完成 Task 2.4 登记的工具集收窄。这是本计划里**唯一**会修改 `round.py` 现有代码的阶段，一次性原子完成，前后测试均保持绿。

**v3 重写说明（处置 cfr-02/cfr2-01/cfr-04/cfr-06/cfr-08/cfr-09/cfr-15）**：本阶段是接缝最密集之处——把 Phase 5 的全部新接口（`RoleInvocationRequest`、`BudgetTracker`、`FanoutSettlement`、`deadline_monotonic`）与 `round.py` 现有代码（`_settle_failed`、`_capability_drift_problems`、`_describe_degraded`、`budget.record_outcome` 等）真正缝合。**处理顺序：先设计"下游结算分支如何消费 `FanoutSettlement`"，再设计"如何调用 `run_fanout`"，最后才是"如何构造调用适配层"**——不是反过来先切换调用路径再回头补结算。

**cfr-02/cfr2-01（Critical，第二轮评审指出仍未闭合）**：v2 的 `_invoke_and_record` 包装层直接把 `RoleInvocationRequest` 对象整体传给 `deps.invoke(request)`——`Deps.invoke` 底层最终调用的是 `claude_runner.invoke()`，其真实签名是"多个具名参数"而非"接受单个 `RoleInvocationRequest` 位置参数"，`deps.invoke(request)` 只会把 `request` 整个对象填进 `invoke()` 的第一个形参 `prompt`，其余必需参数全部缺失，运行时 `TypeError`。**本次修正明确写死一种调用形式**：`Deps.invoke` 的类型标注更新为 `Callable[[RoleInvocationRequest], InvocationResult]`（"接受一个 `RoleInvocationRequest`、内部自行展开"的适配函数），`cli.py` 构造 `Deps` 时传入的 `invoke` 实参必须是这样一个适配闭包——`round.py` 侧的 `_invoke_and_record` 因此不需要关心展开细节，只管把 `RoleInvocationRequest` 传给 `deps.invoke`；真正的 `to_invoke_kwargs()` 展开发生在 `cli.py` 构造这个适配闭包的地方（`lambda request: claude_runner.invoke(**role_invocation.to_invoke_kwargs(request))`）。**这个适配闭包的构造点是 `cli.py`，不是 `round.py`**——`cli.py` 不在本计划"只改 `round.py`"的白名单例外范围内，但它是本计划唯一需要改动的第二个文件，本任务同时登记这处必要改动（不属于范围蔓延：没有这一行接线，Task 2.3 定义的 `RoleInvocationRequest`/`to_invoke_kwargs` 契约就是一个从未被生产代码使用的死代码）。

**cfr-09（沿用 Phase 5 Task 5.4 已实现的动态 timeout，本阶段负责正确传入 `deadline_monotonic`）**：`round.py` 侧只需要把已有的两个常量做一次减法得到一个绝对时刻（`deadline_monotonic = started + ROUND_DEADLINE_S - CLEANUP_RESERVE_S`），不需要任何 `max()` 下限——真正的"不足则不启动"与"动态收缩 `timeout_s`"判断已在 `fanout.run_wave_scheduled` 内部完成（Task 5.4）。

**设计回答（问题 4 续：并发度与预算/截止时间的重新分配）**

现有 `round.py` 的假设是「一次 `deps.invoke()` 调用消耗一份 `grant`（本轮预留全额）与 `timeout_s`（本轮剩余截止时间全额）」。扇出后一轮包含最多 7 次独立子调用（4 finder + 最多 3 judge），这个假设不再成立：

- **预算切分**：`grant`（`round_budget_usd`）现在是本轮全部子调用的总预算池。`fanout.run_fanout` 需要的 `budget: BudgetTracker` 由 `round.py` 在调用前构造：`BudgetTracker(total_usd=grant)`。单个子调用的上限（`single_call_cap_usd`）取 `cfg.round_budget_usd / 7` 的量级（7 = 最坏情形 4 finder + 3 judge）。
- **`record_invocation` 生产路径缺口顺手补上**：`budget.py` 现有 `record_invocation()` 已写好且有测试，但从未在生产路径被调用（`code-review-realmachine-fixes.md` 已指出）。本阶段在每次子调用真正返回后调用它——这与 `BudgetTracker` 是两回事：`BudgetTracker` 管本轮扇出内部的并发原子预留（进程内存），`budget.record_invocation` 管持久化到 SQLite 的跨轮次审计记录。
- **截止时间切分**：`deadline_monotonic = started + ROUND_DEADLINE_S - CLEANUP_RESERVE_S`（绝对时刻，供 `fanout.run_wave_scheduled` 在每一波开始前自行计算剩余时间与动态 `timeout_s`）。

### Task 6.1（处置 cfr-02/cfr-04/cfr-06/cfr-08/cfr-09/cfr-15，cfr2-01，cfr3-01/cfr3-03）：`round.py` 原子切换 + `STAGE1_ALLOWED_TOOLS` 收窄 + 结算分支迁移 + `cli.py` 调用适配

**范围说明**：本任务同时触及 Task 2.4 登记的工具收窄、`round.py` 结算分支从"读单一 `invocation`"迁移为"读 `FanoutSettlement`"（cfr-04）、能力漂移检查从"整轮判定"迁移为"聚合后判定，不可降级"（cfr-06）、`round.py` 本身的调用段替换、以及 `cli.py` 的调用适配闭包（cfr-02/cfr2-01）。五者耦合在一起，必须同一次提交生效（不留过渡态）。

**cfr3-03 补充（第三轮评审指出，承接 rmf-04/rmf-17）**：开放发现处置表宣称本任务会"把 `settlement.protocol_errors` 写入 `round.py` 返回的 `detail` 字段"（rmf-04）与"为每个角色的 `RoleInvocationRequest` 显式设置 `model=DEFAULT_AGENT_MODEL`"（rmf-17），但此前的不变量列表与测试清单都没有覆盖这两点——本任务补上。

**cfr3-01 补充（第三轮评审指出）**：本任务是 `RequestContext`（Task 2.3 新增）唯一的生产值构造点——`_run_round_body` 必须显式构造 `RequestContext(cwd=str(cfg.repo_root), settings_path=SETTINGS_PATH, model=claude_runner.DEFAULT_AGENT_MODEL, stream_log_dir=str(cfg.state_db.parent / "rounds"))` 并传给 `fanout.run_finders`/`fanout.judge_candidate` 的 `context=` 参数——这是"生产环境下这四个值应该等于什么"这个问题在整个计划里**唯一**的真值来源，Task 5.5 的 `_make_request` 只负责消费它，不允许在 Phase 5/6 之间出现第二份重复定义。

**接口契约**：

```python
# round.py
STAGE1_TOOLS: str  # ",".join(sorted(claude_runner.STAGE1_ALLOWED_TOOLS))，收窄后为 "Glob,Grep,Read"

@dataclass
class Deps:
    conn: object
    gh: object
    worktree: object
    outbox: object
    queue: Queue
    invoke: Callable[[RoleInvocationRequest], InvocationResult]  # 签名变化：接受单个 RoleInvocationRequest
    tools: tuple = field(default_factory=tuple)

def _settle_failed(budget: Budget, round_id: str, day: str, *,
                   cost_known: bool, cost_usd: float) -> None: ...
def _capability_drift_problems(settlement: "fanout.FanoutSettlement") -> list[str]: ...
def _format_detail(settlement: "fanout.FanoutSettlement", fallback: str) -> str: ...
    # cfr3-03/rmf-04：settlement.protocol_errors 非空时 "; ".join(...)，
    # 否则回退到 fallback（既有场景下的默认 detail 文案，如 "no eligible candidate"）

def _load_agents(repo_root) -> dict[str, "prompts.AgentDef"]: ...
def _build_request_context(cfg: "Config") -> "role_invocation.RequestContext": ...
    # cfr3-01：唯一的生产值构造点，见上方补充说明

# cli.py
def _build_invoke_adapter() -> Callable[[RoleInvocationRequest], InvocationResult]: ...
    # 内部：lambda request: claude_runner.invoke(**role_invocation.to_invoke_kwargs(request))
```

**不变量**：
1. `Deps.invoke` 字段类型标注与实际调用方式统一为"接受单个 `RoleInvocationRequest`"；生产环境下这个可调用对象的**内部实现**必须真正执行 `claude_runner.invoke(**role_invocation.to_invoke_kwargs(request))` 或等价的完整关键字展开（cfr-02/cfr2-01 核心：不允许把 `RoleInvocationRequest` 整体当位置/关键字参数传给 `invoke()`）——这个适配闭包在 `cli.py` 构造，`round.py`/`fanout.py` 侧只管传递 `RoleInvocationRequest`，不关心底层如何展开。
2. `_settle_failed` 不再接受 `InvocationResult` 对象，改为直接接受 `cost_known`/`cost_usd` 两个具名参数——调用方（无论逻辑上源自单次 invocation 还是 `FanoutSettlement`）各自取出这两个字段传入，函数内部逻辑（`cost_known` 为真走 `budget.settle`，否则走 `budget.abandon`）不变（沿用 rmf-05 已修复的语义）。
3. `_capability_drift_problems` 不再接受 `InvocationResult` 并重新判断，直接接受 `FanoutSettlement` 并透传其 `capability_drift` 列表——漂移判定逻辑已下沉到 `fanout._check_capability_drift`（每次子调用各自判断，Task 5.3 cfr-06），本函数只做"结论非空即失败"的既有格式化，不重复判定。
4. `_describe_degraded` **不改动**——`record_degraded`（Task 5.2）已同时写 `role` 与 `agentType` 两个字段，继续读 `d.get('agentType')` 即可正确工作。
5. `_run_round_body` 内，原有的"外层会话唯一职责是调 Workflow 再原样回显"这段旧代码整体替换为：构造 `agents = _load_agents(cfg.repo_root)` → 构造 `context = _build_request_context(cfg)`（cfr3-01）→ 构造 `deadline_monotonic`/`call_budget: BudgetTracker(total_usd=grant)` → 定义 `_invoke_and_record(request)`（内部调用 `deps.invoke(request)` 并紧接着调用 `budget.record_invocation(...)`）→ 调用 `fanout.run_fanout(..., invoke_fn=_invoke_and_record, budget=call_budget, deadline_monotonic=deadline_monotonic, agents=agents, context=context, conn=deps.conn)` → 从返回的 `fanout_result["settlement"]` 取出 `total_turns`/`total_denials`/`exit_code`/`cost_known`/`total_cost_usd` 填入 `progress` 字典。
6. 能力漂移分支：`drift_problems = _capability_drift_problems(settlement)`，非空则整轮判定为 `capability-drift`（调用 `_settle_failed`+`budget.record_outcome` 后原样返回），**不得**因为漂移角色本身恰好返回了看似合法的 payload 而继续使用其 candidates。
7. 后续 `eligible`/`candidate = dict(...)`/DTO 校验/`classify`/`publish` 等既有代码逐字保留不改——`fanout_result["candidates"]` 的形状与旧 `invocation.payload.get("candidates", [])` 完全一致。**核对点（cfr-04 要求逐一核对，不能遗漏）**：`round.py` 全文对 `invocation.cost_usd`/`invocation.turns`/`invocation.denials`/`invocation.exit_code` 的引用，必须全部替换为 `settlement.total_cost_usd`/`settlement.total_turns`/`settlement.total_denials`/`settlement.exit_code`；`_invoke_and_record` 函数体内部的局部变量 `invocation`（单次调用的返回值）允许保留，那不是聚合值引用。
8. `.claude/harness-settings.json` 的 `permissions.allow` 收窄为 `["Read", "Grep", "Glob"]`（删除 `Skill`/`Workflow`/`TaskOutput`/`TodoWrite`）。
9. **判因链路（cfr3-03/rmf-04）**：`invocation-failed`/`capability-drift` 等结果分支构造 `detail` 字段时，调用 `_format_detail(settlement, fallback=<既有默认文案>)`——`settlement.protocol_errors` 非空时优先使用它（拼接后的判因结论），为空时回退到既有的默认文案，不得让本次改动之后判因结论反而比旧架构更模糊。
10. **每个角色的模型固定为规范 ID（cfr3-03/rmf-17）**：`_load_agents` 装配出的每个角色对应的 `RoleInvocationRequest`（在 `run_finders`/`judge_candidate` 内部构造，Phase 5 Task 5.5）在 Phase 6 接线时必须显式传入 `model=claude_runner.DEFAULT_AGENT_MODEL`——本任务负责在 `round.py` 传给 `fanout.run_finders`/`fanout.judge_candidate` 的入参或它们内部的 `_make_request` 工厂里确保这一点被设置，不依赖 `RoleInvocationRequest.model` 的默认值 `None`（`None` 会让 `build_argv` 不传 `--model`，退回 CLI 自身默认值，不是"规范 ID 已显式设置"）。
11. **`RequestContext` 的生产值全部来自 `Config`/既有常量，不重新硬编码**（cfr3-01）：`_build_request_context(cfg)` 的四个字段——`cwd=str(cfg.repo_root)`、`settings_path=SETTINGS_PATH`（`round.py` 现有常量，不重新拼字符串字面量）、`model=claude_runner.DEFAULT_AGENT_MODEL`、`stream_log_dir=str(cfg.state_db.parent / "rounds")`（与旧架构原有的单一 stream_log 路径的父目录一致，只是现在每个子调用各自一个文件）——这个函数是整个计划里"生产环境这些值该等于什么"的唯一实现，`fanout.py` 侧的 `_make_request` 不允许再有第二份重复定义或硬编码占位符。

**测试清单**（断言点，不是完整测试源码）：
1. `round.STAGE1_TOOLS == "Glob,Grep,Read"`（工具集收窄端到端验证）。
2. 全部 finder/judge 成功、一个候选通过全部裁决 → 本轮结果为 `published`（既有场景，改写 fixture 后行为不变）。
3. 一个 finder 持续传输故障、其余 3 个正常返回空候选 → 本轮判定为 `no-candidate-degraded`（而非旧架构"一个 finder API Error 让整轮 aborted"的历史 bug 复现）。
4. 一轮扇出后，`invocations` 表（`budget.record_invocation` 生产路径）有多条记录，对应多个子调用（验证"补上生产路径调用"这条顺手修复生效）。
5. **cfr-06 端到端**：4 个 finder 中 1 个返回能力漂移（`init_tools` 含 `Bash`）→ 整轮判定为 `capability-drift`，即便其余 3 个 finder 正常。
6. 某子调用 `cost_known=False` → 本轮按 `_settle_failed` 现有语义走 `budget.abandon()`（预留满额）。
7. **cfr-02/cfr2-01 核心**：构造一个"忠实"的 `deps.invoke` 适配闭包（真正执行 `to_invoke_kwargs` 展开），断言它能正确调用一个记录实际收到的关键字参数集合的假 `claude_runner.invoke` 替身，且该集合覆盖全部必需参数（`cwd`/`timeout_s` 等）——验证生产接线确实按契约展开，不是把整个对象塞进第一个形参。
8. `.claude/harness-settings.json` 的 `permissions.allow` 恰好是 `["Read","Grep","Glob"]`（或等价断言，`test_precheck.py`/`test_cli.py` 同步改写）。
9. **cfr3-03 核心之一（rmf-04 端到端）**：构造一个某角色返回 `protocol_errors=["duplicate init events: 2"]` 的场景（该角色最终判定失败）→ 本轮返回的 `detail` 字段包含这条协议错误文本（不是只有 `raw_tail` 摘要或空字符串）。
10. **cfr3-03 核心之二（rmf-17 端到端）**：构造一次完整扇出（全部 finder/judge 成功），断言传给 `deps.invoke` 的**全部**（4 个 finder + 3 个 judge，共 7 次）请求的 `model` 字段均等于 `claude_runner.DEFAULT_AGENT_MODEL`，且不是 `None`。
11. **cfr3-01 核心（端到端生产值验证）**：`_build_request_context(cfg)` 返回的 `RequestContext` 的 `cwd` 等于 `str(cfg.repo_root)`（不是任何测试替身占位符）、`settings_path` 等于 `round.SETTINGS_PATH`、`model` 等于 `claude_runner.DEFAULT_AGENT_MODEL`；进一步跑一次完整扇出（真实 `_build_request_context` 产出的 context，传给 `fanout.run_finders`），断言传给 `deps.invoke` 的请求的 `cwd`/`settings_path` 字段确实等于这些生产值，不是 Phase 5 测试替身里遗留的 `/tmp`/`""`。

- [ ] **Step 1**：按测试清单写测试（改写 `test_round.py` 的 fixture：`_deps(invocation)` 改为 `_deps(invoke_fn)`，新增 `_multi_role_invoke(role_results: dict)` 按 `request.role` 路由；新增/改写上述 11 类断言），跑至因签名/字段不匹配、`fanout`/`role_invocation` 未接入而大面积红——这是预期的中间态。
- [ ] **Step 2**：按接口契约与不变量顺序实现：先落地 `_settle_failed`/`_capability_drift_problems`/`_format_detail` 的新签名（结算分支迁移，cfr-04/cfr3-03），再落地 `_build_request_context` 与 `_run_round_body` 的调用段替换（工具收窄 + `fanout.run_fanout` 接入 + `context=` 传入 + 各角色 `model=DEFAULT_AGENT_MODEL` 显式传入），最后在 `cli.py` 构造真正执行 `to_invoke_kwargs` 展开的适配闭包并传给 `Deps(invoke=...)`（cfr-02/cfr2-01）。
- [ ] **Step 3**：跑通全部新用例；重跑既有 `test_round.py` 全部用例——**预计有大量既有用例因 fixture 形状变化需要同步改写**（凡是构造单个 `InvocationResult` 直接传给 `_deps()` 的既有用例，改成 `_multi_role_invoke({...七个角色...})` 形式），逐条改写但不得删除既有用例覆盖的场景。同步修正 `test_precheck.py`/`test_cli.py` 中断言旧工具集的既有用例。
- [ ] **Step 4**：跑通全量测试套件，全绿。
- [ ] **Step 5（正控，cfr-15 已订正的方向）**：临时把 `fanout.py` 的 `judge_candidate` 短路判断禁用，重跑 **`test_fanout.py`**（不是 `test_round.py`）里 Task 5.5 已写好的 `test_redline_reject_short_circuits_other_judges`，确认它变红；恢复后重跑 `test_round.py`+`test_fanout.py` 全部用例确认恢复绿色。另外，临时把 `cli.py` 的适配闭包改回"直接把 `request` 整体传给 `claude_runner.invoke`"（复现 cfr-02/cfr2-01 指出的错误接线），跑测试清单第 7 条，确认变红；恢复。再临时把 `_format_detail` 改回恒定返回 `fallback`（忽略 `settlement.protocol_errors`），跑用例 9，确认变红（复现 cfr3-03 指出的"宣称但未接线"缺口）；恢复。再临时删除 `model=DEFAULT_AGENT_MODEL` 的显式传入，跑用例 10，确认变红；恢复。最后临时把 `_build_request_context` 的 `cwd` 改回硬编码 `"/tmp"`（模拟"忘记接生产值"），跑用例 11，确认变红（复现 cfr3-01 指出的"生产值来源未验证"缺口）；恢复。
- [ ] **Step 6**：提交（**cfr-08 要求：涵盖全部实际修改的文件**——包括 `round.py`、`cli.py`、`.claude/harness-settings.json`、相关测试文件；若 Phase 5 遗留但尚未提交的文件因实施顺序被合并到本次一起做，必须在提交文件列表里如实列出，不能只提交 `round.py` 而遗漏实际改过的 `cli.py`/`fanout.py`）。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/round.py .claude/scripts/harness/cli.py \
        .claude/harness-settings.json \
        .claude/scripts/harness/tests/test_round.py \
        .claude/scripts/harness/tests/test_precheck.py \
        .claude/scripts/harness/tests/test_cli.py
git commit -m "refactor(harness): round.py 接入控制器驱动扇出，cli.py 落实 to_invoke_kwargs 展开接线，消费 FanoutSettlement 聚合结算含判因链路，各角色显式设规范 model，RequestContext 生产值唯一来源（ADR-002 D1/D2 落地，cfr-02/04/06/09, cfr2-01, cfr3-01/03）" -- \
        .claude/scripts/harness/round.py .claude/scripts/harness/cli.py \
        .claude/harness-settings.json \
        .claude/scripts/harness/tests/test_round.py \
        .claude/scripts/harness/tests/test_precheck.py \
        .claude/scripts/harness/tests/test_cli.py
```

- [ ] **Step 7（cfr-08：干净 worktree 独立复核）**：提交完成后，在一个干净的临时 worktree 上验证提交态本身可运行（工作区可能混有后续任务已开始的改动，掩盖"这次提交本身缺文件"的问题）：

```bash
cd /home/xp/src/zipfs
git worktree add /tmp/phase6-verify HEAD
cd /tmp/phase6-verify/.claude/scripts
/home/linuxbrew/.linuxbrew/bin/python3 -m unittest discover -s harness/tests -t . 2>&1 | tail -20
```

确认全量测试套件在这个干净 checkout 上同样全绿——若不绿，说明本次提交遗漏了某些文件，需立即补一个后续提交把遗漏文件加上。验证完成后清理临时 worktree（`cd /home/xp/src/zipfs && git worktree remove /tmp/phase6-verify`）。

**风险与回滚**：这是本计划里改动面最大的单次提交。回滚点是 `git revert` 本提交——Phase 0–5 的全部新模块在 revert 后仍然存在但不再被 `round.py`/`cli.py` 引用，不会造成孤儿代码之外的任何问题。**在真机验收（Phase 8）之前，systemd timer 仍是 disabled**，即便本任务实现有缺陷也不会自动触发真实副作用——这是本计划风险可控的关键前提，与 ADR 头部记录的用户裁决一致。

---


## Phase 7 · 退役旧资产（JS workflow / skill / 跨语言测试）

**目标**：`round.py` 已在 Phase 6 完全切换到新扇出路径后，删除不再被任何生产代码路径引用的旧资产。**顺序很重要**：必须等 Phase 6 提交完成、全量测试绿之后才能删除，否则删除会让 Phase 6 之前的中间状态无法回滚验证。

**cfr-18 修正（Important，已独立复现，两处）**：v1 草图存在两个问题，均已订正：

1. **任务顺序自相矛盾**：v1 把"删除 JS + 删旧测试"安排为 Task 7.1，"写继任测试"安排为后续 Task 7.2，但 Task 7.2 的文字又声称"此时新旧测试同时存在都为绿"——这在 Task 7.1 已经删除 JS 文件之后不可能成立（旧的跨语言测试此时会因为找不到 `scrollz-propose.js` 而报错，不是"绿"）。**修正为**：Task 7.1（原排序）与 Task 7.2（继任测试）对调执行顺序——先在 JS 仍然存在时写好继任测试并验证通过（此时新旧两条测试都应为绿，因为两者互不依赖对方是否存在），再在**同一个原子提交**里同时删除 JS 源文件与旧的跨语言测试文件。任务编号本身保留（Task 7.1 仍是"删除 JS workflow/skill"、Task 7.2 仍是"跨语言测试替换"），但**执行顺序**与**提交边界**按下方步骤重新排列——具体说，Task 7.2 的"写继任测试"子步骤需要先于 Task 7.1 的"删除 JS"执行；Task 7.1 与 Task 7.2 剩余的"删除"部分合并进同一次 `git commit`。
2. **继任测试的断言值是臆造的，与当前实现真值不符**：v1 断言 `_norm('a\x1fb') == 'a b'`，但**已独立实测确认** `_norm('a\x1fb')` 实际返回 `'a\x1fb'`（`\x1f` 不落在 Python `_JS_SPACE`/`_WS` 正则定义的空白字符集合内——`queue.py` 现有实现刻意按 ECMAScript `\s` 语义排除了 `\x1c`–`\x1f` 这几个控制字符，这是**有意为之的行为**，注释里写明了"不能用 `re` 的 `\s`：它匹配 `\x1c`–`\x1f`，ECMAScript 的 `\s` 不匹配"）。若继任测试断言 `'a b'`，测试本身就会先崩，且这个错误的断言值一旦被写死会**静默改变 canonical key 的规范化语义**——一旦"修好"这条测试让它断言通过（把 `\x1f` 也当空白折叠），会导致所有既有提案的 canonical key 计算结果改变，`rmf-02`/`rmf-13` 修复的跨轮去重会静默失效并复发。**继任测试必须冻结当前 Python 的真实行为，而不是撰写者以为它应该是什么**。下方测试代码已订正为实测值。

### Task 7.2（提前执行，先于 Task 7.1）：写继任测试，冻结当前 canonical key 规范化真值

**背景（回答任务描述里的悬而未决问题）**：`test_canonical_key_cross_language.py` 存在的唯一理由是「`queue.canonical_key`（Python）与 `scrollz-propose.js` 的 `canonicalKey()`（JS）必须逐字节一致，因为一个由 Python 产出、一个由 JS 消费」。Phase 5 Task 5.1 已经让本轮内去重与跨轮去重共用同一个 Python 函数（`fanout.dedupe_and_rank` 直接调 `queue.canonical_key`），JS 版本随本阶段一并删除后，**这条测试校验的两个对象里有一个已经不存在**，测试本身失去校验对象，必须删除——这不是「削减覆盖」，而是「被测的跨语言接缝本身消失了」。

**但删除不能是净减覆盖**：原测试隐含校验了 `canonical_key`/`_norm` 对若干边界输入（`\x1f` 分隔符、全角空格、BOM、大小写）的处理是**确定性且符合当前实现**的（虽然此前是通过「与 JS 比对」这个侧面手段验证，但断言的真正内容是「Python 侧的规范化函数对这些输入产出这个确定的结果」）。因此替换测试直接对 `queue.canonical_key`/`_norm` 断言这些边界输入的**实测**具体输出，不再需要 node 子进程，也不再假设"应该"折叠成什么。

- [ ] **Step 1: 写测试** —— 新建 `.claude/scripts/harness/tests/test_canonical_key_normalization.py`。**此时 `scrollz-propose.js` 与旧的 `test_canonical_key_cross_language.py` 均原样保留，尚未触碰**——新旧两条测试此刻应同时为绿（旧测试测的是"Python 与 JS 一致"，新测试测的是"Python 自身行为符合已冻结真值"，两者互不依赖对方是否存在，因此可以共存验证）：

```python
"""canonical key 规范化的边界行为（原 test_canonical_key_cross_language.py
的继任者）。原测试校验『Python 与 JS 逐字节一致』，JS 侧实现随
scrollz-propose.js 退役而不再存在（ADR-002 D1：本轮内去重与跨轮去重现在
共用同一个 Python 函数，见 fanout.dedupe_and_rank）。本测试改为直接断言
Python 侧规范化函数对同一组边界输入的**当前实际行为**，不再需要 node 子
进程——这不是削减覆盖，是被测的『跨语言』接缝本身随 JS 代码删除而消失了。

评审 cfr-18 订正：以下断言值均为**实测值**（`python3 -c
"from harness.queue import _norm; print(repr(_norm('a\\x1fb')))"` 复现），
不是撰写计划时臆测『应该』折叠成什么。`\x1f` 不落在当前 `_norm()` 的空白
字符集合内（`queue.py` 模块 docstring 明确说明这是有意为之——ECMAscript
的 `\s` 不匹配 `\x1c`–`\x1f`，Python 侧刻意排除了这个区间以保持与 JS 语义
一致），因此 `_norm('a\x1fb')` 的结果是 `'a\x1fb'`（分隔符原样保留），
不是 `'a b'`。若未来任何人"修复"这条测试让它断言分隔符被折叠，那会静默
改变全部既有提案的 canonical key，导致 rmf-02/rmf-13 修复的跨轮去重复发
——这条测试存在的目的正是冻结这一行为，钉住它不被无意改变。
"""
import unittest
from harness.queue import _norm, canonical_key


class TestCanonicalKeyNormalization(unittest.TestCase):
    def test_control_character_separator_is_preserved_not_folded(self):
        # 冻结当前真实行为（cfr-18 订正）：\x1f 是拼接分隔符本身，且不在
        # 当前 _norm() 的空白折叠范围内，原样保留。这不是"应该"的结果，
        # 是"就是"的结果——变了就是回归。
        self.assertEqual(_norm("a\x1fb"), "a\x1fb")
        self.assertEqual(_norm("c\x1cd"), "c\x1cd")
        self.assertEqual(_norm("e\x1df"), "e\x1df")
        self.assertEqual(_norm("g\x1eh"), "g\x1eh")

    def test_leading_trailing_whitespace_variants_stripped(self):
        self.assertEqual(_norm("  前后空白  "), "前后空白")

    def test_internal_multiple_spaces_folded(self):
        self.assertEqual(_norm("多个   空格"), "多个 空格")

    def test_tab_and_newline_folded_to_single_space(self):
        self.assertEqual(_norm("tab\t分隔"), "tab 分隔")
        self.assertEqual(_norm("换行\n与\r\n"), "换行 与")

    def test_fullwidth_and_nbsp_space_folded(self):
        self.assertEqual(_norm("全角　空格"), "全角 空格")
        self.assertEqual(_norm("不换行 空格"), "不换行 空格")

    def test_bom_stripped_at_edge(self):
        self.assertEqual(_norm("BOM﻿"), "bom")

    def test_case_folded(self):
        self.assertEqual(_norm("MiXeD CaSe"), "mixed case")

    def test_empty_string_normalizes_to_empty(self):
        self.assertEqual(_norm(""), "")

    def test_canonical_key_joins_four_fields_with_separator(self):
        key = canonical_key("Goal", "Invariant", "path/To/File.rs", "Oracle")
        self.assertEqual(key, "goal\x1finvariant\x1fpath/to/file.rs\x1foracle")

    def test_canonical_key_is_deterministic(self):
        a = canonical_key("g", "i", "p", "o")
        b = canonical_key("g", "i", "p", "o")
        self.assertEqual(a, b)
```

- [ ] **Step 2**：跑新测试文件，确认全绿（此时 `queue.py` 未改动，`scrollz-propose.js` 与旧跨语言测试均未触碰）。同时跑一次旧的 `test_canonical_key_cross_language.py`，确认它也仍然是绿的——**这一步是本次订正的核心验证点**：证明新旧测试在 JS 尚存在时确实可以共存，而不是像 v1 草图错误声称的那样只在"理论上"共存。
- [ ] **Step 3（正控）**：临时把 `_norm()` 里的 `_WS_EDGE.sub("", text)` 那部分改成不做任何处理（直接 `text.lower()`），跑 `test_bom_stripped_at_edge`，确认失败（因为 BOM 不会被剥离）；恢复。**另加一条反向正控**：临时把 `_norm()` 改成"把 `\x1f` 也当空白折叠"（例如在 `_WS` 正则字符类里加入 `\x1f`），跑 `test_control_character_separator_is_preserved_not_folded`，确认失败——这一步验证的是"如果有人静默改变了规范化语义，这条测试会挡住"，是这条测试存在的核心价值；恢复。

**本任务到此为止，不提交**——继续到 Task 7.1 的删除步骤后一并提交（下方 Task 7.1 已改为在同一个提交里包含本任务新增的测试文件与删除的旧资产）。

### Task 7.1：删除 `.claude/workflows/scrollz-propose.js`、`.claude/skills/scrollz-round/`、旧跨语言测试（同一原子提交）

- [ ] **Step 1**：确认零引用——`rg -n "scrollz-propose|scrollz-round" --type-not=md .claude/scripts/` 应无命中（`docs/harness/*.md` 里提及历史背景的引用不算，那是文档，不影响本检查）。
- [ ] **Step 2**：`git rm .claude/workflows/scrollz-propose.js .claude/skills/scrollz-round/SKILL.md .claude/scripts/harness/tests/test_canonical_key_cross_language.py`（若 `.claude/skills/scrollz-round/` 目录下还有其它文件一并 `git rm -r`）。
- [ ] **Step 3**：跑全量测试套件，确认无回归——此时 `test_canonical_key_normalization.py`（Task 7.2 新增，已 `git add` 但尚未提交）与其余全部测试应为绿；旧跨语言测试已被删除，不会再因为找不到 JS 文件而报红。
- [ ] **Step 4**：提交（**这一次提交同时包含 Task 7.2 新增的测试文件与 Task 7.1 删除的三个旧文件**，是同一个原子提交，不拆成两次）。

```bash
cd /home/xp/src/zipfs
git add .claude/scripts/harness/tests/test_canonical_key_normalization.py
git rm .claude/workflows/scrollz-propose.js .claude/skills/scrollz-round/SKILL.md \
    .claude/scripts/harness/tests/test_canonical_key_cross_language.py
git commit -m "chore(harness): 退役 scrollz-propose workflow/scrollz-round skill/跨语言指纹测试，替换为冻结当前规范化真值的 Python 内部测试（cfr-18）" -- \
    .claude/workflows/scrollz-propose.js .claude/skills/scrollz-round/SKILL.md \
    .claude/scripts/harness/tests/test_canonical_key_cross_language.py \
    .claude/scripts/harness/tests/test_canonical_key_normalization.py
```

### Task 7.3：删除 `.claude/workflows/tests/degraded-dedup.test.mjs`

**背景**：该测试是 `normalizeError`/`recordDegraded` 两个 JS 纯函数的复制式测试（`code-review-realmachine-fixes.md` rmf-11 已指出这类复制式测试的漂移风险——测试复制的 `safeAgent` 因缺 `MAX_AGENT_ATTEMPTS` 常量而实际跑不起来，只是从未被调用所以未暴露）。Phase 5 Task 5.2 的 `test_fanout.py::TestNormalizeError`/`TestRecordDegraded` 已经是这两个函数 Python 版本的等价测试（且修复了 rmf-10 的两个真实漏检），是**净增强**而非平移。

- [ ] **Step 1**：确认 `test_fanout.py` 里 `TestNormalizeError`/`TestRecordDegraded` 已覆盖 `degraded-dedup.test.mjs` 断言过的全部场景——逐条核对：
  - `.mjs` 断言「3 条同类传输故障折叠为 1 条，occurrences=3，attempts=9」→ 对应 `test_folds_hex_request_id`（折叠断言）+ `test_record_degraded` 系列（计数断言）。
  - `.mjs` 断言「1 条不同错误不折叠」→ 对应 `test_does_not_fold_different_error_kinds`。
  - `.mjs` 断言「不同 agent 的同错误不折叠」→ 对应 `test_does_not_fold_different_roles`。
  全部三项均已在 Task 5.2 覆盖，无缺口。
- [ ] **Step 2**：`git rm -r .claude/workflows/tests/`（该目录下若还有其它 `.mjs` 文件，先确认无其它内容依赖）。
- [ ] **Step 3**：跑全量测试套件（Python 侧不受影响；若 CI/其它脚本曾经调用 `node .claude/workflows/tests/degraded-dedup.test.mjs`，检查 `.claude/systemd/`、`Makefile` 或任何 shell 脚本是否引用它——`rg -n "degraded-dedup" --type-not=md .` 确认零命中）。
- [ ] **Step 4**：提交。

```bash
cd /home/xp/src/zipfs
git rm -r .claude/workflows/tests/
git commit -m "chore(harness): 退役 degraded-dedup.test.mjs（Python 侧 test_fanout.py 已等价覆盖且修复 rmf-10）" -- .claude/workflows/tests/
```

### Task 7.4：`docs/harness/redlines.yaml` 说明性更新（不改判定逻辑）

- [ ] `harness-self-modification` 红线条目的 `paths` 列表里 `.claude/workflows/` 与 `.claude/skills/` **保留不删**——即便当前这两个目录下已无 harness 专属文件，红线的意图是「防止未来任何 agent 无人值守地在这两个目录下重新创建编排逻辑」，路径本身不因为目录暂时为空而失去意义。仅在 `reason` 字段追加一句说明：「2026-07-31 起 harness 扇出改为控制器驱动，两目录不再含 harness 编排代码，但仍属禁止无人值守自修改的范围」。
- [ ] 提交（该文件是纯文档性质的说明补充，不涉及测试）。

```bash
cd /home/xp/src/zipfs
git add docs/harness/redlines.yaml
git commit -m "docs(harness): redlines.yaml 补充说明——workflows/skills 目录红线在扇出改为 Python 驱动后仍然有效" -- docs/harness/redlines.yaml
```

### Task 7.5（新增，处置 cfr-17）：同步修订 `spec.md` 与 `plan-stage1b.md` 的实现接缝描述

**背景（评审 cfr-17，Important，已核实）**：v1 草图声称"Stage 1b 与现行 spec 不受影响"，但这个陈述被本计划自身的删除清单证伪——`spec.md` 现行文本把 `Workflow` 写成"已裁定的编排载体"与 Phase B 的主路径（`spec.md:38-39`「编排载体：内置 `Workflow` 工具（已核实存在；skill 指令调用它属合法 opt-in 路径）」），而本计划 Phase 7 已把该工具与调用它的 skill 一并删除；`plan-stage1b.md` B2 明确写着「统一指纹协议：JS 侧 `canonicalKey` 与 Python 侧 `queue.fingerprint` 必须对同一候选产出一致的规范化串，加一条跨语言一致性测试」——这条验收 oracle 指代的 JS 对象在本计划完成后已不存在。**若不同步修订，未来的 Stage 1b 实施者会按冻结的旧文档去找一个已经删除的 Workflow 对象、去写一条注定失败的跨语言测试**。

**功能范围不削减的边界**：本任务只修订"用什么实现接缝达成"的描述与"用什么具体对象做验收判据"，**不改变** B1–B8 任一条目的目标范围。特别是 B2 的核心功能（远端队列对账、拒绝记忆、`possible_duplicate` 复核、统一指纹协议）**全部原样保留**，只是"统一指纹协议"这一条的实现对象从"JS `canonicalKey` vs Python `queue.fingerprint`，需要跨语言测试"改为"Python 侧 `fanout.dedupe_and_rank` 与 `queue.fingerprint`/`queue.canonical_key` 本就是同一份实现，跨语言一致性问题已因架构改变而不复存在，Stage 1b 的验收方式相应简化为对 `queue.canonical_key`/`_norm` 的单元测试覆盖（Phase 7 Task 7.2 的 `test_canonical_key_normalization.py` 已经是这份覆盖的一部分）"——**功能保留、实现手段与验收 oracle 更新**。

- [ ] **Step 1**：修订 `docs/harness/spec.md` 相关段落。具体改动点（只读改动即可核实定位，属文档编辑，不涉及代码）：
  - §二"已裁定决策"表的"编排载体"一行，原文「内置 `Workflow` 工具（已核实存在；skill 指令调用它属合法 opt-in 路径）」，追加脚注：「**2026-07-31 起由 ADR-002 取代**：控制器直接驱动扇出，不再经由 `Workflow` 工具或 skill 间接编排，见 [adr-002-control-flow-ownership.md](./adr-002-control-flow-ownership.md) 与 [plan-control-flow-rewrite.md](./plan-control-flow-rewrite.md)」。
  - §七 Phase B 流程图（"段 1 Workflow scrollz-propose"那一段）追加同样的脚注，说明该流程图描述的是**重写前**的架构，当前实现见控制流重写计划。**不删除原图**——它仍有历史参考价值（后续维护者理解"为什么曾经这样设计、又为什么改掉"需要这段上下文），只标注其时效性。
  - §十五评审处置台账追加一行：「ADR-002（控制流重写）| 采纳，废弃 Workflow 编排载体、改为控制器驱动扇出 | Stage 1a 完成后」。
- [ ] **Step 2**：修订 `docs/harness/plan-stage1b.md` B2 小节。原文「统一指纹协议：JS 侧 `canonicalKey` 与 Python 侧 `queue.fingerprint` 必须对同一候选产出一致的规范化串，加一条跨语言一致性测试（Python 生成样本 → Node 计算 → 比对）」，改为：「统一指纹协议：**该问题已因 ADR-002 控制流重写而结构性消失**——本轮内去重（原由 `scrollz-propose.js` 的 JS `canonicalKey()` 实现）与跨轮去重现在共用同一个 Python 函数 `queue.canonical_key`/`fingerprint`（见 `fanout.dedupe_and_rank`），不存在"两份独立实现需要保持一致"这件事。Stage 1b 实施时无需再写跨语言测试，`.claude/scripts/harness/tests/test_canonical_key_normalization.py`（控制流重写 Phase 7 产出）已经是这份规范化行为的单元测试覆盖，Stage 1b 若需要为 `possible_duplicate` 的相似度判定新增测试，直接扩展该文件或新增同类单元测试即可」。
- [ ] **Step 3**：`plan-stage1b.md` 文首状态行追加一句：「2026-07-31 起，B2 的实现接缝已按 [plan-control-flow-rewrite.md](./plan-control-flow-rewrite.md) Task 7.5 同步修订；B1/B3–B8 的目标范围不受影响」。
- [ ] **Step 4（cfr2-10 订正：验收 oracle 不能是"全文件零/少匹配"）**：Step 1 明确要求"不删除原图"——`spec.md`/`plan-stage1b.md` 中保留了大量描述**重写前**架构的历史 Workflow 段落，这些命中是**预期且刻意保留**的，`rg -n "Workflow" docs/harness/spec.md` 必然产生远多于"两处脚注"的匹配，用这条命令的匹配数量做验收判据必然失败，不能作为本任务的验收 oracle（第二轮评审 cfr2-10 已指出这一具体缺陷）。**改为对明确断言做定点检查**，而不是对全文件做无差别扫描：
  1. 核对 §二"已裁定决策"表"编排载体"一行**确实包含** Step 1 要求追加的脚注文本（用 `rg -n "2026-07-31 起由 ADR-002 取代" docs/harness/spec.md` 命中且仅命中 1 处，作为"脚注已加"的定点验证，不是"Workflow 只出现一次"的验证）。
  2. 同理核对 §七 Phase B 流程图段落包含该脚注（`rg -n -A2 "段 1 Workflow scrollz-propose" docs/harness/spec.md` 人工核对紧随其后是否有脚注文字，不是自动化断言，属人工可复现的检查）。
  3. 核对 §十五评审处置台账新增了"ADR-002（控制流重写）"一行（`rg -n "ADR-002（控制流重写）" docs/harness/spec.md` 命中 1 处）。
  4. 核对 `plan-stage1b.md` B2 小节的"统一指纹协议"描述**确实**替换为 Step 2 给出的新文本（`rg -n "该问题已因 ADR-002 控制流重写而结构性消失" docs/harness/plan-stage1b.md` 命中 1 处），且原文里"JS 侧 `canonicalKey`"与"跨语言一致性测试（Python 生成样本 → Node 计算 → 比对）"这两处**具体指代已删除对象的措辞**在 B2 小节内**不再出现**（`rg -n "canonicalKey|Node 计算" docs/harness/plan-stage1b.md` 命中数为 0——这条窄范围检查是合理的，因为这两个措辞是本任务明确要删除的具体对象引用，不是"Workflow"这种允许历史保留的宽泛词）。
  5. 核对 `plan-stage1b.md` 文首状态行含 Step 3 要求的那句话。
  这五条都是对**本任务实际写下的文字**做定点存在性检查，不是对整个文件的宽泛扫描，因此不会被"历史段落刻意保留"产生的噪音污染。
- [ ] **Step 5**：提交。

```bash
cd /home/xp/src/zipfs
git add docs/harness/spec.md docs/harness/plan-stage1b.md
git commit -m "docs(harness): 同步修订 spec.md/plan-stage1b.md 的 Workflow 相关实现接缝描述（ADR-002 取代旧编排载体，功能范围不削减，cfr-17）" -- docs/harness/spec.md docs/harness/plan-stage1b.md
```

**Phase 7 收尾检查**：`rg -n "Workflow|TaskOutput|scrollz-round" .claude/scripts/harness/*.py .claude/harness-settings.json` 应无命中（历史注释里提及「为什么退役」的说明性文字除外，那类注释允许提及旧名字用于解释背景）。全量测试套件跑通，测试总数应 **净增**（相对 304 基线，扣除本阶段删除的 1 个文件后，仍因 Phase 0–6 新增的测试而净增）。`docs/harness/spec.md`/`plan-stage1b.md` 已同步修订（Task 7.5），不再有指代已删除对象的验收 oracle。

---

## Phase 8 · 真机切换验收（花真钱、写公开仓库，逐步执行）

**目标**：验证新架构在真实 `claude` CLI + 真实公开仓库环境下可靠工作，并给出「切换完成」的验收判据。**延续 `plan-stage1a.md` Task 13 的纪律**：逐步执行、每步之间停下确认、不连跑；`HARNESS_FAULT` 定点故障注入而非随机 kill。

**回答问题 6（迁移路径）**：`plan-stage1a.md` 已明确记录「一次性替换，不做并存」（本计划开头「待决 B」）。切换的具体动作是——Phase 0–7 的全部代码改动本身**不改变** systemd timer 的 `disabled`/`inactive` 状态（延续 ADR 头部裁决：「2 小时定时器在重写完成前不启用」）。Phase 8 是**重写完成后**的验收，完成后由用户决定何时 `enable --now`。

### Task 8.1：contract probe 复跑（验证新工具集下的隔离仍然生效）

- [ ] **Step 1**：手工跑一次 `python3 -m harness.cli probe`（复用现有 `cli.py` 的 `probe` 命令，因为它验证的是「工具集隔离」这个不变量，不因扇出改动而失效——但工具集已收窄为三项，probe 的期望值需要同步更新，此步骤应在 Phase 6 Task 6.1 的测试改写里已经处理，这里是**真机复核**）。
- [ ] **Step 2**：确认输出「负向验证通过：工具集恰为 ['Glob', 'Grep', 'Read']，无 MCP、无插件」。
- [ ] **Step 3**：记录成本（预计低于此前 $0.202，因为工具集更小、prompt 更短）。

### Task 8.2：单角色真机冒烟（一个 finder，不做完整扇出）

- [ ] **Step 1**：写一个最小手工脚本（或临时 Python REPL 会话）只调用 `fanout.run_finders` 中的**一个**角色（`finder:hygiene`，风险最低的视角），观察真实输出是否符合 `fanout_schema.validate_finder_output`。
- [ ] **Step 2**：确认 `agent_attempts` 表出现对应记录（`attempt=1`, `status=success`）。
- [ ] **Step 3**：若失败（真实模型输出不符合 schema 假设），记录具体偏差，评估是否需要调整 prompt 措辞（`prompts.py` 的 `build_finder_prompt` 或对应 `.claude/agents/harness-finder-hygiene.md` 正文）——**这类调整允许在 Phase 8 内小范围修正**，因为 prompt 措辞不是本计划的核心架构决策，出现真机偏差是预期的（`plan-stage1a.md` 历史上也在 Task 13 阶段修正过五个真机缺陷）。

### Task 8.3：完整扇出真机跑通（4 finder + judge 裁决，允许发布）

- [ ] **Step 1**：手工触发一次 `python3 -m harness.cli round`，观察完整链路：4 个 finder 并发 → 去重排序 → judge 裁决（redline 优先）→ 若有候选通过全部 judge → 发布。
- [ ] **Step 2**：核对：`agent_attempts` 表记录 4–7 条（取决于是否短路）；`invocations` 表记录对应成本（验证 Phase 6 补上的 `record_invocation` 生产路径调用生效）；若发布，Issue/提案卡/收据三件事仍如 `plan-stage1a.md` Task 13 Step 4 一样验证。
- [ ] **Step 3**：记录本轮总成本，与 `HANDOVER.md` 记录的旧架构成本（$5.45）比较——**预期显著降低**，因为工具集收窄（不再需要 opus 外层会话、不再需要 `Workflow` 编排开销）。这个对比数字写入 `HANDOVER.md` 更新（Phase 9 收尾时做，非本任务）。

### Task 8.4：故障注入验收（fork 重试路径的真机验证）

**这是本计划新增能力里最需要真机验证的一项**——Phase 0–7 全部用假件测试过 fork 重试的编排逻辑，但「真实 `claude` CLI 在传输故障后确实能 `--resume --fork-session` 恢复」只在 Phase 0 探针里验证过一次简化场景，需要在真实扇出编排中复现。

**cfr-10 修正（Critical，已核实）**：v1 草图的 Step 1 用 `HARNESS_FANOUT_FAULT=<role>:<attempt>` **完全跳过真实 `invoke()` 调用**，直接在 `run_role_with_retry` 内构造一个假的失败 `InvocationResult`。这意味着 attempt 1 **从未真正调用过 CLI，CLI 从未创建过任何真实 session**。随后 Step 2 却断言 `attempt=2` 会 `--resume` 该 session 并成功——但 `--resume` 一个从未被 CLI 创建过的 session ID，其真实行为是未定义的（可能报错、可能被当成新会话处理，PoC 从未测试过这个组合）。**这样的验收即便"通过"（比如账本确实记录了两行），也只证明了 Python 侧状态机能正确流转，完全不能证明"CLI 真的能从传输故障中 fork 恢复"——而后者才是 ADR D2 要验证的核心能力，也是 Phase 0 Task 0.1 already 验证过的能力（在简化场景下），本任务的价值正在于把它接到真实扇出编排里再验证一次，如果验证手法本身不涉及真实 CLI，这个任务就没有履行它的职责**。

**修复设计**：借用 PoC `driver.py` 已经验证过的手法（`interrupt()` 方法，通过 `control_request{subtype:"interrupt"}` 主动中断一个**真实在跑**的会话）。这要求 attempt 1 的调用**不使用**当前 Stage 1 的 `--tools Read,Grep,Glob`（那样无法触发 `can_use_tool` 从而无法在中断前插入等待点，见 Phase 0 Task 0.2 的结论），而是复用 PoC 那套**独立于生产 `invoke()` 的中断专用探针**（`--permission-prompt-tool stdio` + `--tools Write`，专供本任务验证 fork 机制本身，不代表 Stage 1 生产路径会用到这个工具集）——**这是测试专用装置，不改变 Stage 1 生产的工具集收窄决策**。

- [ ] **Step 1（修正）**：新建 `exp/control-flow-rewrite-probe/probe_fanout_fork.py`（复用 `exp/stdio-driver/driver.py` 的 `Invocation` 类与 `interrupt()` 方法，不复制代码，直接 `import`）：
  1. 用 `derive_session_id(round_id, "finder:roadmap", 1)` 算出 attempt 1 的 session_id。
  2. 起一个**真实**进程：`Invocation(session_id=<该值>, tools="Write", extra_args=["--permission-prompt-tool", "stdio"])`，prompt 要求"记住暗号 XYZZY，然后调用 Write 写一个文件"。
  3. 等待 `control_request{subtype:"can_use_tool"}` 出现（证明进程真的启动、真的在等待权限确认——这一步本身就确认了"真实 session 已被 CLI 创建"）。
  4. 发送 `interrupt`，确认拿到匹配的 `control_response`。
  5. 读取该次调用的最终 `result` 事件，确认 `session_id` 字段等于 Step 1 预分配的值（`--session-id` 参数生效的独立证据）。
**cfr-10/cfr2-06 修正（Critical，第二轮评审确认仍未闭合）**：v2 的 Step 3 要求断言"attempt 2 的 `result.result` 中包含 `XYZZY`"，但这个断言在当前接口下**无法执行**——`InvocationResult` 没有保存 `result.result` 原始文本的字段（只有 `payload: dict | None`，经 `payload_parser` 解析后的结构化结果；以及 `raw_tail: str`，只在**失败**路径填充最后 5 行原始输出，成功路径为空字符串）。而默认的 `_extract_payload`（要求顶层 `candidates: list`）与 `_extract_json_object`（要求顶层是任意 `dict`）都会**拒绝**模型直接输出的纯文本"记住的暗号是 XYZZY"——这段文本既不是含 `candidates` 的 JSON，也不是任何 JSON 对象，会被判定为不可解析，`payload` 为 `None`，`ok=False`，探针在这一步就会误判为"fork 后调用失败"，而实际失败原因只是"探针的 parser 选型与探针自己要求模型输出的格式不匹配"，与被验证的 fork 机制无关。

**修复**：本任务是专用验证装置（不是生产路径），探针的 attempt 2 调用注入一个**探针专用** `payload_parser`：`_extract_probe_echo(text: str) -> dict`——不要求任何 JSON 结构，直接把原始文本包一层 `{"raw_text": text}` 返回（`ok` 判定只要求非空文本即视为"parser 成功"，因为探针的验证目的是"文本内容是否含有 XYZZY"，不是"JSON 结构是否合法"）。探针 prompt 相应地明确要求模型"用纯文本回复，不要输出 JSON"，断言点从"`result.result` 包含 XYZZY"改为"`invocation_result.payload["raw_text"]` 包含 XYZZY"——这是该探针作为**测试专用装置**（与 Task 2.2 定义的可注入 `payload_parser` 机制吻合：生产路径用 `_extract_payload`/`_extract_json_object`，测试专用探针可以注入第三种 parser，机制本身在 Task 2.2 就已经支持，不需要新增任何生产代码）。

- [ ] **Step 2**：用 `--resume <该真实 session_id> --fork-session` 发起 attempt 2（走真实 `claude_runner.invoke(..., payload_parser=_extract_probe_echo)`，`RoleInvocationRequest(resume=..., fork_session=True)`），prompt 为"继续，用纯文本（不要输出 JSON）报告刚才记住的暗号"。
- [ ] **Step 3**：断言 attempt 2 返回的 `InvocationResult.payload["raw_text"]` 中包含 `XYZZY`（证明 fork 真的恢复了 attempt 1 的上下文，不是空会话），且 attempt 2 的 `session_id` 是一个**新的、与 attempt 1 不同**的值（与 PoC Q5 一致：fork 产生新 ID）。
- [ ] **Step 4**：把这两次调用接入 `ledger.record_attempt_started`/`record_attempt_finished`（在真实 `round_id` 下），确认 `agent_attempts` 表记录 `attempt=1, session_id=<预分配值>, status=failed_transport`（interrupt 导致的失败）与 `attempt=2, session_id=<CLI新分配值>, parent_session_id=<attempt 1 的 session_id>, status=success`。**这里的账本写入与验证是真实的**——不同于 v1 草图里"account bookkeeping 与被验证的机制脱节"的问题，本步骤记录的是刚刚在 Step 1–3 里真实发生的调用。
- [ ] **Step 5**：确认本轮扇出编排（若跑一次完整 `round`）在其中一个角色遭遇此类中断时，最终仍能正常判定结果（其余角色不受影响）——这一步可以用 Phase 5 已有的假件测试覆盖（并发隔离性质不需要真机验证，见 Task 5.4b 的 `test_one_finder_transport_failure_does_not_affect_other_finders`），Task 8.4 本身只聚焦"真实 CLI fork 恢复能力"这一件事，不重复验证已被假件测试覆盖的并发隔离性质。

**验收判据（Phase 8 整体）**：
1. probe 负向验证通过，工具集恰为三项。
2. 至少一次完整扇出真机跑通并发布（或正确判定 no-candidate/duplicate）。
3. fork 重试路径至少一次真机复现——**必须基于真实创建的 CLI session**（cfr-10 修正），验收 oracle 核对新旧 session ID 确实不同、且新 session 确实恢复了旧会话的上下文（`XYZZY` 暗号测试，断言点是探针专用 parser 产出的 `payload["raw_text"]`，不是不存在的 `result.result` 字段，cfr-10/cfr2-06）。
4. 全部真机操作零意外副作用（不误建重复 Issue、不误 push、`agent_attempts`/`invocations` 表记录与实际调用数一致）。
5. 成本对比数据已记录，供后续（不属于本计划）的预算重新校准参考。

**Phase 8 不做的事（明确排除，避免范围蔓延）**：不启用 systemd timer（用户裁决保留）；不做 Stage 1b 范围内的任何治理项（远端队列对账、拒绝记忆、机器红线 gate 等，`plan-stage1b.md` 冻结范围不变）；不做通用化到 `~/src/my-ade` 的任何代码改动（那是另一个项目的另一次实施，本计划只负责标注接缝，见下节）。

---

## 通用化接缝（问题 7：哪些是 zipfs 专有，哪些应从一开始就可注入）

用户已裁决「先在 zipfs 跑通再搬」。以下逐项标注本计划产出的代码里，哪些是当前**硬编码的 zipfs 专有值**、哪些**已经设计成可注入**、哪些**看似通用但实际暗含 zipfs 假设**——供未来 `~/src/my-ade` 移植时对照，而不是现在就抽象化（抽象化没有第二个使用方验证正确性，容易做错方向；这是本计划刻意不做的事，登记在下方「未采纳方案」）。

| 项 | 当前状态 | 通用化时需要做什么 |
|---|---|---|
| **Agent 定义**（`.claude/agents/harness-*.md` 七个文件） | `prompts.py` 的 `parse_agent_file` 函数本身**与仓库路径无关**——它接受任意 `Path`，是通用的。`round.py` 的 `_ROLE_TO_AGENT_FILENAME`/`_AGENT_FILES_DIR` 硬编码了「七个固定角色名→固定文件名」的映射与 `.claude/agents` 这个相对路径。| 迁移时把角色到文件名的映射改为从 `Config` 或一个新的 `roles.yaml` 读取，而非 Python 字典字面量；`_AGENT_FILES_DIR` 改为 `Config` 字段。这是**小改动**，因为 `parse_agent_file` 本身已经是纯函数，不依赖任何全局状态。|
| **提示词**（agent 文件正文 + `prompts.py` 的 `build_finder_prompt`/`build_judge_prompt` 里的固定措辞，如「不可信数据边界」提示语） | 「不可信数据边界」提示语（`_UNTRUSTED_DATA_NOTICE`）与「BEGIN/END UNTRUSTED CANDIDATE」包裹格式是**通用安全纪律**，与 zipfs 无关，可直接复用。Agent 文件正文（如 finder-roadmap 引用 `docs/ROADMAP.md`）**是 zipfs 专有**——搜索面、文件路径、项目术语都是本仓库特定的。| 迁移时 Agent 定义文件本身需要为目标仓库重写正文（这本来就是「agent 定义」这一层该做的事，`prompts.py` 的装配逻辑不用改）。 |
| **红线表**（`docs/harness/redlines.yaml`） | 完全是 zipfs 专有（磁盘格式路径、崩溃安全提交顺序等）。`fanout.py`/`round.py` 均不解析这个文件（Stage 1a 里红线判定是 judge 提示词层面的软约束,机器 gate 属 Stage 1b 范围,plan-stage1b.md B3 已冻结）。| 迁移时红线表本身需要为目标项目重写内容，加载逻辑（未来 Stage 1b 才会写）从一开始就应该接受可配置路径,不硬编码 `docs/harness/redlines.yaml`。 |
| **仓库路径**（`Config.repo_root`/`repo_slug` 等） | `config.py` 现有的 `REPO_ROOT = Path("/home/xp/src/zipfs")` 硬编码——这是 **Stage 1a 起点代码已有的问题，不是本计划引入的**，本计划不改动 `config.py` 的这一行（不在白名单改动范围内）。| 通用化时这是最先要做的事：`REPO_ROOT` 改为必须显式传入或从环境变量读取,不留默认值指向 zipfs。**本计划范围外，登记进 backlog**。 |
| **并发度**（4 finder + 3 judge 的角色集合） | `fanout.py` 的 `_FINDER_ROLES`/`_JUDGE_ROLE_TO_TYPE` 硬编码四视角三裁决的具体角色名。**并发原语本身**（`ThreadPoolExecutor` + `run_role_with_retry` 的重试/fork 逻辑）与角色数量、角色名称无关,是通用的。| 迁移时把角色集合改为从配置读取的列表,`run_finders`/`judge_candidate` 改为接受角色列表参数而非硬编码常量——这需要在 `~/src/my-ade` 项目里做,不在本计划范围内预先抽象（避免无验证的过度设计）。 |
| **会话身份派生**（`session_identity.py`） | `derive_session_id` 的 `ROLES` 常量硬编码七个角色名,命名空间 UUID 是全新生成、无仓库绑定。**函数本身完全通用**。| 迁移时 `ROLES` 集合需要改为可配置,或干脆去掉这层校验、只做格式与非负 attempt 校验,不限定具体角色名枚举。 |
| **候选 DTO 字段**（`_REQUIRED_CANDIDATE_FIELDS` 等,round.py 现有） | 完全通用的字段集合设计（`title`/`goal`/`invariant`/`oracle` 等）,不含任何 zipfs 特定语义。**这是 Stage 1a 已有的良好设计,本计划不改**。| 可直接复用,无需改动。 |

**登记为 backlog（不阻塞本计划,但有长期价值,不得静默丢弃）**：

1. `config.py` 的 `REPO_ROOT` 硬编码——通用化时的第一个必做项,不在本计划范围（本计划不改动 `config.py` 除 `db.py` schema 追加之外的任何配置模块）。
2. `~/src/my-ade` 的 `--agents <json>` 内联路线——ADR 明确「Q7 通用化仍可能用到它承载 persona」,本计划选择了「Python 侧读取 `.md` 文件拼 prompt」而非 `--agents`,原因是**不需要引入额外的 CLI 标志组合来达成同样效果**（见「未采纳方案」）。若 `~/src/my-ade` 的宿主项目没有仓库内 `.claude/agents/` 目录这个概念（例如它是一个更轻量的工具,不想在目标文件系统上放置 persona 文件）,`--agents <json>` 内联反而是更合适的路线——彼时需要重新评估,不应假设 zipfs 的路线直接适用。
3. Stage 2（开发轮）的 `--permission-prompt-tool stdio` 编排设计——Phase 0 Task 0.2 已明确本计划（Stage 1，只读工具）不使用它,但 Stage 2 需要它来做「拦截—校验—回填」。这是一个**全新的独立设计任务**,不在本计划范围,登记为「下一个 spec/plan 的输入」。
4. 消息级 `forkSession(upToMessageId)`——ADR 与 PoC 均明确「本 PoC 未验证 Python Agent SDK 的消息级 fork,只确认 CLI 会话末尾 `--resume --fork-session`」。本计划采用的是**会话级** fork（末尾续接）,足以覆盖「传输故障打断」的场景（fork 时故障发生在最后一条完好消息之后,续接点自然就是末尾）。若未来场景是「故障发生在半途、需要精确回退到某条消息」,需要先验证消息级 fork 的 Python 侧可用面,这是一个独立的 PoC 任务,不在本计划范围。

---

## 未采纳方案（record-not-adopted）

| 方案 | 为何未采纳 |
|---|---|
| 用 PoC `driver.py` 的 dual-pipe 长命进程模型整体替换现有单发 `invoke()` | 见「待决 A」。Stage 1 的每次子调用是单轮问答,不需要多轮 stdin 喂入能力;`--resume`/`--fork-session` 大概率与调用是单发还是流式无关（Phase 0 会验证）。若 Phase 0 证伪,才转向此方案。 |
| 保留旧 JS workflow 与新 Python 扇出并存,用 feature flag 切换 | 见「待决 B」。systemd timer 当前 disabled,没有实时流量需要双轨验证;维护两套降级/去重逻辑一致性的成本历史上已多次导致漂移缺陷（`STAGE1_TOOLS`、canonical key 跨语言）。 |
| 用 `--agents <json>` 内联 agent 定义替代读取 `.claude/agents/*.md` 文件 | PoC Q7 已验证 `--agents` 完全可行,但本计划的扇出目标（一子任务一顶层进程,不经 `Task` 工具）不需要它——`--agents` 解决的是「模型在会话内部按名字动态调用其它 agent」,而本计划里每个顶层进程本身就是「某一个角色」,不需要在同一进程内切换 persona。改用 `--agents` 反而需要引入 `Task` 工具（PoC Q6 已证实其反例）才能触发,与「不用 Task 扇出」的决策矛盾。**仅登记为 backlog 项 2**,供 `~/src/my-ade` 视其宿主环境重新评估。 |
| 把 Stage 1 只读工具也套上 `--permission-prompt-tool stdio` 拦截 | Phase 0 Task 0.2 预期实测结论：`Read`/`Grep`/`Glob` 不产生 `can_use_tool`,没有可拦截的对象,引入这套机制纯属增加复杂度而无收益。若 Phase 0 实测出乎意料（某只读工具确实触发权限请求）,按 Task 0.2 Step 2 的「例外」路径处理,不改变本条判断的默认结论。 |
| 把 `docs/harness/redlines.yaml` 的机器解析纳入本计划 | 属于 `plan-stage1b.md` B3 已冻结的范围（机器红线 gate）,本计划的扇出改动不改变 Stage 1a/1b 的边界划分——红线在 Stage 1a 仍只是 judge 提示词层面的软约束,不因扇出实现方式变化而升级。 |
| 给 `agent_attempts` 表也接入跨进程崩溃恢复（类似 outbox 的 probe-before-call） | 见「待决 D」。当前 harness 进程崩溃后新一轮从头扫描是**已有行为**,扇出编排内部的中间状态（跑到第几个 finder）本就不是崩溃恢复要保护的对象——outbox 保护的是「已经产生外部副作用（Issue/commit/push）的事务」,扇出阶段在候选被选中之前**没有任何外部副作用**,重新跑一遍的代价只是烧掉那一轮的预算（且预算本身有 `budget.abandon()` 兜底),不产生数据不一致。引入持久化中间状态的复杂度与其防护的风险不成比例。 |


---

## 自审

> 本节随 v5 修订（第四轮定点核验 cfr4-01–cfr4-02 处置后）同步更新。v5 相对 v4 的核心变化：(a) 修正 Task 5.5 联合测试的错误 oracle——`session_id`（UUIDv5）与 `attempt_key`/`stream_log` 的明文段不可能字面相等，订正为三者分别与各自独立计算的预期值比对（同源验证）；`make_request`/`_make_request` 签名统一改为 `(role, attempt)`，fork 续跑时每个 attempt 取得随编号变化的骨架请求，`stream_log` 不再被 fork 后的新 attempt 覆盖（cfr4-01，Task 5.4/5.5）；(b) 终态分类表补齐两处此前未定义的区域——parser 层模型输出失败（`ok=False` 但 `subtype="success"`）归类为可重试，CLI 启动/认证失败（`protocol_errors` 含 `missing init event`）归类为不可重试，与"真正超时"明确区分（cfr4-02，Task 5.3）；(c) rmf-08 的放大倍数从错误的"7 倍"订正为"最坏情形 39 倍"，`_persist_stream()` 改为创建时直接指定 `0o600`（不再是落盘后 `chmod`，消除中间权限窗口）。v4 相对 v3 的变化（cfr3 系列）保留在下方历史记录：(d) 新增 `RequestContext`/`build_stream_log_path` 契约，把生产环境 `cwd`/`settings_path`/`model`/`stream_log` 的取值收敛到 Phase 6 Task 6.1 单一构造点（cfr3-01）；(e) `InvocationResult` 新增 `subtype` 字段透传终态事件原始值，`AttemptRecord.retryable` 的赋值从"二元判断"改为可测试的终态分类表（cfr3-02）；(f) 把开放发现处置表宣称但从未被任务清单断言的三项补进 Task 5.2/5.6/6.1 的实际不变量与测试清单（cfr3-03）。v3 相对 v2 的变化（cfr2 系列）：(g) 全篇代码块降级为「接口契约+不变量+测试清单」（cfr2-05）；(h) `AttemptRecord` 新增 `retryable`/`resumable` 两个布尔位（cfr2-07）；(i) judge task identity 携带 candidate fingerprint（cfr2-02）；(j) `WaveResult` 携带全部 attempts 供 `FanoutSettlement` 正确聚合（cfr2-03）；(k) `BudgetTracker.settle()` 允许变负（cfr2-03）；(l) judge 调用补传 `expected_tools`（cfr2-04）；(m) Task 8.4 探针改用专用 parser（cfr2-06）；(n) 开放发现处置表删除自填的假 backlog 编号（cfr2-09）；(o) Phase 7 Task 7.5 验收 oracle 改为定点检查（cfr2-10）。

### ADR-002 D0/D1/D2 覆盖检查

| ADR 条目 | 落点 |
|---|---|
| D0：`--permission-prompt-tool stdio` 是官方支持的隐藏标志 | Phase 0 Task 0.2 实测 Stage 1 是否需要（cfr-15 订正：须真正打开该开关才能得出有效结论）；本计划结论是「Stage 1 不需要，Stage 2 才需要」，登记 backlog 项 3 |
| D1：控制器驱动扇出，一子任务一顶层 process/session | Phase 5（`fanout.py`）+ Phase 6（`round.py` 接线）；`--session-id` 由 `(round_id, role, attempt)` 确定性派生（Phase 1） |
| D1：编排（去重/短路/聚合）全部在 Python 里，可单测 | Phase 5 全部任务用假件测试，零真实调用；测试替身现在接受真实 `RoleInvocationRequest`（cfr-02），不是宽松 `**kwargs` |
| D1：单个 agent 失败只影响它自己 | Phase 5 Task 5.5（`run_finders`/`judge_candidate`）+ Task 5.4 波次调度的角色隔离（cfr-12：每波并发发起，互不影响） |
| D2：失败后 fork 续跑而非从头重来 | Phase 5 Task 5.4 `run_wave_scheduled` 的 attempt≥2、且 `retryable and resumable` 同时成立时走 `build_continuation_request`（`--resume --fork-session`）；`resumable=False` 时改为全新尝试而非冒充 fork（cfr2-07）；Phase 8 Task 8.4 真机验证（cfr-10/cfr2-06 订正：须先真实创建 session，探针 parser 需匹配纯文本输出） |
| D2：fork 出的新 session id 由 CLI 返回，控制器记进账本，谱系可审计 | Phase 1 `agent_attempts` 表 + `ledger.py`；`parent_session_id` 记录链路（cfr-03/cfr-11 修正：账本写入延后到主线程、真实 session_id 已知之后） |
| 「不得假设『恰好一个 result』全称成立」 | `claude_runner.parse_stream_json` 现有的 `duplicate terminal result events` 检测已覆盖，本计划不改动该逻辑，且明确不通过 `--agents`+`Task` 扇出（Phase 4 说明），从根源避免触发该反例 |
| 「本地分类器自动放行的安全命令不产生 `can_use_tool`」 | 不适用于本计划——finder/judge 只有 `Read`/`Grep`/`Glob`，无 Bash，该坑是 Stage 2（写代码）才会遇到的，登记 backlog 项 3 |
| 「`--max-budget-usd` 是滞后停止触发器，非硬上限」 | Phase 5 `BudgetTracker`（cfr-05）与 Phase 6 `budget.record_invocation` 设计已明确按「事后累计实际成本」而非「传了上限就不超」处理，延续 rmf-05 的既有修复精神 |
| `--agents <json>` 实测通过，可用于通用化 | 明确记录不在本计划中使用（Phase 4 说明 + 未采纳方案），但通用化接缝章节登记为 backlog 项 2，供未来重新评估 |

### 任务描述里「要改的东西」逐项覆盖

| 受影响文件 | 处置 |
|---|---|
| `.claude/scripts/harness/round.py` 扫描段 | Phase 6 Task 6.1（cfr-04/06/08/09/15 修正后重写） |
| `.claude/scripts/harness/claude_runner.py` | Phase 2 Task 2.1（会话参数）/2.2（payload parser，cfr-01）+ Phase 6 Task 6.1（工具收窄，与 round.py 同一提交） |
| `.claude/workflows/scrollz-propose.js`（退役） | Phase 7 Task 7.1（与 Task 7.2 顺序对调，cfr-18） |
| `.claude/skills/scrollz-round/`（退役） | Phase 7 Task 7.1 |
| `.claude/workflows/tests/degraded-dedup.test.mjs`（逻辑迁 Python） | Phase 5 Task 5.2（迁移）+ Phase 7 Task 7.3（删除旧文件，含覆盖核对清单） |
| `test_canonical_key_cross_language.py`（去留） | **裁定：删除**，替换为 `test_canonical_key_normalization.py`（Phase 7 Task 7.2，提前于 Task 7.1 执行），理由是被测的跨语言接缝本身消失（JS 侧实现随 Task 7.1 删除），不是净减覆盖——新测试**冻结当前 Python 实际行为**（cfr-18：`_norm('a\x1fb') == 'a\x1fb'`，已实测确认，不是撰写时臆造的 `'a b'`） |

### 设计问题逐项覆盖

| 问题 | 章节 |
|---|---|
| 1. degraded/重试/短路语义迁移形状 | Phase 5 开头表格 + Task 5.2/5.4/5.5 |
| 2. session 身份怎么定 + 与 outbox 幂等键关系 | Phase 1 开头「设计回答」 |
| 3. fork 重试谱系记录 + 是否新表 | Phase 1（`agent_attempts` 新表，纯追加） |
| 4. 并发度与并发原语 + 失败隔离 | Phase 5 开头「设计回答」（`ThreadPoolExecutor` + `BudgetTracker` 线程安全 + 波次调度，cfr-03/05/12） |
| 5. Stage 1 是否用 `--permission-prompt-tool stdio` | Phase 0 Task 0.2 |
| 6. 迁移路径（一次性替换 vs 并存） | 待决 B + Phase 8 开头 |
| 7. 通用化接缝 | 「通用化接缝」章节 + backlog |

### 非功能需求

- **性能**：Phase 8 Task 8.3 记录真机成本对比（预期显著低于 $5.45，因为省去外层 opus 会话与 Workflow 编排开销）。
- **可观测性**：`agent_attempts` 表是本计划新增的可观测性资产，供未来 `status` CLI 展示子调用谱系（本计划只留查询函数，CLI 展示登记 backlog，非阻塞）；`FanoutSettlement.protocol_errors`（cfr-04 关联 rmf-04 修复）让判因结论不再随扇出改动而丢失。
- **迁移/兼容**：候选 DTO 契约、outbox 幂等键、崩溃恢复语义全部不变——这是本计划反复强调的约束，全篇贯彻。
- **对齐既有脚本工具**：Global Constraints 明确复用 `plan-stage1a.md` 的绝对路径/测试跑法约定，不新增任何工具依赖；`git worktree` 的干净态验证（cfr-08）复用标准 git 工具链，无新依赖。

### 占位符扫描

无 TBD/TODO。**v3 起全篇代码块改为「接口契约+不变量+测试清单」形式**（cfr2-05）——这不是占位符降级，而是刻意的格式变化：完整可执行的函数体已被证明会掩盖签名/算术层面的缺陷（第二轮评审独立执行 v2 代码后才发现 `BudgetTracker.settle()` 的算术错误），改为契约形式后每个任务仍然给出完整的数据类字段、函数签名与可执行的不变量列表，只是"最小实现怎么写"这一步交还给 TDD 的 Step 3，由实施者亲手写、亲手跑红绿。每个任务的测试清单是断言点列表（不是完整测试源码），实施者据此逐条写出可执行测试——这与"占位符"的区别在于：占位符是"不知道该写什么，先搁着"，而契约形式是"已经精确定义了该写什么，只是不在文档里预先写死代码"。Phase 5 Task 5.5 末尾关于 `run_wave_scheduled` 单一 `validate` 参数不足以表达多 judge 不同 schema 的"已知简化点"同样已明确指出改进方向，不是隐藏的占位符。

### 类型/接口一致性

- `InvocationResult` 新增 `session_id`/`payload_parser` 相关字段（Phase 2），`AttemptRecord`（Phase 5 Task 5.3，含 `cost_known`/`denials`/`protocol_errors`/`retryable`/`resumable`，cfr2-07）、`WaveResult`（Phase 5 Task 5.4，cfr2-03）、`FanoutSettlement`（Phase 5 Task 5.6）、`round.py` 消费方式（Phase 6）四处字段名与类型逐一核对一致——`_settle_failed`/`_capability_drift_problems` 的参数已从"接受单一 `InvocationResult`"改为"接受聚合字段/`FanoutSettlement`"，Phase 6 Task 6.1 已按此顺序给出改法。
- `RoleInvocationRequest`（Phase 2 Task 2.3 定义）在 Phase 5 `fanout.py`、Phase 6 `round.py`/`cli.py` 中的构造与消费签名一致；`test_role_invocation.py` 用 `inspect.signature` 机械核对其字段与 `claude_runner.invoke()` 真实参数一致，签名漂移会被提前发现（cfr-02）；`cli.py` 的适配闭包必须真正执行 `to_invoke_kwargs()` 展开，Phase 6 Task 6.1 测试清单第 7 条专门验证这一点（cfr2-01）。
- `AgentDef`（Phase 4 定义）在 Phase 5 `fanout.py`、Phase 6 `round.py` 中的使用签名一致。
- `agent_attempts` 表字段（Phase 1 `db.py` schema）与 `ledger.py` 函数参数、`fanout.py` `run_wave_scheduled` 主线程串行写账本处逐一核对一致（cfr-03/cfr-11：账本写入延后到 worker 线程返回之后，在主线程执行，且显式 `try/except` 包裹，cfr2-08）；状态词枚举（`running`/`success`/`failed_transport`/`capability_drift`）在 `db.py` CHECK 约束、`ledger.record_attempt_finished` 校验、`AttemptRecord.status` 字面量三处逐字统一（cfr2-04）。
- judge task identity（`f"judge:<type>:<fingerprint>"`，Phase 1 Task 1.1）在 session identity 派生、`agent_attempts.attempt_key`、`fanout.py` `AttemptRecord.role`/`all_records`/`degraded` 记录四处统一使用（cfr2-02）。

---


## 执行状态（逐任务同步，跨会话据此判断进度）

> v5 修订（第四轮定点核验 cfr4-01–cfr4-02 处置后）：任务编号相对 v4 不变，但 Task 5.3/5.4/5.5 有实质性修正——Task 5.3 的终态分类表补齐两处此前未定义的区域（parser 层模型输出失败可重试、CLI 启动/认证失败不可重试，cfr4-02）；Task 5.4 的 `make_request` 签名改为 `(role, attempt)`，fork 续跑不再复用上一次请求对象的 `stream_log`（cfr4-01）；Task 5.5 的联合测试 oracle 从错误的"字面相等"订正为"同源比对"，`_make_request` 签名同步（cfr4-01）。Task 2.1 的 rmf-08 处置同步订正（放大倍数 39、创建时直接指定 0600）。若此前有会话已按 v4 的契约开始实施，**请特别核对 Task 5.4/5.5 的 `make_request`/`_make_request` 签名是否已经是 `(role, attempt)` 两参数**，以及 Task 5.5 的联合测试是否已按"同源比对"而非"字面相等"实现——v4 按字面实现的联合测试必然无法通过，需要重写而非补丁。

| # | 任务 | 状态 | 验证证据 | 偏差 |
|---|---|---|---|---|
| 0.1 | 会话原语真机验证（session_id/resume/fork） | 待开始 | | |
| 0.2 | 只读工具是否触发 can_use_tool（cfr-15 订正：须带 stdio 权限开关） | 待开始 | | |
| 1.1 | session_identity.py（judge task identity 携带 fingerprint，cfr2-02） | 已完成 | `python3 -m unittest harness.tests.test_session_identity`：7 tests OK；正控将 `uuid5` 临时替换为 `uuid4` 后，确定性测试按预期失败 | 无 |
| 1.2 | agent_attempts 表 + ledger.py（状态词统一，cfr2-04） | 已完成 | `python3 -m unittest harness.tests.test_ledger harness.tests.test_db`：9 tests OK；删除表定义后 6 tests 因 `no such table` 报错；删除 Python 状态校验后非法状态在 SQL CHECK 处报错 | 无 |
| 2.1 | claude_runner 会话参数扩展（`subtype` 暴露 cfr3-02 前置；stream 落盘 0600，rmf-08） | 已完成 | `python3 -m unittest harness.tests.test_claude_runner`：55 tests OK；全量：328 tests + 13 tests OK；正控删除 session_id/resume 互斥校验后目标测试失败，改回默认权限创建后权限位与源码创建方式测试失败 | 无；保留现有 6 工具 allowlist，未提前执行 Phase 6 收窄 |
| 2.2 | claude_runner 可注入 payload_parser（cfr-01） | 已完成 | `python3 -m unittest harness.tests.test_claude_runner`：60 tests OK；全量：333 tests + 13 tests OK；正控放宽 `_extract_json_object` 允许 list 后拒绝非 dict 测试失败 | 无 |
| 2.3 | RoleInvocationRequest + RequestContext 唯一调用契约（cfr-02, cfr3-01） | 已完成 | `python3 -m unittest harness.tests.test_role_invocation`：7 tests OK；全量：340 tests + 13 tests OK；正控将 `invoke.session_id` 改名后签名一致性测试失败，将 stream path 去掉 task_role 后 identity 测试失败 | 无 |
| 2.4 | STAGE1_ALLOWED_TOOLS 收窄（挪至 Phase 6 Task 6.1 执行） | 延后至 6.1 | 未修改 `STAGE1_ALLOWED_TOOLS`；全量测试 init 工具集仍为 `Glob,Grep,Read,Skill,TaskOutput,Workflow` | 按本节明确要求，本阶段只新增能力、不改变生产调用路径 |
| 3.1 | fanout_schema.py（含 cfr-13 类型前置检查） | 已完成 | `python3 -m unittest harness.tests.test_fanout_schema`：14 tests OK；全量：354 tests + 13 tests OK；正控禁用 finder 顶层未知字段检测后额外字段测试失败，删除 `_check_enum` 类型前置检查后 size/priority/verdict 的 list/dict 用例以未捕获 `TypeError` 失败；每次正控均先清理 `__pycache__` | 无 |
| 4.1 | prompts.py | 已完成 | `python3 -m unittest harness.tests.test_prompts`：6 tests OK；全量：361 tests + 13 tests OK；正控将 frontmatter 正则临时改为永不匹配后，样例解析与全部 7 个真实 agent 定义集成测试共 8 个 error，恢复并清理 `__pycache__` 后 6 tests OK | 无；agent 定义来源由 `parse_agent_file(path)` 参数注入，未硬编码项目路径；未使用 `--agents` 或 `Task` |
| 5.1 | dedupe_and_rank | 已完成 | `python3 -m unittest harness.tests.test_fanout`：5 tests OK；全量：371 tests + 13 tests OK；正控将排序 key 临时改为恒定值后 priority 排序测试失败，恢复前清理 `__pycache__` | 无 |
| 5.2 | normalize_error/record_degraded（双写 agentType 补测试，cfr3-03/rmf-04 反例） | 已完成 | `python3 -m unittest harness.tests.test_fanout`：12 tests OK；全量：378 tests + 13 tests OK；正控将 UUID 正则移到裸 hex 后 UUID 折叠测试失败，删除 `agentType` 双写后别名测试以 `KeyError` 失败；每次恢复前清理 `__pycache__` | 无 |
| 5.3 | run_one_attempt 单次尝试原语（cfr-02/03/06/12, cfr2-07 新增 retryable/resumable, cfr3-02 终态分类表, cfr4-02 补齐 parser 层与 CLI 启动失败分支） | 已完成 | `python3 -m unittest harness.tests.test_fanout`：29 tests OK；全量：395 tests + 13 tests OK；正控用预分配 session id 计算 `resumable` 后 2 个测试失败，退回二元重试判断后预算/重复终态/CLI 启动失败 3 个测试失败，删除 missing-init 分类后 CLI 启动失败测试失败；每次恢复前清理 `__pycache__` | 无 |
| 5.4 | BudgetTracker + run_wave_scheduled 波次调度（cfr-03/05/09/12, cfr2-03 允许变负+全部attempts, cfr2-07 fork判定, cfr4-01 make_request 携带 attempt+stream_log 不覆盖） | 已完成 | `python3 -m unittest harness.tests.test_fanout`：46 tests OK；全量：412 tests + 13 tests OK；正控将超额结算改回 `max` 后 2 个预算测试失败，仅按 retryable fork 后非 resumable 测试失败，复用 attempt 1 骨架后 stream_log 测试失败；恢复并清理 `__pycache__` 后全绿 | 无 |
| 5.5 | run_finders/judge_candidate 基于波次重写（cfr-06/07/14, cfr2-02 fingerprint task identity, cfr2-04 expected_tools, cfr3-01 RequestContext+联合测试, cfr4-01 oracle 订正为同源比对） | 已完成 | `python3 -m unittest harness.tests.test_fanout`：56 tests OK；全量：422 tests + 13 tests OK；正控禁用 redline 短路后目标测试触发非 redline 调用失败，删除 judge 顶层 degraded 汇入后降级测试失败，移除 fingerprint identity 后 2 个 identity 测试失败，固定 stream attempt=1 后重试日志测试失败；恢复并清理 `__pycache__` 后全绿 | 无 |
| 5.6 | run_fanout + FanoutSettlement 聚合（cfr-04/06, cfr2-03 基于全部attempts, cfr3-03 protocol_errors 补测试） | 已完成 | `python3 -m unittest harness.tests.test_fanout`：63 tests OK；全量：429 tests + 13 tests OK；正控改为只聚合每角色最终 attempt 后成本测试失败，删除 judge degraded 顶层汇入后端到端降级测试失败，删除 protocol_errors 聚合后来源测试失败；恢复并清理 `__pycache__` 后全绿 | 无 |
| 6.1 | round.py 接线 + 工具收窄 + 结算分支迁移 + cli.py 适配闭包（cfr-02/04/06/08/09/15, cfr2-01, cfr3-01 RequestContext 构造点, cfr3-03 detail/model 补测试） | 待开始 | | |
| 7.2 | 写继任测试（提前执行，冻结 canonical key 真值，cfr-18） | 待开始 | | |
| 7.1 | 删除 JS workflow/skill + 旧跨语言测试（同一提交，cfr-18） | 待开始 | | |
| 7.3 | 删除 degraded-dedup.test.mjs | 待开始 | | |
| 7.4 | redlines.yaml 说明更新 | 待开始 | | |
| 7.5 | spec.md/plan-stage1b.md 实现接缝同步修订（cfr-17，验收 oracle 改为定点检查 cfr2-10） | 待开始 | | |
| 8.1 | probe 真机复核 | 待开始 | | |
| 8.2 | 单角色真机冒烟 | 待开始 | | |
| 8.3 | 完整扇出真机跑通 | 待开始 | | |
| 8.4 | 故障注入真机验收（cfr-10：须真实创建 session 再中断；cfr2-06 探针专用 parser） | 待开始 | | |
