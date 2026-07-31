import os, tempfile, unittest
from pathlib import Path
from harness import db
from harness.outbox import (InjectedFault, OperationConflict, Outbox,
                             ResponseLost, TerminalOperationError)


class TestOutbox(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        db.migrate(self.conn)
        self.ob = Outbox(self.conn)

    def tearDown(self):
        # 先关连接再删目录：否则 SQLite 连接随 GC 关闭会触发
        # ResourceWarning，掩盖真正的资源泄漏信号
        conn = getattr(self, "conn", None)
        if conn is not None:
            conn.close()
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

    def test_terminal_operation_error_marks_failed_terminal_and_reraises(self):
        """评审 Important-1：确定性业务拒绝（如 422）必须落进 failed_terminal，
        且异常必须原样向上传播——不得被吞掉，调用方需要知道本次调用失败了。
        """
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        with self.assertRaises(TerminalOperationError):
            self.ob.execute(op, call=lambda: (_ for _ in ()).throw(
                TerminalOperationError("422 Validation Failed")),
                probe=lambda: None)
        row = self.conn.execute(
            "SELECT phase FROM operations WHERE operation_id=?",
            (op.operation_id,)).fetchone()
        self.assertEqual(row["phase"], "failed_terminal")
        self.assertEqual(len(self.ob.unresolved()), 1,
                         "failed_terminal 必须被 unresolved() 捕获以阻断预检")

    def test_terminal_operation_error_does_not_consult_probe_again(self):
        """`execute()` 的首次 probe 是通用重入检查（对所有 kind 一视同仁），
        与故障处理无关。确定性失败发生后不应该**再次**调用 probe 去做『其实
        已生效』式的补救——那是 ResponseLost 专属语义。"""
        probe_calls = []

        def probe():
            probe_calls.append(1)
            return None

        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        with self.assertRaises(TerminalOperationError):
            self.ob.execute(
                op,
                call=lambda: (_ for _ in ()).throw(TerminalOperationError("x")),
                probe=probe)
        self.assertEqual(len(probe_calls), 1,
                         "只应有 execute() 开头那一次通用重入 probe，不应为"
                         "确定性失败额外再 probe 一次")

    def test_operation_conflict_marks_existing_and_root_failed_terminal(self):
        """去向决定：payload 冲突同样需要人工介入，复用 failed_terminal 状态，
        使其自动纳入 unresolved() / 预检 outbox_resolved 闸门。"""
        root = self.ob.prepare("r1", "publish_proposal", "fp1", {"title": "x"})
        sub = self.ob.prepare("r1", "commit_proposal", root.operation_id,
                              {"issue": 1})
        with self.assertRaises(OperationConflict):
            self.ob.prepare("r2", "commit_proposal", root.operation_id,
                            {"issue": 2})
        sub_row = self.conn.execute(
            "SELECT phase FROM operations WHERE operation_id=?",
            (sub.operation_id,)).fetchone()
        root_row = self.conn.execute(
            "SELECT phase FROM operations WHERE operation_id=?",
            (root.operation_id,)).fetchone()
        self.assertEqual(sub_row["phase"], "failed_terminal")
        self.assertEqual(root_row["phase"], "failed_terminal")
        unresolved_ids = {o.operation_id for o in self.ob.unresolved()}
        self.assertIn(sub.operation_id, unresolved_ids)
        self.assertIn(root.operation_id, unresolved_ids)

    def test_operation_conflict_does_not_overwrite_settled_root(self):
        """已收敛（settled）的发布不得被后到的冲突 payload 倒着改写历史。"""
        root = self.ob.prepare("r1", "publish_proposal", "fp1", {"title": "x"})
        self.ob.execute(root, call=lambda: {"number": 1}, probe=lambda: None)
        self.ob.settle(root)
        with self.assertRaises(OperationConflict):
            self.ob.prepare("r2", "publish_proposal", "fp1", {"title": "y"})
        row = self.conn.execute(
            "SELECT phase FROM operations WHERE operation_id=?",
            (root.operation_id,)).fetchone()
        self.assertEqual(row["phase"], "settled",
                         "settled 的 root 不得被冲突检测降级")

    def test_record_uncertain_observation_persists_count_and_first_seen(self):
        """评审 Critical A：跨轮次有界观察窗必须持久化，重启后不重置为 0。

        `execute()` 遇到 ResponseLost 且 probe 阴性时，已经会调用
        `record_uncertain_observation()` 把 op 计数推到 1（见下方 execute()
        调用），本用例只需在此基础上验证持久化/重启/再累加。
        """
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        with self.assertRaises(ResponseLost):
            self.ob.execute(op, call=lambda: (_ for _ in ()).throw(
                ResponseLost("boom")), probe=lambda: None)
        self.assertEqual(op.uncertain_observations, 1)
        first_seen = op.uncertain_first_seen_at
        self.assertIsNotNone(first_seen)

        # 模拟进程重启：重新从 DB 读回同一 operation
        reread = self.ob.get("publish_proposal", "nk1")
        self.assertEqual(reread.uncertain_observations, 1)
        self.assertEqual(reread.uncertain_first_seen_at, first_seen)
        self.assertIsNone(reread.result, "信封不是真实外部结果，result 须为 None")
        self.assertEqual(reread.phase, "failed_retryable")

        # 再观察一次：计数累加，首次观察时刻不变
        again = self.ob.record_uncertain_observation(reread)
        self.assertEqual(again.uncertain_observations, 2)
        self.assertEqual(again.uncertain_first_seen_at, first_seen)

    def test_fresh_operation_has_zero_uncertain_observations(self):
        op = self.ob.prepare("r1", "publish_proposal", "nk1", {"title": "x"})
        self.assertEqual(op.uncertain_observations, 0)
        self.assertIsNone(op.uncertain_first_seen_at)

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


class TestReconcileFeedsObservationWindow(unittest.TestCase):
    """reconcile 路径也必须喂观察窗，否则闸门会被整条路径绕过。"""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        db.migrate(self.conn)
        self.ob = Outbox(self.conn)

    def tearDown(self):
        self.conn.close()
        self.tmp.cleanup()

    def _make_uncertain(self):
        op = self.ob.prepare("r1", "publish_proposal", "nk", {"title": "x"})
        with self.assertRaises(ResponseLost):
            self.ob.execute(op, call=lambda: (_ for _ in ()).throw(
                ResponseLost("boom")), probe=lambda: None)
        return op

    def test_reconcile_accumulates_observations_and_finally_goes_terminal(self):
        op = self._make_uncertain()
        blind = {"publish_proposal": lambda o: None}
        for _ in range(self.ob.UNCERTAIN_WINDOW + 2):
            self.ob.reconcile(blind)
        row = self.conn.execute(
            "SELECT phase FROM operations WHERE operation_id=?",
            (op.operation_id,)).fetchone()
        self.assertEqual(row["phase"], "failed_terminal",
                         "只经 reconcile 重访的 operation 必须最终撞到窗口上限")
        self.assertEqual(len(self.ob.unresolved()), 1,
                         "转 terminal 后必须能阻断后续轮次")

    def test_reconcile_adopting_positive_read_clears_uncertainty(self):
        op = self._make_uncertain()
        self.ob.reconcile({"publish_proposal": lambda o: None})
        self.ob.reconcile({"publish_proposal": lambda o: {"number": 7}})
        self.assertEqual(self.ob.get("publish_proposal", "nk").result,
                         {"number": 7})
        self.assertEqual(self.ob.unresolved(), [])

    def test_prepared_ops_are_not_counted_as_uncertain(self):
        """从未 ResponseLost 过的 prepared operation 只是还没执行，不算不确定。"""
        self.ob.prepare("r1", "publish_proposal", "nk2", {"title": "y"})
        for _ in range(self.ob.UNCERTAIN_WINDOW + 2):
            self.ob.reconcile({"publish_proposal": lambda o: None})
        row = self.conn.execute(
            "SELECT phase FROM operations WHERE natural_key='nk2'").fetchone()
        self.assertEqual(row["phase"], "prepared")
        self.assertEqual(self.ob.unresolved(), [])
