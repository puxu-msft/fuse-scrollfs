import os, tempfile, unittest
from pathlib import Path
from harness import db
from harness.outbox import InjectedFault, OperationConflict, Outbox, ResponseLost


class TestOutbox(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        db.migrate(self.conn)
        self.ob = Outbox(self.conn)

    def tearDown(self):
        self.tmp.cleanup()

    def test_prepare_is_durable_before_call(self):
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        row = self.conn.execute(
            "SELECT phase FROM operations WHERE operation_id=?",
            (op.operation_id,)).fetchone()
        self.assertEqual(row["phase"], "prepared")

    def test_execute_records_observed_result(self):
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        result = self.ob.execute(op, call=lambda: {"number": 7}, probe=lambda: None)
        self.assertEqual(result, {"number": 7})
        row = self.conn.execute(
            "SELECT phase, result_json FROM operations WHERE operation_id=?",
            (op.operation_id,)).fetchone()
        self.assertEqual(row["phase"], "observed")
        self.assertIn('"number": 7', row["result_json"])

    def test_probe_before_call_adopts_existing_artifact(self):
        """崩在 after-call：副作用已落地而 op 仍 prepared。重入必须先 probe，
        否则重发会失败（如 git commit 报 nothing to commit）。"""
        calls = []
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        result = self.ob.execute(op, call=lambda: calls.append(1),
                                 probe=lambda: {"number": 9})
        self.assertEqual(result, {"number": 9})
        self.assertEqual(calls, [], "probe 探到已存在的 artifact 后不得再 call")
        self.assertEqual(op.phase, "observed")

    def test_response_lost_adopts_probe_result_instead_of_retrying(self):
        state = {"applied": False}

        def call():
            state["applied"] = True
            raise ResponseLost("connection reset")

        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        result = self.ob.execute(
            op, call=call,
            probe=lambda: {"number": 9} if state["applied"] else None)
        self.assertEqual(result, {"number": 9})

    def test_response_lost_and_not_applied_marks_failed_retryable(self):
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        with self.assertRaises(ResponseLost):
            self.ob.execute(op, call=lambda: (_ for _ in ()).throw(
                ResponseLost("boom")), probe=lambda: None)
        row = self.conn.execute(
            "SELECT phase FROM operations WHERE operation_id=?",
            (op.operation_id,)).fetchone()
        self.assertEqual(row["phase"], "failed_retryable")
        self.assertEqual(self.ob.unresolved(), [],
                         "可重试的 operation 不得阻断下一轮预检")

    def test_reconcile_adopts_late_visible_remote_object(self):
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        with self.assertRaises(ResponseLost):
            self.ob.execute(op, call=lambda: (_ for _ in ()).throw(
                ResponseLost("boom")), probe=lambda: None)
        still_open = self.ob.reconcile({"publish_proposal": lambda o: {"number": 11}})
        self.assertEqual(still_open, [])
        self.assertEqual(self.ob.get("publish_proposal", "nk1").result, {"number": 11})

    def test_prepare_returns_existing_operation_for_same_natural_key(self):
        a = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        b = self.ob.prepare("r2", "publish_proposal", "nk1", {"title": "x"})
        self.assertEqual(a.operation_id, b.operation_id)

    def test_prepare_rejects_same_key_with_different_payload(self):
        self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        with self.assertRaises(OperationConflict):
            self.ob.prepare("r2", "publish_proposal", "nk1", {"title": "y"})

    def test_sub_operation_requires_existing_root(self):
        with self.assertRaises(OperationConflict):
            self.ob.prepare("r1", "commit_proposal", "no-such-root", {})

    def test_root_of_resolves_sub_operation(self):
        root = self.ob.prepare("r1", "publish_proposal", "fp1", {"title": "x"})
        sub = self.ob.prepare("r1", "push_main", root.operation_id, {"issue": 1})
        self.assertEqual(self.ob.root_of(sub).operation_id, root.operation_id)

    def test_open_roots_includes_unsettled_root_even_when_all_ops_observed(self):
        """artifact 齐全但 root 未 settled ⇒ 事务未收敛，必须仍被认作待恢复。"""
        root = self.ob.prepare("r1", "publish_proposal", "fp1", {"title": "x"})
        self.ob.execute(root, call=lambda: {"number": 1}, probe=lambda: None)
        self.assertEqual(len(self.ob.open_operations()), 0)
        self.assertEqual(len(self.ob.open_roots()), 1)
        self.ob.settle(root)
        self.assertEqual(len(self.ob.open_roots()), 0)

    def test_execute_on_observed_operation_is_noop(self):
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        self.ob.execute(op, call=lambda: {"number": 7}, probe=lambda: None)
        again = self.ob.prepare("r2", "publish_proposal", "nk1", {"title": "x"})
        calls = []
        result = self.ob.execute(again, call=lambda: calls.append(1),
                                 probe=lambda: None)
        self.assertEqual(result, {"number": 7})
        self.assertEqual(calls, [])

    def test_fault_injection_stops_at_named_phase(self):
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        os.environ["HARNESS_FAULT"] = "publish_proposal:before-call"
        try:
            with self.assertRaises(InjectedFault):
                self.ob.execute(op, call=lambda: {"number": 1}, probe=lambda: None)
        finally:
            del os.environ["HARNESS_FAULT"]
        row = self.conn.execute(
            "SELECT phase FROM operations WHERE operation_id=?",
            (op.operation_id,)).fetchone()
        self.assertEqual(row["phase"], "prepared")

    def test_unpushed_commits_detects_commit_without_push(self):
        root = self.ob.prepare("r1", "publish_proposal", "fp1", {"title": "x"})
        commit_op = self.ob.prepare("r1", "commit_proposal", root.operation_id, {})
        self.ob.set_commit_sha(commit_op, "abc123")
        self.assertEqual(len(self.ob.unpushed_commits()), 1)
        push_op = self.ob.prepare("r1", "push_main", root.operation_id, {})
        self.ob.execute(push_op, call=lambda: {"pushed": True}, probe=lambda: None)
        self.assertEqual(self.ob.unpushed_commits(), [])


if __name__ == "__main__":
    unittest.main()
