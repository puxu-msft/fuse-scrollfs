# 真机暴露后修复批次 · 合并态对抗性评审

> report_id: `rmf` · reviewed_at_rev: `b7a7f176a65d7fe24fbe7b6fb600bd98a086332d`（`9b498e9..HEAD`，12 提交）
> 评审者：独立 reviewer（Claude Opus 5）· 日期：2026-07-31
> 严重级别用本角色的四档 `blocker/major/minor/nit`，并给出与派活方三档（Critical/Important/Minor）的映射，不另立第五档。
> `blocker` ↔ Critical · `major` ↔ Important · `minor` ↔ Minor · `nit` 无对应，单列。

## 评审范围

只审 `9b498e9..HEAD` 引入的改动：`claude_runner.py`（env 前缀 deny + 认证白名单 + 规范模型 ID + `TaskOutput` 入 allowlist）、`round.py`（工具集单一真相源 + 外层钉 sonnet）、`.claude/skills/scrollz-round/SKILL.md`（`TaskOutput(block=true)`）、`.claude/harness-settings.json`、`.claude/workflows/scrollz-propose.js`（`safeAgent` 重试 3 次 + `recordDegraded` 折叠 + judge 短路 + 降级按否决）、`.claude/workflows/tests/degraded-dedup.test.mjs`。

同批未在范围内但被这批改动**变成承重路径**的既有代码（`round.py` 的 `invocation-failed → abandon()`、`cli.py` 的 `no-candidate → exit 0`），按合并态评审纳入，并在条目里显式标注「既有代码，被本批放大」。

## 总体 verdict

**存在 blocker（2 条）——不得进入 Task 13 第 6/7 步（装 systemd 单元、启 timer）。**

- blocker：2（`rmf-01`、`rmf-02`）
- major：7（`rmf-03`–`rmf-08`、`rmf-13`）
- minor：8（`rmf-09`–`rmf-12`、`rmf-14`–`rmf-17`）
- nit：1（`rmf-18`）
- 主观建议：3

> 计数更正说明：本节最初写于只评审 `9b498e9..b7a7f17` 时，当时为 2/3/6/1。评审过程中派活方陆续追加了 `b3fdf17`（stream 落盘）、`cb726b2`（systemd 产物入库）、`f50a403`/`340774d`（rmf-01/rmf-02 的修复）与一处未提交的工作区改动，评审范围随之扩大，条目相应增加。**上面的数字是当前生效值**，与文末交付声明的 `COUNTS` 一致。范围扩大后的最终评审基线：`340774d` + 工作区对 `round.py` / `test_round.py` 的未提交改动（该状态下 298 + 13 个测试实跑全绿）。

**关键结论一句话**：`rmf-01` 使「启用 timer」这个动作在当前配置下必然产生一串静默失败轮，且每轮按满额 $6 计入预算账本——它会**污染用户 2026-08-07 那次预算复核所依赖的数据**，而那次复核的观察终点（单日 > $80）恰好不会被 $72/日 的虚假花费触发。

## 双视角覆盖证据

**机械核对（做了哪些扫描/对账/查证）**

1. `git log --oneline 9b498e9..HEAD`、`git diff --stat`、逐文件 `git diff` 四份（settings / round.py / SKILL.md / scrollz-propose.js），确认改动面与派活描述一致，无夹带。
2. 全量跑测：`python3 -m unittest discover -s harness/tests -t .` → `Ran 13 tests OK` + `Ran 285 tests OK`；`node .claude/workflows/tests/degraded-dedup.test.mjs` → `PASS`。
3. `rg -n "degraded|rejected"` 全仓对账，确认 `degraded`/`rejected` 在 Python 侧的读取点数量为 **0**。
4. 查证 `TaskOutput` 的真实契约：从已安装的 `claude` 二进制（`/home/xp/.local/share/claude/versions/2.1.220`）中提取其 zod inputSchema 与 tool prompt 原文，核对 `block`/`timeout` 参数、`isReadOnly`、`maxResultSizeChars`、以及 task 解析走 session 内 `taskRegistry`。
5. 查证 systemd 前提：`systemctl --user show-environment`、`ls ~/.config/environment.d/`、`~/.config/scrollz-harness/env` 的**键名**（不读值）、`plan-stage1a.md:3921-3944` 的 unit 定义、`~/.claude/settings.json` 的 `env` 键名与 `model` 字段、项目侧 `.claude/settings.json` 与 `.claude/harness-settings.json` 的完整内容。
6. 对 `_sanitize_env()` 跑了一次**独立正控实验**（构造含 12 个变量的父环境，打印 SURVIVED/REMOVED 两个集合），而不是只读代码判断前缀覆盖面。
7. 交叉核对 `_capability_drift_problems` 的 expected 集合与 `STAGE1_ALLOWED_TOOLS`、`--tools` 实参、`harness-settings.json` 的 `permissions.allow` 三处是否自洽。

**第一人称执行（模拟了哪些流程/分支/用户路径）**

1. 扮演 `scrollz-round` 入口模型走一遍 SKILL.md：调 `Workflow` → 拿 run ID → `TaskOutput(block=true, timeout=600000)` → 未完成 → 再调 → 第三次……走到「超过三次为止」时**发现文档没告诉我该输出什么**，而最近的一条指令是「若 workflow 返回空数组，就输出空数组」。→ `rmf-05`。
2. 扮演 systemd 在 02:00 拉起 `scrollz-harness.service`：从 unit 的 `Environment=` + `EnvironmentFile=` 组装环境 → 进 `load_config()` → 进 `invoke()` → `_sanitize_env()` → 子进程手上一个认证变量都没有。→ `rmf-01`。
3. 扮演传输抖动期间的 workflow：4 finder 正常出候选 → redline judge 三次连撞 API Error → 3 个候选全被 `judge-unavailable` 否决 → 返回 `{candidates:[], rejected, degraded}` → 外层原样回显 → `round.py` 只取 `candidates` → `no-candidate` → `cli.py` return 0 → systemd 记成功。→ `rmf-03`。
4. 扮演 `safeAgent` 在全局抖动下的最坏路径：13 个 agent × 3 次尝试的成本累加，与 `--max-budget-usd 6.00` 的交点。→ `rmf-04`。
5. 扮演 Stage 1b 的实现者接线「拒绝记忆」：读 `rejected` 数组，发现短路后的记录只有 redline 单一视角，且降级记录缺 judge 专有字段。→ `rmf-07`、`rmf-08`。
6. 扮演一个 finder 返回超长 `body_md` 的候选，跟踪它穿过 workflow return → TaskOutput 的 100k 截断 → `_extract_payload`。→ `rmf-09`。

---

## 事实性发现

### rmf-01 — systemd 启动环境不提供任何模型认证通道，启用 timer 后每轮必然失败且按满额计费

```
finding_id: rmf-01
severity: blocker（派活方口径：Critical）
primary_location: .claude/scripts/harness/claude_runner.py:47-62（_INHERITED_AUTH_ENV 的前提声明）
related_locations: docs/harness/plan-stage1a.md:3923-3938（unit 定义，无 Environment=ANTHROPIC_*）；~/.config/scrollz-harness/env（EnvironmentFile，只有 GH_TOKEN / HARNESS_ROUND_BUDGET_USD / HARNESS_DAILY_BUDGET_USD）；.claude/scripts/harness/round.py:351-357（失败后走 abandon）；docs/harness/HANDOVER.md:113（Task 13 第 6/7 步）
evidence_status: verified
```

**问题** —— 代码注释写「生产环境由 systemd unit 的 EnvironmentFile 提供，而不是碰巧从某个交互式 shell 继承」。这句话**今天是假的**：真机上唯一跑通的那一轮，认证恰恰是从交互式 shell 继承的；按计划的 unit 装上去之后，子进程手上一个认证变量都没有。

**证据链（每一步都可复现）**

1. `_INHERITED_AUTH_ENV` 是**继承**通道，不是**提供**通道——它只放行父进程已有的四个变量，自己不设任何值（`claude_runner.py:57-62` + `_sanitize_env` 全文无对应赋值）。
2. `EnvironmentFile` 里没有它们。只读取键名（不读值）：`rg -o "^[A-Z_]+" ~/.config/scrollz-harness/env` → `GH_TOKEN` / `HARNESS_ROUND_BUDGET_USD` / `HARNESS_DAILY_BUDGET_USD`。**无 `ANTHROPIC_AUTH_TOKEN`、无 `ANTHROPIC_BASE_URL`、无 `CLAUDE_CODE_OAUTH_TOKEN`。**
3. systemd user manager 环境里也没有。`systemctl --user show-environment` 只有 `PATH` 与 `DBUS_SESSION_BUS_ADDRESS`；`~/.config/environment.d/` 不存在（`No such file or directory`），所以没有第三条注入路径。
4. unit 本身不补。`plan-stage1a.md:3928-3931` 只有 `Environment=PATH=…` 与 `Environment=GIT_TERMINAL_PROMPT=0`。
5. settings 通道被自己关掉了。`build_argv` 固定传 `--setting-sources project`，用户级 `~/.claude/settings.json` 不参与——而认证恰恰配在那里（该文件 `env` 块含 `ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_BASE_URL`、`ANTHROPIC_DEFAULT_SONNET_MODEL`；仅读键名，未读值）。项目侧两个 settings 均无 `env` 块：`.claude/settings.json` 只有 `permissions`，`.claude/harness-settings.json` 只有 `permissions` + `enableAllProjectMcpServers`。
6. **没有可用的凭据文件回落**——这一条是本条最强的证据，而且它来自项目自己的真机实测：`claude_runner.py:47-49` 记载「deny-by-default 会把它们一并清掉，子进程随即报 `Not logged in · Please run /login`（实测 2026-07-31，apiKeySource=none）」。`~/.claude/.credentials.json` 文件确实存在（`Mar 26` 修改），但**若它是可用的回落，清掉环境变量就不会报 `Not logged in`**。所以那次实测已经证伪了「凭据文件能兜底」。

**失败场景（逐步）** —— 执行 Task 13 第 6/7 步 → `systemctl --user enable --now scrollz-harness.timer` → 每 2 小时：`round` 起 → 预检通过（预检不查认证）→ `reserve()` 占 $6 → `invoke()` → 子进程 `Not logged in` → 无 `success` 终态 → `invocation.ok=False` → `round.py:352` **`budget.abandon()` 按预留满额计费 $6** → `record_outcome(result="invocation-failed")` → `cli.py:154` 返回 1 → systemd 记 failed，而 1a 没有 `OnFailure` 告警单元（属 1b）→ 无人知道。

**二阶伤害（这才是它是 blocker 的理由）** —— 12 轮/日 × $6 = **$72/日 的虚构花费写进 `budget_days`，真实模型花费 $0**。用户 2026-07-31 定的观察终点是「任何单日 > $80 立刻回来重估」——$72 **正好不触发**。于是 2026-08-07 那次「据 `budget_days` 实际花费定真实日上限」的复核，读到的是一串纯属虚构的数字，而它本该是这次观察期的唯一 oracle。

**修复建议**

