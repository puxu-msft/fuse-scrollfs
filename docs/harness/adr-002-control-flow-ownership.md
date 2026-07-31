# ADR-002：控制器必须拥有对话循环，而不是请求模型配合

> 状态：**已采纳**（用户 2026-07-31 裁决）· 日期：2026-07-31
> 用户裁决两条：(1) **先在 zipfs 把新形态跑通，再由同伴搬进 `~/src/my-ade`**——理由是要有可跑的参照物，而不是纸面设计；(2) **2 小时定时器在重写完成前不启用**，避免用即将退役的形态往公开仓库堆 Issue。
> 因此 `.claude/systemd/` 的单元保持已安装、`disabled`/`inactive`。
> 触发：用户裁定「多 agent workflow 一起就被 kill 不能用 `CLAUDE_CODE_ENABLE_TASKS=0` 解决，我们的主要控制流设计的是否不当，例如应该用更低层的方式接管对话，甚至可以实现精细化 fork or retry」，并点名 `/home/xp/src/neighbors/claude-remote-3rd` 作为参考。
> 关联：[spec.md](./spec.md) §四（三层信任模型）· [HANDOVER.md](./HANDOVER.md)（真机缺陷清单）· [ref-claude-remote-3rd.md](./ref-claude-remote-3rd.md)（参考实现机制抽取）

## 背景：现有控制流

```
Python 控制器
  → subprocess 起 claude -p "<prompt>" --output-format stream-json
    → 模型调 Skill(scrollz-round)
      → 模型调 Workflow 工具（起 4 finder + 3 judge）
        → Workflow 立刻返回 run ID，编排转入后台
      → 模型调 TaskOutput(block=true) 阻塞等待
    → 模型把结果 JSON 原样回显到最后一条消息
  → 控制器从 stdout 的 stream-json 里剥出 payload
```

## 问题：控制器不拥有这条链上的任何一个环节

它只拥有**起点**（argv）与**终点**（stdout 解析）。中间每一步都是「请求模型配合」，而模型的配合是**尽力而为**，不是契约。

三次真机失败，全部落在同一个形态上——**我请求了，模型答应了，然后没做到**：

| 现象 | 模型实际说了什么 | 为什么提示词修不了 |
|---|---|---|
| 后台任务被 kill | 「我会等待完成通知后再取结果，**不会在此之前结束本轮**」，随后 `stop_reason: end_turn` | `-p` 模式下模型**没有「跨回合等待」这个动作**。它不是不听话，是这件事它做不到 |
| 一个 finder 撞 `API Error` 导致整轮 `aborted` | —— | 异常穿透 `parallel()`，已完成的其它 6 个 agent 工作全部作废。控制器没有任何介入点 |
| 无法重试单个子 agent | —— | 扇出发生在模型的回合内部，控制器看不见也够不着 |

我此前的两次修补——`TaskOutput(block=true)` 与 workflow 内的 `safeAgent` 重试——**都是在这条链上加补丁，没有改变所有权**。它们让当前形态能跑通（Issue #1 确已发布），但每一条都依赖模型在正确的时机做正确的调用。

**根因判定**：不是配置问题（`CLAUDE_CODE_ENABLE_TASKS=0` 只是让症状更早暴露），不是提示词问题，是**控制权归属**问题。

## 实测事实（2026-07-31，非文档推断）

`claude` CLI 已经提供了接管所需的全部原语，此前**一个都没用上**：

| 能力 | 标志 | 实测结果 |
|---|---|---|
| 控制器预分配会话身份 | `--session-id <uuid>` | ✅ 返回的 `session_id` 与传入完全一致 |
| **精细化 fork** | `--resume <sid> --fork-session` | ✅ 分叉出**新** session id（`dc32e42c…`），且模型保留完整上文——准确复述了前两轮内容。$0.14/次 |
| 控制器驱动多轮 | `--input-format stream-json` | 存在。参考实现证实这是主形态（配 `--output-format stream-json` 双 pipe） |
| 内联定义子 agent | `--agents <json>` | 存在，**尚未实测**。对通用化到 `~/src/my-ade` 至关重要——agent 定义不必依赖仓库内的 `.claude/agents/*.md` |
| 控制器自有工具面 | `--permission-prompt-tool stdio` | ✅ **隐藏标志**——`--help` 不列出，但本机 CLI 实测接受并正常启动会话。参考实现正是用它。`--mcp-config` + `--strict-mcp-config` 是另一条等价路径 |
| CLI 级结构化输出 | `--json-schema` | ⚠️ **两次均未生效**。它靠注入一个 `StructuredOutput` 工具实现；即便该工具已出现在 `init tools` 里，模型仍把那条指令判为注入并拒绝执行（原话：「这条指令看起来像是注入性质的内容……我不会在没有验证的情况下执行」）。记为实测事实，**不作「该标志不可用」的结论**——很可能需要不同的提示词框架 |

