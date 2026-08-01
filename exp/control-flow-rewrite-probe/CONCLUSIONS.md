# Phase 0 会话原语真机验证 · 结论

> report_id: `cfrw-phase0` · 日期：2026-08-02 · CLI 版本 2.1.220
> 计划：[../../docs/harness/plan-control-flow-rewrite.md](../../docs/harness/plan-control-flow-rewrite.md) Phase 0
> 复现：`cd /home/xp/src/zipfs && /home/linuxbrew/.linuxbrew/bin/python3 exp/control-flow-rewrite-probe/probe.py [0.1|0.2|all]`
> 总花费 **约 $0.10**（预算 $2）；零外部写入——未建 Issue、未 push、未读写 `.claude/state/harness.db`。

## Task 0.1 · 待决 A：单发 `-p` + `session_id` / `resume --fork-session`

```
finding_id: cfrw-phase0-01
conclusion_strength: confirmed
```

**假设**：单发 `claude -p`（非 dual-pipe）能用 `--session-id` 预分配会话身份，再用 `--resume <sid> --fork-session` 续跑并保留上文。

**实测**（`artifacts/20260801T173131.277038Z/task-0.1/`）：

| 断言 | 结果 |
|---|---|
| 传入 `--session-id` 被原样接受 | ✅ `e0bfca96-…` 请求值 == init 事件观测值 |
| fork 产生**新** session ID | ✅ `3d7df239-…` ≠ 原 ID |
| 上文保留 | ✅ 第二次回复 `CODE:PLUM`（暗号设在第一次调用） |
| 退出码 | ✅ `[0, 0]` |
| 成本 | $0.03474 + $0.00365 = **$0.0384** |

**结论**：**待决 A 成立**。Phase 2 按推荐路线扩展现有 `invoke()`（新增 `session_id`/`resume`/`fork_session` 参数），**不需要**转 dual-pipe，`claude_runner.py` 的接口形状不变。

## Task 0.2 · 设计问题 5：只读工具是否触发 `can_use_tool`

```
finding_id: cfrw-phase0-02
conclusion_strength: confirmed
```

**计划的预设被推翻。** 计划原文预期「理论上只读工具不应触发」，并打算据此得出「Stage 1 不需要 `--permission-prompt-tool stdio`」。

**实测**（`artifacts/20260801T173642.530407Z/task-0.2/`）：开启 `--permission-prompt-tool stdio` 后，一次 `Read /etc/hostname` **确实产生了一个 `control_request`**：

```jsonl
{"type":"control_request","request_id":"95653b1b-…","request":{"subtype":"can_use_tool","tool_name":"Read","input":{"file_path":"/etc/hostname"}}}
```

`init.tools` 恰为 `["Glob","Grep","Read"]`，最终回复 `HOST:XPU-2`，成本 $0.0527。

### 触发条件必须写清楚——否则这条结论会被误用

本探针的配置是 `--permission-prompt-tool stdio` + `permissionMode: default` + `--setting-sources ""`。这**不是**生产配置。生产 Stage 1 用的是 `--permission-mode dontAsk` + `harness-settings.json` 的 `permissions.allow`，且**不带** stdio 标志——那种配置下 `Read` 在 allow 列表里被直接放行，不产生任何 `control_request`（这也是当前生产回路已经跑通过真实轮次的既有事实）。

所以准确表述是：**只读工具本身并不「豁免」权限门；它们在生产配置下不触发，是因为 allow 列表预先放行了，不是因为工具只读。**

### 对设计问题 5 的正式回答

**Stage 1 不启用 `--permission-prompt-tool stdio`。**

**但理由与计划原本写的不同**，这个区别必须记下来：

- 计划原本的理由：「只读工具不会触发权限请求，所以没有需要拦截的东西」——**这个前提是错的**。
- 实测支持的理由：**启用它反而会给每一次 `Read`/`Grep`/`Glob` 调用都带来一个必须应答的 `control_request`**。Stage 1 的扇出每轮最多 39 次子调用、每次可能多次读文件，等于凭空引入一个必须正确实现的 control 循环，而收益为零——工具集已被 `--tools` allowlist 收窄到只读三件，`permissions.allow` 是主防线，没有任何需要「拦截—校验—回填」的写操作。

**Stage 2（开发轮）情况相反**：届时 agent 会拿到 `Bash`/`Edit`/`Write`，`control_request` 的「拦截—校验—回填」正是控制器逐次审查写操作参数的手段，那时才需要它，并需在 Stage 2 的独立计划里设计其应答循环。

**遗留提醒**：ADR-002 记录的「本地分类器自动放行的安全 Bash 不产生 `can_use_tool`」仍然成立（PoC 实测），与本条不矛盾——两者共同说明 **`can_use_tool` 的触发面既不是「所有工具」也不是「所有写工具」，它取决于工具 × 权限模式 × allow 列表的组合，不能凭工具的读写属性推断**。

## 探针自身踩到的两个坑（记录以免下一个人重踩）

1. **`control_response` 回调必须返回完整信封**，不是内层 `{"behavior": ...}`。返回内层会被原样写进 stdin，CLI 收不到应答而**永久等待**——首次实测直接卡满 180 秒超时，而现场看起来像「模型没响应」，极易误诊。正确形状见 `driver.control_response(request_id, inner)`。

2. **PoC `driver.child_env()` 用的是 7 个名字的黑名单**，漏掉 `ANTHROPIC_MODEL`——父会话的 `opus[1m]` 泄漏进子进程，首次实测的 init 事件里 `model` 是 `claude-opus-5[1m]`（溢价档位）。生产 `claude_runner._sanitize_env()` 早已改为 `CLAUDE_*`/`ANTHROPIC_*` **前缀级 deny-by-default + 显式认证白名单**，不受影响；**这是 PoC 侧的遗留缺陷**。本探针的规避办法是显式传 `--model claude-sonnet-5`。

   若将来要把 `exp/stdio-driver/driver.py` 提升为可复用资产（例如搬进 `~/src/my-ade`），**必须先把这个黑名单换成前缀级白名单**，否则它会在新环境里重现同一个模型档位与成本失控问题。

## 验收判据核对

计划要求：「`CONCLUSIONS.md` 对待决 A 与设计问题 5 均给出 `confirmed` 或 `refuted` 结论，不遗留『假设』；设计问题 5 的结论必须基于『确实打开了拦截开关后观察到的结果』，不得基于『未打开开关时的沉默』。」

- 待决 A：`confirmed` ✅
- 设计问题 5：`confirmed` ✅，且**确实是在打开 `--permission-prompt-tool stdio` 之后观察到的**（观察到的是**阳性**结果，比计划预期的阴性结果更强——阳性结果不依赖「探测手段有效」这个前提）
- 无遗留「假设」✅