1. （必须）把认证注入 `EnvironmentFile`：在 `~/.config/scrollz-harness/env` 补 `ANTHROPIC_AUTH_TOKEN=` 与 `ANTHROPIC_BASE_URL=`（该文件已是 `0600`，与 `GH_TOKEN` 同一信任级别）。这是让注释里那句话变成真的最小动作。
2. （必须）加一条**只读预检**：`precheck` 里断言「`_INHERITED_AUTH_ENV` 中至少一个已设置」，否则 fail closed 且**在 `reserve()` 之前**返回 `precheck-failed`（该路径走 `open_round_record(reserved_usd=0)`，不产生虚构占用）。当前预检完全不查认证，这是「离线测试系统性看不见」的又一实例——它属于「由谁启动我」那一类。
3. （建议）把 `~/.local/state/scrollz-harness/` 先建出来。unit 用 `StandardOutput=append:`，目录不存在时 systemd 会直接拒绝启动服务；`ls` 确认该目录**目前不存在**，而计划里建目录的 `mkdir -p` 与 `daemon-reload` 写在同一个代码块，容易被当成一条命令跳过。

### rmf-02 — 唯一的抗重复机制被硬编码为空集，且控制器已经拿到 `canonical_key` 却丢弃；同时**协调者对本轮的归因不成立**

```
finding_id: rmf-02
severity: blocker（派活方口径：Critical）
primary_location: .claude/scripts/harness/round.py:332-334（known_canonical_keys 恒为 []）
related_locations: .claude/workflows/scrollz-propose.js:184,207-216（seen 集合的唯一来源）；.claude/scripts/harness/round.py:47-49（canonical_key 被列为「可选字段」后即被丢弃）；.claude/scripts/harness/queue.py:28-30（proposals 只存 sha256[:32]，无法反推 canonical key）；.claude/scripts/harness/round.py:413-418（duplicate 分支）；.claude/scripts/harness/cli.py:147（duplicate → exit 0）
evidence_status: verified（机制）／unverified（复现频率，样本量 1）
```

**先证伪协调者的归因。** 协调者判断「本轮候选被 outbox 自然键去重短路，所以 `operations` 没有新行」。**这个归因不成立**，控制器根本没走到发布层：

- 只读查询 `.claude/state/harness.db`（`mode=ro`）：最新一轮 `round_id=5927bef456cd`，`started_at=2026-07-31T16:36:59`、`ended_at=16:53:52`、**`result='invocation-failed'`**、`turns=4`、`exit_code=0`、`reserved_usd=10.0`、`settled_usd=10.0`。
- `result='invocation-failed'` 只由 `round.py:351-357` 这一个分支写入，而该分支在 `deps.queue.classify()`（:413）与 `publisher.publish()`（:424）**之前** `return`。因此本轮既没走 `Queue.classify`，也没走 `outbox.prepare` 的自然键路径——`operations` 无新行的原因是**整条候选处理链从未被执行**，与去重无关。
- 佐证：`operations` 四行的 `round_id` 全部是 `85918d0c61fc`（即 16:10 那轮 `published`），没有任何一行属于 `5927bef456cd`；`HARNESS_FAULT` 注入点没被触发，也是同一个原因——注入点在发布层。
- `exit_code=0` 且 `turns=4` 说明子进程正常退出、终态 `result` 事件被解析到了，所以失败发生在**信封契约层**（`_extract_payload` 或 `protocol_errors`），不是超时、不是崩溃。真正的原因**现在已经取不到了**，见 `rmf-04`。

**这不改变问题的严重性，只改变它的位置。** 协调者观察到的现象（workflow 又产出了同一个候选）是真的，而且它指向一个更靠前的缺陷：

1. `round.py:333` 把 `known_canonical_keys` 写死成 `[]`。这是 workflow 里 `seen` 集合的**唯一**外部来源（`scrollz-propose.js:184`）。传空 = 跨轮去重整个关掉，每一轮的 `seen` 都从零开始。
2. 兜底确实在：若同一候选再次胜出，`Queue.classify()` 会按 `fingerprint` 命中 `proposals` 里那一行（实测该表**恰好 1 行**：`cb5b8798ad0c58599cb9dcb0ef899a85` / `state='proposed'` / `issue_number=1`）返回 `exact_duplicate` → `result='duplicate'` → `cli.py:147` **返回 0**。
3. 于是最坏形态是：**每 2 小时花掉一整轮的钱（实测 $5.57）跑完 4 finder + 若干 judge，最后在最后一步被丢弃，退出码 0，systemd 记成功，账本记 `duplicate`，仓库零产出——而且没有任何机制让下一轮的结果不一样。**
4. 控制器**已经收到了修好它所需的数据却扔掉**：`pickCandidateFields()` 输出含 `canonical_key`，`round.py:48` 把它列进 `_OPTIONAL_CANDIDATE_FIELDS`（即「放行但不用」），随后再没有任何读取点。而 `proposals` 表只存 `sha256[:32]` 摘要（`queue.py:28-30`），摘要**不可逆**，所以不补存就永远反推不出 canonical key——这正是它被冻到 1b 的技术原因，但那个原因只有三行代码的厚度。

**证据强度的诚实划分**：「机制被关掉、canonical_key 被丢弃、摘要不可反推」三条是 `verified`（读代码 + 查表结构可复算）。「会**每轮**重复提同一候选」是 `unverified`——我只有 1 个已发布候选样本，协调者观察到的第二次复现是**弱正证据**（1 个样本），不足以支撑频率结论。但**动作不需要它承重**：无论复现率是 100% 还是 20%，「唯一的抗重复机制处于关闭状态」本身就足以证成修复。

**修复建议（1a 范围内，代价最小，且是补齐不是砍）**

- 新增一张**纯追加**的表（不改任何既有表定义，符合 HANDOVER 记的 db 不变量）：
  ```sql
  CREATE TABLE IF NOT EXISTS proposal_keys(
      fingerprint TEXT PRIMARY KEY,
      canonical_key TEXT NOT NULL,
      created_at REAL NOT NULL);
  ```
- `round.py` 在 `publisher.publish()` 之前写入 `candidate["canonical_key"]`（缺失则跳过，不阻断）；下一轮拼 prompt 时用
  `SELECT canonical_key FROM proposal_keys JOIN proposals USING(fingerprint) WHERE proposals.state IN ('proposed', …)`
  填 `known_canonical_keys`。约 25 行 + 2 个测试。
- 效果可证伪：同一候选再次出现时，workflow 在 `seen.has(key)` 处丢弃它并**顺位裁决下一个候选**（`scrollz-propose.js:207-216` 之后 `ranked` 仍有其余项），因此活性恢复——这一点我用真实 workflow 文件跑过（见 `rmf-10` 的 PoC 装置），不是纸面推断。
- **明确不建议**的做法：把 `duplicate` 改成非 0 退出码。那只是把静默换成噪声，不解决「提不出新东西」。
- 顺带（可延后，不阻塞）：把已提案的 `title` 列表也传给 finder 提示词，让重复候选在**产生前**就被避免，省掉那部分 token。这属于优化，不属于修复。

### rmf-03 — 本批把「响亮的失败」改成了「安静的成功」：全降级轮退出 0，且降级证据在最可能发生的路径上被整个丢弃

```
finding_id: rmf-03
severity: major（派活方口径：Important）
primary_location: .claude/workflows/scrollz-propose.js:218-220（早退路径不带 degraded）
related_locations: .claude/workflows/scrollz-propose.js:305-312（另两个 return 带 degraded）；.claude/scripts/harness/round.py:371（只读 candidates，degraded/rejected 无任何读取点）；.claude/scripts/harness/cli.py:147（no-candidate → return 0）
evidence_status: verified
```

**问题** —— `recordDegraded()` 的计数没有任何观测价值，而且比「死代码」更糟：修复前一个 agent 的传输故障会让整轮 `aborted` → `invocation-failed` → **exit 1**，systemd 看得见；修复后同样的故障变成 `candidates: []` → `no-candidate` → **exit 0**，systemd 记成功。

**反例（已实际执行，不是推演）** —— 我写了一个装置，用 `new Function('agent','parallel','args', …)` 注入 globals 后**执行真实的 `scrollz-propose.js`**（把首行 `export const meta =` 改成 `const meta =`），不复制任何源码：

- 场景 A「4 个 finder 全部持续传输故障」：实际发生 **12 次** agent 尝试（4×3），返回值是
  `{"candidates":[],"reason":"no-candidate-after-dedupe"}` —— **`degraded` 完全不在返回值里**。12 次失败的记录在 `deduped.length === 0` 的早退处（:218-220）被整个扔掉。
- 场景 B「finder 正常、redline judge 持续故障」：7 次 agent 调用，返回
  `{"candidates":[], "rejected":[…1 条…], "degraded":[{"agentType":"harness-judge-redline","occurrences":1,"attempts":3,…}]}`。
  `degraded` 在这里确实带出来了——**然后被 Python 侧丢掉**：`round.py:371` 只做 `invocation.payload.get("candidates", [])`，全仓 `rg "degraded"` 在 `.claude/scripts/` 下**零命中**。

于是两条路径殊途同归：一轮花了真金白银、4 个 finder 产出了候选、只是裁决通道挂了，最终对外表现为 `{"result":"no-candidate"}` + exit 0，与「仓库里确实没东西可提」**完全不可区分**。2 小时节拍下这可以持续一整天而无人察觉。

**修复建议**

1. `scrollz-propose.js:219` 的早退补上 `degraded`（并顺手补 `rejected: []`，让三个 return 点形状一致）。这是 1 行。
2. `round.py` 读 `invocation.payload.get("degraded")`：非空时
   （a）写进返回值 `detail`，让 `round.log` 里看得见；
   （b）**结果码单独分型**：新增 `result="degraded-no-candidate"`（与真正的 `no-candidate` 区分），`cli.py` 对它返回非 0。降级不是正常的空扫描，不能共用同一个成功谓词。
3. 同理把 `rejected` 的条数也带进 `detail`——「3 个候选全被否决」与「finder 一个候选都没找到」是两种完全不同的健康状态，现在都塌缩成 `no-candidate`。

### rmf-04 — `b3fdf17` 只关闭了判因链的一半：取证材料有了，解析器算出的结论仍然没有落任何盘

```
finding_id: rmf-04
severity: major（派活方口径：Important）
primary_location: .claude/scripts/harness/round.py:356-357（detail 只有 raw_tail，protocol_errors 被丢弃）
related_locations: .claude/scripts/harness/claude_runner.py:138（protocol_errors 字段本身）；.claude/scripts/harness/cli.py:126-129（probe 路径**打印**了 protocol_errors，两条路径不一致）；rounds 表 schema（实测列为 round_id/mode/started_at/ended_at/reserved_usd/settled_usd/turns/denials/result/exit_code —— **无 protocol_errors**）
evidence_status: verified
```

**问题** —— `parse_stream_json()` 已经把失败原因精确算出来了（`missing init event` / `duplicate terminal result events` / `unparseable stream line` / `unparseable or malformed payload in success result` 四类），然后 `round.py` 在返回时只带 `raw_tail`（末 5 行），把结论扔了。`b3fdf17` 补的是**原材料**（完整 stream 落盘），不是**结论**。所以下一次复现时，接手者仍然要拿着 stream 文件**手工重算一遍解析器刚刚算过的东西**。

