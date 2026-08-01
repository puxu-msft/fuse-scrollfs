import datetime as dt
import tempfile, unittest
from pathlib import Path
from harness import db
from harness.budget import Budget, BudgetError

DAY = "2026-07-30"


class TestBudget(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        # 幂等：测试体内可能已手动 close()，tearDown 再 close 一次是安全的
        # no-op（sqlite3.Connection.close 允许重复调用）。
        self.addCleanup(self.conn.close)
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
        而不是 1.0；超支由独立于业务 result 的金额谓词承载。"""
        self.b.reserve("r1", DAY)
        self.b.settle("r1", DAY, 5.0)
        self.assertAlmostEqual(self.b.spent_today(DAY), 5.0)
        row = self.conn.execute(
            "SELECT reserved_usd, settled_usd FROM rounds WHERE round_id=?",
            ("r1",)).fetchone()
        self.assertAlmostEqual(row["settled_usd"], 5.0)
        self.assertGreater(row["settled_usd"], row["reserved_usd"])
        self.assertTrue(self.b.breached("r1"))

    def test_normal_business_outcome_does_not_erase_budget_breach(self):
        """预留 X、实花 X+δ 后正常发布：业务结果与超支事实必须同时可读，
        且 settle 不得暂借业务 result 字段承载 breach。"""
        self.b.reserve("r1", DAY)
        self.b.settle("r1", DAY, 1.2)
        settled = self.conn.execute(
            "SELECT result, reserved_usd, settled_usd FROM rounds WHERE round_id=?",
            ("r1",),
        ).fetchone()
        self.assertIsNone(settled["result"])
        self.assertGreater(settled["settled_usd"], settled["reserved_usd"])
        self.assertTrue(self.b.breached("r1"))

        self.b.record_outcome("r1", result="published")
        recorded = self.conn.execute(
            "SELECT result, reserved_usd, settled_usd FROM rounds WHERE round_id=?",
            ("r1",),
        ).fetchone()
        self.assertEqual(recorded["result"], "published")
        self.assertGreater(recorded["settled_usd"], recorded["reserved_usd"])
        self.assertTrue(self.b.breached("r1"))

    def test_non_breach_remains_distinguishable_from_breach(self):
        self.b.reserve("r1", DAY)
        self.b.settle("r1", DAY, 0.8)
        self.b.record_outcome("r1", result="published")
        self.assertFalse(self.b.breached("r1"))

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

    def test_open_round_record_does_not_touch_daily_budget(self):
        """恢复轮/早期失败轮不消耗新预算：`open_round_record()` 只建一条
        `reserved_usd=0` 的账本行，绝不占用 `budget_days`（评审 Important
        #6/#7）。"""
        self.b.open_round_record("r-resume", mode="resume")
        row = self.conn.execute(
            "SELECT mode, reserved_usd FROM rounds WHERE round_id=?",
            ("r-resume",)).fetchone()
        self.assertEqual(row["mode"], "resume")
        self.assertAlmostEqual(row["reserved_usd"], 0.0)
        day_row = self.conn.execute(
            "SELECT reserved_usd, settled_usd FROM budget_days"
            " WHERE day=?", (DAY,)).fetchone()
        self.assertIsNone(day_row, "open_round_record 不得触碰 budget_days")

    def test_open_round_record_is_idempotent(self):
        self.b.open_round_record("r1", mode="resume")
        self.b.open_round_record("r1", mode="resume")  # 重复调用不得报错
        rows = self.conn.execute(
            "SELECT COUNT(*) AS n FROM rounds WHERE round_id=?",
            ("r1",)).fetchall()
        self.assertEqual(rows[0]["n"], 1)

    def test_record_outcome_updates_only_specified_fields(self):
        self.b.reserve("r1", DAY)
        self.b.record_outcome("r1", result="published", turns=5, denials=1,
                              exit_code=0)
        row = self.conn.execute(
            "SELECT mode, result, turns, denials, exit_code, reserved_usd"
            " FROM rounds WHERE round_id=?", ("r1",)).fetchone()
        self.assertEqual(row["result"], "published")
        self.assertEqual(row["turns"], 5)
        self.assertEqual(row["denials"], 1)
        self.assertEqual(row["exit_code"], 0)
        # mode 未在本次调用传入，不得被覆盖成 None
        self.assertEqual(row["mode"], "pending")
        # reserved_usd 完全不受影响——record_outcome 不得触碰金额字段
        self.assertAlmostEqual(row["reserved_usd"], 1.0)

    def test_record_outcome_is_idempotent(self):
        self.b.reserve("r1", DAY)
        self.b.record_outcome("r1", result="published", turns=5)
        self.b.record_outcome("r1", result="published", turns=5)  # 重放
        row = self.conn.execute(
            "SELECT result, turns FROM rounds WHERE round_id=?",
            ("r1",)).fetchone()
        self.assertEqual(row["result"], "published")
        self.assertEqual(row["turns"], 5)

    def test_record_outcome_unknown_round_raises(self):
        with self.assertRaises(BudgetError):
            self.b.record_outcome("no-such-round", result="published")

    def test_settle_orphaned_settles_against_its_own_started_at_day(self):
        """结算悬挂预留必须按该 round 自己 `started_at` 所在的日历日，不能
        用「今天」——否则会错误扣减不属于它的 `budget_days` 行，而真正持有
        那份预留的旧日期行则永久悬挂（评审 Critical #1 修法 2）。"""
        old_day = "2026-01-01"
        self.b.reserve("orphan", old_day)
        # 手工把 started_at 改到那一天对应的时间戳，模拟「昨天预留、今天恢复」
        old_ts = dt.datetime.fromisoformat(old_day).timestamp()
        self.conn.execute(
            "UPDATE rounds SET started_at=? WHERE round_id=?",
            (old_ts, "orphan"))

        self.b.settle_orphaned("orphan")

        old_day_row = self.conn.execute(
            "SELECT reserved_usd, settled_usd FROM budget_days"
            " WHERE day=?", (old_day,)).fetchone()
        self.assertAlmostEqual(old_day_row["reserved_usd"], 0.0)
        self.assertAlmostEqual(old_day_row["settled_usd"], 1.0)
        # 「今天」这一天完全不应被触及
        today_row = self.conn.execute(
            "SELECT * FROM budget_days WHERE day=?", (DAY,)).fetchone()
        self.assertIsNone(today_row)

    def test_settle_orphaned_is_idempotent(self):
        """`reserve()` 用 `time.time()` 写 `started_at`，而 `settle_orphaned()`
        按 `started_at` 反推日历日结算——与本文件里手工传入的 `DAY` 常量
        （用作 `budget_days.day` 的显式参数）并非同一件事。这里按
        `settle_orphaned()` 实际会计算出的真实今天日期查账，而不是硬编码
        的 `DAY` 常量，否则断言会查询一个从未被写入的行。"""
        real_today = dt.date.today().isoformat()
        self.b.reserve("orphan", real_today)
        self.b.settle_orphaned("orphan")
        self.b.settle_orphaned("orphan")  # 已结算，no-op
        row = self.conn.execute(
            "SELECT settled_usd FROM budget_days WHERE day=?",
            (real_today,)).fetchone()
        self.assertAlmostEqual(row["settled_usd"], 1.0)

    def test_settle_orphaned_unknown_round_raises(self):
        with self.assertRaises(BudgetError):
            self.b.settle_orphaned("no-such-round")


if __name__ == "__main__":
    unittest.main()
