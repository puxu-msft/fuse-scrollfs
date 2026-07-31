---
report_id: stdio-driver-poc
attempt_id: stdio-driver-poc-01
status: settled
reviewed_at_rev: 6e172532386589d2d256ce94d48f6a787ddf322a
claude_cli_version: 2.1.220
---

# Claude CLI stdio 双 pipe PoC 结论

## 假设、成功判据与预算

本 PoC 验证：Python 控制器能以 stdin/stdout 双 pipe 持有真实 `claude --print --verbose --input-format stream-json --output-format stream-json` 进程的回合循环，并以 `type:"result"` 结算每轮；能拦截需授权工具、拒绝或通过 `updatedInput` 改写；能隔离失败会话并只 fork/retry 该会话；能用 `--agents <json>` 内联 agent 定义。六项均以实际发送字节、实际 stdout JSONL、文件系统效果、会话 ID 和 `total_cost_usd` 为 oracle；只看到进程退出 0 不算通过。

预算上限为 $3；每个真实调用均带 `--max-budget-usd`，单进程不超过 $0.30。预计 6 个场景、7～8 个真实进程，若每次接近 $0.30，最坏约 $2.40；脚本去除 `GH_TOKEN` 和 `GITHUB_TOKEN`，并把工具面收窄到各场景所需集合。

## 环境与复现

- 仓库：`/home/xp/src/zipfs`，测试修订：`6e172532386589d2d256ce94d48f6a787ddf322a`。
- CLI：`/home/xp/.local/bin/claude`，版本 `2.1.220`。
- Python：`/home/linuxbrew/.linuxbrew/bin/python3`，仅标准库。
- 工作目录：子进程固定在 `/tmp`；实验代码和证据只写入 `/home/xp/src/zipfs/exp/stdio-driver/`。
- 重跑：`cd /home/xp/src/zipfs && /home/linuxbrew/.linuxbrew/bin/python3 exp/stdio-driver/driver.py <scenario>`，其中 `<scenario>` 为 `basic`、`safe-bash`、`permission-deny`、`permission-rewrite`、`retry`、`agents` 或 `all`。
- 每次运行的 `artifacts/run-*/<scenario>/wire.in.bin` 与 `wire.out.bin` 分别保存确切 stdin/stdout 字节；`sent-index.jsonl` 额外保存每次写入的长度、Base64 与 Python `repr`。这些证据可能含提示词和模型回复，但不记录认证 token。

## Q1/Q2/Q3：双 pipe、回合结算 oracle 与多轮

finding_id: stdio-driver-poc-01  
conclusion_strength: confirmed  
hypothesis: stdin/stdout 同为 pipe 时，控制器可向长命 `claude` 进程逐行写 user JSONL，并在普通无后台任务回合中以 `type:"result"` 结算该轮；同一进程的下一轮保留上文。  
environment: Claude Code 2.1.220；`--tools ""`；`--replay-user-messages`；单进程预算 $0.30。  
reproduction: `/home/linuxbrew/.linuxbrew/bin/python3 /home/xp/src/zipfs/exp/stdio-driver/driver.py basic`。证据目录：`/home/xp/src/zipfs/exp/stdio-driver/artifacts/run-20260731T103554.216711Z-basic/basic-multiturn/`。  
result: 控制器实际写入两行，第一行确切 UTF-8 为 `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Remember codeword MANGO. Reply exactly FIRST."}]}}\n`，第二行同形，文本为 `What codeword did I tell you? Reply exactly CODE:<word>.`；原始字节和完整 hex 分别在 `wire.in.bin` 与命令输出中。stdout 每轮依次出现 `system/init → user replay → assistant → result`，两次 `result` 分别为 `FIRST` 与 `CODE:MANGO`，且 `session_id` 均为 `2cd3ae78-f7d0-4ee2-9a51-5ffefcf78339`。在这两个无工具普通回合中，每次 stdin 写入后到下一次写入前均恰有一个 `result`，总数为 2；`--replay-user-messages` 产生两个可观察的 `type:"user"` 回显，能按严格的输入顺序与随后第一个 `result` 配对，但 user replay 本身未携带独立 request ID。第一个 `result` 后子进程仍存活，第二轮准确回答 `CODE:MANGO`；关闭 stdin 后进程正常退出。两轮 `total_cost_usd` 分别为 `$0.0196725` 和累计 `$0.0218935`。Q6 的后台 Task 场景则实测到一次输入可产生第二个顶层 result，故“所有回合恰好一次”这一全称命题被 refuted。  
conclusion: 双 pipe 形态可行。精确输入格式是每行一个 `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"..."}]}}` 后接 LF。普通无后台任务回合可由控制器为每个进程维持 FIFO 的“已发送但未结算输入”队列，以该输入之后的第一个 `type:"result"` 出队；`--replay-user-messages` 有助于核对输入已被 CLI 接收和顺序未漂移，但它不是独立 correlation ID。若允许 `Task` 等后台机制，则不能假设恰好一个 result，必须额外识别 `origin:{kind:"task-notification"}`、任务状态和回合生命周期，或更稳妥地禁止该路径并采用一子任务一顶层进程。进程必须保持存活才能直接继续同一对话；关闭 stdin 会让 CLI 在已完成回合后退出。

