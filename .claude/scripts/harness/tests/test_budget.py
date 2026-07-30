import tempfile, unittest
from pathlib import Path
from harness import db
from harness.budget import Budget, BudgetError

DAY = "2026-07-30"


class TestBudget(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        db.migrate(self.conn)
        self.b = Budget(self.conn, round_budget_usd=1.0, daily_budget_usd=2.5)

    def tearDown(self):
        self.tmp.cleanup()

    def test_reserve_is_durable_before_spending(self):
        grant = self.b.reserve("r1", DAY)
        self.assertEqual(grant, 1.0)
        row = self.conn.execute("SELECT reserved_usd FROM budget_days WHERE day=?",
                                (DAY,)).fetchone()
        self.assertEqual(row["reserved_usd"], 1.0)

    def test_crash_before_settle_still_counts_against_daily_budget(self):
        """崩溃 → 重启 → 再花一次，必须被日预算拦住。"""
        for i in range(2):
            self.b.reserve(f"r{i}", DAY)  # 预留后崩溃，从不结算
        with self.assertRaises(BudgetError):
            self.b.reserve("r3", DAY)

    def test_crash_before_settle_survives_real_reconnect(self):
        """重启不是同对象再调一次——必须真的关闭连接、重开、重建 Budget 实例，
        证明预留确已持久化到磁盘而非只存活在进程内存里。"""
        self.b.reserve("r0", DAY)
        self.conn.close()

        conn2 = db.connect(Path(self.tmp.name) / "h.db")
        b2 = Budget(conn2, round_budget_usd=1.0, daily_budget_usd=2.5)
        b2.reserve("r1", DAY)  # 占用 2.0，剩 0.5 < round_budget 1.0
        with self.assertRaises(BudgetError):
            b2.reserve("r2", DAY)
        conn2.close()

    def test_repeated_reserve_same_round_is_idempotent(self):
        """同一 round_id 重试 reserve 不得叠加全局预留（评审 Critical）。

        修复前：reserve("r") 两次 -> budget_days.reserved_usd 变成 2、
        rounds 表只记一份，settle("r") 只释放 1，另一份永久留在
        reserved_usd 里。这里断言：重复 reserve 后，reserved_usd 仍是
        一份的量；settle 一次即可把它完全释放至 0。
        """
        grant1 = self.b.reserve("r1", DAY)
        grant2 = self.b.reserve("r1", DAY)  # 同 round_id 重试
        self.assertEqual(grant1, grant2)
        row = self.conn.execute("SELECT reserved_usd FROM budget_days WHERE day=?",
                                (DAY,)).fetchone()
        self.assertAlmostEqual(row["reserved_usd"], 1.0)  # 不是 2.0

        rounds_row = self.conn.execute(
            "SELECT reserved_usd FROM rounds WHERE round_id=?", ("r1",)).fetchone()
        self.assertAlmostEqual(rounds_row["reserved_usd"], 1.0)

        self.b.settle("r1", DAY, 0.0)
        row = self.conn.execute("SELECT reserved_usd, settled_usd FROM budget_days"
                                " WHERE day=?", (DAY,)).fetchone()
        self.assertAlmostEqual(row["reserved_usd"], 0.0)  # 完全释放，无幽灵残留
        self.assertAlmostEqual(row["settled_usd"], 0.0)

    def test_reserve_conflicting_amount_for_same_round_raises(self):
        """同一 round_id 若以不同预留金额重试，说明配置在两次调用间被改过，
        语义不确定，必须拒绝而不是悄悄接受某一个值。"""
        self.b.reserve("r1", DAY)
        b_diff = Budget(self.conn, round_budget_usd=1.5, daily_budget_usd=2.5)
        with self.assertRaises(BudgetError):
            b_diff.reserve("r1", DAY)

    def test_reserve_after_settle_for_same_round_raises(self):
        """round 已结算后，预留早已释放；同 round_id 再次 reserve 会制造
        新的一份占用，必须拒绝而不是当作幂等重试处理。"""
        self.b.reserve("r1", DAY)
        self.b.settle("r1", DAY, 0.5)
        with self.assertRaises(BudgetError):
            self.b.reserve("r1", DAY)

    def test_settle_releases_unused_reservation(self):
        self.b.reserve("r1", DAY)
        self.b.settle("r1", DAY, 0.2)
        self.assertAlmostEqual(self.b.spent_today(DAY), 0.2)
        self.b.reserve("r2", DAY)
        self.b.settle("r2", DAY, 0.3)
        self.assertAlmostEqual(self.b.spent_today(DAY), 0.5)

    def test_settle_overrun_is_not_silently_truncated(self):
        """实际花费超过预留时必须足额入账，不能截断到 reserved（评审
        Important #1）。这里预留 1.0、实花 5.0，spent_today 必须反映 5.0
        而不是 1.0，round 要被标记为 budget_breach。"""
        self.b.reserve("r1", DAY)
        self.b.settle("r1", DAY, 5.0)
        self.assertAlmostEqual(self.b.spent_today(DAY), 5.0)
        row = self.conn.execute(
            "SELECT settled_usd, result FROM rounds WHERE round_id=?",
            ("r1",)).fetchone()
        self.assertAlmostEqual(row["settled_usd"], 5.0)
        self.assertEqual(row["result"], "budget_breach")

    def test_settle_negative_or_nonfinite_actual_usd_rejected(self):
        self.b.reserve("r1", DAY)
        with self.assertRaises(BudgetError):
            self.b.settle("r1", DAY, -0.1)
        with self.assertRaises(BudgetError):
            self.b.settle("r1", DAY, float("nan"))
        with self.assertRaises(BudgetError):
            self.b.settle("r1", DAY, float("inf"))

    def test_settle_unknown_round_raises(self):
        with self.assertRaises(BudgetError):
            self.b.settle("no-such-round", DAY, 0.1)

    def test_abandon_unknown_round_raises(self):
        with self.assertRaises(BudgetError):
            self.b.abandon("no-such-round", DAY)

    def test_abandon_charges_full_reservation(self):
        self.b.reserve("r1", DAY)
        self.b.abandon("r1", DAY)
        self.assertAlmostEqual(self.b.spent_today(DAY), 1.0)

    def test_remaining_grant_shrinks_across_invocations(self):
        self.b.reserve("r1", DAY)
        self.assertAlmostEqual(self.b.remaining_grant("r1"), 1.0)
        self.b.record_invocation("r1", "inv-1", 0.4)
        self.assertAlmostEqual(self.b.remaining_grant("r1"), 0.6)

    def test_record_invocation_is_idempotent_on_replay(self):
        """崩溃后重放同一 invocation 的 result 不得重复计费（评审
        Important #3）：同一 invocation_id 多次 record_invocation 只计一次。
        """
        self.b.reserve("r1", DAY)
        self.b.record_invocation("r1", "inv-1", 0.4)
        self.b.record_invocation("r1", "inv-1", 0.4)  # 重放
        self.b.record_invocation("r1", "inv-1", 0.4)  # 再重放
        self.assertAlmostEqual(self.b.remaining_grant("r1"), 0.6)


if __name__ == "__main__":
    unittest.main()