对照证据：`cli.py` 的 `probe` 路径把 `res.protocol_errors` 明确打进输出（:128），`round` 路径没有。同一份数据、两条路径、两种待遇——这本身就是漂移。

**这次事故的实测代价** —— `round_id=5927bef456cd` 花了 $10、跑了 16m53s、`exit_code=0`、`turns=4`，然后：`~/.local/state/scrollz-harness/` **目录当时不存在**（`ls` 确认），手工运行的 stdout 随终端一起消失，`rounds` 表没有原因列，`raw_tail` 没有被持久化到任何地方。**结果是协调者只能猜，而且猜错了**（见 `rmf-02`）。这是「不可观测」造成真实误判的一个完整闭环样本。

**判因步骤（写给接手者，下次复现直接照做）**

前提：`b3fdf17` 之后每轮的完整 stream 落在 `.claude/state/rounds/<round_id>.jsonl`（该目录**目前尚不存在**——它由第一次 `invoke()` 的 `mkdir(parents=True)` 创建，所以在下一轮跑完之前不要指望它）。stderr 被追加在文件里 `===== stderr =====` 分隔线之后。

```bash
cd /home/xp/src/zipfs
# 0. 找到失败轮
python3 -c "import sqlite3;c=sqlite3.connect('file:.claude/state/harness.db?mode=ro',uri=True);\
print([dict(zip([d[0] for d in c.execute('select * from rounds limit 0').description], r)) \
for r in c.execute(\"select * from rounds where result='invocation-failed' order by started_at desc limit 3\")])"
F=.claude/state/rounds/<round_id>.jsonl
```

1. **先判信封完整性**（对应 `missing/duplicate init|result`）：
   `rg -c '"subtype":"init"' $F` 应为 1；`rg -c '"type":"result"' $F` 应为 1。任一 ≠ 1 → 结论即为 `duplicate/missing …`，到此为止。
2. **再判终态类型**：取第一个 `"type":"result"` 事件，看 `subtype`。
   - `subtype != "success"`（`error_max_turns` / `error_max_budget_usd` / `error_during_execution`）→ **不是** `_extract_payload` 的问题，是模型侧终态失败。同时读 `total_cost_usd` 与 `num_turns` 判断是不是撞了预算/回合上限。
   - `subtype == "success"` → 进第 3 步。
3. **判信封契约**（对应 `unparseable or malformed payload`）：把该事件的 `result` 字段原文喂回同一份代码复算，别用肉眼比对：
   ```bash
   cd .claude/scripts && python3 -c "
   import json,sys
   from harness.claude_runner import _extract_payload
   ev=[json.loads(l) for l in open('$F') if '\"type\":\"result\"' in l][0]
   print(repr(_extract_payload(ev['result'])))"
   ```
   返回 `None` 即命中，再按 `_extract_payload` 的三条拒绝规则定位是哪一条：（a）闭合 fence 之后还有解释文字；（b）剥壳后 `json.loads` 失败（**截断**是典型，见 `rmf-14`）；（c）顶层不是 dict 或 `candidates` 不是 list。
4. **查非法行**（对应 `unparseable stream line`）：
   `while IFS= read -r l; do printf '%s' "$l" | python3 -m json.tool >/dev/null 2>&1 || echo "BAD: ${l:0:200}"; done < $F`
5. 以上全过而仍失败 → 看 `rounds.exit_code` 与文件里 `===== stderr =====` 之后那一段。

**修复建议** —— 把第 1/2/4 步永久消掉：`round.py:356` 改成
`"detail": "; ".join(invocation.protocol_errors) or invocation.raw_tail`（或两者都带）。一行，`round.log` 里直接就有答案。同时 `capability-drift`/`invalid-candidate` 分支也应带上 `stream_log` 路径，让日志自带取证入口。

### rmf-05 — 成本已知时仍按预留满额计费，预算账本今日已有约 $32.5 是虚构的（实测数据）

```
finding_id: rmf-05
severity: major（派活方口径：Important）
primary_location: .claude/scripts/harness/round.py:352（invocation-failed → budget.abandon）
related_locations: .claude/scripts/harness/round.py:364（capability-drift 同样 abandon）；.claude/scripts/harness/budget.py:129-136（abandon 按 reserved_usd 全额）；.claude/scripts/harness/round.py:348-349（此前已把真实 cost 存进 progress）；docs/harness/HANDOVER.md 预算观察期段落
evidence_status: verified
```

**问题** —— `abandon()` 的语义是「结果未知按最坏值计费」，这在成本**真的**未知时是对的。但 `invocation-failed` 分支里成本往往**是已知的**：`_parse_terminal_result()` 对非 `success` 的终态事件同样解析并返回 `total_cost_usd`/`num_turns`（`claude_runner.py:252-258` 明确写了「cost/turns 校验独立于 subtype」），代码自己也在前两行把它存进了 `progress["cost_known"]=True; progress["cost"]=invocation.cost_usd`——然后紧接着的分支不用它。

**实测证据（只读查 `.claude/state/harness.db`）**

| round_id | result | turns | exit_code | reserved | settled |
|---|---|---|---|---|---|
| 5927bef456cd | invocation-failed | 4 | 0 | 10.00 | **10.00** |
| 9ea2d3089e32 | invocation-failed | 3 | 1 | 6.00 | **6.00** |
| 85918d0c61fc | published | 3 | 0 | 6.00 | 5.5748286 |
| 9b3c39f0fa21 | invocation-failed | 2 | 1 | 6.00 | **6.00** |
| d19c9a99cca1 | invocation-failed | 2 | 1 | 3.00 | **3.00** |
| 01a6c2fe7880 / a257029d3ae6 / f4f16254eb31 / 55c9d0bbe0c8 | invocation-failed | 1–4 | 0/1 | 1.50 | **1.50** ×4 |
| 9abdfbf27435 | unhandled-exception | — | — | 1.50 | **1.50** |

`budget_days` 今日：`settled_usd = 41.0748286`。其中**唯一一个由实测成本得来的数字是 $5.5748286**，其余约 **$35.5 全部是 `reserved_usd` 的原样回填**。而每一行的 `turns` 都非 NULL（1–4），说明终态 result 事件都被解析到了，**成本当时是已知的**。

**为什么这是 major 而不是 minor** —— 它与 `rmf-01` 复合：用户 2026-08-07 那次复核的判据原文是「复核 `budget_days` 实际花费，据此定真实日上限」，触发条件是「任何单日 > $80」。今天的 `$41.07` 里已经有 86% 是虚构的。若 `rmf-01` 未修，这个比例会变成 100%。**一个观察期的 oracle 被系统性偏置，而偏置方向恰好是「看起来花得比实际多」——它会让人把日上限定得过高，正好是这次观察想避免的错误方向。**

**修复建议** —— 在 `invocation-failed` / `capability-drift` 分支按「是否解析到终态 result」二分：
```python
if invocation.turns or invocation.cost_usd:   # 终态事件已解析，成本可信
    budget.settle(round_id, day, invocation.cost_usd)
else:                                          # 连 result 事件都没有，才 abandon
    budget.abandon(round_id, day)
```
更干净的形态是给 `InvocationResult` 加一个显式的 `terminal_result_seen: bool`（`parse_stream_json` 里 `result_count >= 1` 时置位），别用 `turns or cost` 这种间接推断——间接推断在 `turns=0 且 cost=0` 的合法终态上会误判。**注意这条不是「放宽预算保护」**：`settle()` 在 `actual > reserved` 时仍会记 `budget_breach` 并足额入账，保护面不变，只是不再把已知的 $0.30 记成 $10。

### rmf-06 — env「deny-by-default」名不副实：它是前缀级的，而 Claude Code 的环境变量面并不被这两个前缀覆盖

```
finding_id: rmf-06
severity: major（派活方口径：Important）
primary_location: .claude/scripts/harness/claude_runner.py:347-351（前缀过滤）
related_locations: .claude/scripts/harness/claude_runner.py:341-346（注释里声明的不变量）；.claude/scripts/harness/config.py（无 env allowlist）
evidence_status: verified
```

**问题** —— 注释立的不变量是「无人值守 agent 拿到什么模型、有什么运行时能力，**不能取决于谁启动了它**」。实现只删 `ANTHROPIC_*` 与 `CLAUDE_*` 两个前缀。Claude Code 读的环境变量**有相当一部分不在这两个前缀里**，它们全部穿透。

**反例（跑了正控实验，不是读代码推断）** —— 构造一个含 12 个变量的父环境喂给 `_sanitize_env()`，打印 SURVIVED / REMOVED 两个集合：

```
SURVIVED: ANTHROPIC_AUTH_TOKEN, ANTHROPIC_BASE_URL, API_TIMEOUT_MS,
          BASH_DEFAULT_TIMEOUT_MS, CLAUDECODE, DISABLE_TELEMETRY, HOME,
          HTTPS_PROXY, MAX_THINKING_TOKENS, MCP_CONNECTION_NONBLOCKING,
          PATH, USE_BUILTIN_RIPGREP
REMOVED : ANTHROPIC_MODEL, CLAUDE_CODE_ENABLE_TASKS
```

两条独立佐证，避免「变量能穿透」被误当成「变量无害」：

1. 这些名字**确实被 claude 读取**——在已安装的二进制里检索到字面量 `API_TIMEOUT_MS`、`MAX_THINKING_TOKENS`、`USE_BUILTIN_RIPGREP`，以及 `CLAUDECODE`（21 处）。
2. 这些名字**此刻就设在真实启动环境里**——`env` 实测当前会话已导出 `CLAUDECODE`、`API_TIMEOUT_MS`、`BASH_DEFAULT_TIMEOUT_MS`、`BASH_MAX_TIMEOUT_MS`、`DISABLE_TELEMETRY`、`DISABLE_ERROR_REPORTING`、`DISABLE_FEEDBACK_COMMAND`、`MCP_CONNECTION_NONBLOCKING` 共 8 个。而 Task 13 的第 1–5 步**正是从这样一个会话里手工执行的**，所以那几轮真机数据是在被污染的环境下取得的。

最讽刺的一个是 **`CLAUDECODE`**：它差一个下划线就落在 `CLAUDE_` 前缀里。`"CLAUDECODE".startswith("CLAUDE_")` 是 `False`，于是这个「我正跑在 Claude Code 里」的标志位原样透传给了 headless 子进程。这正是本批修复想根除的那一类「取决于谁启动了我」。

**为什么这不是「已经够好了」** —— 本模块对**工具**用的是 allowlist（`_validate_tools` 要求恰好相等，多一个少一个都拒），对**环境**却用 blocklist。两种形态放在同一个文件里，而环境面恰恰是本批五个真机缺陷中两个的成因。

**修复建议** —— 把 `_sanitize_env` 反转成 allowlist：从空 dict 出发，显式拷入 `HOME`/`PATH`/`LANG`/`LC_*`/`TZ`/`USER`/`LOGNAME`/`SHELL`/`TMPDIR` + `_INHERITED_AUTH_ENV` 四项 + 本模块自设的两项，其余一律不带。代理变量（`HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`）若确实需要就显式列进白名单——**显式列出与碰巧继承，在可审计性上不是一回事**。这个改法的正控很好写：把上面那 12 个变量的实验反过来断言 SURVIVED 集合恰好等于白名单。

