import subprocess, tempfile, unittest
from pathlib import Path
from harness.config import GIT
from harness.gitops import PublishWorktree


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
        from harness.gitops import ReplayConflict
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


if __name__ == "__main__":
    unittest.main()
