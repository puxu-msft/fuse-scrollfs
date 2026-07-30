"""事前预留式预算（spec §七）。

关键不变量：花钱之前先落盘预留。否则「崩溃 → 重启 → 再花一次」可无限越过日预算。
"""

from __future__ import annotations

import sqlite3
import time


class BudgetError(Exception):
    pass


class Budget:
    def __init__(self, conn: sqlite3.Connection, round_budget_usd: float,
                 daily_budget_usd: float):
        self.conn = conn
        self.round_budget = round_budget_usd
        self.daily_budget = daily_budget_usd

    def _day_row(self, day: str) -> sqlite3.Row:
        self.conn.execute(
            "INSERT OR IGNORE INTO budget_days(day, reserved_usd, settled_usd)"
            " VALUES(?,0,0)", (day,))
        return self.conn.execute("SELECT * FROM budget_days WHERE day=?",
                                 (day,)).fetchone()

    def spent_today(self, day: str) -> float:
        row = self._day_row(day)
        # 已结算 + 尚未结算的预留，两者都算已占用
        return row["settled_usd"] + row["reserved_usd"]

    def reserve(self, round_id: str, day: str) -> float:
        self.conn.execute("BEGIN IMMEDIATE")
        try:
            row = self._day_row(day)
            occupied = row["settled_usd"] + row["reserved_usd"]
            if occupied + self.round_budget > self.daily_budget:
                raise BudgetError(
                    f"日预算不足：已占用 {occupied:.2f} + 本轮 {self.round_budget:.2f}"
                    f" > 上限 {self.daily_budget:.2f}")
            self.conn.execute(
                "UPDATE budget_days SET reserved_usd = reserved_usd + ?"
                " WHERE day=?", (self.round_budget, day))
            self.conn.execute(
                "INSERT OR REPLACE INTO rounds(round_id, mode, started_at,"
                " reserved_usd, denials) VALUES(?, 'pending', ?, ?, 0)",
                (round_id, time.time(), self.round_budget))
            self.conn.execute("COMMIT")
        except Exception:
            self.conn.execute("ROLLBACK")
            raise
        return self.round_budget

    def settle(self, round_id: str, day: str, actual_usd: float) -> None:
        """幂等：已结算的 round 再次调用是 no-op（评审 I-09）。

        释放额度用该 round **实际记录的 reserved_usd**，而不是当前配置值——
        配置改过之后用配置值会释放错数量。
        """
        self.conn.execute("BEGIN IMMEDIATE")
        try:
            row = self.conn.execute(
                "SELECT reserved_usd, ended_at FROM rounds WHERE round_id=?",
                (round_id,)).fetchone()
            if row is None or row["ended_at"] is not None:
                self.conn.execute("COMMIT")
                return
            reserved = row["reserved_usd"]
            charged = min(max(actual_usd, 0.0), reserved)
            self.conn.execute(
                "UPDATE budget_days SET reserved_usd = MAX(reserved_usd - ?, 0),"
                " settled_usd = settled_usd + ? WHERE day=?",
                (reserved, charged, day))
            self.conn.execute(
                "UPDATE rounds SET settled_usd=?, ended_at=? WHERE round_id=?",
                (charged, time.time(), round_id))
            self.conn.execute("COMMIT")
        except Exception:
            self.conn.execute("ROLLBACK")
            raise

    def abandon(self, round_id: str, day: str) -> None:
        """结果未知：按该 round 的预留全额计费。同样幂等。"""
        row = self.conn.execute(
            "SELECT reserved_usd FROM rounds WHERE round_id=?",
            (round_id,)).fetchone()
        self.settle(round_id, day, row["reserved_usd"] if row else 0.0)

    def record_invocation(self, round_id: str, cost_usd: float) -> None:
        self.conn.execute(
            "UPDATE rounds SET settled_usd = COALESCE(settled_usd,0) + ?"
            " WHERE round_id=?", (cost_usd, round_id))

    def remaining_grant(self, round_id: str) -> float:
        row = self.conn.execute(
            "SELECT reserved_usd, COALESCE(settled_usd,0) AS spent FROM rounds"
            " WHERE round_id=?", (round_id,)).fetchone()
        if row is None:
            return 0.0
        return max(row["reserved_usd"] - row["spent"], 0.0)