### rmf-07 — `safeAgent` 的重试没有预算感知、没有退避、不区分错误可重试性，最坏情况把「降级出提案」变成「烧穿预算零产出」

```
finding_id: rmf-07
severity: major（派活方口径：Important）
primary_location: .claude/workflows/scrollz-propose.js:142-153（safeAgent）
related_locations: .claude/workflows/scrollz-propose.js:107（MAX_AGENT_ATTEMPTS=3）；.claude/scripts/harness/round.py:332-334（args 未传预算信息）；.claude/scripts/harness/claude_runner.py:169（--max-budget-usd）
evidence_status: verified（调用次数与缺失的机制）／unverified（失败调用的计费比例）
```

**问题** —— 修复的动机完全成立（不能让一个 agent 的传输故障作废整轮），但 `MAX_AGENT_ATTEMPTS = 3` 是个**无条件的固定次数**：不看剩余预算、不退避、不区分错误类型。

**最坏路径的量级（调用次数已实测）** —— 用真实 workflow 跑「4 finder 全故障」场景，实际发生 **12 次** agent 尝试。全链最坏是 4 finder + 3 候选 × 3 judge = 13 个 agent，×3 = **39 次调用**。契约探针实测单 agent（仅回显一个字符串、opus 档、cacheRead 249k）花 $0.9985；钉 sonnet 后按实测整轮 $5.45/13 个 agent ≈ $0.42。**关键在于：`API Error: Server error mid-response` 是「响应中途」失败，输入侧的 token 已经付过了**，所以失败尝试的成本接近成功尝试。39 × $0.42 ≈ $16，远超每轮 $6 的 `--max-budget-usd`。

于是抖动期的实际结局是：撞上 `--max-budget-usd` → 外层会话被终止 → 无 `success` 终态 → `invocation-failed` → `abandon()` 满额计费。**修复前是「一个 finder 挂掉、整轮作废」，修复后在持续抖动下变成「烧满预算、整轮作废」——同样作废，但多花了钱。** 只有在抖动是**短暂**的情况下重试才是净收益，而恰恰这一点没有被机制保证：重试之间**零退避**，3 次尝试可能全部落在同一个几秒的故障窗口内。注释写「传输层故障会连续多次出现，一次重试挡不住一段抖动」——这句话是对的，但它恰恰论证了**需要退避**，而不是论证「立刻连试三次」。

诚实标注：「失败调用按接近成功调用计费」是我按 `mid-response` 语义与 cacheRead 结构推断的，**没有实测账单可以证实**（`invocations` 表实测为空，`record_invocation` 在生产路径上从未被调用）。因此这条的成本量级是 `unverified`；但「无预算感知、无退避、不分错误类型」三点是读代码即可确认的 `verified`。

**修复建议（补齐，不是砍）**

1. **传预算进 workflow**：`round.py` 拼 prompt 时把 `grant_usd` 一并放进 args；`safeAgent` 在每次重试前检查「已降级的 agent 数 × 估计单价」是否逼近上限，逼近则把后续 `MAX_AGENT_ATTEMPTS` 降为 1。这是让重试**有界于钱**而不是有界于次数。
2. **加退避**。若 Workflow 运行时不提供 `setTimeout`（脚本头注明「无 `Date.now()`/`Math.random()`」，定时器可用性未知），退而求其次：把重试**穿插**到别的 agent 之间（先把所有 agent 各试一遍，失败的收集起来第二轮再试），天然获得数十秒级的间隔而不需要任何定时器。这个改法同时更省——第二轮只重试真正失败的那些。
3. **区分可重试性**：`agentType` 不存在、schema 结构性不匹配这类**确定性**错误重试 3 次无意义。判据不必复杂——`normalizeError` 的结果里不含 `API Error` / `overloaded` / `timeout` / `ECONN` 等传输特征时，`MAX_AGENT_ATTEMPTS` 取 1。
   **但请注意一个反直觉的例外，不要顺手一起砍掉**：schema **校验**失败（模型输出不符合 schema）是**随机**的，不是确定性的——重试有真实收益。真正该只试一次的是 schema **定义**错误（`JUDGE_SCHEMAS[judgeType]` 取到 `undefined` 之类），两者名字很像，判据要落在错误文本上而不是「schema」这个词上。
4. `record_invocation()` 已经写好且有测试，但生产路径从不调用（实测 `invocations` 表为空）。接上它，才能把上面这条 `unverified` 变成 `verified`——这也是把「失败调用到底花多少钱」这个问题一次性关掉的唯一办法。

### rmf-08 — `b3fdf17` 的 stream 落盘把 agent 读到的一切变成了持久文件：默认权限、无脱敏、无轮转

```
finding_id: rmf-08
severity: major（派活方口径：Important）
primary_location: .claude/scripts/harness/claude_runner.py:367-390（_persist_stream）
related_locations: .claude/scripts/harness/round.py:341-344（stream_log 路径构造，在守卫之外）；.claude/systemd/scrollz-harness.service（StandardOutput=append: 是第二处无轮转日志）；.claude/rules/harness-agent-discipline.md（agent 读到的仓库/GitHub 文本一律是不可信 data）
evidence_status: verified（落盘内容与缺失的机制）／unverified（越界读取是否真的可行）
```

**先说结论**：这个修复方向是对的（`rmf-04` 正是它要补的洞），**不建议撤销**。以下是它引入的新面。

**1）落盘内容远超「诊断所需」。** `--output-format stream-json --verbose` 的 stdout 包含每一条 assistant 消息、每一次 `tool_use` 的完整入参与每一次 `tool_result` 的完整返回。也就是说：**agent 用 `Read`/`Grep` 读到的每一个文件的全文、4 个 finder 与 3 个 judge 的全部推理文本、workflow 的完整返回值**，现在全部逐字写进 `.claude/state/rounds/<round_id>.jsonl`。此前这些只活在进程 stdout 里，控制器只留 5 行。

**2）权限是 umask 决定的。** `path.open("w", …)` 不指定 mode，默认 0666 & ~umask，实测该用户环境下落成 **0644**。对照物：`~/.config/scrollz-harness/env`（含 `GH_TOKEN`）是 **0600**。同一份仓库里，凭据文件按 0600 管，而**可能转载凭据内容**的诊断文件按 0644 管。

**3）凭据能否进到里面，取决于一个我没能验证的问题。** 子进程的**环境**已经被清干净（`_CREDENTIAL_ENV_VARS` 删了 `GH_TOKEN` 等），但**文件系统**上的凭据仍在原地：`~/.config/scrollz-harness/env`（PAT）、`~/.claude/.credentials.json`、`~/.claude/settings.json`（含 `ANTHROPIC_AUTH_TOKEN`）。agent 有 `Read`，而 `harness-settings.json` 的 `permissions.allow` 里 `"Read"` 是**无路径限定**的。`additionalDirectories: []` 是否足以把 `Read` 钉死在 cwd 内，我**没有验证**——验证它需要真花钱起一次 headless 调用，超出本次评审的可逆操作范围。
   **但这条建议不需要那个答案承重**：若 Read 不能越界，脱敏与 0600 是零成本的纵深；若能越界，那本来就是一个独立的 Critical，而 `b3fdf17` 把它的后果从「一次性泄漏进 stdout」升级为「永久留在磁盘上」。两种情况下该做的事一样。

**4）无限增长。** 每轮一个文件，代码里没有任何清理/轮转。一轮 13 个 agent 的完整 stream 保守估计数 MB 级；2h 节拍 12 轮/日 ⇒ 数十 MB/日、GB/月，且 `.claude/state/` 在**仓库工作树内**（已确认被 `.gitignore` 的 `/.claude/state/` 覆盖，`git check-ignore -v` 命中，所以不会误提交；但强制清理未跟踪文件的操作会一并删掉，这在共用工作树里是真实风险）。同一台机上还有第二处无轮转日志：unit 的 `StandardOutput=append:` 指向的 `round.log`。

**5）落盘时机是「事后一次性」而非流式。** `capture_output=True` 全缓冲，文件在子进程结束**之后**才写。超时路径已经正确覆盖（`exc.output`/`exc.stderr`，还处理了 bytes 分支——这一点做得对）。但若 python 进程自身被 SIGKILL（systemd `TimeoutStartSec=1500` 兜底硬杀），**什么都不会留下**，而那正是最需要取证的情形。当前超时分层让这条路径很窄（见 `rmf-15`），所以只作提示。

**修复建议**

1. `path.open("w")` → 先 `os.open(path, O_WRONLY|O_CREAT|O_TRUNC, 0o600)`（或写完 `path.chmod(0o600)`）。一行，零争议。
2. 加一遍**落盘前脱敏**：对已知形状做正则替换（`ghp_[A-Za-z0-9]{36,}`、`github_pat_[A-Za-z0-9_]{50,}`、`sk-ant-[A-Za-z0-9_-]{20,}`、`Bearer\s+\S+`、`ANTHROPIC_AUTH_TOKEN=\S+`）。这与 `gitops.py` 已有的 token 脱敏是同一件事，形态可以直接复用——**别新造一套**。
3. 保留策略：只留最近 N 轮（N=50 起步）或 M 天，在 `_persist_stream` 成功后顺手清理同目录的旧文件；`round.log` 交给 `logrotate` 或改用 journal（systemd user 单元默认就有 journal，`StandardOutput=append:` 反而是主动放弃了自带轮转的那条路）。
4. `except OSError` 覆盖不全其声明的不变量（「落盘失败不得影响本轮结论」）：`stream_log` 传成非路径类型是 `TypeError`，编码异常是 `ValueError`。更要紧的是——**路径构造 `cfg.state_db.parent / "rounds" / …` 在 `round.py:344` 求值，位于守卫之外**：`state_db` 若不是 `Path` 就直接 `AttributeError` 打死整轮。提交信息自己记了「test_round 的 Cfg 假件缺 state_db，加 stream_log 后 24 个测试同时 AttributeError」——那正是这条通路在生产侧的同一形状。建议把路径构造挪进 `_persist_stream` 内部（传 `state_dir` + `round_id`），并把 `except` 放宽到 `Exception`。

### rmf-09 — SKILL.md 的「等到第三次就放弃」分支没有定义输出，而最近的一条指令把模型推向输出空数组；且「三次」在结构上不可达

```
finding_id: rmf-09
severity: minor
primary_location: .claude/skills/scrollz-round/SKILL.md（步骤 2 末句「直到拿到结果或超过三次为止」）
related_locations: 同文件「不要在没有 workflow 结果时编造候选。若 workflow 返回空数组，就输出空数组」；.claude/scripts/harness/round.py:24,29,323-330（ROUND_DEADLINE_S / CLEANUP_RESERVE_S / timeout_s）；.claude/scripts/harness/claude_runner.py:32（BG_WAIT_CEILING_MS）
evidence_status: verified
```

