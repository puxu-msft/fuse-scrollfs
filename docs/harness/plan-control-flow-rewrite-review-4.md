# 控制流重写计划 · 第四轮定点核验（原评审者）

> reviewed_at_rev: `85cb5ab7f0836a2180fea97e90aded1ef6b85f79` · 日期：2026-08-02
> **verdict: needs-fix** · 关闭 1 · 仍开/部分 3 · **阻塞 2**
> 范围严格限定为第三轮三个阻塞项 + rmf-08，未做全面复评（上轮「形态正确、新引入归零、七项设计要求保留」按 verified 采纳）。

> 落盘说明：评审者运行时约束仍禁止创建文件，内联交付，协调者代为固化，未作删改；转录未经其复核。

**转录核验**：`plan-control-flow-rewrite-review-3.md` 未发现实质转录失真。

## 协调者提问的回答：「只改这四条」是否造成不一致

**是。** 局部修复留下两处接缝不一致：
1. 联合测试把 **UUID 与明文 identity 段直接比较**；
2. per-attempt stream path **没有贯穿波次调度**；
3. retryability 表只覆盖 validate 层 schema 错误，**漏掉 parser 层同类失败**。

## 阻塞项

### cfr3-01 — 仍开（阻塞）

**问题**：联合测试要求 `request.session_id` 与 `attempt_key`、stream path 中的 `round_id:task_role:attempt` 段**逐字一致**，但 session ID 是 **UUIDv5，不可能与该明文段相等**——这条测试按规格写出来必然失败。

另有第二个缺口：契约**没有要求 attempt 2/3 重建 `stream_log`**，fork 后可能继续覆盖 attempt 1 的日志。

**必须的修复**：修正联合测试 oracle——分别计算预期 UUID、attempt key 与 stream path，再验证三者**使用同一个 `task_role`**（而非三者字面相等）；并增加重试波次断言：**每个 attempt 的 stream path 必须随 attempt 编号变化**。

### cfr3-02 — 仍开（阻塞）

**问题**：分类表漏掉**真实的 parser 层 schema 失败**。模型返回无法解析或缺 `candidates` 时：`InvocationResult.ok=False`、**`subtype='success'`**、protocol error 为 `unparseable or malformed payload...`。此时**进不了**表中「`invocation.ok=True` 且 validate 返回错误」的 schema 分支，`_classify_retryable` 的结果**未定义**。

另：`subtype=None` 被一律视为可重试，但 **CLI 启动/认证失败同样可能没有终态事件**。

**必须的修复**：补齐分类表与测试——parser 层模型输出失败**可重试**；timeout / 传输文本**可重试**；missing init、CLI 参数错误、认证失败**不可重试**。

## 非阻塞

### cfr3-03 — 已关闭

三项均已进入实际任务的不变量、测试清单与正控：Task 5.2 双写 `agentType`；Task 5.6 聚合 `protocol_errors`；Task 6.1 用 `_format_detail` 消费错误并断言全部请求显式使用 `DEFAULT_AGENT_MODEL`。

### rmf-08 — 部分关闭

权限位测试 `stat.S_IMODE(path.stat().st_mode) == 0o600` **可证伪**，成立。

两处问题：
- **「写完再 chmod」存在短暂的 umask 权限窗口**，宜直接以 mode 0600 创建。
- **放大倍数「7 倍」不诚实**：按「每 attempt 一个文件」且最多 3 次尝试，最坏为 **12 个 finder 文件 + 最多 27 个 judge 文件 = 39 个**。redline 短路也**不恒为 5 个**——被否决后还会继续裁决后续候选。