脱敏 JSONL 片段：

```jsonl
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Remember codeword MANGO. Reply exactly FIRST."}]}}
{"type":"result","subtype":"success","is_error":false,"session_id":"2cd3…8339","total_cost_usd":0.0196725,"result":"FIRST"}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"What codeword did I tell you? Reply exactly CODE:<word>."}]}}
{"type":"result","subtype":"success","is_error":false,"session_id":"2cd3…8339","total_cost_usd":0.0218935,"result":"CODE:MANGO"}
```

## Q4：`--permission-prompt-tool stdio`

finding_id: stdio-driver-poc-02  
conclusion_strength: confirmed  
hypothesis: 隐藏标志能把需要授权的工具调用暴露为 `control_request{request.subtype:"can_use_tool"}`，控制器能以精确的 `control_response` deny，或以 `updatedInput` 替换参数后 allow；本地分类器认定安全的 Bash 不经过该门。  
environment: Claude Code 2.1.220；三个独立进程均带 `--permission-prompt-tool stdio`，分别只开放 `Bash` 或 `Write`，各预算 $0.30。  
reproduction: 依次运行 `driver.py safe-bash`、`driver.py permission-deny`、`driver.py permission-rewrite`。证据目录分别为 `artifacts/run-20260731T103643.201557Z-safe-bash/`、`artifacts/run-20260731T103812.032330Z-permission-deny/`、`artifacts/run-20260731T103823.675318Z-permission-rewrite/`。  
result: `printf SAFE_OK` 确实经 Bash 执行，事件序列包含 assistant tool use 与 user tool result，但 `control_request` 数量为 0，最终 `SAFE_DONE`，成本 `$0.04004525`，确认本机“安全命令自动放行、不产生 can_use_tool”的坑仍成立。对 `/tmp/stdio-driver-deny-marker.txt` 的 Write 请求产生一个 `can_use_tool`；控制器写回 `{"type":"control_response","response":{"request_id":"dd67…4bb6","subtype":"success","response":{"behavior":"deny","message":"PoC deny"}}}\n`，随后 stdout 还回显同一 `control_response`，文件未创建，`result.permission_denials` 记录该 Write，最终 `DENIED`，成本 `$0.0362215`。改写场景原请求为写 `/tmp/stdio-driver-rewrite-original.txt` 和 `ORIGINAL`；控制器写回 allow 并把 `updatedInput` 改为白名单内 `/home/xp/src/zipfs/exp/stdio-driver/rewrite-effective.txt` 与 `REWRITTEN`。原路径不存在，有效路径存在且内容逐字节为 `REWRITTEN`，证明替换发生在执行前；成本 `$0.00991125`。该回合工具已成功执行，但模型随后触发 cyber refusal，故“工具参数被机械改写并执行”已确认，“模型会正常总结改写后的路径”不成立也不用于结论。  
conclusion: stdio 权限往返、deny 和 `updatedInput` 改写均可行。精确响应信封是 `{"type":"control_response","response":{"request_id":"<control_request.request_id>","subtype":"success","response":{"behavior":"deny","message":"..."}}}` 或 allow 形态的 `response:{"behavior":"allow","updatedInput":{...}}`，每个 JSON 对象后接 LF。该机制不能作为完整权限边界，因为本地分类器自动放行的安全 Bash 绕过 `can_use_tool`；`--tools` allowlist 和既有 permissions 必须继续作为主边界。

脱敏 JSONL 片段：