**问题（第一人称走出来的）** —— 我按 SKILL.md 扮演入口模型走一遍：调 `Workflow` → 拿 ID → `TaskOutput(block=true, timeout=600000)` → 未完成 → 再调 → 第三次仍未完成 →「超过三次为止」。**然后呢？文档没写。** 而距离最近的一条相关指令是「若 workflow 返回空数组，就输出空数组」，加上步骤 3「把返回值原样作为最后一条消息输出」。一个尽责的模型在这里最可能做的事，就是输出 `{"candidates": []}`——它能被 `_extract_payload` 正常接受，于是控制器记 `no-candidate`、`cli.py` 返回 0。**一次「等不到结果」被上报成「仓库里没东西可提」**，与 `rmf-03` 是同一类塌缩，只是发生在模型侧。

**「三次」在结构上到不了** —— `TaskOutput` 的 `timeout` 上限实测为 **600000 ms**（从已安装二进制里取到的 zod inputSchema 原文：`timeout: v.number().min(0).max(600000).default(30000)`，`block: v.boolean().default(true)`）。而外层子进程的 `timeout_s = ROUND_DEADLINE_S(1200) − elapsed − CLEANUP_RESERVE_S(60) ≤ 1140s`。两次 600s 已经是 1200s > 1140s——**第二次调用就会被 `subprocess.run` 的超时先杀掉**，第三次永远不会发生。文档写的上限与运行时的真实上限差了一倍。

**修复建议**

1. 给放弃分支一个**不可与正常空结果混淆**的输出，例如要求模型输出
   `{"candidates": [], "wait_timeout": true}`（`_extract_payload` 会放行顶层多余字段），`round.py` 见到 `wait_timeout` 即记 `result="wait-timeout"` 并返回非 0。
2. 把「三次」改成「两次」，或直接改成「重复调用直到本回合被终止为止，绝不因为等不到而自行收尾」——后者更稳，因为真实上限由外层超时执行，不需要模型自己数数。
3. 顺带消掉一个第二真相源：`BG_WAIT_CEILING_MS = 1_200_000` 与 `ROUND_DEADLINE_S = 1200` 是同一个数被写了两遍（一个毫秒一个秒）。注释要求「BG ceiling 必须大于单次 invocation 的 timeout_s」，当前 1200000 > 1140000 成立，但这个成立依赖 `CLEANUP_RESERVE_S=60` 恰好为正。把 BG ceiling 表达成 `ROUND_DEADLINE_S * 1000` 就永远成立——这与本批 `STAGE1_TOOLS` 的单一真相源改造是同一个动作。

### rmf-10 — `normalizeError` 的折叠**既过度又不足**，而验收测试只覆盖了那个恰好会通过的 ID 格式

```
finding_id: rmf-10
severity: minor
primary_location: .claude/workflows/scrollz-propose.js:113-122（normalizeError）
related_locations: .claude/workflows/tests/degraded-dedup.test.mjs:61-63（只用 req_<hex> 一种格式）
evidence_status: verified（四组反例均在 node 上跑过）
```

**不足（该折叠的没折叠）** —— 两条正则只认「≥10 位纯数字」与「≥8 位纯 `[0-9a-f]`」。实际的请求/追踪 ID 格式里，**只有裸 hex 命中**。实测：

| 输入 | 规范化结果 | 两条是否折叠 |
|---|---|---|
| `…(trace 9f3a2b7c-1d4e-4f8a-9b2c-1234567890ab)` | `…(trace <id>-1d4e-4f8a-9b2c-<ts>ab)` | **否** |
| `…(trace 0c8d51ea-7b62-4a19-8e30-0987654321fe)` | `…(trace <id>-7b62-4a19-8e30-<ts>fe)` | |
| `…(req 01JQZ8XK3MPQR7VN2WT4YB6HDG)`（ULID） | 原样 | **否** |
| `…(id Zx9Kq2LmPw==)`（base64） | 原样 | **否** |

**UUID 是最常见的 trace ID 形式，而它恰恰不折叠**：中间三段 4 字符的分组长度不足 8，原样留下，两个不同 UUID 于是产生两条 `degraded` 记录——正是这个函数要消灭的现象。注释里举的例子 `req_9f3a2b7c` 能过，是因为它正好是裸 hex。

**过度（不该折叠的折叠了）** —— 末尾 `.slice(0, 300)` 会把**任何共享前 300 字符的两个不同错误**并成一条。实测：两条 zod 校验错误共享一段 304 字符的样板前缀、后半段分别是 `MISSING body_md on candidate 1` 与 `MISSING slug on candidate 2`，规范化后**完全相等**（`true`）。zod 的多字段报错正是这种「长样板前缀 + 尾部差异」的形状，这不是构造出来的极端输入。

**第三个洞**：非 `Error` 抛出物走 `String(err)` → `[object Object]`。实测 `{code:'ECONNRESET'}` 与 `{code:'ETIMEDOUT'}` 都规范化成 `[object Object]`，全部并成一条。

**验收测试为什么没发现** —— 测试的三条样本全是 `req_9f3a2b7c` / `req_11ee44aa99` / `req_deadbeef42` 形式，**即实现恰好能处理的那一种**。正控做了（同类确实折叠成 `occurrences:3`），但没有任何一条负控去问「别的 ID 格式会怎样」。这是「oracle 与断言不是同一件事」的一个标准形态：断言是「同类样板错误被折叠」，检查的是「hex 形式的同类错误被折叠」。

**修复建议**

1. 加一条覆盖面更广的 ID 正则，放在现有两条之后：`/\b[0-9A-Za-z][0-9A-Za-z_-]{7,}\b/g` 太宽会吃掉正常单词，更稳的是**按已知格式各来一条**：UUID（`/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi`，且**必须放在裸 hex 规则之前**，否则第一段先被吃掉就匹配不上了）、ULID（`/\b[0-9A-HJKMNP-TV-Z]{26}\b/g`）、`req_\S+`、`trace[-_]?id[=: ]\S+`。
2. 截断改成「保留头 200 + 尾 100」而不是纯截头，尾部差异就不会丢。
3. `String((err && err.message) || err)` 改成对非字符串走 `JSON.stringify` 兜底（再 `catch` 回退到 `String`）。
4. 每条新规则配一对**同类必折叠 / 异类必不折叠**的断言——只有正控的测试挡不住这类缺陷。

### rmf-11 — 复制式测试已经漂移了，而且是**静默**漂移：复制过去的 `safeAgent` 现在跑不起来，测试照样 PASS

```
finding_id: rmf-11
severity: minor
primary_location: .claude/workflows/tests/degraded-dedup.test.mjs:44-54（复制的 safeAgent）
related_locations: 同文件 :7-9（复制取舍的说明）；.claude/workflows/scrollz-propose.js:107（MAX_AGENT_ATTEMPTS 的定义处，未被复制）
evidence_status: verified
```

**问题** —— 取舍本身写得很坦白（「workflow 脚本没有模块导出，无法 import，因此改了那两个函数就要同步改这里」）。但漂移**已经发生了**，而且没有任何东西发现：

- `normalizeError` 与 `recordDegraded` 两个函数体目前与源文件**逐字一致**（我逐段比对确认）。
- 但 `safeAgent` 被一并复制了，**而它依赖的 `const MAX_AGENT_ATTEMPTS = 3` 没有被复制**。复制过去的 `safeAgent` 引用了一个未声明的标识符。
- 实测：把测试文件复制一份、在末尾追加一次 `await safeAgent(...)` 调用，node 报
  **`ReferenceError: MAX_AGENT_ATTEMPTS is not defined`**。原测试之所以 PASS，是因为它**从不调用 `safeAgent`**，只直接调 `recordDegraded`。

也就是说：文件里有一份**已经坏掉的**被测代码副本，测试对此完全无感。这正是复制式测试的典型失效——它保护的是「我记得同步的那部分」。

**更好的做法（可行性已实证，不是纸面建议）** —— workflow 确实无法 `import`，但**可以被执行**。我用大约 6 行装置跑通了：

```js
import { readFileSync } from 'node:fs';
const src = readFileSync(SRC, 'utf8').replace(/^export const meta =/m, 'const meta =');
const fn = new Function('agent', 'parallel', 'args', `return (async () => {\n${src}\n})();`);
const parallel = (fns) => Promise.all(fns.map((f) => f()));
const result = await fn(fakeAgent, parallel, fakeArgs);
```

好处是**零复制**：测的就是真文件。而且它一次性把测试面从「两个纯函数」扩到「整条编排语义」——去重、排序、judge 短路、降级按否决、返回值形状全都可断言。**这个装置在本次评审里当场抓到了 `rmf-03` 的早退路径漏 `degraded`**，而现有的复制式测试结构上不可能看见那个 bug（它根本不覆盖返回值）。

若因为某种我不知道的理由不接受上面的做法，**最低限度**也应加一条防漂移断言：测试启动时读源文件、切出两个函数的源码区间、与本文件里的副本做字符串相等断言，不等就 fail。约 10 行，把静默漂移变成响亮失败。

### rmf-12 — 降级裁决对象的形状与 `pickVerdictFields` 不一致；judge 短路让 1b 的拒绝记忆只会拿到 redline 单一视角

```
finding_id: rmf-12
severity: minor
primary_location: .claude/workflows/scrollz-propose.js:277（judge-unavailable 的构造）
related_locations: .claude/workflows/scrollz-propose.js:250-256（pickVerdictFields）；:287-297（短路）；docs/harness/spec.md:153,382（拒绝记忆与 reconsider_when）；.claude/scripts/harness/round.py:48（verdicts 被列为可选字段但不校验形状）
evidence_status: verified
```

**形状不一致** —— `pickVerdictFields` 保证每种 judge 各带一个专有字段（`evidence` / `invariant_at_risk` / `suggested_oracle`），而降级构造的 `{judge, verdict:'reject', reason:'judge-unavailable'}` 三个字段都没有。

**今天不会崩，我核过了**：任何 `reject` 都会在 `verdicts.some(...)` 处把候选推进 `rejected` 并 `continue`，所以降级 verdict **永远不会**出现在最终返回的 `candidates[0].verdicts` 里；而 `rejected` 数组 Python 侧完全不读，`round.py` 的 `_candidates_shape_error()` 与 `validate_candidate()` 也都不校验 `verdicts` 的内部形状（它只在 `_OPTIONAL_CANDIDATE_FIELDS` 里被放行）。所以**不存在下游崩溃**——派活时怀疑的那条路径可以排除。

**真正的代价在 1b。** spec §十二 要求 rejected 记录带 `reconsider_when` 与决定版本。当 1b 把 `rejected` 接进拒绝记忆时会同时撞上两件事：
1. 降级记录里没有 judge 专有字段，是异形行；
2. **短路使得 redline 否决的候选永远只有一条 verdict**。这不是小事：`reconsider_when` 的语义取决于**为什么被否决**——「已完成」（`completed` judge）是永久性的，「红线」是可随决策版本变化的，「oracle 不可证伪」是改写判据后即可复议的。只留 redline 一条，等于把「这个候选还能不能再提」的判据面砍掉三分之二。

**修复建议** —— **不建议撤销短路**（省钱的理由成立，redline 也确实永远第一个跑且永不跳过）。建议：
1. 降级 verdict 补齐该 judge 的专有字段占位（值填 `null` 或 `'unavailable'`），让 `rejected` 的行形状恒定；并加一个 `degraded: true` 标记，使「降级导致的否决」与「真实否决」在记忆里可区分——**把降级当否决记进拒绝记忆是错的**，那会让一次网络抖动永久拉黑一个候选。
2. 在 `rejected` 条目里记下 `skipped_judges: ['harness-judge-completed','harness-judge-oracle']`，让 1b 知道这条记录的判据面是残缺的、不能据此推导永久性结论。这两条都是几行，且必须**赶在 1b 接线之前**做，否则拒绝记忆一开张就带着系统性偏置。