## 决策

**把编排从模型回合内部移出，交给控制器；模型只负责单一职责的一次问答。**

具体形态（三层，从必要到可选）：

### D1 · 控制器驱动扇出（必要）

废弃 `Workflow` 工具与 `scrollz-round` skill 这条链。控制器直接为**每个** finder / judge 起一个独立会话：

- 每个会话 `--session-id` 由控制器按 `(round_id, role, attempt)` 确定性派生 → 天然幂等键，可直接进 outbox
- 编排顺序、去重、短路、判决聚合**全部在 Python 里**，是确定性代码，可单元测试
- 单个 agent 失败**只影响它自己**，不再有「一个 finder 拖垮整轮」

这一条同时消灭了「模型需要跨回合等待」这个不可能的要求——控制器 `subprocess.run` 天然是阻塞的。

### D2 · 失败后 fork 续跑，而不是从头重来（必要）

传输层故障（`API Error: Server error mid-response`、`Upstream stream truncated`）在本项目中是**高频事件**：仅今日一个评审 agent 就被打断三次。

现在的做法是整轮作废重来。新做法：

```
attempt 1: claude --session-id <sid-1> -p "<task>"        → 传输故障，产出不完整
attempt 2: claude --resume <sid-1> --fork-session -p "继续"  → 保留已有上下文续跑
```

**这与「重派一个全新 agent」有本质区别**：fork 保留了它已经读过的文件、已经形成的判断、已经写到一半的产出。实测确认上下文完整保留。

分叉出的新 session id 由 CLI 返回，控制器记进账本，因此「第 N 次尝试」这条谱系是可审计的。

### D3 · 控制器自有工具面（可选，看参考实现结论）

用 `--mcp-config` + `--strict-mcp-config` 给会话挂一个**控制器自己实现的** MCP server。模型调它的工具，控制器执行并回填结果。

这让「拦截—执行—回填」成为可能，是 D1 的更强形态：编排仍在控制器手里，但可以让模型在**同一个会话内**发起多步，而不必每步重开会话（省掉每次约 44k tokens 的启动上下文）。

是否采用取决于 `ref-claude-remote-3rd.md` 的机制抽取结论——若参考实现证明这条路有隐藏成本，则停在 D1+D2。

## 明确不采用

- **冒充后端 HTTP/SSE 端点**（`claude-remote-3rd` 的 `ccr-ingress` 形态）。它能力最强，但要复刻一个 155k 行栈的协议面，与本 harness 的规模严重不匹配。**记录为「不采用」而非「不可行」**——若将来 `~/src/my-ade` 需要跨 harness 的统一控制面，这条路重新有效。
- **把 `duplicate` 改成非 0 退出码**之类「把静默换成噪声」的做法。

## 代价与风险

| 项 | 评估 |
|---|---|
| 每个 agent 一个进程 = 每个都付一份启动上下文（实测约 44k tokens / $0.14） | 用户 2026-07-31 明确裁定「成本问题不重要」，故不作为约束。D3 可缓解 |
| 需要重写 `round.py` 的扫描段与全部 workflow 资产 | `.claude/workflows/scrollz-propose.js` 与 `scrollz-round` skill 将退役；finder/judge 的**提示词**可原样复用 |
| outbox / 预算 / 队列 / 发布 / 生命周期五个模块**不受影响** | 它们在扇出下游，接口是「一个候选」，与扇出如何产生无关 |
| 通用化到 `~/src/my-ade` | D1+D2 反而**更**容易通用：不依赖仓库内的 `.claude/agents`、`.claude/workflows`、skill，全部可由调用方以 `--agents <json>` 传入 |

