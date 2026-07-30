import unittest
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
    """评审 Critical 的端到端回归：`create_issue` 响应丢失、但对象已生效，
    而探测在头几次调用里『暂不可见』（模拟 Search 异步索引延迟）——
    natural-key 恢复绝不能因为探测阴性就重发 `create_issue`，否则同一
    提案会产生两个 Issue。

    `FakeGitHub.find_issue_by_marker()`（生产路径实际调用的方法）模拟的
    是修复后的强一致语义——本身不会重现这个 bug，这是修复应有的效果。
    `find_issue_by_marker_delayed()` 是本文件专门加的『延迟探测』替身，
    仅用于**正控**：证明如果恢复路径仍旧使用一个会返回延迟阴性的探测
    （旧版基于 Search 索引的语义），确实会导致重复 Issue——从而确认这条
    回归测试本身有能力抓住待修复的失效模式，而不是一个自始至终都不可能
    变红的空测试。
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

    def _run_recovery_flow(self, probe_method: str):
        """通用恢复流程：第一次 `execute()` 可能因『探测立即命中』而在内部
        直接收敛（强一致场景），也可能因『探测阴性』而向上抛出异常、需要
        下一轮再 `execute()` 一次（延迟探测场景）——两条路径都要能收敛，
        这里统一处理，不对哪条路径生效做预设。"""
        marker = "HARNESS-OP:idx-delay-1"
        op = self.outbox.prepare("r1", "publish_proposal", "fp1",
                                 {"title": "t", "body_md": "b"})
        body = f"body {marker}"
        self.gh.fail_next("create_issue", applied=True)
        probe_fn = getattr(self.gh, probe_method)

        def do_execute():
            return self.outbox.execute(
                op, call=lambda: self.gh.create_issue("t", body, []),
                probe=lambda: probe_fn(marker))

        try:
            do_execute()
        except Exception:
            do_execute()  # 恢复轮：outbox 重入时先 probe 再决定是否重发

    def test_positive_control_delayed_probe_does_cause_duplicate(self):
        """正控：用会延迟可见的探测（旧版 Search 索引语义），前 3 次探测
        阴性——证明这条测试确实能抓住『探测不可靠导致重复 Issue』这个
        失效模式，而不是一个无论如何都通过的空测试。"""
        self.gh.simulate_delayed_marker_visibility(
            "HARNESS-OP:idx-delay-1", calls_until_visible=3)
        self._run_recovery_flow("find_issue_by_marker_delayed")
        self.assertEqual(len(self.gh.issues), 2,
                         "正控必须复现重复 Issue——若这里不是 2，说明测试"
                         "本身失去了抓住该失效模式的能力")
        self.assertEqual(self.gh.calls.count("create_issue"), 2)

    def test_fix_strongly_consistent_probe_prevents_duplicate(self):
        """修复验证：生产路径实际使用的 `find_issue_by_marker()` 是强一致
        的（无延迟窗口），恢复应在同一次 `execute()` 内部就收敛（探测立即
        命中已生效的 Issue），绝不重发 `create_issue`，全程只产生一个
        Issue。"""
        self._run_recovery_flow("find_issue_by_marker")
        self.assertEqual(len(self.gh.issues), 1,
                         "同一提案必须只产生一个 Issue")
        self.assertEqual(self.gh.calls.count("create_issue"), 1,
                         "探到已生效后不得重发 create_issue")


if __name__ == "__main__":
    unittest.main()
