"""崩溃点子矩阵：每个 operation 三个注入相位，重启后必须收敛且不重复。"""

import subprocess, tempfile, unittest
from pathlib import Path
from harness import db
from harness.config import GIT
from harness.gitops import PublishWorktree
from harness.lifecycle import State
from harness.outbox import Outbox, ResponseLost, TerminalOperationError
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


# 机器派生的覆盖表（评审 Critical B）：不再手写，从 `Outbox.OPERATION_KINDS`
# × `Outbox.FAULT_PHASES` 的笛卡尔积生成——生产 registry 新增 operation 时
# 测试自动同步，不会因为忘记手写一行而悄悄漏项。
REQUIRED_CRASH_COVERAGE = {
    kind: set(Outbox.FAULT_PHASES) for kind in Outbox.OPERATION_KINDS
}

# 每个用例登记它覆盖的 operation:phase；下面的覆盖表测试据此校验无漏项
COVERED: set[str] = set()


class TestCrashPoints(CrashMatrixBase):
    def _crash_at(self, point: str):
        """用 HARNESS_FAULT 在指定 operation:phase 定点崩溃。

        `COVERED.add(point)` 必须放在 `assertRaises` 确认异常真的抛出**之后**
        才登记——登记的应是"这个点被实际触达并崩溃过"，而不是"循环遍历过
        这个 required 字符串"（评审 Important #1）。若 `HARNESS_FAULT` 写错、
        或该 phase 从未被 `_fault_check()` 命中，`assertRaises` 会让上层
        subTest 失败，但覆盖表本身不该在那种情况下也标记"已覆盖"。
        """
        import os
        from harness.outbox import InjectedFault
        os.environ["HARNESS_FAULT"] = point
        try:
            with self.assertRaises(InjectedFault):
                self.publisher("r1").publish(CANDIDATE)
        finally:
            del os.environ["HARNESS_FAULT"]
        COVERED.add(point)

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
        """服务端未生效：评审 Critical A 之后，阴性读取不再立即授权重发。

        `record_uncertain_observation()` 的跨轮次有界观察窗（`Outbox.
        UNCERTAIN_WINDOW`）内，每一轮 resume 都只累计『仍不确定』的观察
        次数，绝不重新调用底层 `call()`——即便探测持续阴性、即便该阴性
        其实反映了『真的没创建』这个事实（本场景 `applied=False` 正是如此），
        outbox 也无法仅凭一次或多次阴性读取区分『真没创建』与『延迟未
        可见』，因此**不猜**：窗口耗尽后转 `failed_terminal`，交人工介入。
        """
        self.gh.fail_next(method, applied=False)
        with self.assertRaises(ResponseLost):
            self.publisher("r1").publish(CANDIDATE)
        self.assertEqual(self.gh.calls.count(method), 1)
        # 窗口内的后续轮次只累计观察，绝不重发底层调用
        for _ in range(Outbox.UNCERTAIN_WINDOW - 2):
            with self.assertRaises(ResponseLost):
                self.publisher("r2").publish(CANDIDATE)
        self.assertEqual(self.gh.calls.count(method), 1,
                         "窗口内任何阴性读取都不得授权重发底层调用")
        # 窗口耗尽：转 failed_terminal，交人工介入，绝不猜测式地再重发
        with self.assertRaises(TerminalOperationError):
            self.publisher("r3").publish(CANDIDATE)
        self.assertEqual(self.gh.calls.count(method), 1,
                         "窗口耗尽后仍不得重发——转人工而非猜测")
        unresolved = Outbox(self.conn).unresolved()
        self.assertEqual(len(unresolved), 1,
                         "failed_terminal 必须被 unresolved() 捕获以阻断预检")

    def test_create_issue_response_lost_but_applied(self):
        self._resume_after_lost_but_applied("create_issue")

    def test_create_issue_response_lost_not_applied(self):
        self._resume_after_lost_not_applied("create_issue")

    def test_receipt_response_lost_but_applied(self):
        self._resume_after_lost_but_applied("create_comment")

    def test_receipt_response_lost_not_applied(self):
        self._resume_after_lost_not_applied("create_comment")

    def test_checkpoint_after_local_commit_before_push(self):
        """局部 checkpoint 测试：用 `stop_after` 提前返回模拟"只做到这一步"，
        不是模拟进程中止（不经过 kill/信号），随后直接调
        `Publisher.publish()` 恢复，也不走 `run_round → precheck →
        open_roots → resume` 的生产恢复链路（评审 Important #2）。

        它验证的是"如果只做到 commit 这一步，从这个中间状态重新调用
        publish() 能否收敛"，这本身有价值，但证明力不等于表驱动的 12 相位
        矩阵（`test_every_required_crash_phase_recovers`）——那才是经由
        `HARNESS_FAULT` 真实抛出异常、并模拟完整进程重启的崩溃恢复测试。
        真实崩溃恢复的生产入口集成测试见 Task 12。
        """
        p = self.publisher("r1")
        p.publish(CANDIDATE, stop_after="commit")
        self.assertTrue(self.wt.local_has_operation(p.last_operation_id))
        result = self.publisher("r2").publish(CANDIDATE)
        self.assertEqual(result["state"], State.RECEIPT_COMPLETE)
        self.assert_converged()

    def test_checkpoint_after_push_before_receipt(self):
        """局部 checkpoint 测试，同上——`stop_after="push"` 提前返回，不经过
        生产恢复入口。见 `test_checkpoint_after_local_commit_before_push`
        的 docstring（评审 Important #2）。
        """
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

    def test_worktree_lost_after_commit_before_push_regenerates_commit(self):
        """本地 commit 完成、尚未 push 时 worktree 丢失：必须重新生成提案提交
        并真正 push，不能把已失效的旧 SHA 当作"已推送"上报（评审 Critical）。

        与 `test_worktree_wiped_between_rounds_still_converges` 的区别：那个
        用例在 **push 之后** 丢 worktree，origin/main 已经带着提案提交，新建的
        worktree 天然把它继承回来。本用例在 **push 之前** 丢 worktree——
        origin/main 上还没有提案提交，`ensure(allow_reset=False)` 重建的
        worktree HEAD 就是裸的 origin/main，旧 SHA 已不在其祖先链上。

        修复前的实现里，`probe_commit()` 先信 SQLite 缓存的旧 SHA、从不核对
        它是否仍在当前 worktree 历史里，于是 push 会把这个裸 HEAD（等于
        origin/main 自己）当成"已推送"，远端根本没有提案卡，而账本却把
        commit/push 两个 operation 都记成 observed，永久卡在 inconsistent、
        无法再恢复。
        """
        p1 = self.publisher("r1")
        p1.publish(CANDIDATE, stop_after="commit")
        self.assertTrue(self.wt.local_has_operation(p1.last_operation_id),
                        "前置条件：commit 必须先在本地真实存在")
        # 模拟本地 worktree 丢失（磁盘故障/误删），而不是正常 push 后的清理
        subprocess.run([GIT, "worktree", "remove", "--force",
                        str(self.wt.path)], cwd=self.local, capture_output=True)
        self._restart()

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
        result = unittest.TextTestRunner(verbosity=0).run(suite)
        self.assertTrue(
            result.wasSuccessful(),
            f"重跑 TestCrashPoints 时出现失败/错误，覆盖表不可信："
            f"failures={result.failures!r} errors={result.errors!r}")
        required = {f"{op}:{phase}"
                    for op, phases in REQUIRED_CRASH_COVERAGE.items()
                    for phase in phases}
        missing = required - COVERED
        self.assertEqual(missing, set(), f"崩溃矩阵漏项：{sorted(missing)}")
        self.assertEqual(len(required), 12, "required 相位数应为 4 operation × 3 phase")

        # 评审 Critical B 附带项：registry 中不得存在未被测试覆盖的
        # operation——即 `Outbox.OPERATION_KINDS` 必须与
        # `REQUIRED_CRASH_COVERAGE` 的 key 完全一致，不多不少。
        self.assertEqual(set(Outbox.OPERATION_KINDS),
                         set(REQUIRED_CRASH_COVERAGE.keys()),
                         "Outbox.OPERATION_KINDS 与覆盖表 key 不一致——"
                         "registry 里存在未被覆盖表纳入的 operation，或反之")

    def test_regression_probe_detects_added_operation_as_missing(self):
        """回归探测（评审 Critical B）：证明"registry 派生"这条链路真的能
        发现漏项，而不是一个无论如何都通过的空检查——临时向
        `Outbox.OPERATION_KINDS` 追加一个假 operation，派生出的 required
        集合必须包含它、且不在 `COVERED` 里（因为没有任何用例真的跑过它），
        验证完毕立即移除，不污染其余测试。
        """
        fake_kind = "fake_op_for_regression_probe"
        original = Outbox.OPERATION_KINDS
        Outbox.OPERATION_KINDS = original + (fake_kind,)
        try:
            derived = {kind: set(Outbox.FAULT_PHASES)
                      for kind in Outbox.OPERATION_KINDS}
            required = {f"{op}:{phase}"
                        for op, phases in derived.items()
                        for phase in phases}
            missing = required - COVERED
            expected_missing = {f"{fake_kind}:{phase}"
                                for phase in Outbox.FAULT_PHASES}
            self.assertEqual(
                missing, expected_missing,
                "追加假 operation 后，派生覆盖表必须能识别出它未被覆盖——"
                "若这里不等于预期，说明覆盖表检测本身失去了抓漏项的能力")
        finally:
            Outbox.OPERATION_KINDS = original
        self.assertEqual(Outbox.OPERATION_KINDS, original,
                         "验证完毕后必须恢复原始 registry，不得污染其余测试")


if __name__ == "__main__":
    unittest.main()