### rmf-13 — 复核 `rmf-02` 修复自身的缺口（协调者自查发现 + 评审复核）：**修法方向确认，但它依赖一个不可信且未校验的字段，且该字段能让整轮崩溃**

```
finding_id: rmf-13
severity: major（派活方口径：Important）
primary_location: .claude/scripts/harness/round.py:428-434（duplicate 分支的 remember_canonical_key，工作区未提交）
related_locations: .claude/scripts/harness/round.py:440-441（published 分支）；.claude/scripts/harness/queue.py:68-84（remember_canonical_key）；.claude/scripts/harness/round.py:47-49（canonical_key 在 _OPTIONAL_CANDIDATE_FIELDS 里，validate_candidate 对它零校验）；.claude/scripts/harness/queue.py:24-30（_norm/fingerprint）；.claude/workflows/scrollz-propose.js:174-178（canonicalKey）；.claude/scripts/harness/publish.py:88-98（resume 路径的 payload 不含四个原文字段）
evidence_status: verified
基线：298 + 13 测试在含该未提交改动的工作区下全绿（实跑）
```

**复核结论：三问分别为「确认」「确认并补充一处协调者未点到的缺口」「推翻——但换来另一个更严重的问题」。**

#### 第一问：修法是否真的恢复了活性？`canonical_key` 一定存在吗？

**方向确认，但存在一条静默失效路径。**

先确认好的一面：把写入点扩到 `duplicate` 分支是**结构上正确**的动作，而且它的价值比协调者说的更大——它把**所有**写入点缺口从「永久」降级成「浪费一轮」。任何原因导致某个提案没被记住（崩溃、旧数据、恢复轮），下次同一候选再出现时都会在 `duplicate` 分支补记。这是自愈，不是补丁。

但 `canonical_key` **不一定存在**：

1. 它是 Workflow 附加的字段（`deduped.push({...c, canonical_key: key})` → `pickCandidateFields`），正常路径上确实有；
2. **但 `validate_candidate()` 对它零要求**——它在 `_OPTIONAL_CANDIDATE_FIELDS` 里，既不必需，也不做任何类型或长度校验；
3. 而候选要穿过**外层模型的一次原样回显**才到达控制器。SKILL.md 要求「原样输出」，那是提示词约束，不是机制约束。模型少抄一个字段，DTO 校验照样通过；
4. `remember_canonical_key` 遇到空值**静默返回**（`if not canonical_key: return`，注释明说「少一条去重记忆是退化」）。

三者叠加：**`canonical_key` 一旦丢失，`rmf-02` 描述的永久卡死原样复发，而且没有日志、没有计数、没有任何测试会发现**。修复的有效性挂在一个不可信、未校验、缺失即静默的字段上。

**根治办法（强烈建议，且它同时解决第三问）**：控制器**自己算**，根本不消费模型给的这个字段。canonical key 按定义就是 `fingerprint()` 做 sha256 **之前**的那个 blob——`queue.py:28-30` 现成写着：

```python
def canonical_key(goal, invariant, primary_path, oracle) -> str:
    return "\x1f".join(_norm(x) for x in (goal, invariant, primary_path, oracle))

def fingerprint(*args) -> str:
    return hashlib.sha256(canonical_key(*args).encode("utf-8")).hexdigest()[:32]
```

四行，零新概念，且控制器手上一定有那四个字段（它们是 `_REQUIRED_CANDIDATE_FIELDS`，DTO 校验已保证非空字符串）。**这样「记不住」在结构上不可能发生。**

需要同时处理一个**已经存在的第二真相源**：workflow 的 `canonicalKey()` 与 Python 的 `_norm()` 必须逐字节一致，因为 `known_canonical_keys` 由 Python 产出、由 JS 比对。两者**现在就已经不一致**——实测同一输入 `a` + `\x1f` + `b`：

| 实现 | 结果 | 原因 |
|---|---|---|
| JS `.trim().toLowerCase().replace(/\s+/g,' ')` | `a\x1fb`（原样） | JS 的 `\s` **不**匹配 `\x1f` |
| Python `re.sub(r'\s+',' ', s.strip().lower())` | `a b` | `'\x1f'.isspace()` 为 `True`，`re` 的 `\s` **匹配**它 |

`\x1c`–`\x1f` 这几个字符正好落在差异区间，而 `\x1f` 恰恰又是拼接用的分隔符本身。字段里出现它属于低概率，但这是「两份实现必须逐字相同」的经典形态，与本批刚做过的 `STAGE1_TOOLS` 单一真相源是同一个问题。建议 Python 侧改用显式字符类对齐 JS 语义，并加一条**跨语言一致性测试**：同一组含控制字符、全角空格、不换行空格的样本，node 与 python 各算一遍、断言相等。没有这条测试，两份实现的漂移不可见。

#### 第二问：还有没有其它覆盖不到的状态？

逐条走了 `_run_round_body` 的**全部** return 点：

| 返回点 | 是否该记 | 现状 | 判定 |
|---|---|---|---|
| `precheck-failed` / `budget-exhausted` / `deadline-exhausted` | 无候选 | 不记 | ✅ 正确 |
| `invocation-failed` / `capability-drift` | 无候选 | 不记 | ✅ 正确 |
| `invalid-candidate`（形状或 DTO） | 不该记 | 不记 | ✅ 正确（即便误记，`known_canonical_keys()` 的 JOIN 也会滤掉孤儿 key） |
| `no-candidate` | 无候选 | 不记 | ✅ 正确 |
| `duplicate` | 该记 | **本次新增** | ✅ 修复正确 |
| `published` | 该记 | 已有 | ✅ |
| **`resume`（恢复轮）** | 该记 | **不记，且拿不到** | ❌ **缺口** |

**`resume` 是真缺口，协调者没有点到。** 构造路径：某轮 `publisher.publish()` 里 Issue 已建、`queue.record(..., 'proposed')` 已落盘（这两步在 `publish.py:88-98` 紧挨着），随后在 `commit_proposal`/`push_main` 阶段崩溃 → `remember_canonical_key`（在 `publish()` **返回之后**才调）从未执行。下一轮走 `resume` 分支，`publisher.resume()` 用的是 outbox 里持久化的 root payload，而那个 payload 只有 `{fingerprint, title, slug, lane, labels, body_md}`——**四个原文字段与 canonical_key 都不在里面**。于是这个提案与 Issue #1 处境相同：在册（`state='proposed'`）却无 key。

**它会被 `duplicate` 分支的修复自愈**（下次同候选再来时补记），代价是一轮的钱，所以严重性可控。两条建议：
1. 把 `remember_canonical_key` 移进 `Publisher.publish()` 内部、紧跟 `queue.record()`——两者写的是同一个提案的两半，**分开写就一定存在中间态**。这样连崩溃窗口都消掉。
2. 若采纳「控制器自己算」，顺手把四个原文字段（或直接把 canonical key）放进 `publish_proposal` 的 outbox payload，恢复轮就也能补记；这属于纯追加字段，不动既有结构。

#### 第三问：会不会把不该进去重集的候选记进去，屏蔽合法方向？

**协调者担心的那个方向可以排除，但换来一个更严重的问题。**

先排除担心的方向——`known_canonical_keys()` 的 `JOIN … WHERE p.state='proposed'` 设计得对：
- `classify() != "new"` 其实覆盖 `exact_duplicate` **和** `rejected_active` 两种。`rejected_active` 的提案 state 是 `rejected`/`closed-by-user`，JOIN 直接滤掉，**不会**进去重集；等 `reconsider_when` 成熟后 `classify` 返回 `new`、重新发布、state 回到 `proposed`，key 才重新生效。语义自洽。
- `INSERT OR IGNORE` 以 fingerprint 为主键，而 fingerprint 与 canonical key 由同四字段导出，一一对应（除非 128-bit 摘要碰撞）。不会张冠李戴。
- 这两点各有测试覆盖（`test_queue.py`：非 proposed 不返回、首写为准、孤儿 key 被滤掉、None 被跳过）。

**但这个修复把一个不可信字段接进了持久状态，而 `validate_candidate` 对它零校验。** 两个可构造的后果：

1. **整轮崩溃（可用性）**：候选带 `canonical_key` 为 `{"a": 1}` → 非空、通过 DTO（未知字段检查只看**键名**，而 `canonical_key` 在白名单内）→ `remember_canonical_key` 把 dict 绑进 sqlite。实测：
   `sqlite3.ProgrammingError: Error binding parameter 2: type 'dict' is not supported`（list 同样报错；float 与 bytes 则被**静默接受**，分别存成 `'3.5'` 与 `b'x'`）。
   该异常会穿到 `run_round` 的 `_finalize_unhandled_exception` 并被**原样重抛**——本轮按最坏值计费、CLI 崩掉、systemd 记 failed。**每一轮都会如此，直到有人手工介入。** 这是 HANDOVER 里 blocker 04（「控制器完全不校验模型返回的 candidate」）的一个新实例：那次给 11 个必需字段加了完整校验，`canonical_key` 因为「反正没人用」被放进可选白名单不校验——**现在有人用了，校验没跟上**。
2. **持久抑制（正确性）**：`canonical_key` 是纯字符串时会被原样写入，并在下一轮作为 `known_canonical_keys` 发给 workflow 做 `seen.has(key)` 相等比对。一个被提示词注入的 finder（本仓库公开，Issue / 评论 / 提交信息任何人可写，这正是 `harness-agent-discipline.md` 第 1 条设定的威胁模型）可以**算出另一个合法候选的 canonical key、填进自己的 `canonical_key` 字段**，从而在自己的提案保持 `proposed` 期间**持续屏蔽那个候选**。代价只是提出一个能过三个 judge 的低风险提案。
   这不是理论洁癖：控制器的整个设计前提是「模型输出不可信、连 labels 都要控制器自己派生」，而这里恰好开了一个「由模型值决定未来哪些候选会被丢弃」的口子。

**修复建议（按优先级）**

1. **首选：不消费该字段，控制器自己算**（第一问那四行）。一步同时关掉「缺失即静默失效」「dict 崩溃」「注入抑制」三个问题，并让 `canonical_key` 退化成纯调试信息。**这是我推荐的做法。**
2. 若仍要消费模型值：在 `validate_candidate` 补一条「`canonical_key` 若存在必须是 `str` 且长度 ≤ 2000」，并在 `remember_canonical_key` 里加 `if not isinstance(canonical_key, str): return`。这是兜底不是替代——它挡得住崩溃，挡不住注入抑制。
3. 无论选哪条，都补一条**交叉校验**：控制器自算的 key 与模型给的 key 不相等时记一条可见告警（不阻断）。这是把「模型是否在如实回显」变成可观测量的最便宜办法，也顺带成为 `rmf-09` 那条怀疑的通用探针。

### rmf-14 — 本轮把回路的关键一环押在了上游明确标注 `[Deprecated]` 的工具上，而 harness 不钉 CLI 版本

