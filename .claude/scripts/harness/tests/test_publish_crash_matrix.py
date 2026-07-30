"""崩溃点子矩阵：每个 operation 四个崩溃点，重启后必须收敛且不重复。"""

import subprocess, tempfile, unittest
from pathlib import Path
from harness import db
from harness.config import GIT
from harness.gitops import PublishWorktree
from harness.lifecycle import State
from harness.outbox import Outbox, ResponseLost
from harness.publish import Publisher
from harness.queue import Queue, fingerprint
from harness.tests.fakes import FakeGitHub

CANDIDATE = {
    "title": "archive: 尾日志加 per-record CRC",
    "lane": "defect",
    "labels": ["harness", "harness:proposed", "T1", "size:M", "lane:defect"],
    "body_md": "## 意图\n补 CRC\n",
    "slug": "tail-journal-crc",
    "fingerprint": fingerprint("加 CRC", "尾日志完整性",
                               "crates/scrollz/src/archive.rs", "坏块 fail-closed"),
}


def run(cwd, *args):
    return subprocess.run([GIT, *args], cwd=cwd, capture_output=True,
                          text=True, check=True).stdout.strip()


class CrashMatrixBase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self._build_fixture()

    def _build_fixture(self):
        """可重复调用：表驱动崩溃测试的每个相位都需要全新的仓库与库。

        重复调用时先关闭上一次的连接——否则每个相位遗留一个未关闭的 sqlite3
        连接，在 `-W error::ResourceWarning` 下会报 unclosed database。
        """
        if getattr(self, "conn", None) is not None:
            self.conn.close()
        root = Path(self.tmp.name) / f"fx{len(list(Path(self.tmp.name).iterdir()))}"
        root.mkdir(parents=True, exist_ok=True)
        self.root = root
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

        self.conn = db.connect(root / "h.db")
        db.migrate(self.conn)
        self.gh = FakeGitHub(permission="WRITE")
        self.wt = PublishWorktree(self.local, self.local / ".worktree/_publish")
        self.queue = Queue(self.conn)

    def tearDown(self):
        self.conn.close()
        self.tmp.cleanup()

    def publisher(self, round_id: str) -> Publisher:
        return Publisher(Outbox(self.conn), self.gh, self.wt, self.queue, round_id)

    def assert_converged(self):
        """最终状态一致：Issue 唯一、提案卡在远端唯一、收据唯一。"""
        self.assertEqual(len(self.gh.issues), 1, "Issue 必须唯一")
        number = next(iter(self.gh.issues))
        rel = f"docs/proposals/{number}-{CANDIDATE['slug']}.md"
        shas = run(self.local, "log", "origin/main", "--grep", "HARNESS-OP:",
                   "--format=%H").splitlines()
        self.assertEqual(len(shas), 1, "提案卡提交必须唯一")
        proc = subprocess.run([GIT, "cat-file", "-e", f"origin/main:{rel}"],
                              cwd=self.local, capture_output=True)
        self.assertEqual(proc.returncode, 0, f"{rel} 必须存在于远端 main")
        receipts = [c for c in self.gh.comments[number]
                    if c["body"].startswith("HARNESS-RECEIPT")]
        self.assertEqual(len(receipts), 1, "发布收据必须唯一")

        # 数据库终态 oracle：artifact 齐全 ≠ 事务收敛。若 operation 停在
        # prepared，后续轮次会永久认为仍需恢复（评审 R5-C-03）
        rows = self.conn.execute(
            "SELECT kind, phase FROM operations").fetchall()
        stuck = [(r["kind"], r["phase"]) for r in rows
                 if r["phase"] not in ("observed", "settled")]
        self.assertEqual(stuck, [], f"存在未收敛的 operation：{stuck}")
        roots = [r for r in rows if r["kind"] == "publish_proposal"]
        self.assertEqual([r["phase"] for r in roots], ["settled"],
                         "收据校验通过后 root 必须 settled")


