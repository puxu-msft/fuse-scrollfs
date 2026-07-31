import subprocess, tempfile, unittest
from pathlib import Path
from unittest import mock
from harness.config import GIT
from harness.gitops import (
    AmbiguousOperation,
    InvalidProposalPath,
    NonFastForward,
    PublishWorktree,
    PushRejected,
    ReplayConflict,
)


def run(cwd, *args):
    return subprocess.run([GIT, *args], cwd=cwd, capture_output=True,
                          text=True, check=True).stdout.strip()


class TestPublishWorktree(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.remote = root / "remote.git"
        self.local = root / "local"
        subprocess.run([GIT, "init", "--bare", "-b", "main", str(self.remote)],
                       check=True, capture_output=True)
        subprocess.run([GIT, "clone", str(self.remote), str(self.local)],
                       check=True, capture_output=True)
        run(self.local, "config", "user.email", "h@example.com")
        run(self.local, "config", "user.name", "harness")
        (self.local / "README.md").write_text("seed\n")
        run(self.local, "add", "README.md")
        run(self.local, "commit", "-m", "seed")
        run(self.local, "push", "origin", "main")
        self.wt = PublishWorktree(self.local, self.local / ".worktree/_publish")

    def tearDown(self):
        self.tmp.cleanup()

    def test_ensure_creates_detached_worktree_at_origin_main(self):
        self.wt.ensure()
        head = run(self.wt.path, "rev-parse", "HEAD")
        origin_main = run(self.local, "rev-parse", "origin/main")
        self.assertEqual(head, origin_main)
        # detached：symbolic-ref 应失败
        proc = subprocess.run([GIT, "symbolic-ref", "-q", "HEAD"],
                              cwd=self.wt.path, capture_output=True, text=True)
        self.assertNotEqual(proc.returncode, 0, "必须是 detached HEAD")

    def test_commit_carries_operation_trailer_and_push_publishes(self):
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        sha = self.wt.commit("docs(proposals): add #1", "op123",
                             "docs/proposals/1-demo.md")
        self.assertTrue(self.wt.local_has_operation("op123"))
        self.wt.push()
        self.assertTrue(self.wt.remote_has_operation(
            "op123", "docs/proposals/1-demo.md"))
        body = run(self.local, "log", "-1", "--format=%B", "origin/main")
        self.assertIn("HARNESS-OP:op123", body)

    def test_non_fast_forward_replays_same_operation_without_duplicating(self):
        """并发者先推了 main：必须 fetch+重放同一 operation，不得另建第二张卡。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        self.wt.commit("docs(proposals): add #1", "op123",
                       "docs/proposals/1-demo.md")
        # 模拟他人推进 main
        (self.local / "other.txt").write_text("other\n")
        run(self.local, "add", "other.txt")
        run(self.local, "commit", "-m", "other work")
        run(self.local, "push", "origin", "main")

        self.wt.push()

        self.assertTrue(self.wt.remote_has_operation(
            "op123", "docs/proposals/1-demo.md"))
        count = run(self.local, "log", "origin/main", "--grep",
                    "HARNESS-OP:op123", "--format=%H")
        self.assertEqual(len(count.splitlines()), 1, "同一 operation 只能有一个提交")
        self.assertEqual(
            run(self.local, "show", "origin/main:other.txt"), "other",
            "他人的提交不得被丢弃")

    def test_remote_has_operation_false_before_push(self):
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/2-x.md", "# x\n")
        self.wt.commit("docs(proposals): add #2", "op999",
                       "docs/proposals/2-x.md")
        self.assertFalse(self.wt.remote_has_operation(
            "op999", "docs/proposals/2-x.md"))

    def test_replayed_commit_keeps_harness_identity(self):
        """重放后 committer 必须仍是 harness——否则无法分辨哪些提交是它做的。

        本机实测：不固定身份时 committer 会变成仓库 local config 的人类身份。
        """
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        self.wt.commit("docs(proposals): add #1", "op123",
                       "docs/proposals/1-demo.md")
        (self.local / "other.txt").write_text("other\n")
        run(self.local, "add", "other.txt")
        run(self.local, "commit", "-m", "other work")
        run(self.local, "push", "origin", "main")

        self.wt.push()

        who = run(self.local, "log", "origin/main", "--grep", "HARNESS-OP:op123",
                  "--format=%an <%ae>|%cn <%ce>")
        self.assertEqual(who, "scrollz-harness <harness@localhost>|"
                              "scrollz-harness <harness@localhost>")

    def test_ensure_self_heals_when_worktree_dir_was_deleted(self):
        """崩溃或人工清理删掉目录、注册残留：ensure() 必须自愈而非永久卡死。"""
        import shutil
        self.wt.ensure()
        shutil.rmtree(self.wt.path)
        self.wt.ensure()
        self.assertTrue((self.wt.path / ".git").exists())
        self.assertEqual(run(self.wt.path, "rev-parse", "HEAD"),
                         run(self.local, "rev-parse", "origin/main"))

    def test_replay_conflict_aborts_cleanly_and_raises(self):
        """重放冲突：必须抛 ReplayConflict 且不留冲突残留。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# harness 版本\n")
        self.wt.commit("docs(proposals): add #1", "op123",
                       "docs/proposals/1-demo.md")
        target = self.local / "docs/proposals"
        target.mkdir(parents=True, exist_ok=True)
        (target / "1-demo.md").write_text("# 他人版本\n")
        run(self.local, "add", "docs/proposals/1-demo.md")
        run(self.local, "commit", "-m", "conflicting")
        run(self.local, "push", "origin", "main")

        with self.assertRaises(ReplayConflict):
            self.wt.push()
        self.assertEqual(run(self.wt.path, "status", "--porcelain"), "",
                         "冲突残留必须已 abort 清理")

    # ------------------------------------------------------------ Critical：路径逃逸

    def test_write_proposal_rejects_absolute_path(self):
        self.wt.ensure()
        outside = Path(self.tmp.name) / "abs-outside.txt"
        with self.assertRaises(InvalidProposalPath):
            self.wt.write_proposal(str(outside), "OVERWRITTEN")
        self.assertFalse(outside.exists(), "非法输入不得产生任何文件")

    def test_write_proposal_rejects_dotdot_escape(self):
        self.wt.ensure()
        outside = Path(self.tmp.name) / "outside.txt"
        with self.assertRaises(InvalidProposalPath):
            self.wt.write_proposal("../../../outside.txt", "OVERWRITTEN")
        self.assertFalse(outside.exists(), "非法输入不得产生任何文件")

    def test_write_proposal_rejects_nested_symlink_escape(self):
        """严格模式正则本身能挡住字面 `..`，但挡不住中间目录是 symlink 的逃逸。"""
        self.wt.ensure()
        outside_dir = Path(self.tmp.name) / "outside-dir"
        outside_dir.mkdir()
        (self.wt.path / "docs").symlink_to(outside_dir, target_is_directory=True)
        with self.assertRaises(InvalidProposalPath):
            self.wt.write_proposal("docs/proposals/1-demo.md", "PWNED")
        self.assertEqual(list(outside_dir.iterdir()), [],
                         "symlink 逃逸也不得在工作区外产生文件")

    # -------------------------------------------------------- HTTPS+PAT push/fetch

    def test_fetch_uses_https_url_and_explicit_refspec_when_token_configured(self):
        """配置了 token 后，_fetch() 必须走 HTTPS URL 并显式写 tracking ref
        （按 remote 名字 fetch 不会自动更新它，见 push 修复设计笔记）。"""
        wt = PublishWorktree(self.local, self.local / ".worktree/_authed",
                             gh_token="ghp_SECRETTOKEN123", repo_slug="acme/widgets")
        calls = []

        def fake_run(cmd, *a, **kw):
            calls.append(cmd)
            return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

        with mock.patch("harness.gitops.subprocess.run", side_effect=fake_run):
            wt._fetch()

        self.assertEqual(calls, [[
            GIT, "-c", "credential.helper=", "fetch",
            "https://x-access-token:ghp_SECRETTOKEN123@github.com/acme/widgets.git",
            "+main:refs/remotes/origin/main",
        ]])

    def test_push_argv_uses_https_url_when_token_configured(self):
        wt = PublishWorktree(self.local, self.local / ".worktree/_authed",
                             gh_token="ghp_SECRETTOKEN123", repo_slug="acme/widgets")
        self.assertEqual(wt._push_argv(), [
            GIT, "-c", "credential.helper=", "push",
            "https://x-access-token:ghp_SECRETTOKEN123@github.com/acme/widgets.git",
            "HEAD:main",
        ])

    def test_push_calls_https_argv_end_to_end_when_token_configured(self):
        """push() 本体（非仅 _push_argv）在配置 token 时确实传出 HTTPS 参数。"""
        wt = PublishWorktree(self.local, self.local / ".worktree/_authed",
                             gh_token="ghp_SECRETTOKEN123", repo_slug="acme/widgets")
        calls = []

        def fake_run(cmd, *a, **kw):
            calls.append(cmd)
            return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

        with mock.patch("harness.gitops.subprocess.run", side_effect=fake_run):
            wt.push()

        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0], [
            GIT, "-c", "credential.helper=", "push",
            "https://x-access-token:ghp_SECRETTOKEN123@github.com/acme/widgets.git",
            "HEAD:main",
        ])

    def test_fetch_and_push_fall_back_to_remote_name_without_token(self):
        """无 token（本地测试场景的默认构造）时必须回退到原有按 remote 名字的
        行为，不得意外触发 HTTPS 分支——这是既有 20 条 gitops 测试成立的前提。"""
        self.assertEqual(self.wt._push_argv(),
                         [GIT, "push", "origin", "HEAD:main"])
        calls = []

        def fake_run(cmd, *a, **kw):
            calls.append(cmd)
            return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

        with mock.patch("harness.gitops.subprocess.run", side_effect=fake_run):
            self.wt._fetch()
        self.assertEqual(calls, [[GIT, "fetch", "origin", "main"]])

    def test_push_error_message_redacts_token(self):
        """token 泄漏是本次修复最关键的一条：push 失败时异常消息里绝不能出现
        token 明文。"""
        token = "ghp_SUPERSECRETTOKENVALUE"
        wt = PublishWorktree(self.local, self.local / ".worktree/_authed",
                             gh_token=token, repo_slug="acme/widgets")
        # 模拟一次会把 URL（含 token）原样回显进 stderr 的失败——即便实测中
        # git 本身通常不会这样做，也要防御性地保证脱敏对这种最坏情况生效。
        leaking_stderr = (
            f"fatal: unable to access "
            f"'https://x-access-token:{token}@github.com/acme/widgets.git/': "
            f"Could not resolve host")

        def fake_run(cmd, *a, **kw):
            return subprocess.CompletedProcess(cmd, 1, stdout="",
                                               stderr=leaking_stderr)

        with mock.patch("harness.gitops.subprocess.run", side_effect=fake_run):
            with self.assertRaises(RuntimeError) as ctx:
                wt.push()

        message = str(ctx.exception)
        self.assertNotIn(token, message, "异常消息不得包含 token 明文")
        self.assertIn("<REDACTED>", message)

    def test_git_helper_error_message_redacts_token_in_argv(self):
        """`_git()` 抛出的 RuntimeError 里，即便 token 出现在 argv 本身
        （fetch 走 HTTPS 时），也必须被脱敏。"""
        token = "ghp_ANOTHERSECRETTOKEN"
        wt = PublishWorktree(self.local, self.local / ".worktree/_authed",
                             gh_token=token, repo_slug="acme/widgets")

        def fake_run(cmd, *a, **kw):
            return subprocess.CompletedProcess(cmd, 1, stdout="",
                                               stderr="fatal: Could not resolve host")

        with mock.patch("harness.gitops.subprocess.run", side_effect=fake_run):
            with self.assertRaises(RuntimeError) as ctx:
                wt._fetch()

        message = str(ctx.exception)
        self.assertNotIn(token, message, "异常消息不得包含 token 明文")
        self.assertIn("<REDACTED>", message)

    def test_write_proposal_accepts_legal_path(self):
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        self.assertEqual(
            (self.wt.path / "docs/proposals/1-demo.md").read_text(), "# demo\n")

    def test_commit_rejects_illegal_rel_path(self):
        self.wt.ensure()
        with self.assertRaises(InvalidProposalPath):
            self.wt.commit("bad", "op1", "../../../outside.txt")

    # ------------------------------------------------------ Important #1：push 分类

    def test_branch_protection_style_rejection_does_not_trigger_replay(self):
        """pre-receive hook 拒绝、远端并未领先本地：不得触发重放，须原样报错。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        self.wt.commit("docs(proposals): add #1", "op123",
                       "docs/proposals/1-demo.md")

        hook = self.remote / "hooks" / "pre-receive"
        hook.write_text("#!/bin/sh\necho 'policy: branch protection' >&2\nexit 1\n")
        hook.chmod(0o755)

        with mock.patch.object(self.wt, "_replay_onto_remote") as replay_spy:
            with self.assertRaises(PushRejected):
                self.wt.push()
            replay_spy.assert_not_called()

    def test_push_retry_exhausted_raises_after_exactly_three_pushes_two_replays(self):
        """对手每次都能在我们 push 前抢先推进 main：3 次 push、2 次有效 replay，
        异常保留最后一次 stderr。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        self.wt.commit("docs(proposals): add #1", "op123",
                       "docs/proposals/1-demo.md")

        racer = Path(self.tmp.name) / "racer"
        subprocess.run([GIT, "clone", str(self.remote), str(racer)],
                       check=True, capture_output=True)
        run(racer, "config", "user.email", "racer@example.com")
        run(racer, "config", "user.name", "racer")

        push_count = {"n": 0}
        real_run = subprocess.run

        def fake_run(cmd, *a, **kw):
            if isinstance(cmd, list) and cmd[:2] == [GIT, "push"]:
                push_count["n"] += 1
                real_run([GIT, "fetch", "origin", "main"], cwd=racer,
                         capture_output=True, text=True)
                real_run([GIT, "reset", "--hard", "origin/main"], cwd=racer,
                         capture_output=True, text=True)
                fname = f"race-{push_count['n']}.txt"
                (racer / fname).write_text("x\n")
                real_run([GIT, "add", fname], cwd=racer,
                         capture_output=True, text=True)
                real_run([GIT, "commit", "-m", f"race {push_count['n']}"],
                         cwd=racer, capture_output=True, text=True)
                real_run([GIT, "push", "origin", "main"], cwd=racer,
                         capture_output=True, text=True)
            return real_run(cmd, *a, **kw)

        with mock.patch.object(self.wt, "_replay_onto_remote",
                              wraps=self.wt._replay_onto_remote) as replay_spy:
            with mock.patch("subprocess.run", side_effect=fake_run):
                with self.assertRaises(NonFastForward) as ctx:
                    self.wt.push()
            self.assertEqual(replay_spy.call_count, 2,
                             "3 次 push 只应触发 2 次 replay（第 3 次失败后直接抛出）")
        self.assertEqual(push_count["n"], 3, "必须恰好尝试 3 次 push")
        self.assertTrue(ctx.exception.last_stderr, "异常必须携带最后一次 stderr")
        self.assertEqual(ctx.exception.attempts, 3)

    # ------------------------------------------------ Important #2：ensure 静默改状态

    def test_ensure_allow_reset_false_preserves_pending_commit_when_no_cherry_pick(self):
        """无残留 cherry-pick/merge 时，allow_reset=False 不得改动待推 commit 的
        HEAD 与 index。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        sha = self.wt.commit("docs(proposals): add #1", "op123",
                             "docs/proposals/1-demo.md")

        self.wt.ensure(allow_reset=False)

        self.assertEqual(run(self.wt.path, "rev-parse", "HEAD"), sha,
                         "allow_reset=False 时 HEAD 不得被改动")
        self.assertEqual(run(self.wt.path, "status", "--porcelain"), "",
                         "allow_reset=False 时 index 不得被改动")

    # ------------------------------------------------ Important #3：operation 检测

    def test_operation_id_prefix_does_not_collide(self):
        """`op12` 不得命中 `op123` 的提交（子串 grep 的典型误判）。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        self.wt.commit("docs(proposals): add #1", "op123",
                       "docs/proposals/1-demo.md")
        self.assertIsNone(self.wt.operation_commit_sha("op12"))
        self.assertFalse(self.wt.local_has_operation("op12"))

    def test_duplicate_operation_marker_raises_ambiguous(self):
        """同一 operation_id 命中多个提交：拒绝静默取第一条，须报一致性错误。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        self.wt.commit("docs(proposals): add #1", "op123",
                       "docs/proposals/1-demo.md")
        self.wt.write_proposal("docs/proposals/2-x.md", "# x\n")
        self.wt.commit("docs(proposals): add #2 (duplicate marker)", "op123",
                       "docs/proposals/2-x.md")
        with self.assertRaises(AmbiguousOperation):
            self.wt.operation_commit_sha("op123")

    def test_remote_has_operation_requires_matching_changed_path(self):
        """远端命中 marker 的提交若未恰好改动预期路径，须判为 False。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        self.wt.commit("docs(proposals): add #1", "op123",
                       "docs/proposals/1-demo.md")
        self.wt.push()
        self.assertFalse(self.wt.remote_has_operation(
            "op123", "docs/proposals/2-other.md"),
            "trailer 命中但改动路径不符，不得判为 True")

    # -------------------------------------------------------- Important #4：异常路径

    def test_replay_conflict_raised_before_any_reset_when_operation_sha_missing(self):
        """operation_sha 为 None 时，必须在任何 reset 之前就抛 ReplayConflict。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        self.wt.commit("docs(proposals): add #1", "op123",
                       "docs/proposals/1-demo.md")
        pre_head = run(self.wt.path, "rev-parse", "HEAD")

        (self.local / "other.txt").write_text("other\n")
        run(self.local, "add", "other.txt")
        run(self.local, "commit", "-m", "other work")
        run(self.local, "push", "origin", "main")

        self.wt.operation_sha = None
        with self.assertRaises(ReplayConflict):
            self.wt._replay_onto_remote()
        self.assertEqual(run(self.wt.path, "rev-parse", "HEAD"), pre_head,
                         "operation_sha 缺失时不得先 reset 再报错")

    def test_assert_single_path_blocks_multi_path_operation_commit(self):
        """operation 提交若改了两个路径，_assert_single_path 必须阻断重放，
        且远端 main 与 worktree HEAD 均不受影响。"""
        self.wt.ensure()
        self.wt.write_proposal("docs/proposals/1-demo.md", "# demo\n")
        target = self.wt.path / "docs/proposals/1-demo.md"
        target.write_text("# demo\n")
        extra = self.wt.path / "docs/proposals/extra.txt"
        extra.write_text("extra\n")
        run(self.wt.path, "add", "--",
            "docs/proposals/1-demo.md", "docs/proposals/extra.txt")
        run(self.wt.path, "-c", "user.name=scrollz-harness",
            "-c", "user.email=harness@localhost", "commit", "-m",
            "docs(proposals): add #1\n\nHARNESS-OP:op123\n")
        self.wt.operation_sha = run(self.wt.path, "rev-parse", "HEAD")
        self.wt.operation_path = "docs/proposals/1-demo.md"
        pre_head = run(self.wt.path, "rev-parse", "HEAD")

        (self.local / "other.txt").write_text("other\n")
        run(self.local, "add", "other.txt")
        run(self.local, "commit", "-m", "other work")
        run(self.local, "push", "origin", "main")

        with self.assertRaises(ReplayConflict):
            self.wt._replay_onto_remote()

        self.assertEqual(run(self.wt.path, "rev-parse", "HEAD"), pre_head,
                         "多路径提交阻断后 worktree HEAD 不得被改动")
        remote_head = run(self.local, "rev-parse", "origin/main")
        self.assertEqual(
            run(self.local, "log", "-1", "--format=%s", remote_head),
            "other work", "远端 main 不得被污染")


if __name__ == "__main__":
    unittest.main()
