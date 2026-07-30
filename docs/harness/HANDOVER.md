# Stage 1a 交接状态 / HANDOVER

> 末次更新：2026-07-31。新会话接手请先读本文，再读 [plan-stage1a.md](./plan-stage1a.md) 文末的执行状态表与评审处置台账。
> 权威文档：规格 [spec.md](./spec.md) v7 · 1a 计划 [plan-stage1a.md](./plan-stage1a.md) · 1b 冻结范围 [plan-stage1b.md](./plan-stage1b.md)
> 进度账本：`.superpowers/sdd/progress.md`（git-ignored，崩溃后靠它 + `git log` 恢复认知）

## 一句话现状

**Task 1–12 已实施完毕并全部经过对抗评审与修复，205 测试全绿；Task 13（真机验收）尚未开工，且合并态评审判定 `needs-rework`——在 5 个 blocker 全部关闭前，不得执行任何真实 GitHub 写入、main push 或 timer 启用。**

## 已建成的东西

`.claude/scripts/harness/` 共 14 个模块：

| 模块 | 职责 | 关键不变量 |
|---|---|---|
| `config` | 路径常量、凭据加载 | 绝对路径（systemd user PATH 不含 claude/cargo） |
| `db` | SQLite schema（WAL） | 只追加表，不改既有表定义 |
| `outbox` | **所有外部副作用的唯一入口** | 重入时 probe-before-call；`failed_terminal` 才阻断本轮 |
| `lifecycle` | Stage 1 发布生命周期有序派生函数 | 必须有序求值；256 组合穷举 |
| `ghclient` | GitHub 访问（经 `gh`） | Issue DTO 规范化 `labels: list[str]`；写/读错误分型 |
| `gitops` | 发布工作区（detached worktree） | 路径严格模式校验；重放只动本 operation 的单个提交 |
| `budget` | 事前预留式预算 | 花钱**之前**落盘；`round_id` 是幂等键 |
| `queue` | 去重、lane 上限、typed 复议谓词 | 自然语言条件一律不可机器判定 |
| `publish` | 发布编排 | 每步无条件走 `execute`；probe 现场核验 git 对象 |
| `precheck` | 启动硬预检 | 纯读门通过才进副作用层；dirty 检查先于 reset |
| `claude_runner` | `claude -p` 调用层 | 工具 allowlist 入口强制；缺 init 事件即失败 |
| `round` | 一轮编排 | 恢复优先于新扫描 |
| `cli` | `round`/`status`/`doctor`/`probe` | |

Claude 侧资产：`.claude/agents/harness-{finder,judge}-*.md`（7 个）、`.claude/workflows/scrollz-{propose,contract-probe}.js`、`.claude/skills/scrollz-{round,contract-probe}/`、`.claude/rules/harness-agent-discipline.md`、`.claude/harness-settings.json`、`docs/harness/redlines.yaml`。

## 契约探针实测结论（已冻结，勿再猜测）

2026-07-31 两次真实调用（共约 1.35 美元、零外部写入）确认：

1. **`--output-format stream-json` 强制要求 `--verbose`**——`claude --help` 查不到，只有真跑才报错。
2. **`--permission-mode dontAsk` 拒绝一切不在 `permissions.allow` 里的工具**。`deny` 是纵深，**`allow` 才是主防线**。原先 allow 为空导致 `Workflow` 被拒、重试至烧完预算。
3. **Workflow API 四项形状实测通过**：`export const meta` 首字节、顶层 `args` 全局、`agent(prompt, opts)` 位置参数、传 `schema` 直接返回已校验对象。
4. **隔离生效**：init 的 `tools` 恰为请求的五个，`mcp_servers` 与 `plugins` 均空，被拒调用 0 次。
5. **成本远高于原假设**：单 agent 仅回显字符串花费 $0.9985（opus-5，cacheRead 249k）——agent 继承默认模型与完整全局上下文。已给 finder/judge 钉 `model: 'sonnet'`；**真实每轮成本仍需 Task 13 首轮实测**，据此校准预算与节拍。

## 阻断 Task 13 的 5 个 blocker

合并态评审全部构造反例复现。修复状态：

| # | 问题 | 状态 |
|---|---|---|
| 01 | 预算预留发生在检查 `open_roots()` **之前** → 崩溃后旧预留占满日预算，恢复路径永远到不了 | 修复中 |
| 02 | GitHub Search 是**异步索引**，探测阴性不证明 Issue 未创建 → 重发 → 同一提案两个 Issue | 修复中 |
| 03 | probe 仅凭 SQLite 缓存的 SHA 判定 commit 存在 → 虚假确认 push | **已修**（0b601cd，含正控） |
| 04 | 控制器**完全不校验**模型返回的 candidate → `slug="../escape"` 先建公开 Issue 再永久卡死 | 修复中 |
| 05 | Task 13 产物不存在（协调者曾错误表述为「13 个任务全部完成」） | 已纠正表述 |

同批 Important（同样需在启用 timer 前关闭）：labels 未派生、能力漂移非 fail-closed、**CLI 把成功恢复报为失败**（systemd 会因此每次成功恢复都告警）、轮次账本除成本外未结算、截止下限反而放大剩余时间、`doctor` 非只读且对未完成 root 假绿、崩溃矩阵自证覆盖、spec 与 1b 阶段边界互相矛盾。

## Task 13 的执行顺序（不得连跑，逐项停）

```
1. doctor（纯只读，需先修 merged-11 使其真正只读）
2. probe（只花钱、零写入，需先修 merged-08 使退出码可信）
3. bootstrap_labels.py 建 18 个 label（可逆，需回读校验）
4. 手工跑一轮真实 round  ← 第一次在公开仓库留下 Issue 与 main 提交
5. HARNESS_FAULT 定点故障注入的三轮恢复验收
6. 安装 systemd unit 但**先不启用**
7. 确认无误后才 enable timer（2h 节拍）
```

第 4 步之前必须向用户明确确认。

## 方法论沉淀（这轮反复验证有效）

- **每条修复都要正控**：临时还原旧实现跑一次，确认测试真的会红。一条永远不会红的测试和一条永远绿的测试，输出上一模一样。本轮至少四次靠正控证伪了「已修复」的假象。
- **接缝比模块危险**：5 个 blocker 里 4 个是纯接缝——每个模块单独看都对，192 个测试全绿，因为每个测试都自己构造依赖，撞不上真实的跨模块假设。
- **账本不等于产物**：`outbox` 记着 SHA 只说明「曾经创建过」，不说明「现在还在」。probe 必须去看产物。
- **自填字段不是保证**：执行状态表、覆盖表、「已修复」标记，都是被检查者自己填的。要么引入独立 ground truth，要么就别当保证用。
- **subagent 越权修改要拦，但越权发现要收**：Task 5 的 agent 纠正了协调者对 ResourceWarning 的错误归因、却没去改不在授权范围的文件——这个处置是对的。