class TestHappyPath(CrashMatrixBase):
    def test_publish_reaches_receipt_complete(self):
        result = self.publisher("r1").publish(CANDIDATE)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assert_converged()

    def test_publish_is_idempotent_on_rerun(self):
        self.publisher("r1").publish(CANDIDATE)
        result = self.publisher("r2").publish(CANDIDATE)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assert_converged()


# 机器可断言的覆盖表：手写矩阵允许，但漏项必须被测试本身抓住（评审 C-03）
REQUIRED_CRASH_COVERAGE = {
    "publish_proposal": {"before-call", "after-call", "after-observe"},
    "commit_proposal": {"before-call", "after-call", "after-observe"},
    "push_main": {"before-call", "after-call", "after-observe"},
    "publication_receipt": {"before-call", "after-call", "after-observe"},
}

# 每个用例登记它覆盖的 operation:phase；下面的覆盖表测试据此校验无漏项
COVERED: set[str] = set()


class TestCrashPoints(CrashMatrixBase):
    def _crash_at(self, point: str):
        """用 HARNESS_FAULT 在指定 operation:phase 定点崩溃。"""
        import os
        from harness.outbox import InjectedFault
        COVERED.add(point)
        os.environ["HARNESS_FAULT"] = point
        try:
            with self.assertRaises(InjectedFault):
                self.publisher("r1").publish(CANDIDATE)
        finally:
            del os.environ["HARNESS_FAULT"]

    def _restart(self):
        """丢弃全部内存对象并重开 SQLite，模拟真正的进程重启。"""
        self.conn.close()
        self.conn = db.connect(self.root / "h.db")
        self.wt = PublishWorktree(self.local, self.local / ".worktree/_publish")
        self.queue = Queue(self.conn)

    def _resume_after_lost_but_applied(self, method: str):
        """服务端已生效、响应丢失：execute 会 probe 到对象并**正常返回**，
        不抛异常。首轮就应收敛，且底层 call 只发生一次。"""
        self.gh.fail_next(method, applied=True)
        result = self.publisher("r1").publish(CANDIDATE)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assertEqual(self.gh.calls.count(method), 1,
                         "探到已生效后不得重发")
        self.assert_converged()

    def _resume_after_lost_not_applied(self, method: str):
        """服务端未生效：可恢复错误必须传播，下一轮重试后收敛。"""
        self.gh.fail_next(method, applied=False)
        with self.assertRaises(ResponseLost):
            self.publisher("r1").publish(CANDIDATE)
        result = self.publisher("r2").publish(CANDIDATE)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assert_converged()

    def test_create_issue_response_lost_but_applied(self):
        self._resume_after_lost_but_applied("create_issue")

    def test_create_issue_response_lost_not_applied(self):
        self._resume_after_lost_not_applied("create_issue")

    def test_receipt_response_lost_but_applied(self):
        self._resume_after_lost_but_applied("create_comment")

    def test_receipt_response_lost_not_applied(self):
        self._resume_after_lost_not_applied("create_comment")

    def test_crash_after_local_commit_before_push(self):
        p = self.publisher("r1")
        p.publish(CANDIDATE, stop_after="commit")
        self.assertTrue(self.wt.local_has_operation(p.last_operation_id))
        result = self.publisher("r2").publish(CANDIDATE)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assert_converged()

    def test_crash_after_push_before_receipt(self):
        p = self.publisher("r1")
        p.publish(CANDIDATE, stop_after="push")
        result = self.publisher("r2").publish(CANDIDATE)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assert_converged()

    def test_commit_sha_survives_process_restart(self):
        """进程重启后必须能从 outbox 取回绑定 SHA，否则重放会失败（评审 C-05）。"""
        p1 = self.publisher("r1")
        p1.publish(CANDIDATE, stop_after="commit")
        # 丢弃全部内存对象，重开 SQLite，模拟真正的进程重启
        self.conn.close()
        self.conn = db.connect(self.root / "h.db")
        self.wt = PublishWorktree(self.local, self.local / ".worktree/_publish")
        self.queue = Queue(self.conn)
        result = self.publisher("r2").publish(CANDIDATE)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assert_converged()

    def test_precheck_does_not_reset_away_unpushed_commit(self):
        """预检的 reset 不得毁掉待恢复提交——生产路径必须与测试路径一致。"""
        from harness.outbox import Outbox
        from harness.precheck import run_prechecks
        p1 = self.publisher("r1")
        p1.publish(CANDIDATE, stop_after="commit")
        sha_before = run(self.wt.path, "rev-parse", "HEAD")
        outbox = Outbox(self.conn)
        run_prechecks(type("C", (), {"gh_token": "t"})(), self.gh, self.wt,
                      outbox, tools=(), probes={})
        self.assertEqual(run(self.wt.path, "rev-parse", "HEAD"), sha_before,
                         "待推送提交被 reset 掉了")

    def test_every_required_crash_phase_recovers(self):
        """表驱动：12 个 operation:phase 各崩一次，重启后都必须收敛到同一终态。

        每个相位用全新 fixture，避免前一个相位的残留掩盖问题；
        每次恢复前关闭并重开 SQLite、丢弃全部内存对象，模拟真正的进程重启。
        """
        for kind, phases in sorted(REQUIRED_CRASH_COVERAGE.items()):
            for phase in sorted(phases):
                point = f"{kind}:{phase}"
                with self.subTest(point=point):
                    self._build_fixture()
                    self._crash_at(point)
                    self._restart()
                    result = self.publisher("r2").publish(CANDIDATE)
                    self.assertEqual(result["state"], State.RECEIPT_COMPLETE,
                                     f"{point} 恢复后未收敛")
                    self.assert_converged()

    def test_resume_from_sub_operation_finds_root(self):
        """未结的是子 operation 时，恢复必须解析到 root，不能拿子 payload 当候选。"""
        from harness.outbox import Outbox
        self._crash_at("push_main:before-call")
        self._restart()
        outbox = Outbox(self.conn)
        open_ops = outbox.open_operations()
        self.assertTrue(any(o.kind != "publish_proposal" for o in open_ops),
                        "本场景应存在子 operation 未结")
        roots = outbox.open_roots()
        self.assertEqual(len(roots), 1, "同一提案的多个未结子 operation 应聚合为一个 root")
        self.assertEqual(roots[0].kind, "publish_proposal")
        result = self.publisher("r2").resume(open_ops[0].operation_id)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assert_converged()

    def test_worktree_wiped_between_rounds_still_converges(self):
        """本地工作区丢失但远端已发布：不得重新发布第二份。"""
        self.publisher("r1").publish(CANDIDATE, stop_after="push")
        subprocess.run([GIT, "worktree", "remove", "--force",
                        str(self.wt.path)], cwd=self.local, capture_output=True)
        result = self.publisher("r2").publish(CANDIDATE)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assert_converged()


class TestCrashCoverageTable(unittest.TestCase):
    """防止手写矩阵将来再次漏项：每个 required operation:phase 都必须有用例登记。

    注意：本测试依赖 TestCrashPoints 先跑过（unittest 按类名字母序，
    TestCrashCoverageTable < TestCrashPoints），故显式先跑一遍它们。
    """

    def test_every_required_operation_phase_is_covered(self):
        """防止将来有人删掉表驱动循环、改回手写而漏项。

        ResponseLost 那几条测的是网络不确定性，**不计入**相位覆盖——
        它们不经过 HARNESS_FAULT，持久化状态也不同，不能冒充相位测试。
        """
        suite = unittest.TestLoader().loadTestsFromTestCase(TestCrashPoints)
        unittest.TextTestRunner(verbosity=0).run(suite)
        required = {f"{op}:{phase}"
                    for op, phases in REQUIRED_CRASH_COVERAGE.items()
                    for phase in phases}
        missing = required - COVERED
        self.assertEqual(missing, set(), f"崩溃矩阵漏项：{sorted(missing)}")
        self.assertEqual(len(required), 12, "required 相位数应为 4 operation × 3 phase")


if __name__ == "__main__":
    unittest.main()