```jsonl
{"type":"control_request","request_id":"dd67…4bb6","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/tmp/stdio-driver-deny-marker.txt","content":"HELLO"},"tool_use_id":"toolu_…"}}
{"type":"control_response","response":{"request_id":"dd67…4bb6","subtype":"success","response":{"behavior":"deny","message":"PoC deny"}}}
{"type":"control_request","request_id":"87d3…25ff","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/tmp/stdio-driver-rewrite-original.txt","content":"ORIGINAL"},"tool_use_id":"toolu_…"}}
{"type":"control_response","response":{"request_id":"87d3…25ff","subtype":"success","response":{"behavior":"allow","updatedInput":{"file_path":"/home/xp/src/zipfs/exp/stdio-driver/rewrite-effective.txt","content":"REWRITTEN"}}}}
```

## Q5：单个子任务失败后只重试自身

finding_id: stdio-driver-poc-03  
conclusion_strength: confirmed  
hypothesis: 控制器可预分配两个独立 session 并并行驱动；其中一个被控制器中断而失败时，另一个正常完成；随后只对失败 session 执行 `--resume <sid> --fork-session`，既保留失败会话的输入上下文，又产生新 session ID。  
environment: 两个并发进程分别用 `--session-id <uuid>`；目标进程开放 `Write` 并带 `--permission-prompt-tool stdio`，控制器在收到 Write 的 `can_use_tool` 后发送 stdio `interrupt`；兄弟与 fork retry 进程均 `--tools ""`；每个进程预算 $0.30。另有一个早期预算截断探针用于覆盖用户给出的失败例，但最终隔离结论以显式 interrupt 场景为主。  
reproduction: `/home/linuxbrew/.linuxbrew/bin/python3 /home/xp/src/zipfs/exp/stdio-driver/driver.py retry`。主要证据目录：`/home/xp/src/zipfs/exp/stdio-driver/artifacts/run-20260731T104743.480383Z-retry/`；预算截断补充证据：`artifacts/run-20260731T103925.787139Z-retry/`。  
result: 兄弟会话按预分配 ID `c655997a-bc03-42f2-8a60-c8056c0d5907` 返回 `SIBLING_OK`、退出 0、成本 `$0.0018175`。目标会话按预分配 ID `aaceb70b-fbbd-4ea5-bdb7-ad7eb4de7adc` 暴露 Write `can_use_tool`；控制器发送 `control_request{subtype:"interrupt"}`，得到匹配 `control_response{subtype:"success",response:{still_queued:[]}}`，随后该目标返回唯一 `result`：`is_error:true`、`subtype:"error_during_execution"`、`terminal_reason:"aborted_streaming"`、退出 1、成本 `$0.0047245`，且未创建目标文件。控制器未重启或重试兄弟会话，只执行 `--resume aace…7adc --fork-session`；fork 的 `system/init.session_id` 和 `result.session_id` 均为新 ID `d4a0c718-d6eb-43c7-ab6d-aaabadc9966d`，并准确从被中断任务的输入恢复 `RETRY:PEAR`，retry 成本 `$0.0050475`。补充的 `--max-budget-usd 0.001` 探针同样只让目标 session 返回 `error_max_budget_usd`，兄弟正常完成，随后 fork 恢复 `RETRY:PEAR`；但该探针实际累计 `$0.048197`，说明 CLI 预算是事后停止触发器，不能假设费用严格不超过阈值。  
conclusion: 以“一个子任务一个 session/process”的控制器模型可以只重试失败项，兄弟项不受影响；`--resume <failed_sid> --fork-session` 保留已持久化会话上下文并创建可审计的新谱系节点。这里确认的是 CLI 会话末尾 fork，不是 ADR 提到的任意消息级 `forkSession(upToMessageId)`；后者本 PoC 未验证。正式实现应优先用控制器的显式中断/超时作为确定性失败注入，并把 `--max-budget-usd` 视为停止触发器而非严格费用上限。

脱敏 JSONL 片段：

```jsonl
{"type":"result","subtype":"success","is_error":false,"session_id":"c655…5907","total_cost_usd":0.0018175,"result":"SIBLING_OK"}
{"type":"control_request","request_id":"b9d6…08af","request":{"subtype":"interrupt"}}
{"type":"control_response","response":{"subtype":"success","request_id":"b9d6…08af","response":{"still_queued":[]}}}
{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"aace…7adc","terminal_reason":"aborted_streaming","total_cost_usd":0.0047245}
{"type":"system","subtype":"init","session_id":"d4a0…966d","tools":[]}
{"type":"result","subtype":"success","is_error":false,"session_id":"d4a0…966d","total_cost_usd":0.0050475,"result":"RETRY:PEAR"}
```