```
finding_id: rmf-14
severity: minor
primary_location: .claude/skills/scrollz-round/SKILL.md（TaskOutput 阻塞等待）
related_locations: .claude/scripts/harness/claude_runner.py:64-73（STAGE1_ALLOWED_TOOLS 的说明）；.claude/scripts/harness/config.py:11（CLAUDE 指向会自动更新的启动器）；.claude/scripts/harness/round.py:182-199（_capability_drift_problems）
evidence_status: verified
```

**问题** —— 从已安装的 `claude` 二进制（`~/.local/share/claude/versions/2.1.220`）里取到 TaskOutput 的 tool prompt 原文，第一个词就是：

> `[Deprecated] — for bash and remote_agent tasks, prefer Read on the output file path; for local_agent tasks, use the Agent tool result directly`
> `DEPRECATED: Background tasks return their output file path in the tool result, and you receive a <task-notification> with the same path when the task completes.`

同时 `config.py:11` 的 `CLAUDE = "/home/xp/.local/bin/claude"` 指向一个**按版本目录分发的启动器**（实测 `~/.local/share/claude/versions/` 下并存 `2.1.214`/`2.1.217`/`2.1.220`），既没有版本钉死，也没有任何预检断言版本。

**失效时的行为是 fail-closed，这一点我核过了**：若将来的版本移除 TaskOutput，`--tools …,TaskOutput` 要么让 claude 启动即报错（无 init 事件 → `missing init event` → `invocation-failed`），要么 init 的 `tools` 少一项 → `_capability_drift_problems` 判 `capability-drift`。两条都不会静默放行。**所以这不是安全问题，是可用性与可观测性问题**：在 1a 没有 `OnFailure` 告警的前提下，它表现为「每 2 小时静默失败一次」。

**修复建议**

1. `precheck` 里加一条只读断言：`claude --version` 的输出落在一个已知可用集合内（或至少记录进 `round.log`，让「昨天还好好的」有据可查）。成本近乎零。
2. 把「TaskOutput 是 deprecated、替代路径是读 `<task-notification>` 给出的 output file path」写进 `SKILL.md` 与 `HANDOVER`。上游给的替代路径**恰恰对 `-p` 模式不适用**（notification 跨回合才到，而 headless 会话没有下一回合），这个矛盾必须留档，否则下一个人看到 `[Deprecated]` 会顺手「改成推荐做法」，把真机三次才验出来的结论推翻。
3. 记一个观察终点（对齐 `provisional-needs-an-observation-endpoint`）：**一旦出现 `capability-drift` 且缺失项是 `TaskOutput`**，立即回来重估等待机制，而不是等到某个日期。

### rmf-15 — `TaskOutput` 的返回值有 100k 字符上限，而候选文本长度上限是第二真相源

```
finding_id: rmf-15
severity: minor
primary_location: .claude/workflows/scrollz-propose.js:30-61（CANDIDATE_SCHEMA 对所有字符串字段无 maxLength）
related_locations: .claude/scripts/harness/round.py:51-55（_MAX_LONG_TEXT=20000 等，事后校验）；.claude/scripts/harness/claude_runner.py:178-208（_extract_payload）
evidence_status: verified（上限值与 schema 现状）／unverified（真实越限概率）
```

**问题** —— 从二进制里取到 TaskOutput 工具的定义含 `maxResultSizeChars: 1e5`。也就是说 workflow 的返回值**超过 10 万字符就会被截断**，模型拿到的是半截 JSON，原样回显后 `_extract_payload` 的 `json.loads` 失败 → `invocation-failed` → 按满额计费（叠加 `rmf-05`）。

长度约束目前分散在两处且**方向相反**：workflow 侧的 `CANDIDATE_SCHEMA` 对 `body_md` 等字符串**完全没有 maxLength**，而 Python 侧 `_MAX_LONG_TEXT = 20000` 是**候选到达之后**才检查——也就是说超长文本会先穿过 TaskOutput 的 100k 闸口，被截断成非法 JSON，然后连 DTO 校验都走不到。真机 Issue #1 的 `body_md` 约 2.5 KB，离上限还远，所以这条的现实概率低；但它属于「已知上限没有被表达进 schema」，而本批刚刚以 `STAGE1_TOOLS` 为例确立了「让漂移无法发生」的形态。

**修复建议** —— 在 `CANDIDATE_SCHEMA` 里给 `body_md` 加 `maxLength: 20000`、给 `title` 等短字段加 `maxLength: 300`，与 `round.py` 的常量对齐（并在两边互相注明来源）。schema 校验发生在 agent 返回的**第一时间**，超长直接被拒并触发 `safeAgent` 重试，比事后在 100k 闸口被截断成乱码好得多。

### rmf-16 — `.claude/systemd/` 逐条核对：分层与 fail-closed 基本正确，三处需调整

```
finding_id: rmf-16
severity: minor
primary_location: .claude/systemd/scrollz-harness.service
related_locations: .claude/systemd/scrollz-harness.timer；.claude/systemd/install.sh；.claude/scripts/harness/round.py:24,29（ROUND_DEADLINE_S / CLEANUP_RESERVE_S）；.claude/scripts/harness/claude_runner.py:32（BG_WAIT_CEILING_MS）
evidence_status: verified
```

**先记核对通过的部分**（这些我逐条查了，没问题，写下来是为了让下一轮不必重查）：

| 项 | 核对方式 | 结论 |
|---|---|---|
| 仓库副本与已安装副本一致 | `diff -u` 两个文件各一次 | ✅ 逐字节相同 |
| `EnvironmentFile=%h/.config/scrollz-harness/env` 与 `rmf-01` 修复相容 | 该文件是认证补入点；**无 `-` 前缀**，文件缺失即启动失败 | ✅ fail-closed，正确 |
| `StandardOutput=append:` 的目录存在 | `ls -ld ~/.local/state/scrollz-harness` | ✅ 已预建（`install.sh` 的 `mkdir -p` 也覆盖） |
| 超时分层顺序 | `TaskOutput` 单次 ≤600s < 子进程 `timeout_s` ≤1140s < `BG_WAIT_CEILING_MS` 1200s ≈ `ROUND_DEADLINE_S` 1200s < `TimeoutStartSec` 1500s | ✅ 顺序正确，注释也讲清了理由 |
| `Type=oneshot` + `Restart=no` | 与「事前预留式预算 + 恢复优先」相容，自动重启会在恢复路径之外制造重入 | ✅ |
| 未登录时能否跑 | `loginctl show-user xp -p Linger` → **`Linger=yes`** | ✅ 无人值守成立（这条容易漏，`After=default.target` 单看会让人以为需要图形登录） |
| `install.sh` 只装不启用 | `set -euo pipefail`、`install -m 0644`、末尾只打印启用命令 | ✅ 与「装但先不启用」的纪律一致；实测 timer 现为 `disabled` / `inactive` |
| `Persistent=true` 的补跑放大 | systemd 对错过的触发只补**一次**，不会累积补 12 次 | ✅ 预算无放大风险 |

**需要调整的三处**

1. **`flock -n` 的退出码与轮次失败不可分**（minor，但会直接污染告警）。`flock -n` 冲突时退出 1；`cli.py` 失败时也返回 1。于是 systemd 日志里「上一轮还在跑所以本轮放弃」与「本轮真的失败了」长得一模一样。前者是**正常**的自我保护，后者需要人看。`flock` 支持 `-E`（实测 `--help` 有 `-E, --conflict-exit-code <number>`），建议改成 `flock -n -E 75 …`（75 = `EX_TEMPFAIL`），并在 unit 里加 `SuccessExitStatus=75`，让单例闸挡下的轮次**不**记为 failed。
2. **两处日志都没有轮转**。`round.log` 用 `StandardOutput=append:`，等于主动放弃了 systemd journal 自带的容量管理；`rmf-08` 的 stream 落盘是第二处。建议二选一：改用 journal（删掉这两行 `append:` 即可，`journalctl --user -u scrollz-harness` 一样能看，且有 `SystemMaxUse` 兜底），或给 `round.log` 配 logrotate。
3. **没有 `OnFailure=`**（实测 `rg -c OnFailure` 无命中）。这本身按计划划归 1b，我不主张把 1b 的告警体系提前。**但它与本报告的几条结论叠加后性质变了**：`rmf-03`（全降级轮 exit 0）、`rmf-09`（等待放弃 exit 0）、`rmf-13`（`canonical_key` 为 dict 时每轮崩）三种失败模式里，有两种**根本不触发 systemd 的失败通道**，第三种触发了也没人收。启用 timer 之前，最低成本的替代是：让 `install.sh` 顺带装一个每日跑一次的检查（读 `rounds` 表最近 12 轮，若无一轮 `result='published'` 或存在 `unhandled-exception` 就往 `round.log` 写一行醒目结论）。这不是 1b 的告警体系，只是让「静默失败」在一天之内被看见。

### rmf-17 — 规范模型 ID 只钉了外层会话，13 个内层 agent 仍用别名 `'sonnet'`

```
finding_id: rmf-17
severity: minor
primary_location: .claude/workflows/scrollz-propose.js:195,270（model: 'sonnet'）
related_locations: .claude/scripts/harness/claude_runner.py:34-39（DEFAULT_AGENT_MODEL 的不变量声明）；.claude/agents/harness-{finder,judge}-*.md（frontmatter 无 model 字段，实测 7 个文件均只有 name/description/tools）
evidence_status: verified
```

**问题** —— 注释立的规矩是「无人值守 agent 的模型必须钉死为**规范 ID**，不能用 `sonnet` 这类别名」。实现只对**外层那一次**调用生效（`round.py` 传 `model=DEFAULT_AGENT_MODEL`）。一轮里 13 个内层 agent 全部走别名 `model: 'sonnet'`，而它们才是成本大头（外层实测占 $0.87 中的 $0.6466 那次是 opus；钉 sonnet 后主要成本转到内层）。

**严重性为什么只是 minor（这条我特意往低了判）** —— 当初暴露的危害是**环境变量改写别名解析**（`ANTHROPIC_MODEL=opus[1m]` 让 `--model sonnet` 解析成 `sonnet[1m]`）。那个通道已经被 `_sanitize_env` 的前缀清除堵死了，`ANTHROPIC_DEFAULT_SONNET_MODEL` 也在被清之列（实测该变量此刻就设在父环境里，且确实被清）。所以**现存的、已证实的 hazard 已经被环境层覆盖**，钉外层的规范 ID 属于纵深而非主防线。按「补救必须打得中靶子」的判据，我不把它抬成 major。

**残留风险（可证伪）** —— 别名到具体模型的映射由 CLI 版本决定。预算观察期跨越一周，期间 claude 若自动更新（`rmf-14` 已确认无版本钉），`'sonnet'` 可能解析到不同的模型档位，成本随之变化，而**账本无法解释这个变化**——`rounds` 表不记录实际使用的模型。

**修复建议** —— 把 `model: 'sonnet'` 换成 `model: 'claude-sonnet-5'`（与 `DEFAULT_AGENT_MODEL` 同源；理想形态是由控制器经 `args` 下发，消掉第三处硬编码）。另外把 init 事件里的模型信息记进 `rounds`，让「这一轮到底用的什么模型」可回溯。

