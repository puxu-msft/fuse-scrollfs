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
from harness.role_invocation import RoleInvocationRequest, to_invoke_kwargs
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

    def test_published_with_inconsistent_state_exits_nonzero(self):
        """真实反例（评审 Critical B）：`result="published"` 曾无条件退出
        0，即便 `state` 是 `inconsistent`（收据核验发现绑定冲突）。新发布
        与恢复必须共用同一个成功谓词——终态必须是
        `publication-receipt-complete` 才退出 0。"""
        result = {"round_id": "r1", "mode": "scan", "result": "published",
                  "issue": 1, "state": State.INCONSISTENT}
        self.assertEqual(self._run_with_result(result), 1)

    def test_published_with_proposal_published_state_exits_nonzero(self):
        """真实反例（评审 Critical B）：`state="proposal-published"` 表示
        收据尚未核验通过（`receipt_present` 为 False），旧实现同样会被
        无条件判 0。"""
        result = {"round_id": "r1", "mode": "scan", "result": "published",
                  "issue": 1, "state": State.PUBLISHED}
        self.assertEqual(self._run_with_result(result), 1)

    def test_budget_exhausted_exits_nonzero(self):
        result = {"round_id": "r1", "mode": "scan", "result": "budget-exhausted"}
        self.assertEqual(self._run_with_result(result), 1)

    def test_every_round_result_has_an_explicit_exit_code_case(self):
        cases = {
            "precheck-failed": ({}, 1),
            "resumed": ({"state": State.RECEIPT_COMPLETE}, 0),
            "budget-exhausted": ({}, 1),
            "deadline-exhausted": ({}, 1),
            "invocation-failed": ({}, 1),
            "capability-drift": ({}, 1),
            "invalid-candidate": ({}, 1),
            "no-candidate-degraded": ({}, 1),
            "no-candidate": ({}, 0),
            "duplicate": ({}, 0),
            "published": ({"state": State.RECEIPT_COMPLETE}, 0),
            "unhandled-exception": ({}, 1),
        }
        for result_name, (extra, expected) in cases.items():
            with self.subTest(result=result_name):
                result = {
                    "round_id": "r1",
                    "mode": "scan",
                    "result": result_name,
                    **extra,
                }
                self.assertEqual(self._run_with_result(result), expected)

    def test_round_result_vocabulary_matches_exit_policy_registry(self):
        from harness.round import ROUND_RESULTS

        self.assertEqual(set(cli.ROUND_EXIT_POLICIES), set(ROUND_RESULTS))


class TestCliInvokeAdapter(unittest.TestCase):
    def test_adapter_expands_every_required_invoke_keyword(self):
        request = RoleInvocationRequest(
            role="finder:code",
            prompt="find",
            tools="Glob,Grep,Read",
            grant_usd=0.2,
            max_turns=9,
            settings_path="settings.json",
            cwd="/repo",
            timeout_s=42.0,
            model="claude-sonnet-5",
            stream_log="stream.jsonl",
            session_id="11111111-1111-4111-8111-111111111111",
        )
        seen = {}

        def fake_invoke(**kwargs):
            seen.update(kwargs)
            return "sentinel"

        with mock.patch.object(cli, "invoke", side_effect=fake_invoke):
            result = cli._build_invoke_adapter()(request)

        self.assertEqual(result, "sentinel")
        self.assertEqual(seen, to_invoke_kwargs(request))
        self.assertNotIn("role", seen)
        self.assertEqual(seen["cwd"], "/repo")
        self.assertEqual(seen["timeout_s"], 42.0)


class TestCliProbePromptParserSeam(unittest.TestCase):
    """真实接缝测试（评审 Critical B）：probe 实际会发出的提示词
    （`cli.PROBE_PROMPT`）要求模型回复什么，就把该回复原样喂给
    `parse_stream_json()`，断言 probe 的判定与之一致——不允许再手工构造
    `payload={"candidates": []}` 绕过「提示词 → parser」这段接缝。

    修复前的反例：旧提示词是「回复 OK，不要调用任何工具」，模型正确遵从后
    `_extract_payload("OK")` 返回 None，`res.ok` 恒为 False，probe 在设计上
    永远不可能通过——而旧的单测全部手工构造了合法 payload，从未真正验证过
    这条提示词能否产生一个会被 parser 接受的回复。
    """

    def _stream_lines(self, result_text: str) -> list[str]:
        import json as _json
        init_event = _json.dumps({
            "type": "system", "subtype": "init",
            "tools": sorted(cli.STAGE1_TOOLS.split(",")),
            "mcp_servers": [], "plugins": [],
        })
        result_event = _json.dumps({
            "type": "result", "subtype": "success",
            "total_cost_usd": 0.01, "num_turns": 1,
            "result": result_text,
        })
        return [init_event, result_event]

    def test_prompt_expected_reply_parses_to_accepted_payload(self):
        """模型若严格遵从 `cli.PROBE_PROMPT`，应回复
        `cli.PROBE_EXPECTED_REPLY`——把这段文本原样喂给
        `parse_stream_json()`，必须得到 `ok=True` 且
        `payload == {"candidates": []}`。"""
        from harness.claude_runner import parse_stream_json

        lines = self._stream_lines(cli.PROBE_EXPECTED_REPLY)
        result = parse_stream_json(lines)
        self.assertTrue(result.ok, f"protocol_errors={result.protocol_errors}")
        self.assertEqual(result.payload, {"candidates": []})

    def test_old_prompt_reply_would_have_failed_the_parser(self):
        """正控：旧提示词「回复 OK，不要调用任何工具」对应的模型回复
        `"OK"` 喂给同一个 parser 必须失败——证明本测试确实在验证这段接缝，
        而不是无论输入什么都通过。"""
        from harness.claude_runner import parse_stream_json

        lines = self._stream_lines("OK")
        result = parse_stream_json(lines)
        self.assertFalse(result.ok)
        self.assertIsNone(result.payload)


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

    def test_probe_command_invokes_with_probe_prompt(self):
        """wiring 检查（评审 Critical B）：`main(["probe"])` 实际传给
        `invoke()` 的 `prompt` 必须是 `cli.PROBE_PROMPT`——防止提示词与
        `TestCliProbePromptParserSeam` 验证的文本各说各话。"""
        cfg = _Cfg(self.root)
        inv = InvocationResult(
            True, {"candidates": []}, 0.01, 1, exit_code=0,
            init_seen=True, init_tools=sorted(cli.STAGE1_TOOLS.split(",")),
            init_mcp_servers=[], init_plugins=[], init_errors=[],
            protocol_errors=[])
        with mock.patch.object(cli, "load_config", return_value=cfg), \
             mock.patch.object(cli, "_wire",
                               return_value=(mock.Mock(), mock.Mock(), mock.Mock())), \
             mock.patch.object(cli, "invoke", return_value=inv) as mock_invoke:
            cli.main(["probe"])
        self.assertEqual(mock_invoke.call_args.kwargs["prompt"], cli.PROBE_PROMPT)

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