## Q6：`--agents <json>` 内联 agent

finding_id: stdio-driver-poc-04  
conclusion_strength: confirmed  
hypothesis: `--agents` 能以内联 JSON 定义一个仓库外 agent，并且父模型能通过唯一开放的 `Task` 工具实际调用它，不依赖 `.claude/agents/*.md`。  
environment: `--agents '{"poc-inline":{"description":"Returns the requested marker for the stdio PoC.","prompt":"You are the inline PoC agent. When invoked, reply exactly INLINE_AGENT_OK.","tools":[]}}'`；`--tools Task`；单进程预算 $0.30；`--setting-sources ""`。  
reproduction: `/home/linuxbrew/.linuxbrew/bin/python3 /home/xp/src/zipfs/exp/stdio-driver/driver.py agents`。主要证据目录：`/home/xp/src/zipfs/exp/stdio-driver/artifacts/run-20260731T104216.079069Z-agents/inline-agent/`。  
result: `system/init.agents` 明确包含 `poc-inline`，且 `system/init.tools` 仅为 `Task`。父模型发出一个真实 `tool_use`：`name:"Task"`、`input.subagent_type:"poc-inline"`；stdout 随后出现 `system/task_started{subagent_type:"poc-inline"}`、带同一 `parent_tool_use_id` 的子 agent 文本 `INLINE_AGENT_OK`、`system/task_notification{status:"completed",summary:"INLINE_AGENT_OK"}`，最终父结果为 `PARENT_DONE`。主要重跑的首个 result 成本为 `$0.022261`。继续保持 stdin 打开并静默观察时，CLI 又自行启动一轮并发出第二个同内容 result；另一次在首个 result 后发送 interrupt，则产生第二个 `origin:{kind:"task-notification"}` 的 `error_during_execution` result。由 wire 顺序可见，`task_notification` 在首个 result 前已经完成，故第二轮不是尚未完成 agent 的晚到通知，而是长命 stream-json + `Task` 路径上的额外顶层采样/通知处理行为。  
conclusion: `--agents <json>` 可完全内联 agent 定义并被真实调用，因此未来迁移到 `~/src/my-ade` 不必依赖目标仓库内 `.claude/agents/*.md`。但它暴露了一个反例：带后台 `Task` 的一次 stdin 写入，顶层 `type:"result"` 在进程保持存活时可能不止一次；因此“每次输入后恰好一个 result”只在不产生后台 Task 的普通回合中 confirmed，而“任意回合都恰好一次”的全称命题被 refuted。正式 Python 编排不应把内联 `Task` 当 D1 的扇出实现；应直接一子任务一顶层 process/session，并以 `(process, sent-turn sequence)` 关联结果。`--agents` 可用于携带 persona/config，但不能单独修复后台任务竞态。

脱敏 JSONL 片段：

```jsonl
{"type":"system","subtype":"init","tools":["Task"],"agents":["claude","Explore","general-purpose","Plan","poc-inline","statusline-setup"]}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Task","input":{"subagent_type":"poc-inline","description":"Get PoC marker","prompt":"Please report your marker for the stdio PoC."}}]}}
{"type":"system","subtype":"task_started","subagent_type":"poc-inline","task_type":"local_agent"}
{"type":"assistant","parent_tool_use_id":"toolu_…","message":{"content":[{"type":"text","text":"INLINE_AGENT_OK"}]}}
{"type":"system","subtype":"task_notification","status":"completed","summary":"INLINE_AGENT_OK"}
{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.022261,"result":"PARENT_DONE"}
```

## 总结、风险与正式实现建议

总体判定：**有条件可行**，结论分量为“足以据此进入正式重写设计与实现”。核心 D0/D1/D2 均有真机证据：Python 能持有长命 stdio 对话；普通回合可按 `result` 结算并多轮保留上下文；权限请求可 deny/改写；独立 session 可隔离失败并只 fork/retry；`--agents` 可内联定义。条件是不得把“任意一次输入恰好一个 result”或“所有工具都经过 can_use_tool”当作不变量——后台 `Task` 和本地安全分类器分别给出反例。

正式实现建议：

