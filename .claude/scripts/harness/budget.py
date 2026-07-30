"""事前预留式预算（spec §七）。

关键不变量：花钱之前先落盘预留。否则「崩溃 → 重启 → 再花一次」可无限越过日预算。
"""

from __future__ import annotations

import math
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
        """预留本轮预算。`round_id` 是幂等键（评审 Critical）。

        同一 `round_id` 重试且预留金额相同 → 直接返回既有 grant，不重复占用
        `budget_days.reserved_usd`。预留金额不同（多半是配置在两次调用间被
        改过）→ 抛 `BudgetError`，拒绝在不确定语义下悄悄叠加。round 已结算
        （`ended_at` 非空）后再收到同 `round_id` 的 reserve → 同样拒绝，因为
        它对应的预留早已释放，重新预留会制造第二份幽灵占用。
        """
        self.conn.execute("BEGIN IMMEDIATE")
        try:
            existing = self.conn.execute(
                "SELECT reserved_usd, ended_at FROM rounds WHERE round_id=?",
                (round_id,)).fetchone()
            if existing is not None:
                if existing["ended_at"] is not None:
                    raise BudgetError(
                        f"round {round_id} 已结算，不能重新 reserve")
                if not math.isclose(existing["reserved_usd"], self.round_budget,
                                    rel_tol=1e-9, abs_tol=1e-9):
                    raise BudgetError(
                        f"round {round_id} 已用不同预留金额 reserve 过："
                        f"{existing['reserved_usd']:.2f} != "
                        f"{self.round_budget:.2f}")
                self.conn.execute("COMMIT")
                return self.round_budget  # 幂等：同 round 重试，返回既有 grant
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
                "INSERT INTO rounds(round_id, mode, started_at,"
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

        `actual_usd` 超过预留时**不截断**：全额记入 `settled_usd`（评审
        Important #1），并把该 round 标记为 `budget_breach`，让账本能解释
        「花得比预留多」而不是悄悄吞掉差额——差额会持续占用当日预算，后续
        `reserve()` 会据此正确拒绝超支。未知 round 抛 `BudgetError`（评审
        Minor #4），只有真正已结算过的 round 才 no-op。
        """
        if not math.isfinite(actual_usd):
            raise BudgetError(f"actual_usd 必须是有限数值，收到 {actual_usd!r}")
        if actual_usd < 0:
            raise BudgetError(f"actual_usd 不得为负数，收到 {actual_usd!r}")
        self.conn.execute("BEGIN IMMEDIATE")
        try:
            row = self.conn.execute(
                "SELECT reserved_usd, ended_at FROM rounds WHERE round_id=?",
                (round_id,)).fetchone()
            if row is None:
                raise BudgetError(f"未知 round：{round_id}")
            if row["ended_at"] is not None:
                self.conn.execute("COMMIT")
                return
            reserved = row["reserved_usd"]
            charged = actual_usd  # 足额入账，不按 reserved 截断
            breach = actual_usd > reserved
            self.conn.execute(
                "UPDATE budget_days SET reserved_usd = MAX(reserved_usd - ?, 0),"
                " settled_usd = settled_usd + ? WHERE day=?",
                (reserved, charged, day))
            if breach:
                self.conn.execute(
                    "UPDATE rounds SET settled_usd=?, ended_at=?,"
                    " result='budget_breach' WHERE round_id=?",
                    (charged, time.time(), round_id))
            else:
                self.conn.execute(
                    "UPDATE rounds SET settled_usd=?, ended_at=? WHERE round_id=?",
                    (charged, time.time(), round_id))
            self.conn.execute("COMMIT")
        except Exception:
            self.conn.execute("ROLLBACK")
            raise

    def abandon(self, round_id: str, day: str) -> None:
        """结果未知：按该 round 的预留全额计费。同样幂等。未知 round 抛错。"""
        row = self.conn.execute(
            "SELECT reserved_usd FROM rounds WHERE round_id=?",
            (round_id,)).fetchone()
        if row is None:
            raise BudgetError(f"未知 round：{round_id}")
        self.settle(round_id, day, row["reserved_usd"])

    def record_invocation(self, round_id: str, invocation_id: str,
                           cost_usd: float) -> None:
        """记一次调用的实际花费，以 `invocation_id` 为幂等键（评审 Important #3）。

        控制器在「收到 result」与「记账完成」之间崩溃并重放同一 result 时，
        重复调用本方法必须只计一次——否则重放会重复计费。用独立的
        `invocations` 表（主键 `invocation_id`）去重，而不是直接对
        `rounds.settled_usd` 做无去重的累加。
        """
        self.conn.execute("BEGIN IMMEDIATE")
        try:
            self.conn.execute(
                "INSERT OR IGNORE INTO invocations(invocation_id, round_id,"
                " cost_usd, created_at) VALUES(?,?,?,?)",
                (invocation_id, round_id, cost_usd, time.time()))
            self.conn.execute("COMMIT")
        except Exception:
            self.conn.execute("ROLLBACK")
            raise

    def remaining_grant(self, round_id: str) -> float:
        row = self.conn.execute(
            "SELECT reserved_usd FROM rounds WHERE round_id=?",
            (round_id,)).fetchone()
        if row is None:
            return 0.0
        spent = self.conn.execute(
            "SELECT COALESCE(SUM(cost_usd),0) AS spent FROM invocations"
            " WHERE round_id=?", (round_id,)).fetchone()["spent"]
        return max(row["reserved_usd"] - spent, 0.0)
