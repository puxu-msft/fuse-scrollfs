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

    def test_settle_releases_unused_reservation(self):
        self.b.reserve("r1", DAY)
        self.b.settle("r1", DAY, 0.2)
        self.assertAlmostEqual(self.b.spent_today(DAY), 0.2)
        self.b.reserve("r2", DAY)
        self.b.settle("r2", DAY, 0.3)
        self.assertAlmostEqual(self.b.spent_today(DAY), 0.5)

    def test_abandon_charges_full_reservation(self):
        self.b.reserve("r1", DAY)
        self.b.abandon("r1", DAY)
        self.assertAlmostEqual(self.b.spent_today(DAY), 1.0)

    def test_remaining_grant_shrinks_across_invocations(self):
        self.b.reserve("r1", DAY)
        self.assertAlmostEqual(self.b.remaining_grant("r1"), 1.0)
        self.b.record_invocation("r1", 0.4)
        self.assertAlmostEqual(self.b.remaining_grant("r1"), 0.6)


if __name__ == "__main__":
    unittest.main()
