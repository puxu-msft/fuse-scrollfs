import subprocess, tempfile, unittest
from pathlib import Path
from harness import db
from harness.config import GIT
from harness.gitops import PublishWorktree
from harness.outbox import Outbox, TerminalOperationError
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


class RaisingOutbox:
    """Important-2/Minor-3 用：包一层真实 Outbox，让 reconcile 按需抛异常。

    包住 unpushed_commits 是为了断言它在 reconcile 失败后**不会**被调用——
    reconcile 失败意味着持久化事实不可信，不能据此再去推导 has_unpushed_commit。
    """

    def __init__(self, real_outbox: Outbox, reconcile_exc: Exception):
        self._real = real_outbox
        self._reconcile_exc = reconcile_exc

    def reconcile(self, probes):
        raise self._reconcile_exc

    def unpushed_commits(self):
        raise AssertionError(
            "reconcile 失败后不得再调用 unpushed_commits：持久化事实不可信")

    def unresolved(self):
        return self._real.unresolved()


class Cfg:
    gh_token = "tok"


class TestPrecheck(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        self.addCleanup(self.conn.close)
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

    def test_terminal_failed_operations_never_touch_worktree(self):
        """评审 Important-2：已存在 failed_terminal 时，预检必须在触碰发布
        工作区之前就停下——之前的顺序是 unpushed_commits() → worktree
        ensure/reset → 最后才查 outbox_resolved，于是本该立即交人工介入的
        这一轮，仍会先对发布工作区做 fetch/prune/reset 等副作用。"""
        op = self.outbox.prepare("r0", "publish_proposal", "nk", {})
        self.outbox._mark(op, "failed_terminal", None)
        worktree = FakeWorktree()
        results = run_prechecks(Cfg(), FakeGitHub("WRITE"), worktree,
                                self.outbox, tools=("/usr/bin/git",))
        self.assertTrue(any(r.name == "outbox_resolved" and not r.ok
                            for r in results))
        self.assertFalse(worktree.ensured,
                         "存在 failed_terminal 时不得调用 worktree.ensure()")
        with self.assertRaises(PrecheckFailed):
            assert_all_ok(results)

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

    def test_paused_sentinel_never_touches_worktree(self):
        """Important-2：暂停时预检不得对发布工作区产生任何副作用。

        断言 FakeWorktree.ensure 从未被调用——而不仅仅是「最终结果失败」，
        因为「先 reset 再返回失败」同样会让最终结果失败，却已经造成了副作用。
        """
        gh = FakeGitHub("WRITE")
        gh.create_issue("PAUSED", "b", ["harness:paused"])
        worktree = FakeWorktree()
        results = run_prechecks(Cfg(), gh, worktree, self.outbox,
                                tools=("/usr/bin/git",))
        with self.assertRaises(PrecheckFailed):
            assert_all_ok(results)
        self.assertFalse(worktree.ensured,
                         "暂停场景下不得调用 worktree.ensure()")

    def test_read_permission_never_touches_worktree(self):
        """Important-2：权限不足（READ）时同样不得触碰发布工作区。"""
        worktree = FakeWorktree()
        results = run_prechecks(Cfg(), FakeGitHub("READ"), worktree,
                                self.outbox, tools=("/usr/bin/git",))
        with self.assertRaises(PrecheckFailed):
            assert_all_ok(results)
        self.assertFalse(worktree.ensured,
                         "READ 权限场景下不得调用 worktree.ensure()")

    def test_reconcile_exception_becomes_structured_failure(self):
        """Minor-3：reconcile 抛异常要落进结构化结果，且不得继续碰 worktree。

        既不能从一个抛异常的 reconcile 结果推导 has_unpushed_commit，
        也不能让 worktree.ensure() 在未经核实的状态上执行 reset/clean——
        RaisingOutbox.unpushed_commits() 会在被调用时直接 AssertionError，
        RaisingOutbox 本身也不提供 worktree 交互，只验证「不再往下走」。
        """
        raising = RaisingOutbox(self.outbox, RuntimeError("db 只读，读不到"))
        worktree = FakeWorktree()
        results = run_prechecks(Cfg(), FakeGitHub("WRITE"), worktree,
                                raising, tools=("/usr/bin/git",))
        reconcile_check = [r for r in results if r.name == "outbox_reconcile"]
        self.assertEqual(len(reconcile_check), 1)
        self.assertFalse(reconcile_check[0].ok)
        self.assertIn("db 只读", reconcile_check[0].detail)
        self.assertFalse(worktree.ensured,
                         "reconcile 失败后不得再对 worktree 做 reset/clean")
        with self.assertRaises(PrecheckFailed):
            assert_all_ok(results)

    def test_deterministic_422_blocks_round_and_precheck_end_to_end(self):
        """Important-1 端到端：Fake transport 返回 422 → 真实 GhCli 抛
        TerminalOperationError → Outbox.execute 标记 failed_terminal 并
        重新抛出 → 下一轮 run_prechecks 判 outbox_resolved 失败，且绝不
        触碰发布工作区（即使当时并无待推提交）。

        这条覆盖此前评审指出的死代码路径：生产路径此前从不产生
        failed_terminal（它只在测试里靠私有 _mark() 人工制造），
        本测试证明现在生产路径（ghclient → outbox）真能走到这一状态。
        """
        import json
        from harness.config import Config
        from harness.ghclient import GhCli
        from harness.tests.fakes import FakeGhTransport

        cfg = Config(
            repo_root=Path(self.tmp.name), state_db=Path(self.tmp.name) / "h.db",
            publish_worktree=Path(self.tmp.name) / "wt", repo_slug="acme/widgets",
            gh_token="fake-token", round_budget_usd=1.0, daily_budget_usd=5.0,
            max_turns=10, proposed_cap=20, lane_cap=6)
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr='{"message":"Validation Failed"}'
                                              "\ngh: Validation Failed (HTTP 422)")
        real_gh = GhCli(cfg, runner=transport)

        op = self.outbox.prepare("r1", "publish_proposal", "fp1",
                                 {"title": "t", "body_md": "b"})
        with self.assertRaises(TerminalOperationError):
            self.outbox.execute(
                op,
                call=lambda: real_gh.create_issue("t", "b", []),
                probe=lambda: None)
        row = self.conn.execute(
            "SELECT phase FROM operations WHERE operation_id=?",
            (op.operation_id,)).fetchone()
        self.assertEqual(row["phase"], "failed_terminal")

        worktree = FakeWorktree()
        results = run_prechecks(Cfg(), FakeGitHub("WRITE"), worktree,
                                self.outbox, tools=("/usr/bin/git",))
        outbox_check = [r for r in results if r.name == "outbox_resolved"]
        self.assertEqual(len(outbox_check), 1)
        self.assertFalse(outbox_check[0].ok,
                         "422 造成的 failed_terminal 必须阻断本轮预检")
        self.assertFalse(worktree.ensured,
                         "存在 failed_terminal 时不得触碰发布工作区")
        with self.assertRaises(PrecheckFailed):
            assert_all_ok(results)


