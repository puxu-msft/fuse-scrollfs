import sqlite3
import tempfile
import unittest
from pathlib import Path

from harness import db
from harness import ledger


class TestLedger(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "harness.db")
        db.migrate(self.conn)

    def tearDown(self):
        self.conn.close()
        self.tmp.cleanup()

    def _start(
        self,
        *,
        role="finder:roadmap",
        attempt=1,
        session_id="session-1",
        parent_session_id=None,
    ):
        ledger.record_attempt_started(
            self.conn,
            round_id="round-1",
            role=role,
            attempt=attempt,
            session_id=session_id,
            parent_session_id=parent_session_id,
        )

    def test_started_then_finished_round_trip(self):
        self._start()
        ledger.record_attempt_finished(
            self.conn,
            attempt_key="round-1:finder:roadmap:1",
            status="success",
            cost_usd=0.125,
            turns=3,
        )

        attempts = ledger.attempts_for_round(self.conn, "round-1")
        self.assertEqual(len(attempts), 1)
        self.assertEqual(attempts[0]["status"], "success")
        self.assertEqual(attempts[0]["cost_usd"], 0.125)
        self.assertEqual(attempts[0]["turns"], 3)
        self.assertIsNotNone(attempts[0]["ended_at"])

    def test_fork_attempt_records_real_session_lineage(self):
        self._start(session_id="real-session-1")
        self._start(
            attempt=2,
            session_id="cli-returned-session-2",
            parent_session_id="real-session-1",
        )

        attempts = ledger.attempts_for_round(self.conn, "round-1")
        self.assertEqual([row["attempt"] for row in attempts], [1, 2])
        self.assertIsNone(attempts[0]["parent_session_id"])
        self.assertEqual(attempts[1]["session_id"], "cli-returned-session-2")
        self.assertEqual(attempts[1]["parent_session_id"], "real-session-1")

    def test_duplicate_attempt_key_raises_integrity_error(self):
        self._start()
        with self.assertRaises(sqlite3.IntegrityError):
            self._start()

    def test_unknown_round_returns_empty_list(self):
        self.assertEqual(ledger.attempts_for_round(self.conn, "missing"), [])

    def test_capability_drift_is_accepted_by_python_and_database(self):
        self._start()
        ledger.record_attempt_finished(
            self.conn,
            attempt_key="round-1:finder:roadmap:1",
            status="capability_drift",
            cost_usd=0.0,
            turns=1,
        )
        self.assertEqual(
            ledger.attempts_for_round(self.conn, "round-1")[0]["status"],
            "capability_drift",
        )

    def test_degraded_is_rejected_before_sql_execution(self):
        self._start()
        # 只断言「拒绝了、且消息点名了非法值」，不绑死措辞——本条曾因把错误
        # 消息改精确（invalid attempt status → invalid terminal attempt status）
        # 而误红，可行为一步没变。测的应是「SQL 之前被拒」这个性质。
        with self.assertRaisesRegex(ValueError, "degraded"):
            ledger.record_attempt_finished(
                self.conn,
                attempt_key="round-1:finder:roadmap:1",
                status="degraded",
                cost_usd=0.0,
                turns=1,
            )
        self.assertEqual(
            ledger.attempts_for_round(self.conn, "round-1")[0]["status"],
            "running",
        )


if __name__ == "__main__":
    unittest.main()


class TestStatusVocabularyIsPinned(unittest.TestCase):
    """`ATTEMPT_STATUSES` 与建表 CHECK 必须始终一致（评审 cfr2-04）。

    这是同一语义的第二份真相：Python 侧的集合与 SQL 侧的 CHECK 分开写，
    改一处忘另一处不会有任何报错——直到某个真实状态写不进去才炸，而那时
    表现是 `sqlite3.IntegrityError`，看起来像数据问题而不是配置漂移。

    本项目已经栽过同一形态一次（`STAGE1_TOOLS` 与 `STAGE1_ALLOWED_TOOLS`
    两份硬编码，加 `TaskOutput` 时立刻漂移）。那次靠入口强制拦下了，但正确的
    做法是让漂移**无法发生**——这条测试把两者钉在一起。
    """

    def test_python_set_matches_sql_check_constraint(self):
        import re as _re

        from harness import db as _db
        from harness.ledger import ATTEMPT_STATUSES

        schema = _db.SCHEMA if hasattr(_db, "SCHEMA") else None
        if schema is None:
            src = Path(_db.__file__).read_text(encoding="utf-8")
        else:
            src = schema
        block = _re.search(
            r"CREATE TABLE IF NOT EXISTS agent_attempts\b.*?\);",
            src, _re.S)
        self.assertIsNotNone(block, "未找到 agent_attempts 建表语句")
        check = _re.search(r"status\s+TEXT NOT NULL CHECK\s*\(\s*status IN\s*\((.*?)\)\s*\)",
                           block.group(0), _re.S)
        self.assertIsNotNone(check, "未找到 status 的 CHECK 约束")
        sql_statuses = set(_re.findall(r"'([a-z_]+)'", check.group(1)))
        self.assertEqual(
            sql_statuses, set(ATTEMPT_STATUSES),
            "ledger.ATTEMPT_STATUSES 与建表 CHECK 漂移了——"
            "改一处必须同步另一处，否则合法状态会在写入时抛 IntegrityError")


class TestFinishedRequiresTerminalStatus(unittest.TestCase):
    """`record_attempt_finished` 只接受**终态**（评审 cfr-p12-merged-03）。

    `ATTEMPT_STATUSES` 同时含运行态 `running` 与三个终态，而 finished 用整个
    集合校验——于是可以把一行标成 `running` 却同时写入 `ended_at`，产出一条
    **自相矛盾的审计行**：按 `status` 判它还在跑，按 `ended_at` 判它已结束。

    数据库词表保持四值不变（`running` 是合法的行状态），只是 finished 这个
    **动作**不接受它。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        db.migrate(self.conn)
        self.addCleanup(self.conn.close)
        ledger.record_attempt_started(
            self.conn, round_id="r1", role="finder:code", attempt=1,
            session_id="s1", parent_session_id=None)

    def test_running_is_rejected_as_a_finished_status(self):
        with self.assertRaises(ValueError):
            ledger.record_attempt_finished(
                self.conn, attempt_key="r1:finder:code:1",
                status="running", cost_usd=0.1, turns=1)

    def test_terminal_statuses_are_accepted(self):
        for status in ("success", "failed_transport", "capability_drift"):
            with self.subTest(status=status):
                ledger.record_attempt_finished(
                    self.conn, attempt_key="r1:finder:code:1",
                    status=status, cost_usd=0.1, turns=1)

    def test_terminal_set_is_the_full_set_minus_running(self):
        self.assertEqual(
            set(ledger.ATTEMPT_TERMINAL_STATUSES),
            set(ledger.ATTEMPT_STATUSES) - {"running"},
            "终态集合必须由完整词表派生，不能是第三份硬编码")