### rmf-18 — `_HARNESS_OWNED_CLAUDE_ENV` 是惰性集合，其语义与名字相反

```
finding_id: rmf-18
severity: nit
primary_location: .claude/scripts/harness/claude_runner.py:41-45
related_locations: .claude/scripts/harness/claude_runner.py:347-359
evidence_status: verified
```

**问题** —— 注释写「本模块**自己**要设进子进程环境的 `CLAUDE_*`/`ANTHROPIC_*` 变量」，但该集合在过滤循环里的作用是**豁免删除**，即「保留父进程的值」。当前唯一成员 `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` 随后被无条件覆写（:359），所以**豁免与否结果完全一样**——这段逻辑目前是惰性的。

**为什么仍值得记** —— 它是个反向陷阱：将来往这个集合里加一个**不会被无条件覆写**的变量，就等于开了一条静默的父进程继承通道，而名字和注释都会让人以为它表示的是「本模块自己设的」。

**修复建议** —— 要么删掉这个集合（当前完全冗余），要么把语义改成真正的「本模块自设表」：`_HARNESS_OWNED_CLAUDE_ENV = {"CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS": str(BG_WAIT_CEILING_MS)}`，过滤时一律删除，之后 `safe_env.update(_HARNESS_OWNED_CLAUDE_ENV)`。这样名字、注释与行为三者一致，且不可能变成继承通道。若采纳 `rmf-06` 的 env allowlist 改造，这条自然消失。

---

## 核过之后判定「未发现问题」的项（附核法，供下轮免于重查）

### `TaskOutput` 有没有扩大权限边界？—— 未发现问题

派活时怀疑三点：它能读到什么、能否读到本轮不该看到的数据、`task_id` 是否可被模型任意构造。我从已安装的 `claude` 二进制里取出该工具的**真实定义**（而不是读文档或猜）：

- **输入 schema 原文**：`task_id: v.string().describe("The task ID to get output from")`、`block: v.boolean().default(true).describe("Whether to wait for completion")`、`timeout: v.number().min(0).max(600000).default(30000).describe("Max wait time in ms")`。
- **只读性**：该工具定义里 `isReadOnly(e){return true}`、`searchHint: "read output/logs from a background task"`。没有任何写路径。
- **作用域**：`task_id` 的解析走会话内的 `taskRegistry`（二进制里多处 `Cannot destructure property 'taskRegistry'` 的报错串佐证），而后台任务的启动文案原文写着 **`Note: it does not survive exiting this session.`** 因此伪造 `task_id` 只能命中**本会话**的注册表，取不到其他会话/其他轮次的任务输出。
- **横向对照**：`--tools` 与 `harness-settings.json` 的 `permissions.allow` 都恰好列出六项，`_validate_tools` 要求解析后**恰好相等**，`_capability_drift_problems` 每轮再核一次 init 事件里的实际工具集。多一个少一个都 fail-closed。

结论：加入 `TaskOutput` 不扩大写能力、不扩大文件系统读取面、不跨会话。**这一项没有问题。**（它另有两个非权限问题，见 `rmf-14`、`rmf-15`。）

### `harness-settings.json` 的 `allow` 里有 `TodoWrite` 而 `STAGE1_ALLOWED_TOOLS` 里没有 —— 未发现问题

`--tools` 才是决定 init 工具集的那一层，`allow` 是叠加在其上的许可过滤器（真机 probe 实测 init 的 `tools` 恰为请求的那几个）。`allow` 是 `--tools` 的超集不会放大能力，且 `TodoWrite` 早于本批就在（`git diff` 确认本批只加了 `TaskOutput` 一行）。保持现状即可；若要更整齐，可让 `allow` 与 `STAGE1_ALLOWED_TOOLS` 同源生成——那属于洁癖，不属于缺陷。

### 前缀级 env 清除有没有误删 claude 运行必需的变量 —— 未发现问题（真正的问题在反方向）

`HOME`、`PATH`、`LANG`、`TZ` 等均不带这两个前缀，不受影响；`_sanitize_env` 从完整环境出发再删（`claude_runner.py:372-374` 的注释也点明这是评审 C-06 的结论），真机也已跑通。**误删方向没有问题；有问题的是漏删方向**，见 `rmf-06`。

---

## 主观建议

```
[建议] .claude/workflows/scrollz-propose.js 整体 — 让 workflow 可被「执行式测试」而不是「复制式测试」覆盖 — 预期影响：一次性把去重、排序、judge 短路、降级语义、返回值形状全部纳入自动化，且零复制零漂移 — 推荐做法：采纳 rmf-11 里那 6 行装置，把 degraded-dedup.test.mjs 重写成注入 fake agent 的端到端验收；现有两个纯函数的用例作为其中一小节保留。
```

```
[建议] .claude/scripts/harness/round.py + budget.py — 把「一轮到底花了多少钱」变成实测量而非推算量 — 预期影响：预算观察期（至 2026-08-07）的结论从「按预留上限推算」升级为「按实际调用累加」，`rmf-05` 与 `rmf-07` 里两处 unverified 同时被关掉 — 推荐做法：`record_invocation()` 已写好且有测试却从未被生产路径调用（实测 invocations 表为空），在 invoke 返回后无条件调它，invocation_id 用 `round_id + 序号`。
```

```
[建议] docs/harness/ — 给这一批「真机才暴露」的缺陷补一条可复用的判据清单 — 预期影响：把 HANDOVER 里那句「凡是『由谁启动我』『我活多久』『上游抖动时编排怎么办』三类问题，离线测试系统性地看不见」从一句总结变成上线前的检查项 — 推荐做法：在 spec 的验收章节加第四类「我把什么写到了磁盘上」（rmf-08 属于它），并对每类各列一条**只读**的现场核法（如本报告用过的：systemctl show-environment、_sanitize_env 正控实验、二进制里取工具 schema、只读查 state DB）。
```

---

## 我最没把握的三个判断

1. **`rmf-07` 里「失败的 agent 调用按接近成功调用计费」这个前提。** 它支撑着「39 次调用 ≈ $16、会撞穿 $6 上限」这个量级结论。我的依据是 `mid-response` 意味着输入侧 token 已付、以及契约探针 249k cacheRead 的成本结构——**但我没有任何一条实际账单**（`invocations` 表为空）。若失败调用实际几乎不计费，那么整条「重试放大成本」的论证会从 major 掉到 minor，只剩「无退避」这一条成立。**分量档位：仅为倾向，需实测**；对应的动作（传预算进 workflow）我认为仍应做，但它可以靠「重试次数应有界于钱而非次数」这条独立理由证成，不需要这个观察承重。
2. **`rmf-08` 里「agent 的 `Read` 能否越出 cwd」。** 我明确没有验证——验证它要真花钱起一次 headless 调用。这个答案会把 `rmf-08` 在「泄漏凭据」这一维上的严重性从「取决于」变成「确定」或「排除」。我用「无论答案如何该做的事一样」绕开了它，这个绕法我认为成立，但**它确实留下了一个未回答的问题，而那个问题本身可能是独立的 Critical**。建议下一轮用一次 probe 顺带验：让 probe 的提示词要求读一个 cwd 之外的无害文件（如 `/etc/hostname`），看 init 后是否出现 denial。
3. **`rmf-02` 的复现频率。** 我把「机制被关掉」标为 verified、「会每轮重复提同一候选」标为 unverified，样本只有协调者观察到的 1 次。我对**机制**的判断有把握，对**频率**没有。这个区分影响的是修复的紧迫性排序而不是修复本身——但如果实际复现率很低（例如 finder 的随机性足以每轮产出不同候选），那么把它定为 blocker 就偏重了。我仍按 blocker 记，理由是「唯一的抗重复机制处于关闭状态」这件事本身与频率无关，且启用 timer 是不可逆地开始烧钱。

## 执行本契约时遇到的摩擦

1. **评审范围在评审过程中被追加了四次**（`b3fdf17`、`cb726b2`、`f50a403`+`340774d`、一处未提交的工作区改动）。这不是坏事——真机修复本来就在推进——但它让「reviewed_at_rev」这个字段失去了单一取值，我只能在头部注明「最终基线 = `340774d` + 工作区未提交改动」。**下次若仍是这种边修边审的形态，建议每次追加时明确给出新的基线 rev**，否则报告与代码的对应关系只能靠时间戳推断。
2. **三次被上游传输故障打断**（两次 `Server error mid-response`、一次 `Upstream stream truncated`）。分段增量落盘的纪律完全兑现了它的价值：三次续跑都没有丢失已完成的评审方向。唯一的代价是我需要在每次续跑时重新确认文件已有内容的边界，而计数摘要写在**文件头部**导致它在追加过程中失效了一次——已用一次最小 Edit 更正并注明原值。**下次的做法应当是把计数放在文件末尾**，或从一开始就写成「见文末交付声明」。
3. **一次 Bash 护栏拦截**：我在报告正文里引用了一条强制清理未跟踪文件的 git 命令字面量，被 git 纪律护栏拦下（护栏按命令形态匹配，不区分「正在执行」与「正在被引用」）。改写措辞后通过。记下来是因为**这是一个会重复发生的形态**：安全评审报告天然要引用危险命令。
4. **一次输入校验拦截**：报告里为了说明 JS 与 Python 的 `\s` 差异，正文中带了字面控制字符与不换行空格，被工具层拒收。改成描述性写法后通过。同样是评审这类内容时会重复遇到的形态。
5. **无法验证的两项均已在正文标注**（Read 能否越界、失败调用的计费），它们需要花钱的真机调用，超出了本次「可逆操作可自决」的范围。

---

## 交付声明

- 本报告由独立 reviewer 撰写，评审对象**未被修改**：本次会话对被评审代码的 `Edit`/`Write`/`Bash` 写通道**零使用**，唯一写入的文件是本报告（派活白名单内），另在 `/tmp` 下创建了两个一次性验证脚本（`wf-harness-poc.mjs`、`t.mjs`）。
- 未执行任何 git 提交、暂存或分支操作；未改动共用工作树中他人的未提交改动。
- 对状态数据库的全部访问均为 `mode=ro` 只读连接；对 systemd 的全部访问均为 `show-environment` / `is-enabled` / `is-active` 等只读查询，**未安装、未启用、未启动任何单元**。
- 测试为实跑：`python3 -m unittest discover`（13 + 298，全绿）、`node degraded-dedup.test.mjs`（PASS）、以及正文中标注为「实测」的全部 node/python 一次性实验。

---

## 基线尾注（落盘后发生的变化）

`rmf-13` 撰写时，其评审对象（`duplicate` 分支的 `remember_canonical_key`）尚在工作区未提交；报告落盘期间它已被提交为 **`fd3a23c`**（`fix(harness): 被判重复的候选也进去重集，否则修复自身留下永久卡死（rmf-02 自查）`）。经核对，提交内容与我评审的工作区版本一致（`round.py:428` 起的注释与调用逐字相同），**`rmf-13` 的三条结论与全部行号引用均不受影响**，只需把该条的「工作区未提交」读作「已提交于 `fd3a23c`」。

因此本报告的最终评审基线为：**`fd3a23c`**（= `340774d` + 该次提交）。
