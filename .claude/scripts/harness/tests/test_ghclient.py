import unittest
from harness.outbox import Outbox
from harness.tests.fakes import FakeGitHub


class TestFakeContract(unittest.TestCase):
    """Fake 必须满足与真实实现同一份契约——否则崩溃矩阵测的是假东西。"""

    def setUp(self):
        self.gh = FakeGitHub(permission="WRITE")

    def test_create_issue_then_find_by_marker(self):
        issue = self.gh.create_issue("t", "body HARNESS-OP:abc", ["harness"])
        self.assertEqual(issue["number"], 1)
        found = self.gh.find_issue_by_marker("HARNESS-OP:abc")
        self.assertEqual(found["number"], 1)

    def test_find_by_marker_returns_none_when_absent(self):
        self.assertIsNone(self.gh.find_issue_by_marker("HARNESS-OP:zzz"))

    def test_replace_labels_preserves_nothing_by_itself(self):
        issue = self.gh.create_issue("t", "b", ["harness", "T1"])
        self.gh.replace_labels(issue["number"], ["harness", "harness:proposed"])
        self.assertEqual(sorted(self.gh.get_issue_labels(issue["number"])),
                         ["harness", "harness:proposed"])

    def test_comment_marker_roundtrip(self):
        issue = self.gh.create_issue("t", "b", [])
        self.gh.create_comment(issue["number"], "HARNESS-RECEIPT\nop=abc")
        found = self.gh.find_comment_by_marker(issue["number"], "op=abc")
        self.assertIn("op=abc", found["body"])

    def test_fault_injection_raises_response_lost_after_applying(self):
        """模拟『服务端已生效但响应丢失』：对象必须已经存在。"""
        from harness.outbox import ResponseLost
        self.gh.fail_next("create_issue", applied=True)
        with self.assertRaises(ResponseLost):
            self.gh.create_issue("t", "b HARNESS-OP:xyz", [])
        self.assertIsNotNone(self.gh.find_issue_by_marker("HARNESS-OP:xyz"))

    def test_fault_injection_not_applied_leaves_nothing(self):
        from harness.outbox import ResponseLost
        self.gh.fail_next("create_issue", applied=False)
        with self.assertRaises(ResponseLost):
            self.gh.create_issue("t", "b HARNESS-OP:xyz", [])
        self.assertIsNone(self.gh.find_issue_by_marker("HARNESS-OP:xyz"))

    def test_ensure_label_fault_injection_covered(self):
        """发现 5：ensure_label 也会进 outbox（写操作），必须支持故障注入。"""
        from harness.outbox import ResponseLost
        self.gh.fail_next("ensure_label", applied=True)
        with self.assertRaises(ResponseLost):
            self.gh.ensure_label("harness", "ededed", "desc")
        self.assertIn("harness", self.gh.list_labels())

    def test_ensure_label_fault_not_applied_leaves_nothing(self):
        from harness.outbox import ResponseLost
        self.gh.fail_next("ensure_label", applied=False)
        with self.assertRaises(ResponseLost):
            self.gh.ensure_label("harness", "ededed", "desc")
        self.assertNotIn("harness", self.gh.list_labels())


class TestIndexDelayDoesNotDuplicateIssue(unittest.TestCase):
    """评审 Critical A 的端到端回归：`create_issue` 响应丢失、但对象已生效，
    而探测在头几次调用里『暂不可见』（模拟索引/端点最终一致性延迟）——
    natural-key 恢复绝不能因为探测阴性就重发 `create_issue`，否则同一
    提案会产生两个 Issue。

    延迟模拟现在直接作用于**生产代码实际调用的方法本身**
    （`FakeGitHub.find_issue_by_marker()`），不再需要一个专门的、生产代码
    从不调用的『delayed』替身方法（评审 Critical A：故障注入必须覆盖生产
    路径，否则测不出『生产路径读延迟导致重复创建』这个真实失效模式）。
    """

    def setUp(self):
        import tempfile
        from pathlib import Path
        from harness import db
        from harness.outbox import Outbox
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        self.addCleanup(self.conn.close)
        db.migrate(self.conn)
        self.outbox = Outbox(self.conn)
        self.gh = FakeGitHub("WRITE")

    def tearDown(self):
        self.tmp.cleanup()

    def _run_recovery_flow(self, rounds: int):
        """通用恢复流程：每一轮都是一次独立的 `execute()` 调用（模拟跨轮次
        resume），最多做 `rounds` 轮，命中收敛（不抛异常）就提前停止。"""
        marker = "HARNESS-OP:idx-delay-1"
        op = self.outbox.prepare("r1", "publish_proposal", "fp1",
                                 {"title": "t", "body_md": "b"})
        body = f"body {marker}"
        self.gh.fail_next("create_issue", applied=True)

        def do_execute():
            return self.outbox.execute(
                op, call=lambda: self.gh.create_issue("t", body, []),
                probe=lambda: self.gh.find_issue_by_marker(marker))

        for _ in range(rounds):
            try:
                do_execute()
                return
            except Exception:
                continue  # 恢复轮：outbox 重入时先 probe 再决定是否重发

    def test_positive_control_old_immediate_resend_would_duplicate(self):
        """正控（评审 Critical A）：证明"探测阴性 → 立即重发 create_issue"
        这条**旧逻辑**确实会产生重复 Issue——用来确认这条回归测试本身有
        能力抓住待修复的失效模式，而不是一个自始至终都不可能变红的空
        测试。这里不经过 `Outbox.execute()` 的窗口保护，直接手写旧行为。
        """
        marker = "HARNESS-OP:idx-delay-1"
        self.gh.simulate_delayed_marker_visibility(marker, calls_until_visible=3)
        self.gh.fail_next("create_issue", applied=True)
        body = f"body {marker}"
        try:
            self.gh.create_issue("t", body, [])
        except Exception:
            pass
        # 旧逻辑：探测阴性就立即重发，不经过任何观察窗保护
        if self.gh.find_issue_by_marker(marker) is None:
            self.gh.create_issue("t", body, [])
        self.assertEqual(len(self.gh.issues), 2,
                         "正控必须复现重复 Issue——若这里不是 2，说明测试"
                         "本身失去了抓住该失效模式的能力")
        self.assertEqual(self.gh.calls.count("create_issue"), 2)

    def test_fix_windowed_observation_prevents_duplicate_even_with_delay(self):
        """修复验证：即便生产路径本身的探测遭遇延迟（`simulate_delayed_
        marker_visibility()` 作用于 `find_issue_by_marker()`），`Outbox.
        execute()` 的跨轮次有界观察窗也不会在阴性读取后立即重发
        `create_issue`——多轮恢复内收敛后全程只产生一个 Issue。"""
        marker = "HARNESS-OP:idx-delay-1"
        self.gh.simulate_delayed_marker_visibility(marker, calls_until_visible=3)
        self._run_recovery_flow(rounds=Outbox.UNCERTAIN_WINDOW + 2)
        self.assertEqual(len(self.gh.issues), 1,
                         "同一提案必须只产生一个 Issue")
        self.assertEqual(self.gh.calls.count("create_issue"), 1,
                         "探测阴性期间不得重发 create_issue，哪怕延迟多轮")


if __name__ == "__main__":
    unittest.main()