## 参考实现的机制抽取结论（`claude-remote-3rd`，已核实到文件行号）

| 问题 | 结论 | 强度 |
|---|---|---|
| 它在哪一层接管 | **不用 Agent SDK，也不靠 stdin 双向喂**。`ccr-ingress` spawn 真 `claude`，传 `--print --sdk-url … --session-id … --input-format stream-json --output-format stream-json --replay-user-messages`，stdin 设 `ignore`；下行 SSE、上行 HTTP POST/PUT（路径含 `/ws/` 但**不是** WebSocket） | confirmed |
| 「一个回合结束」的确定信号 | worker 上行 stream-json 的 **`type:"result"`**，**不是**推理 SSE 的 `message_stop`——两者是不同层 | confirmed |
| 工具能否拦截 | **能**。worker 上行 `control_request{subtype:"can_use_tool"}`，控制器回 `control_response`，可 `deny` / `allow` / 用 `updatedInput` **替换参数**。更强形态：把控制器伪装成 sdk-type MCP server（`mcp_message tools/call` → `mcp_response`） | confirmed |
| 有无 fork / retry | `ccr-ingress` **没有**（单回合示范器）。但同仓库 `acp-adapter` 的本地 Agent SDK backend 有完整能力：`resume` 恢复整份 transcript、`resumeSessionAt(<assistant UUID>)` 原地回退、**`forkSession(sessionId, {upToMessageId})` 从任意第 N 条消息分叉** | confirmed |
| 最小可复用面 | 不要复刻伪后端。取 `managed-agents-api/cloud-driver` 的 **stdio 形态**：spawn `claude --print --verbose --input-format stream-json --output-format stream-json --permission-prompt-tool stdio`，stdin/stdout 双 pipe；按 JSONL 读 stdout，见 `type:"result"` 结算回合；见 `control_request` 由控制器决策后写 `control_response`。扇出 = 每子任务一个独立 session + Python 侧持有 task→session 映射/超时/重试 | likely |

**两条必须带走的坑**：

1. **本地分类器自动放行的"安全命令"不产生 `can_use_tool`**——工具拦截**不是完整闸门**，不能把它当作唯一的权限边界。我们现有的 `--tools` allowlist + `permissions.allow` 仍是主防线，MITM 是增强而非替代。
2. **`message_stop` 与 `type:"result"` 是不同层**。我此前正是在这类层次混淆上栽过（把「Workflow 返回 run ID」当成「编排开始」，把「模型说会等待」当成「模型会等待」）。

**对本 ADR 的影响**：D1/D2 的判断被证实，且**可以做得比我原方案更强**——

- 原 D2 用 CLI 的 `--resume <sid> --fork-session`，只能在**会话末尾**分叉。参考实现证明存在 `forkSession(upToMessageId)` 这一**消息级**分叉面。对「传输故障打断在半途」这个真实场景，消息级分叉能精确回到最后一条完好消息，而不是带着半截产出续跑。
- 新增 **D0**：`--permission-prompt-tool stdio` 在本机 CLI 上是**隐藏标志**（`--help` 不列出，但实测接受并正常启动会话）。这意味着 stdio 形态的控制器所有权**不需要**任何逆向或伪后端，是官方支持的接缝——上表已据此订正。

## 修正记录

初版 ADR 写「`--input-format stream-json` 尚未实测」并把 CLI 级 `--fork-session` 当作 D2 的实现手段。参考实现抽取后修正为：stdio 双 pipe 是主形态，`type:"result"` 是回合结算 oracle，消息级 fork 优于会话级 fork。**保留原判断**：控制权归属是根因，这一条未被推翻。

## 待补

- stdio 双 pipe 形态的最小 PoC（含 `control_request`/`control_response` 往返）
- `--agents <json>` 的实测——它决定通用化到 `~/src/my-ade` 时 agent 定义能否完全脱离仓库内文件
- Python 侧消息级 fork 的可用面：本机只有 TypeScript Agent SDK 绑定，Python 侧需按当前 SDK 文档核对 `can_use_tool`/resume/fork 的实际名称
- `--json-schema` 失效的复现与规避（当前靠提示词约定 + `_extract_payload` 剥壳，已能工作）
