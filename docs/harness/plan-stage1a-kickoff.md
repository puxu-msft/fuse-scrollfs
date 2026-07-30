# Stage 1a Kick-off 提示词

> 复制以下整段作为新会话的第一条消息即可开工。

---

在 `/home/xp/src/zipfs`（crate 名 scrollz，远端 `puxu-msft/fuse-scrollfs`）实施 scrollz 自主改进 harness 的 **Stage 1a**。

**权威文档**（按此顺序读，冲突时以 spec 为准）：
- 规格：`docs/harness/spec.md`（v6，回答做什么/为什么）
- 计划：`docs/harness/plan-stage1a.md`（13 个 TDD 任务，回答怎么做；文末有评审处置台账与执行状态表）
- 后续范围：`docs/harness/plan-stage1b.md`（治理与可观测，**本次不做**，但不得把其中条目当作可以省略）

**执行方式**：用 `superpowers:subagent-driven-development`，一个任务派一个全新 subagent，任务之间由主会话评审。每个任务严格按计划里的五步走：写失败测试 → 跑到确认失败 → 写最小实现 → 跑到通过 → 提交。

**开工前必须知道的六件事**：

1. **主工作树可能有他人未提交改动**。提交一律用 `git commit -m <msg> -- <本任务的路径>` 限定路径，绝不 `git add -A`。
2. **零第三方依赖**。只用 Python 3 标准库（`unittest` / `sqlite3` / `subprocess` / `itertools`），不建 venv、不装 pip 包。测试跑法：`cd /home/xp/src/zipfs/.claude/scripts && /home/linuxbrew/.linuxbrew/bin/python3 -m unittest harness.tests.<模块> -v`。
3. **绝对路径**（systemd 的 user PATH 不含这些目录）：`python3`=`/home/linuxbrew/.linuxbrew/bin/python3`、`claude`=`/home/xp/.local/bin/claude`、`gh`=`/usr/bin/gh`、`git`=`/usr/bin/git`、`flock`=`/home/linuxbrew/.linuxbrew/bin/flock`。
4. **Task 1–12 不产生任何外部副作用**，可以放手做。**Task 13 会真实写入公开仓库**（建 label、建 Issue、推 main、启用 timer），必须逐步执行并在每步之间停下来确认，不要连跑。
5. **Task 2 与 Task 5 的实现已在 `/tmp` 离线验证过**（lifecycle 7 用例含 256 组合穷举；gitops 修过三个实测缺陷：重放身份、worktree 注册残留自愈、cherry-pick 冲突 abort）。计划里的代码是验证后的版本，照抄即可，但仍要按 TDD 顺序先写测试。
6. **Task 10 的 Workflow 脚本 API 曾经写错过一次**（用了 `export default async function({args})` 与 `agent({agentType, prompt})`），已按工具 schema 改为 `export const meta` 字面量 + 顶层 `args` 全局 + `agent(prompt, opts)` + `schema` 结构化返回。**不要凭记忆改回去**；如需确认，读 Workflow 工具自身的 schema。

**完成的标志**：Task 13 全部走完，`systemctl --user list-timers scrollz-harness.timer` 显示已启用，`gh issue list --label harness` 有真实提案，远端 main 有对应提案卡，定点故障注入的三轮恢复实测通过。

**每完成一个任务**：更新 `plan-stage1a.md` 文末执行状态表的该行（状态 + 验证证据 + 偏差），与代码一起提交。
