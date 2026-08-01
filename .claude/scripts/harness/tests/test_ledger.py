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
        with self.assertRaisesRegex(ValueError, "invalid attempt status"):
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
