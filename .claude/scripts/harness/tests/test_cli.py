"""CLI 出口码：round 恢复态与 probe 负向验证（评审 Important #5）；
doctor 纯只读诊断门（评审 Important-2 追加事项）。

`main()` 在 `round`/`probe` 分支里都会碰真实 db/gh/worktree 依赖，这里全部
打桩隔离，只验证退出码判定逻辑本身。
"""

import tempfile
import unittest
from pathlib import Path
from unittest import mock

from harness import cli, db
from harness.claude_runner import InvocationResult
from harness.lifecycle import State
from harness.outbox import Outbox
from harness.publish import Publisher
from harness.queue import Queue
from harness.tests.fakes import FakeGitHub
from harness.tests.test_precheck import FakeWorktree


class _Cfg:
    def __init__(self, root):
        self.repo_root = root
        self.state_db = root / "h.db"
        self.publish_worktree = root / ".worktree/_publish"
        self.gh_token = "tok"


class TestCliRoundExitCode(unittest.TestCase):
    """`round` 命令：成功恢复（RECEIPT_COMPLETE）必须退出 0，其余恢复态必须
    非零——否则一次半途而废的恢复会被 systemd 误报为成功。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def _run_with_result(self, result: dict) -> int:
        cfg = _Cfg(self.root)
        with mock.patch.object(cli, "load_config", return_value=cfg), \
             mock.patch.object(cli, "_wire",
                               return_value=(mock.Mock(), mock.Mock(), mock.Mock())), \
             mock.patch.object(cli, "Outbox"), \
             mock.patch.object(cli, "Queue"), \
             mock.patch.object(cli, "run_round", return_value=result):
            return cli.main(["round"])

    def test_resumed_with_receipt_complete_exits_zero(self):
        result = {"round_id": "r1", "mode": "resume", "result": "resumed",
                  "issue": 1, "state": State.RECEIPT_COMPLETE}
        self.assertEqual(self._run_with_result(result), 0)

    def test_resumed_with_inconsistent_state_exits_nonzero(self):
        """真实反例：恢复轮跑完但收据核验未通过，之前的实现只看字符串
        `result="resumed"` 就判定成功，这里必须非零。"""
        result = {"round_id": "r1", "mode": "resume", "result": "resumed",
                  "issue": 1, "state": State.INCONSISTENT}
        self.assertEqual(self._run_with_result(result), 1)

    def test_resumed_without_state_field_exits_nonzero(self):
        result = {"round_id": "r1", "mode": "resume", "result": "resumed"}
        self.assertEqual(self._run_with_result(result), 1)

    def test_published_still_exits_zero(self):
        result = {"round_id": "r1", "mode": "scan", "result": "published",
                  "issue": 1, "state": State.RECEIPT_COMPLETE}
        self.assertEqual(self._run_with_result(result), 0)

    def test_budget_exhausted_exits_nonzero(self):
        result = {"round_id": "r1", "mode": "scan", "result": "budget-exhausted"}
        self.assertEqual(self._run_with_result(result), 1)


class TestCliProbeExitCode(unittest.TestCase):
    """`probe` 命令：必须同时要求 res.ok、exit_code==0、无 protocol_errors，
    不能只看 init 事件看着干净（评审 Important #5 实测：failed_probe_exit=0）。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def _run_with_invocation(self, inv: InvocationResult) -> int:
        cfg = _Cfg(self.root)
        with mock.patch.object(cli, "load_config", return_value=cfg), \
             mock.patch.object(cli, "_wire",
                               return_value=(mock.Mock(), mock.Mock(), mock.Mock())), \
             mock.patch.object(cli, "invoke", return_value=inv):
            return cli.main(["probe"])

    def test_clean_success_exits_zero(self):
        inv = InvocationResult(
            True, {"candidates": []}, 0.01, 1, exit_code=0,
            init_seen=True, init_tools=sorted(cli.STAGE1_TOOLS.split(",")),
            init_mcp_servers=[], init_plugins=[], init_errors=[],
            protocol_errors=[])
        self.assertEqual(self._run_with_invocation(inv), 0)

    def test_nonzero_exit_code_with_clean_init_fails(self):
        """真实反例：claude 进程退出 1，但 init 事件本身看着干净——旧实现
        只看 init 字段，这里必须判失败。"""
        inv = InvocationResult(
            False, None, 0.0, 0, exit_code=1,
            init_seen=True, init_tools=sorted(cli.STAGE1_TOOLS.split(",")),
            init_mcp_servers=[], init_plugins=[], init_errors=[],
            protocol_errors=[])
        self.assertEqual(self._run_with_invocation(inv), 1)

    def test_protocol_errors_present_fails(self):
        inv = InvocationResult(
            False, None, 0.0, 0, exit_code=0,
            init_seen=True, init_tools=sorted(cli.STAGE1_TOOLS.split(",")),
            init_mcp_servers=[], init_plugins=[], init_errors=[],
            protocol_errors=["duplicate terminal result events: 2"])
        self.assertEqual(self._run_with_invocation(inv), 1)

    def test_res_ok_false_fails_even_with_zero_exit_code(self):
        inv = InvocationResult(
            False, None, 0.02, 1, exit_code=0,
            init_seen=True, init_tools=sorted(cli.STAGE1_TOOLS.split(",")),
            init_mcp_servers=[], init_plugins=[], init_errors=[],
            protocol_errors=[])
        self.assertEqual(self._run_with_invocation(inv), 1)

    def test_missing_init_event_fails(self):
        inv = InvocationResult(False, None, 0.0, 0, exit_code=0, init_seen=False)
        self.assertEqual(self._run_with_invocation(inv), 1)