1. D1 扇出采用“一子任务一顶层 `claude` process/session”，Python 持有 `task_id → session_id → attempt_id` 映射；不要让一个父模型在内部用 `Task` 扇出。
2. 普通顶层回合可使用每进程 FIFO：写 user JSONL，等待该输入之后的第一个顶层 `result`；同时记录 replayed user、`session_id`、`uuid` 和 `origin`。若工具面允许后台任务，则必须另建 task lifecycle 状态机，不可只数 `result`。
3. `--replay-user-messages` 保留，用作“输入被接受且顺序一致”的观测信号；不要把它误当 request ID。控制器应自有单调 `turn_seq`。
4. 收到 `control_request{can_use_tool}` 时按 `request_id` 回精确信封；deny 和 allow/`updatedInput` 都需要审计原输入、有效输入与效果。`--tools` allowlist 和 permissions 仍是主边界，stdio MITM 只是增强。
5. 失败后记录该 attempt 的最终 `result`、退出码与 session ID，再以 `--resume <sid> --fork-session` 新建 attempt；不能重试已成功兄弟。预算阈值不能代替真实费用累计器。
6. stdin EOF 会结束长命进程；正常关闭应先确认没有待处理 task/notification。若要主动停止正在执行的回合，可发送 `control_request{subtype:"interrupt"}` 并等匹配 response/result。

性能与资源观察：无工具普通回合约 1.3～2.8 秒；安全 Bash 约 5.8 秒；内联 agent 约 7～13 秒且上下文/成本明显更高。所有已执行探针的 `total_cost_usd` 按每个真实进程最终累计值去重后合计 `$0.51849725`，低于 $3；所有进程的 argv 单次上限均不超过 `$0.30`。未向子进程传 `GH_TOKEN`/`GITHUB_TOKEN`，未读取或写入生产 `harness.db`，未运行 `harness.cli round`。

## 未验证与边界

- 未验证 Python Agent SDK 的消息级 `forkSession(upToMessageId)`；本 PoC 只确认 CLI 会话末尾 `--resume --fork-session`。
- 未验证长期运行、数百轮、进程崩溃、半行 JSON、stdout backpressure、API 传输层真正断流后的恢复；当前失败注入是可复现的 stdio interrupt，预算截断仅作补充。
- 未验证所有 built-in tool 的分类；只确认一个无害 Bash 自动放行、一个 Write 进入 `can_use_tool`。
- 未验证后台 `Task` 第二个 result 的全部触发条件与版本稳定性；已确认它存在，足以否定全称“恰好一次”，但不据此推断所有 Task 都必然重复。

## Overall Verdict

conclusion_strength: confirmed  
verdict: 有条件可行。可以进入 D0/D1/D2 的正式实现，但必须采用顶层独立 process/session 扇出，并把后台 Task 与自动放行工具列为显式例外，而不是把 stdio result/can_use_tool 误写成全覆盖不变量。

## The three judgments I am least confident about

1. “后台 Task 的第二个 result 属于稳定协议语义”——仅为倾向，需更多版本和提示词样本；当前足以决定的是更窄结论：全称“恰好一个 result”已经被一个真实反例推翻。
2. “普通回合的第一个 result 可长期作为 FIFO 结算点”——足以支撑当前 D1 顶层独立进程设计，但长期压力、传输断流和多种工具组合尚未验证；正式实现需保留异常 origin/重复 result 的观测和报警。
3. “CLI fork 保留所有有价值的半途状态”——本次只证明输入上下文 `PEAR` 被保留，没有证明未完成工具输出、部分 assistant 文本或内存状态都可恢复；若这些状态承重，需单独加 oracle。

## Friction encountered while executing this contract

- 首个 Write 改写回合在工具已成功执行后触发上游 cyber refusal；通过文件系统 oracle 将“机械参数替换已生效”与“模型后续总结失败”分开，没有把 refusal 误判为改写失败。
- 极低 `--max-budget-usd 0.001` 实际仍产生 `$0.048197`，说明预算是滞后停止门；改用可复现的 stdio interrupt 作为主要失败注入。
- 初版证据收集在读到首个 result 后关闭/静默等待策略不严，后台 Task 又触发第二轮；补充了逐行接收时间戳和 interrupt 对照，确认这是真实反例而非文件拼接假象。

## Delivery Declaration

delivery_complete: true  
completed_at: 2026-07-31T10:53:00Z  
finding_total: 4  
confirmed: 4  
likely: 0  
inconclusive: 0  
refuted: 0


