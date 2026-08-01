"""`agent_attempts` 谱系账本：记录每个角色每次尝试的会话身份与终态。

本模块刻意做成**纯数据层**——它按调用方给的值写表，不推断、不派生。语义约束
由 Phase 5 的调用方保证，但两条最容易搞错的记在这里，因为读代码的人先看到的是这里：

1. **`session_id` 的含义随 `attempt` 变化**（评审 cfr2-07）：
   - `attempt == 1` → 存 `session_identity.derive_session_id()` 的**预派生**值
   - `attempt >= 2` → 必须存 **CLI 实际返回的**新 session id，**不是**预派生值

   为什么：fork 重试走 `--resume <sid> --fork-session`，而 CLI 会分配一个**新**
   session id。若拿预分配 ID 冒充「实际 ID」，下一次 fork 会去 resume 一个**从未被
   CLI 创建过的会话**。评审原话：这等于把 Phase 8 测试里的 fake 挪进了生产重试路径。
   同理 `parent_session_id` 必须指向上一次的**真实** id。

2. **`ATTEMPT_STATUSES` 与建表 CHECK 是同一语义的两份真相**，必须逐字一致。
   `test_ledger.TestStatusVocabularyIsPinned` 把两者钉在一起——改一处忘另一处会被
   它拦下，而不是等到某个合法状态写不进去时抛 `sqlite3.IntegrityError`（那时的表现
   像数据问题，不像配置漂移）。

   注意状态词里**没有 `degraded`**：「降级」是编排层对「重试耗尽」的结论，不是单次
   attempt 的状态。单次 attempt 只有成功 / 传输失败 / 能力漂移三种终态。
"""

from __future__ import annotations

import sqlite3
import time

ATTEMPT_STATUSES = frozenset(
    {"running", "success", "failed_transport", "capability_drift"}
)

# 终态集合：**由完整词表派生**，不是第三份硬编码（评审 cfr-p12-merged-03）。
# `record_attempt_finished` 此前用整个 `ATTEMPT_STATUSES` 校验，于是可以把一行
# 标成 `running` 却同时写入 `ended_at`——按 status 判它还在跑、按 ended_at 判它
# 已结束，一条自相矛盾的审计行。数据库词表保持四值（`running` 是合法的行状态），
# 只是 finished 这个**动作**不接受它。
ATTEMPT_TERMINAL_STATUSES = ATTEMPT_STATUSES - {"running"}


def record_attempt_started(
    conn: sqlite3.Connection,
    *,
    round_id: str,
    role: str,
    attempt: int,
    session_id: str,
    parent_session_id: str | None,
) -> None:
    attempt_key = f"{round_id}:{role}:{attempt}"
    conn.execute(
        "INSERT INTO agent_attempts("
        " attempt_key, round_id, role, attempt, session_id, parent_session_id,"
        " status, created_at) VALUES(?,?,?,?,?,?,?,?)",
        (
            attempt_key,
            round_id,
            role,
            attempt,
            session_id,
            parent_session_id,
            "running",
            time.time(),
        ),
    )


def record_attempt_finished(
    conn: sqlite3.Connection,
    *,
    attempt_key: str,
    status: str,
    cost_usd: float,
    turns: int,
) -> None:
    if status not in ATTEMPT_TERMINAL_STATUSES:
        raise ValueError(
            f"invalid terminal attempt status: {status!r}"
            f"（合法终态：{sorted(ATTEMPT_TERMINAL_STATUSES)}；"
            f"'running' 是行状态，不是可结算的终态）")
    conn.execute(
        "UPDATE agent_attempts SET status=?, cost_usd=?, turns=?, ended_at=?"
        " WHERE attempt_key=?",
        (status, cost_usd, turns, time.time(), attempt_key),
    )


def attempts_for_round(conn: sqlite3.Connection, round_id: str) -> list[dict]:
    rows = conn.execute(
        "SELECT * FROM agent_attempts WHERE round_id=? ORDER BY attempt, role",
        (round_id,),
    ).fetchall()
    return [dict(row) for row in rows]
