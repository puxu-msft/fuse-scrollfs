import subprocess, tempfile, unittest
from pathlib import Path
from unittest import mock
from harness import db
from harness.claude_runner import InvocationResult
from harness.config import GIT
from harness.gitops import PublishWorktree
from harness.outbox import Outbox
from harness.publish import Publisher
from harness.queue import Queue, fingerprint
from harness import round as round_module
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

    def test_open_root_resumes_without_invoking_model(self):
        """恢复优先于新扫描：有未结 root 时绝不起模型（评审 C-02）。

        先用一次 stop_after="issue" 的 publish() 制造一个 observed-但-未-settled
        的 publish_proposal root（Issue 已建、卡片未提交/未 push/收据未写），
        这正是「建 Issue 响应丢失」之后真实会留下的持久化状态。随后传入的
        `invoke` 是被调用即失败的哨兵——一旦 run_round 在有未结 root 时仍去
        起模型，本测试必须变红。
        """
        candidate = dict(CANDIDATE_PAYLOAD["candidates"][0])
        candidate["fingerprint"] = fingerprint(
            candidate["goal"], candidate["invariant"],
            candidate["primary_path"], candidate["oracle"])
        prior_publisher = Publisher(Outbox(self.conn), self.gh, self.wt,
                                    Queue(self.conn), "prior-round")
        prior_publisher.publish(candidate, stop_after="issue")

        # 制造后的状态必须真的是「未结 root」，否则下面的断言是在测一个
        # 从未发生过的场景（core-must-match-the-claim）。
        outbox = Outbox(self.conn)
        self.assertTrue(outbox.open_roots(), "前置状态必须先造出一个未结 root")

        sentinel_invoke = mock.Mock(
            side_effect=AssertionError("有未结 root 时不得起模型"))
        deps = Deps(conn=self.conn, gh=self.gh, worktree=self.wt,
                   outbox=Outbox(self.conn), queue=Queue(self.conn),
                   invoke=sentinel_invoke, tools=("/usr/bin/git",))

        result = run_round(self.cfg, deps)

        sentinel_invoke.assert_not_called()
        self.assertEqual(result["mode"], "resume")
        self.assertEqual(result["result"], "resumed")
        row = self.conn.execute(
            "SELECT settled_usd FROM rounds WHERE round_id=?",
            (result["round_id"],)).fetchone()
        self.assertAlmostEqual(row["settled_usd"], 0.0,
                               msg="恢复轮不应产生新的模型花费")

    def test_remaining_time_budget_is_passed_to_invoke_as_timeout(self):
        """单调截止：剩余时间被真实传给 invoke，且必须小于整轮截止。

        用受控的 `time.monotonic()` 序列模拟「round 开始后已经过 200 秒」，
        断言 `invoke` 实际收到的 `timeout_s` 等于 `ROUND_DEADLINE_S - 200`，
        既为正数、不超过整轮截止，也严格小于整轮截止——即确实为
        checkpoint/结算预留了时间窗，不是把整轮时间全部塞给模型。
        """
        seen_timeouts = []

        def capturing_invoke(**kw):
            seen_timeouts.append(kw.get("timeout_s"))
            return InvocationResult(True, {"candidates": []}, 0.01, 1)

        deps = self._deps(None)
        deps.invoke = capturing_invoke

        elapsed = 200.0
        times = [1_000_000.0, 1_000_000.0 + elapsed]
        state = {"n": 0}

        def fake_monotonic():
            idx = min(state["n"], len(times) - 1)
            state["n"] += 1
            return times[idx]

        with mock.patch.object(round_module.time, "monotonic",
                               side_effect=fake_monotonic):
            run_round(self.cfg, deps)

        self.assertEqual(len(seen_timeouts), 1, "invoke 必须恰好被调用一次")
        timeout_s = seen_timeouts[0]
        self.assertIsNotNone(
            timeout_s, "timeout_s 必须真的把 remaining_time 传给 invoke，"
                      "而不是被忽略或传 None")
        self.assertGreater(timeout_s, 0)
        self.assertLessEqual(timeout_s, round_module.ROUND_DEADLINE_S)
        self.assertLess(
            timeout_s, round_module.ROUND_DEADLINE_S,
            "必须为 checkpoint/结算预留时间窗，不能把整轮时间全部给模型")
        self.assertAlmostEqual(
            timeout_s, round_module.ROUND_DEADLINE_S - elapsed,
            msg="timeout_s 必须等于 round_deadline 减去已流逝的单调时间，"
               "而不是任意常量")


if __name__ == "__main__":
    unittest.main()
