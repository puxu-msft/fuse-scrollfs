from __future__ import annotations

import sqlite3
from pathlib import Path

SCHEMA = """
CREATE TABLE IF NOT EXISTS operations (
    operation_id TEXT PRIMARY KEY,
    round_id     TEXT NOT NULL,
    kind         TEXT NOT NULL,
    natural_key  TEXT NOT NULL,
    payload_json TEXT NOT NULL DEFAULT '{}',
    payload_hash TEXT NOT NULL,
    phase        TEXT NOT NULL CHECK (phase IN
                   ('prepared','observed','settled',
                    'failed_retryable','failed_terminal')),
    commit_sha   TEXT,
    result_json  TEXT,
    created_at   REAL NOT NULL,
    updated_at   REAL NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_operations_natural
    ON operations(kind, natural_key);
CREATE INDEX IF NOT EXISTS idx_operations_round ON operations(round_id);

CREATE TABLE IF NOT EXISTS rounds (
    round_id      TEXT PRIMARY KEY,
    mode          TEXT NOT NULL,
    started_at    REAL NOT NULL,
    ended_at      REAL,
    reserved_usd  REAL NOT NULL DEFAULT 0,
    settled_usd   REAL,
    turns         INTEGER,
    denials       INTEGER NOT NULL DEFAULT 0,
    result        TEXT,
    exit_code     INTEGER
);

CREATE TABLE IF NOT EXISTS budget_days (
    day           TEXT PRIMARY KEY,
    reserved_usd  REAL NOT NULL DEFAULT 0,
    settled_usd   REAL NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS proposals (
    fingerprint     TEXT PRIMARY KEY,
    operation_id    TEXT,
    issue_number    INTEGER,
    lane            TEXT NOT NULL,
    title           TEXT NOT NULL,
    state           TEXT NOT NULL,
    reconsider_when TEXT,
    decided_at      REAL,
    created_at      REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_proposals_state ON proposals(state);

-- 提案的 canonical key（评审 rmf-02）。单独一张纯追加表而不是给 proposals 加列：
-- 既有表定义不改是本库的不变量。proposals 只存 sha256 摘要，摘要不可逆，因此
-- 跨轮去重所需的原文 key 必须单独留存，否则 known_canonical_keys 永远只能传空集，
-- 跨轮去重等于关闭。
CREATE TABLE IF NOT EXISTS proposal_keys (
    fingerprint   TEXT PRIMARY KEY,
    canonical_key TEXT NOT NULL,
    created_at    REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS invocations (
    invocation_id TEXT PRIMARY KEY,
    round_id      TEXT NOT NULL,
    cost_usd      REAL NOT NULL,
    created_at    REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_invocations_round ON invocations(round_id);
"""


def connect(path: Path) -> sqlite3.Connection:
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(path, isolation_level=None)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA foreign_keys=ON")
    conn.execute("PRAGMA synchronous=FULL")
    conn.row_factory = sqlite3.Row
    return conn


def migrate(conn: sqlite3.Connection) -> None:
    conn.executescript(SCHEMA)
