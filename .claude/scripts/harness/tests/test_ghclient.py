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


if __name__ == "__main__":
    unittest.main()
