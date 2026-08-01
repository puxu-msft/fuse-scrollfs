from __future__ import annotations

import sqlite3
import time

ATTEMPT_STATUSES = frozenset(
    {"running", "success", "failed_transport", "capability_drift"}
)


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
    if status not in ATTEMPT_STATUSES:
        raise ValueError(f"invalid attempt status: {status!r}")
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