class TestCliDoctorIsReadOnly(unittest.TestCase):
    """`doctor` 必须走纯读诊断入口 `inspect_facts()`，不能是带副作用的
    `run_prechecks()`——否则会 reconcile/fetch/reset 发布工作区，且对尚未
    收敛的 `prepared` root 假绿（评审 Important-2 追加事项）。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.conn = db.connect(self.root / "h.db")
        db.migrate(self.conn)
        self.addCleanup(self.conn.close)

    def _run_doctor(self, gh, worktree):
        cfg = _Cfg(self.root)
        with mock.patch.object(cli, "load_config", return_value=cfg), \
             mock.patch.object(cli, "_wire",
                               return_value=(self.conn, gh, worktree)):
            return cli.main(["doctor"])

    def test_prepared_root_reports_failure_without_touching_worktree(self):
        """库里放一个 prepared root，跑 doctor：必须返回非 0（把未完成事务
        报出来），且 `FakeWorktree.ensured` 必须为 False——纯读绝不触碰
        worktree。"""
        gh = FakeGitHub("WRITE")
        outbox = Outbox(self.conn)
        candidate = {
            "fingerprint": "fp-doctor-1", "title": "t", "slug": "s",
            "lane": "defect", "labels": ["harness"], "body_md": "b",
        }
        seed_worktree = FakeWorktree()
        Publisher(outbox, gh, seed_worktree, Queue(self.conn), "r0").publish(
            candidate, stop_after="issue")
        self.assertTrue(outbox.open_roots(),
                        "前置状态必须先造出一个未收敛 root")

        doctor_worktree = FakeWorktree()
        exit_code = self._run_doctor(gh, doctor_worktree)

        self.assertNotEqual(exit_code, 0,
                            "存在未收敛 root 时 doctor 必须报告失败，不能假绿")
        self.assertFalse(doctor_worktree.ensured,
                         "doctor 是纯只读诊断，绝不能调用 worktree.ensure()")

    def test_clean_state_exits_zero_without_touching_worktree(self):
        gh = FakeGitHub("WRITE")
        worktree = FakeWorktree()
        exit_code = self._run_doctor(gh, worktree)
        self.assertEqual(exit_code, 0)
        self.assertFalse(worktree.ensured)


if __name__ == "__main__":
    unittest.main()
