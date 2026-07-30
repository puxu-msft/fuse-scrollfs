import subprocess, tempfile, unittest
from pathlib import Path
from harness import db
from harness.claude_runner import InvocationResult
from harness.config import GIT
from harness.gitops import PublishWorktree
from harness.outbox import Outbox
from harness.queue import Queue
from harness.round import Deps, run_round
from harness.tests.fakes import FakeGitHub

CANDIDATE_PAYLOAD = {
    "candidates": [{
        "title": "archive: 尾日志加 per-record CRC",
        "lane": "defect", "goal": "补 CRC", "invariant": "尾日志完整性",
        "primary_path": "crates/scrollz/src/archive.rs",
        "oracle": "翻转一个字节后读取必须 fail-closed",
        "slug": "tail-journal-crc", "size": "M", "priority": "T1",
        "needs_decision": False, "body_md": "## 意图\n补 CRC\n",
        "labels": ["harness", "harness:proposed", "T1", "size:M", "lane:defect"],
    }]
}


class Cfg:
    def __init__(self, root):
        self.repo_root = root
        self.gh_token = "tok"
        self.round_budget_usd = 1.0
        self.daily_budget_usd = 5.0
        self.max_turns = 20
        self.proposed_cap = 20
        self.lane_cap = 6


def run(cwd, *a):
    return subprocess.run([GIT, *a], cwd=cwd, capture_output=True, text=True,
                          check=True).stdout.strip()


class TestRound(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.remote = root / "remote.git"
        self.local = root / "local"
        subprocess.run([GIT, "init", "--bare", "-b", "main", str(self.remote)],
                       check=True, capture_output=True)
        subprocess.run([GIT, "clone", str(self.remote), str(self.local)],
                       check=True, capture_output=True)
        run(self.local, "config", "user.email", "h@e.com")
        run(self.local, "config", "user.name", "h")
        (self.local / "README.md").write_text("seed\n")
        run(self.local, "add", "README.md")
        run(self.local, "commit", "-m", "seed")
        run(self.local, "push", "origin", "main")

        self.conn = db.connect(root / "h.db")
        db.migrate(self.conn)
        self.addCleanup(self.conn.close)
        self.gh = FakeGitHub("WRITE")
        self.cfg = Cfg(self.local)
        self.wt = PublishWorktree(self.local, self.local / ".worktree/_publish")

    def tearDown(self):
        self.tmp.cleanup()

    def _deps(self, invocation):
        return Deps(conn=self.conn, gh=self.gh, worktree=self.wt,
                    outbox=Outbox(self.conn), queue=Queue(self.conn),
                    invoke=lambda **kw: invocation, tools=("/usr/bin/git",))

    def test_successful_round_publishes_and_settles_budget(self):
        inv = InvocationResult(True, CANDIDATE_PAYLOAD, 0.30, 8)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "published")
        self.assertEqual(len(self.gh.issues), 1)
        row = self.conn.execute("SELECT settled_usd FROM rounds").fetchone()
        self.assertAlmostEqual(row["settled_usd"], 0.30)

    def test_empty_candidates_is_a_clean_noop_round(self):
        inv = InvocationResult(True, {"candidates": []}, 0.05, 3)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "no-candidate")
        self.assertEqual(len(self.gh.issues), 0)

    def test_failed_invocation_charges_full_reservation(self):
        inv = InvocationResult(False, None, 0.0, 0, exit_code=1)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "invocation-failed")
        day_row = self.conn.execute("SELECT settled_usd FROM budget_days").fetchone()
        self.assertAlmostEqual(day_row["settled_usd"], 1.0,
                               msg="结果未知必须按最坏上限计费")

    def test_needs_decision_candidate_gets_that_label_not_proposed(self):
        payload = {"candidates": [dict(CANDIDATE_PAYLOAD["candidates"][0],
                                       needs_decision=True)]}
        inv = InvocationResult(True, payload, 0.2, 5)
        run_round(self.cfg, self._deps(inv))
        number = next(iter(self.gh.issues))
        labels = self.gh.get_issue_labels(number)
        self.assertIn("harness:needs-decision", labels)
        self.assertNotIn("harness:proposed", labels)

    def test_lane_cap_blocks_that_lane_in_next_round(self):
        q = Queue(self.conn)
        for i in range(6):
            q.record(f"fp{i}", "defect", f"t{i}", "proposed")
        inv = InvocationResult(True, CANDIDATE_PAYLOAD, 0.1, 3)
        deps = self._deps(inv)
        result = run_round(self.cfg, deps)
        self.assertEqual(result["result"], "no-candidate")
        self.assertEqual(len(self.gh.issues), 0, "lane 已满不得再发布该 lane")

    def test_precheck_failure_aborts_before_spending(self):
        self.gh.permission = "READ"
        inv = InvocationResult(True, CANDIDATE_PAYLOAD, 0.3, 8)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "precheck-failed")
        rows = self.conn.execute("SELECT COUNT(*) AS n FROM budget_days").fetchone()
        self.assertEqual(rows["n"], 0, "预检失败不得预留预算")


if __name__ == "__main__":
    unittest.main()
