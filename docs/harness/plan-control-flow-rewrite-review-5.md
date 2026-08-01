# 控制流重写计划 · 第五轮最终核验（原评审者）

> reviewed_at_rev: `b5ee9dc1aa987c1ff94c0ab89a2c9652756bfbfc` · 日期：2026-08-02
> **verdict: `ready`** · **阻塞项：0**
> 范围严格限定为第四轮两个阻塞项 + rmf-08 数字核验。

> 落盘说明：评审者运行时约束仍禁止创建文件，内联交付，协调者代为固化，未作删改；转录未经其复核。

## 结论

> 计划已足以开工 Phase 0；剩余实现细节应由计划规定的 TDD 红绿循环验证。

## cfr4-01 — 已关闭

**正控有效**：调度器按 `(role, attempt)` 调用 `make_request`。若改回不接 `attempt`，要么立即因参数数量不匹配失败，要么复用 attempt 1 路径使 fork 前后 `stream_log` 相同——**测试会红**。

订正后的联合 oracle 分别计算预期 session UUID、attempt key 与 stream path；若 `attempt_key` 误用裸 `role` 而 session 使用含 fingerprint 的 task role，**attempt key 断言会明确失败**。

## cfr4-02 — 已关闭

**已对照真实 `claude_runner.py` 核实**（未采信协调者与撰写者的转述）：

- `TimeoutExpired` 在 `invoke()` 中**直接返回**新 `InvocationResult`，不调用 `parse_stream_json` → `protocol_errors=[]`
- 普通进程退出仍调用 `parse_stream_json`，空或非法 stdout 会产生 `missing init event`

**故该区分依据成立。**

分类表按**命中即返回**排列：`duplicate` → `missing init` → `budget exhausted` → `parser failure` → `validate failure` → 默认传输失败。重叠组合由更具体规则优先处理，**语义无歧义**。

## rmf-08 数字核验 — 39 正确

推导：4 finder × 3 attempts = 12；最多 3 candidates × 3 judges × 3 attempts = 27；合计 **39**。

计划中的 `_MAX_RANKED_CANDIDATES` 明确为 3，旧 JS 路径也使用 `ranked.slice(0, 3)`。

## 五轮收敛轨迹

| 轮次 | 发现 | 关闭 | 新引入 | 阻塞 |
|---|---|---|---|---|
| 1（`2e8fbda`）| 19 | — | — | 10 Critical |
| 2（`8336241`）| — | 9 | **4** | 6 Critical |
| 3（`24b6e49`）| — | 7 | **0** | 3 |
| 4（`85cb5ab`）| — | 1 | 0 | 2 |
| 5（`b5ee9dc`）| — | 2 | 0 | **0** |

**新引入从 4 降到 0 并保持**，是本轮判 `ready` 的主要依据。
