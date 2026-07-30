import tempfile, unittest
from pathlib import Path
from harness import db
from harness.queue import Queue, fingerprint


class TestQueue(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        db.migrate(self.conn)
        self.q = Queue(self.conn)

    def tearDown(self):
        self.tmp.cleanup()

    def test_fingerprint_is_stable_and_order_insensitive_to_whitespace(self):
        a = fingerprint("加 CRC", "块完整性", "crates/scrollz/src/archive.rs",
                        "损坏块必须 fail-closed")
        b = fingerprint("加 CRC ", " 块完整性", "crates/scrollz/src/archive.rs",
                        "损坏块必须 fail-closed")
        self.assertEqual(a, b)

    def test_exact_duplicate_detected(self):
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "defect", "t", "proposed", issue_number=1)
        self.assertEqual(self.q.classify(
            {"fingerprint": fp, "lane": "defect", "title": "t"}), "exact_duplicate")

    def test_rejected_without_ready_condition_blocks_reproposal(self):
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "perf", "t", "rejected",
                      reconsider_when="not_before:2099-01-01")
        self.assertEqual(self.q.classify(
            {"fingerprint": fp, "lane": "perf", "title": "t"}), "rejected_active")

    def test_rejected_with_satisfied_condition_becomes_new(self):
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "perf", "t", "rejected",
                      reconsider_when="not_before:2000-01-01")
        self.assertEqual(self.q.classify(
            {"fingerprint": fp, "lane": "perf", "title": "t"}), "new")

    def test_unparseable_reconsider_when_never_auto_expires(self):
        """自然语言条件不得伪装成自动复议。"""
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "perf", "t", "rejected",
                      reconsider_when="等 fuser 升级之后再说")
        self.assertFalse(self.q.reconsider_ready(fp, {}))
        self.assertEqual(self.q.classify(
            {"fingerprint": fp, "lane": "perf", "title": "t"}), "rejected_active")

    def test_lane_and_total_caps(self):
        for i in range(3):
            self.q.record(f"fp{i}", "hygiene", f"t{i}", "proposed")
        self.assertTrue(self.q.lane_full("hygiene", cap=3))
        self.assertFalse(self.q.lane_full("perf", cap=3))
        self.assertTrue(self.q.total_full(cap=3))
        self.assertFalse(self.q.total_full(cap=4))

    def test_main_sha_changed_predicate(self):
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "roadmap", "t", "rejected",
                      reconsider_when="main_sha_changed:aaaa")
        self.assertFalse(self.q.reconsider_ready(fp, {"main_sha": "aaaa"}))
        self.assertTrue(self.q.reconsider_ready(fp, {"main_sha": "bbbb"}))


if __name__ == "__main__":
    unittest.main()