def _run_git(cwd, *args):
    return subprocess.run([GIT, *args], cwd=cwd, capture_output=True,
                          text=True, check=True).stdout.strip()


class TestPrecheckRealWorktreeIntegration(unittest.TestCase):
    """Important-1：用真实 bare repo + 真实 PublishWorktree 验证不会假绿。

    覆盖评审指出的具体回归路径：`worktree.ensure(allow_reset=True)` 内部先
    `reset --hard` 再 `clean -fd`，若预检代码先调用 ensure 再问 is_clean()，
    脏改动会在被检查之前就被清空，导致「dirty publish worktree fails」这条
    验收在实现里悄悄变成「先清干净再宣布干净」的假绿。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.remote = root / "remote.git"
        self.local = root / "local"
        subprocess.run([GIT, "init", "--bare", "-b", "main", str(self.remote)],
                       check=True, capture_output=True)
        subprocess.run([GIT, "clone", str(self.remote), str(self.local)],
                       check=True, capture_output=True)
        _run_git(self.local, "config", "user.email", "h@example.com")
        _run_git(self.local, "config", "user.name", "harness")
        (self.local / "README.md").write_text("seed\n")
        (self.local / "tracked.txt").write_text("original\n")
        _run_git(self.local, "add", "README.md", "tracked.txt")
        _run_git(self.local, "commit", "-m", "seed")
        _run_git(self.local, "push", "origin", "main")

        self.publish_path = self.local / ".worktree/_publish"
        self.wt = PublishWorktree(self.local, self.publish_path)
        # 预建工作区（clean 状态），随后在其中制造脏改动
        self.wt.ensure()

        self.conn = db.connect(root / "h.db")
        self.addCleanup(self.conn.close)
        db.migrate(self.conn)
        self.outbox = Outbox(self.conn)

    def tearDown(self):
        self.tmp.cleanup()

    def test_dirty_real_worktree_fails_and_preserves_content(self):
        untracked = self.publish_path / "untracked.txt"
        untracked.write_text("please keep me\n")
        tracked = self.publish_path / "tracked.txt"
        tracked.write_text("locally modified, do not discard\n")

        results = run_prechecks(Cfg(), FakeGitHub("WRITE"), self.wt,
                                self.outbox, tools=("/usr/bin/git",))

        wt_check = [r for r in results if r.name == "publish_worktree_clean"]
        self.assertEqual(len(wt_check), 1)
        self.assertFalse(wt_check[0].ok,
                         "脏的发布工作区必须判失败，不能被静默 reset 后放行")
        with self.assertRaises(PrecheckFailed):
            assert_all_ok(results)

        # 关键断言：文件内容必须原样还在——这是防「假绿」的核心，
        # 只断言"返回失败"挡不住将来有人把清理挪到检查之前。
        self.assertTrue(untracked.exists(), "未跟踪文件不得被 clean -fd 删除")
        self.assertEqual(untracked.read_text(), "please keep me\n")
        self.assertEqual(tracked.read_text(), "locally modified, do not discard\n",
                         "已跟踪文件的本地修改不得被 reset --hard 丢弃")


if __name__ == "__main__":
    unittest.main()
