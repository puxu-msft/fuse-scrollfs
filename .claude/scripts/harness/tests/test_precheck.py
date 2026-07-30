import tempfile, unittest
from pathlib import Path
from harness import db
from harness.outbox import Outbox
from harness.precheck import PrecheckFailed, assert_all_ok, run_prechecks
from harness.tests.fakes import FakeGitHub


class FakeWorktree:
    def __init__(self, clean=True):
        self._clean = clean
        self.ensured = False
        self.allow_reset_seen = None

    def ensure(self, allow_reset: bool = True):
        self.ensured = True
        self.allow_reset_seen = allow_reset

    def is_clean(self):
        return self._clean


class Cfg:
    gh_token = "tok"


class TestPrecheck(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        db.migrate(self.conn)
        self.outbox = Outbox(self.conn)

    def tearDown(self):
        self.tmp.cleanup()

    def test_all_pass_with_write_permission(self):
        results = run_prechecks(Cfg(), FakeGitHub("WRITE"), FakeWorktree(),
                                self.outbox, tools=("/usr/bin/git",))
        self.assertTrue(all(r.ok for r in results), [r.detail for r in results])
        assert_all_ok(results)

    def test_read_only_token_fails_closed(self):
        results = run_prechecks(Cfg(), FakeGitHub("READ"), FakeWorktree(),
                                self.outbox, tools=("/usr/bin/git",))
        with self.assertRaises(PrecheckFailed) as ctx:
            assert_all_ok(results)
        self.assertIn("viewer_permission", str(ctx.exception))

    def test_missing_tool_reports_exact_path(self):
        results = run_prechecks(Cfg(), FakeGitHub("WRITE"), FakeWorktree(),
                                self.outbox, tools=("/no/such/binary",))
        failed = [r for r in results if not r.ok]
        self.assertTrue(any("/no/such/binary" in r.detail for r in failed))

    def test_dirty_publish_worktree_fails(self):
        results = run_prechecks(Cfg(), FakeGitHub("WRITE"),
                                FakeWorktree(clean=False), self.outbox,
                                tools=("/usr/bin/git",))
        self.assertTrue(any(r.name == "publish_worktree_clean" and not r.ok
                            for r in results))

    def test_terminal_failed_operations_block_the_round(self):
        """只有 failed_terminal 才阻断：它需要人工介入，机器无法自行收敛。"""
        op = self.outbox.prepare("r0", "publish_proposal", "nk", {})
        self.outbox._mark(op, "failed_terminal", None)
        results = run_prechecks(Cfg(), FakeGitHub("WRITE"), FakeWorktree(),
                                self.outbox, tools=("/usr/bin/git",))
        self.assertTrue(any(r.name == "outbox_resolved" and not r.ok
                            for r in results))

    def test_pending_operations_do_not_block_the_round(self):
        """prepared / failed_retryable **不得**阻断预检。

        阻断会让 run_round 直接返回 precheck-failed，
        「恢复优先于新扫描」永远走不到——这正是死锁。
        """
        self.outbox.prepare("r0", "publish_proposal", "nk", {})
        results = run_prechecks(Cfg(), FakeGitHub("WRITE"), FakeWorktree(),
                                self.outbox, tools=("/usr/bin/git",))
        outbox_check = [r for r in results if r.name == "outbox_resolved"]
        self.assertEqual(len(outbox_check), 1)
        self.assertTrue(outbox_check[0].ok,
                        "未决 operation 必须交给恢复路径，不能在预检处阻断")

    def test_paused_sentinel_blocks_the_round(self):
        gh = FakeGitHub("WRITE")
        gh.create_issue("PAUSED", "b", ["harness:paused"])
        results = run_prechecks(Cfg(), gh, FakeWorktree(), self.outbox,
                                tools=("/usr/bin/git",))
        self.assertTrue(any(r.name == "not_paused" and not r.ok for r in results))


if __name__ == "__main__":
    unittest.main()
