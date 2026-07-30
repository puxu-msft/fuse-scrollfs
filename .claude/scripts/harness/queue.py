"""队列治理（spec §十二）。

两级去重：精确指纹硬拦；语义相近只报 possible_duplicate 交 judge 复核。
reconsider_when 必须是 typed 谓词，否则「自动失效」无从实现。
"""

from __future__ import annotations

import datetime as dt
import hashlib
import re
import sqlite3
import time

_WS = re.compile(r"\s+")


def _norm(text: str) -> str:
    return _WS.sub(" ", text.strip().lower())


def fingerprint(goal: str, invariant: str, primary_path: str, oracle: str) -> str:
    blob = "\x1f".join(_norm(x) for x in (goal, invariant, primary_path, oracle))
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()[:32]


class Queue:
    def __init__(self, conn: sqlite3.Connection):
        self.conn = conn

    def record(self, fp: str, lane: str, title: str, state: str,
               issue_number: int | None = None,
               reconsider_when: str | None = None) -> None:
        self.conn.execute(
            "INSERT OR REPLACE INTO proposals(fingerprint, lane, title, state,"
            " issue_number, reconsider_when, decided_at, created_at)"
            " VALUES(?,?,?,?,?,?,?,?)",
            (fp, lane, title, state, issue_number, reconsider_when,
             time.time(), time.time()))

    def _get(self, fp: str) -> sqlite3.Row | None:
        return self.conn.execute(
            "SELECT * FROM proposals WHERE fingerprint=?", (fp,)).fetchone()

    def classify(self, candidate: dict) -> str:
        row = self._get(candidate["fingerprint"])
        if row is None:
            return "new"
        if row["state"] in ("rejected", "closed-by-user"):
            return "new" if self.reconsider_ready(
                candidate["fingerprint"], candidate.get("ctx", {})) \
                else "rejected_active"
        return "exact_duplicate"

    def reconsider_ready(self, fp: str, ctx: dict) -> bool:
        row = self._get(fp)
        if row is None or not row["reconsider_when"]:
            return False
        cond = row["reconsider_when"]
        kind, _, arg = cond.partition(":")
        if kind == "not_before":
            try:
                return dt.date.today() >= dt.date.fromisoformat(arg)
            except ValueError:
                return False
        if kind == "main_sha_changed":
            return bool(ctx.get("main_sha")) and ctx["main_sha"] != arg
        if kind == "dependency_issue_closed":
            return arg in {str(n) for n in ctx.get("closed_issues", [])}
        if kind == "decision_version_gt":
            try:
                return int(ctx.get("decision_version", 0)) > int(arg)
            except (TypeError, ValueError):
                return False
        # 无法机器判定：只能人工复议
        return False

    def lane_full(self, lane: str, cap: int) -> bool:
        n = self.conn.execute(
            "SELECT COUNT(*) AS n FROM proposals WHERE lane=? AND state='proposed'",
            (lane,)).fetchone()["n"]
        return n >= cap

    def total_full(self, cap: int) -> bool:
        n = self.conn.execute(
            "SELECT COUNT(*) AS n FROM proposals WHERE state='proposed'"
        ).fetchone()["n"]
        return n >= cap
