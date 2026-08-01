import inspect
import json
import pathlib
import math
import stat
import subprocess
import unittest

from harness.claude_runner import (
    DEFAULT_AGENT_MODEL, _HARNESS_OWNED_CLAUDE_ENV, _INHERITED_AUTH_ENV,
    STAGE1_ALLOWED_TOOLS, _extract_json_object, _extract_payload,
    UnsafeInvocationError,
    build_argv,
    invoke,
    parse_stream_json,
)

VALID_TOOLS = ",".join(sorted(STAGE1_ALLOWED_TOOLS))


def _init_line(tools=("Read", "Grep", "Glob"), mcp_servers=(),
               session_id=None):
    event = {"type": "system", "subtype": "init",
             "tools": list(tools), "mcp_servers": list(mcp_servers)}
    if session_id is not None:
        event["session_id"] = session_id
    return json.dumps(event)


def _success_line(cost=0.1, turns=1, candidates=None):
    candidates = [] if candidates is None else candidates
    return json.dumps({"type": "result", "subtype": "success",
                       "total_cost_usd": cost, "num_turns": turns,
                       "result": json.dumps({"candidates": candidates})})


def _error_line(cost=0.1, turns=1, subtype="error_max_turns"):
    return json.dumps({"type": "result", "subtype": subtype,
                       "total_cost_usd": cost, "num_turns": turns,
                       "result": ""})


class TestArgv(unittest.TestCase):
    def test_argv_pins_the_exact_isolation_combination(self):
        argv = build_argv(prompt="/scrollz-round", tools=VALID_TOOLS,
                          grant_usd=0.75, max_turns=40,
                          settings_path=".claude/harness-settings.json")
        joined = " ".join(argv)
        self.assertIn("--setting-sources project", joined)
        self.assertIn("--settings .claude/harness-settings.json", joined)
        self.assertIn("--strict-mcp-config", joined)
        self.assertIn("--permission-mode dontAsk", joined)
        self.assertIn("--output-format stream-json", joined)
        self.assertIn("--max-budget-usd 0.75", joined)
        self.assertIn("--max-turns 40", joined)
        self.assertNotIn("bypassPermissions", joined)
        self.assertNotIn("--dangerously-skip-permissions", joined)

    def test_argv_includes_verbose_flag(self):
        """--output-format=stream-json 与 --print 同时使用时，claude 要求 --verbose，
        否则直接拒绝启动（真实探针实测出来的约束）。"""
        argv = build_argv(prompt="/scrollz-round", tools=VALID_TOOLS,
                          grant_usd=0.5, max_turns=10,
                          settings_path=".claude/harness-settings.json")
        self.assertIn("--verbose", argv)

    def test_tools_never_include_write_capabilities_in_stage1(self):
        argv = build_argv(prompt="/scrollz-round", tools=VALID_TOOLS,
                          grant_usd=0.5, max_turns=10,
                          settings_path=".claude/harness-settings.json")
        idx = argv.index("--tools")
        self.assertNotIn("Bash", argv[idx + 1])
        self.assertNotIn("Edit", argv[idx + 1])
        self.assertNotIn("Write", argv[idx + 1])

    def test_build_argv_rejects_tools_with_bash_directly(self):
        """直接传入危险值，而不是只用安全样本验证。"""
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools="Read,Bash", grant_usd=0.5,
                      max_turns=10, settings_path="s.json")

    def test_build_argv_rejects_tools_default(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools="default", grant_usd=0.5,
                      max_turns=10, settings_path="s.json")

    def test_build_argv_rejects_subset_of_allowlist(self):
        """少一个工具（例如漏了 Workflow）也必须拒绝——不是"只要不多就行"。"""
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools="Read,Grep,Glob,Skill",
                      grant_usd=0.5, max_turns=10, settings_path="s.json")

    def test_build_argv_rejects_unknown_extra_tool(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS + ",Task",
                      grant_usd=0.5, max_turns=10, settings_path="s.json")

    def test_build_argv_rejects_zero_grant_usd(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.0,
                      max_turns=10, settings_path="s.json")

    def test_build_argv_rejects_negative_grant_usd(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=-1.0,
                      max_turns=10, settings_path="s.json")

    def test_build_argv_rejects_infinite_grant_usd(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=math.inf,
                      max_turns=10, settings_path="s.json")

    def test_build_argv_rejects_nan_grant_usd(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=math.nan,
                      max_turns=10, settings_path="s.json")

    def test_build_argv_rejects_zero_max_turns(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                      max_turns=0, settings_path="s.json")

    def test_build_argv_rejects_negative_max_turns(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                      max_turns=-5, settings_path="s.json")

    def test_build_argv_rejects_empty_settings_path(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                      max_turns=10, settings_path="")

    def test_build_argv_rejects_none_settings_path(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                      max_turns=10, settings_path=None)

    def test_argv_includes_session_id(self):
        session_id = "11111111-1111-4111-8111-111111111111"
        argv = build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                          max_turns=10, settings_path="s.json",
                          session_id=session_id)
        self.assertEqual(argv[argv.index("--session-id") + 1], session_id)

    def test_argv_includes_resume_and_fork_session(self):
        resume = "22222222-2222-4222-8222-222222222222"
        argv = build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                          max_turns=10, settings_path="s.json", resume=resume,
                          fork_session=True)
        self.assertEqual(argv[argv.index("--resume") + 1], resume)
        self.assertIn("--fork-session", argv)

    def test_session_id_and_resume_are_mutually_exclusive(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                       max_turns=10, settings_path="s.json",
                       session_id="11111111-1111-4111-8111-111111111111",
                       resume="22222222-2222-4222-8222-222222222222")

    def test_fork_session_requires_resume(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                       max_turns=10, settings_path="s.json",
                       fork_session=True)

    def test_session_id_must_be_a_uuid(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                       max_turns=10, settings_path="s.json",
                       session_id="not-a-uuid")

    def test_resume_must_be_a_uuid(self):
        with self.assertRaises(UnsafeInvocationError):
            build_argv(prompt="p", tools=VALID_TOOLS, grant_usd=0.5,
                       max_turns=10, settings_path="s.json",
                       resume="not-a-uuid")


class TestParse(unittest.TestCase):
    def test_extracts_payload_cost_and_turns(self):
        lines = [
            _init_line(tools=["Read", "Grep"]),
            json.dumps({"type": "assistant", "message": {"content": []}}),
            json.dumps({"type": "result", "subtype": "success",
                        "total_cost_usd": 0.42, "num_turns": 12,
                        "result": '```json\n{"candidates": [{"title": "x"}]}\n```'}),
        ]
        res = parse_stream_json(lines)
        self.assertTrue(res.ok, res.protocol_errors)
        self.assertAlmostEqual(res.cost_usd, 0.42)
        self.assertEqual(res.turns, 12)
        self.assertEqual(res.payload["candidates"][0]["title"], "x")
        self.assertEqual(res.protocol_errors, [])

    def test_error_result_is_not_ok_but_still_reports_cost(self):
        lines = [_init_line(), _error_line(cost=0.9, turns=40)]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertAlmostEqual(res.cost_usd, 0.9)
        self.assertEqual(res.turns, 40)

    def test_init_event_exposes_session_id(self):
        session_id = "33333333-3333-4333-8333-333333333333"
        res = parse_stream_json([_init_line(session_id=session_id),
                                 _success_line()])
        self.assertEqual(res.session_id, session_id)

    def test_terminal_subtype_is_exposed(self):
        res = parse_stream_json([
            _init_line(),
            _error_line(subtype="error_max_budget_usd"),
        ])
        self.assertFalse(res.ok)
        self.assertEqual(res.subtype, "error_max_budget_usd")

    def test_init_event_exposes_tools_and_mcp_for_negative_verification(self):
        """Round 0 的负向验证依赖这个：必须能看到实际生效的工具集与 MCP 列表。"""
        lines = [_init_line(tools=["Read"], mcp_servers=[]),
                 _success_line()]
        res = parse_stream_json(lines)
        self.assertEqual(res.init_tools, ["Read"])
        self.assertEqual(res.init_mcp_servers, [])

    def test_fence_inside_json_string_does_not_truncate_payload(self):
        """body_md 是 Markdown，内部可能含代码 fence——不得据此截断 payload。"""
        lines = [
            _init_line(),
            json.dumps({"type": "result", "subtype": "success",
                       "total_cost_usd": 0.1, "num_turns": 2,
                       "result": '```json\n{"candidates":[{"title":"x",'
                                 '"body_md":"example } ``` remainder"}]}\n```'}),
        ]
        res = parse_stream_json(lines)
        self.assertTrue(res.ok, res.protocol_errors)
        self.assertEqual(res.payload["candidates"][0]["title"], "x")
        self.assertIn("```", res.payload["candidates"][0]["body_md"])

    def test_trailing_text_after_closing_fence_is_rejected(self):
        """上游契约是"只输出单个 JSON 代码块，不加解释"；闭合 fence 之后还有
        说明文字说明违反契约，必须判失败。"""
        lines = [
            _init_line(),
            json.dumps({"type": "result", "subtype": "success",
                       "total_cost_usd": 0.1, "num_turns": 2,
                       "result": '```json\n{"candidates": []}\n```\n'
                                 '希望这对你有帮助！'}),
        ]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertIsNone(res.payload)

    def test_missing_init_event_is_flagged(self):
        """缺 init 事件时不得当作『干净』——absence-as-success 是假绿。"""
        lines = [_success_line()]
        res = parse_stream_json(lines)
        self.assertFalse(res.init_seen)
        self.assertFalse(res.ok)
        self.assertIn("missing init event", res.protocol_errors)

    def test_unparseable_payload_is_not_ok(self):
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "total_cost_usd": 0.1, "num_turns": 2,
                            "result": "我觉得可以做 X"})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertIsNone(res.payload)

    def test_payload_array_is_no_longer_silently_wrapped(self):
        """顶层是数组时不再无条件包装成 candidates——必须显式是含 candidates
        列表的对象。"""
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "total_cost_usd": 0.1, "num_turns": 1,
                            "result": "[1, 2]"})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertIsNone(res.payload)

    def test_object_without_candidates_field_is_rejected(self):
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "total_cost_usd": 0.1, "num_turns": 1,
                            "result": '{"foo": "bar"}'})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertIsNone(res.payload)

    def test_json_object_parser_accepts_judge_payload(self):
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "total_cost_usd": 0.1, "num_turns": 1,
                            "result": '{"verdict":"pass","reason":"r"}'})]
        res = parse_stream_json(lines, payload_parser=_extract_json_object)
        self.assertTrue(res.ok, res.protocol_errors)
        self.assertEqual(res.payload, {"verdict": "pass", "reason": "r"})

    def test_json_object_parser_rejects_non_dict_top_level(self):
        self.assertIsNone(_extract_json_object('[{"verdict":"pass"}]'))
        self.assertIsNone(_extract_json_object('"pass"'))

    def test_json_object_parser_rejects_text_after_closing_fence(self):
        self.assertIsNone(_extract_json_object(
            '```json\n{"verdict":"pass"}\n```\nextra'))

    def test_default_payload_parser_remains_finder_parser(self):
        default = inspect.signature(parse_stream_json).parameters[
            "payload_parser"].default
        self.assertIs(default, _extract_payload)
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "total_cost_usd": 0.1, "num_turns": 1,
                            "result": '{"verdict":"pass"}'})]
        self.assertFalse(parse_stream_json(lines).ok)

    # ---- 恰好一个 terminal result ----

    def test_success_then_error_is_not_ok(self):
        """成功之后又来一个 error result：不得让旧 success 的 ok=True 状态
        粘住——必须整体判为协议错误。"""
        lines = [_init_line(),
                 _success_line(cost=0.1, turns=1),
                 _error_line(cost=0.9, turns=40)]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("duplicate terminal result" in e
                            for e in res.protocol_errors))
        # 取自第一个事件，不与第二个混合
        self.assertAlmostEqual(res.cost_usd, 0.1)
        self.assertEqual(res.turns, 1)

    def test_error_then_success_is_not_ok(self):
        lines = [_init_line(),
                 _error_line(cost=0.9, turns=40),
                 _success_line(cost=0.1, turns=1)]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("duplicate terminal result" in e
                            for e in res.protocol_errors))
        self.assertAlmostEqual(res.cost_usd, 0.9)
        self.assertEqual(res.turns, 40)

    def test_two_success_results_is_not_ok(self):
        lines = [_init_line(),
                 _success_line(cost=0.1, turns=1),
                 _success_line(cost=0.2, turns=2)]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("duplicate terminal result" in e
                            for e in res.protocol_errors))

    def test_missing_terminal_result_is_flagged(self):
        lines = [_init_line()]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertIn("missing terminal result event", res.protocol_errors)

    # ---- 恰好一个 init ----

    def test_dangerous_init_followed_by_clean_init_is_not_ok(self):
        """后一个"干净" init 不得掩盖前一个危险能力集；最终能力集应保留第一
        次记录的（危险的）那份，同时整体判失败。"""
        lines = [_init_line(tools=["Read", "Bash", "Edit"]),
                 _init_line(tools=["Read"]),
                 _success_line()]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("duplicate init events" in e
                            for e in res.protocol_errors))
        self.assertEqual(res.init_tools, ["Read", "Bash", "Edit"])

    # ---- 非法 stream 行 ----

    def test_corrupted_line_after_success_makes_not_ok(self):
        lines = [_init_line(), _success_line(),
                 "not json at all, truncated proxy error page <<<"]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("unparseable stream line" in e
                            for e in res.protocol_errors))

    # ---- cost/turns 字段异常 ----

    def test_missing_cost_field_is_not_ok_and_does_not_raise(self):
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "num_turns": 1, "result": "{}"})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("missing total_cost_usd" in e
                            for e in res.protocol_errors))

    def test_string_cost_field_is_not_ok_and_does_not_raise(self):
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "total_cost_usd": "not-a-number", "num_turns": 1,
                            "result": "{}"})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("non-numeric total_cost_usd" in e
                            for e in res.protocol_errors))

    def test_negative_cost_field_is_not_ok(self):
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "total_cost_usd": -0.5, "num_turns": 1,
                            "result": "{}"})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("invalid total_cost_usd" in e
                            for e in res.protocol_errors))

    def test_nan_cost_field_is_not_ok_and_does_not_raise(self):
        # json.dumps 会把 NaN 写成裸的 "NaN"，需要手工拼字符串（json.loads 也
        # 接受这个非标准扩展）。
        lines = [_init_line(),
                 '{"type": "result", "subtype": "success", '
                 '"total_cost_usd": NaN, "num_turns": 1, "result": "{}"}']
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("invalid total_cost_usd" in e
                            for e in res.protocol_errors))

    def test_infinite_cost_field_is_not_ok_and_does_not_raise(self):
        lines = [_init_line(),
                 '{"type": "result", "subtype": "success", '
                 '"total_cost_usd": Infinity, "num_turns": 1, "result": "{}"}']
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("invalid total_cost_usd" in e
                            for e in res.protocol_errors))

    def test_negative_turns_field_is_not_ok(self):
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "total_cost_usd": 0.1, "num_turns": -3,
                            "result": "{}"})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("invalid num_turns" in e
                            for e in res.protocol_errors))

    def test_string_turns_field_is_not_ok_and_does_not_raise(self):
        lines = [_init_line(),
                 json.dumps({"type": "result", "subtype": "success",
                            "total_cost_usd": 0.1, "num_turns": "many",
                            "result": "{}"})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertTrue(any("non-numeric num_turns" in e
                            for e in res.protocol_errors))


class FakeRunner:
    """可注入的 subprocess.run 替身：记录调用参数，弹出预设的 CompletedProcess
    或抛出 TimeoutExpired，不 fork 真进程、不访问公网。"""

    def __init__(self, result=None, exc=None):
        self._result = result
        self._exc = exc
        self.calls: list[dict] = []

    def __call__(self, argv, cwd=None, capture_output=True, text=True,
                timeout=None, env=None):
        self.calls.append({"argv": argv, "cwd": cwd, "timeout": timeout,
                           "env": env})
        if self._exc is not None:
            raise self._exc
        return self._result


class TestInvoke(unittest.TestCase):
    """invoke() 的四条行为：绝不真的调用 claude、绝不访问公网，全部用可注入
    的 fake runner 驱动。"""

    def _valid_stdout(self):
        return "\n".join([_init_line(), _success_line()])

    def test_nonzero_exit_forces_not_ok_even_with_success_in_stdout(self):
        runner = FakeRunner(result=subprocess.CompletedProcess(
            args=["claude"], returncode=1, stdout=self._valid_stdout(),
            stderr="boom"))
        res = invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5,
                    max_turns=5, settings_path="s.json", cwd="/tmp",
                    timeout_s=5.0, env={"HOME": "/home/x", "PATH": "/bin"},
                    runner=runner)
        self.assertFalse(res.ok)
        self.assertEqual(res.exit_code, 1)

    def test_credential_env_vars_are_stripped_home_and_path_kept(self):
        runner = FakeRunner(result=subprocess.CompletedProcess(
            args=["claude"], returncode=0, stdout=self._valid_stdout(),
            stderr=""))
        input_env = {
            "HOME": "/home/x", "PATH": "/usr/bin:/bin",
            "GH_TOKEN": "secret-gh", "GITHUB_TOKEN": "secret-gh2",
            "SSH_AUTH_SOCK": "/tmp/ssh.sock", "GIT_ASKPASS": "/bin/askpass",
            "SSH_ASKPASS": "/bin/sshaskpass",
            "GH_CONFIG_DIR": "/home/x/.config/gh",
            "XDG_CONFIG_HOME": "/home/x/.config",
            "GIT_CONFIG_GLOBAL": "/home/x/.gitconfig",
            "GIT_CONFIG_SYSTEM": "/etc/gitconfig",
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "credential.helper",
            "GIT_CONFIG_VALUE_0": "store",
        }
        invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5, max_turns=5,
              settings_path="s.json", cwd="/tmp", timeout_s=5.0,
              env=input_env, runner=runner)
        passed_env = runner.calls[0]["env"]
        for key in ("GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK",
                   "GIT_ASKPASS", "SSH_ASKPASS", "GH_CONFIG_DIR",
                   "XDG_CONFIG_HOME", "GIT_CONFIG_GLOBAL",
                   "GIT_CONFIG_SYSTEM", "GIT_CONFIG_COUNT",
                   "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"):
            self.assertNotIn(key, passed_env, f"{key} 未被清除")
        self.assertEqual(passed_env["HOME"], "/home/x")
        self.assertEqual(passed_env["PATH"], "/usr/bin:/bin")
        self.assertEqual(passed_env["GIT_TERMINAL_PROMPT"], "0")
        self.assertEqual(passed_env["GIT_CONFIG_NOSYSTEM"], "1")

    def test_timeout_is_mapped_to_exit_code_124(self):
        runner = FakeRunner(exc=subprocess.TimeoutExpired(
            cmd=["claude"], timeout=5.0))
        res = invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5,
                    max_turns=5, settings_path="s.json", cwd="/tmp",
                    timeout_s=5.0, env={"HOME": "/home/x", "PATH": "/bin"},
                    runner=runner)
        self.assertFalse(res.ok)
        self.assertEqual(res.exit_code, 124)
        self.assertIsNone(res.subtype)

    def test_invoke_passes_session_identity_arguments(self):
        runner = FakeRunner(result=subprocess.CompletedProcess(
            args=["claude"], returncode=0, stdout=self._valid_stdout(),
            stderr=""))
        session_id = "44444444-4444-4444-8444-444444444444"
        invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5, max_turns=5,
               settings_path="s.json", cwd="/tmp", timeout_s=5.0,
               env={"HOME": "/home/x", "PATH": "/bin"}, runner=runner,
               session_id=session_id)
        argv = runner.calls[0]["argv"]
        self.assertEqual(argv[argv.index("--session-id") + 1], session_id)

    def test_invoke_uses_injected_payload_parser(self):
        stdout = "\n".join([
            _init_line(),
            json.dumps({"type": "result", "subtype": "success",
                        "total_cost_usd": 0.1, "num_turns": 1,
                        "result": '{"verdict":"pass","reason":"r"}'})
        ])
        runner = FakeRunner(result=subprocess.CompletedProcess(
            args=["claude"], returncode=0, stdout=stdout, stderr=""))
        res = invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5,
                     max_turns=5, settings_path="s.json", cwd="/tmp",
                     timeout_s=5.0, env={"HOME": "/home/x", "PATH": "/bin"},
                     runner=runner, payload_parser=_extract_json_object)
        self.assertTrue(res.ok, res.protocol_errors)
        self.assertEqual(res.payload["verdict"], "pass")

    def test_parent_claude_control_vars_cannot_reach_the_child(self):
        """父进程的 CLAUDE_CODE_*/ANTHROPIC_* 控制变量必须被清除。

        真机实测（2026-07-31）：交互会话把 `ANTHROPIC_MODEL=opus[1m]` 与
        `CLAUDE_CODE_ENABLE_TASKS=0` 透传给了 headless 子进程，导致
        (a) `--model sonnet` 被解析成溢价的 `sonnet[1m]`；
        (b) 后台任务基础设施被禁用，多 agent workflow 一起即被 kill。
        无人值守 agent 的模型与运行时能力不得由「谁启动了它」决定。
        """
        runner = FakeRunner(result=subprocess.CompletedProcess(
            args=["claude"], returncode=0, stdout=self._valid_stdout(),
            stderr=""))
        hostile = {
            "HOME": "/home/x", "PATH": "/usr/bin:/bin",
            "ANTHROPIC_MODEL": "opus[1m]",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "haiku",
            "ANTHROPIC_SMALL_FAST_MODEL": "whatever",
            "CLAUDE_CODE_ENABLE_TASKS": "0",
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "935793",
            "CLAUDE_CODE_CHILD_SESSION": "1",
            "CLAUDE_CODE_SESSION_ID": "babd6c5f-dead-beef",
            "CLAUDE_EFFORT": "high",
            "CLAUDE_PID": "6538",
            "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE": "90",
            "ANTHROPIC_AUTH_TOKEN": "sk-secret",
            "ANTHROPIC_BASE_URL": "https://api.example",
        }
        invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5, max_turns=5,
              settings_path="s.json", cwd="/tmp", timeout_s=5.0,
              env=hostile, runner=runner)
        passed = runner.calls[0]["env"]
        leaked = [k for k in passed
                  if (k.startswith(("ANTHROPIC_", "CLAUDE_"))
                      and k not in _HARNESS_OWNED_CLAUDE_ENV
                      and k not in _INHERITED_AUTH_ENV)]
        self.assertEqual(leaked, [],
                         f"父会话控制变量泄漏到 headless 子进程: {leaked}")
        self.assertEqual(passed["HOME"], "/home/x")
        self.assertEqual(passed["PATH"], "/usr/bin:/bin")
        # 认证通道必须保留，否则子进程 Not logged in、轮次根本跑不起来
        self.assertEqual(passed["ANTHROPIC_AUTH_TOKEN"], "sk-secret")
        self.assertEqual(passed["ANTHROPIC_BASE_URL"], "https://api.example")

    def test_model_is_pinned_to_a_canonical_id_not_an_alias(self):
        """别名会被 ANTHROPIC_MODEL 影响，必须传规范 ID。"""
        argv = build_argv("/x", VALID_TOOLS, 0.5, 5, "s.json",
                          model=DEFAULT_AGENT_MODEL)
        self.assertIn("--model", argv)
        picked = argv[argv.index("--model") + 1]
        self.assertNotIn("[", picked, "不得使用带上下文变体后缀的模型名")
        self.assertTrue(picked.startswith("claude-"),
                        f"必须是规范模型 ID，实际 {picked!r}")

    def test_full_stream_is_persisted_for_post_mortem(self):
        """失败轮必须留下完整 stream，否则事后无法判因。

        真机实测（2026-07-31）：一轮 $10 的 round 报 invocation-failed，而进程
        只保留了 5 行 raw_tail，无法判断到底是 payload 提取失败、协议异常还是
        预算耗尽——花了钱却拿不到诊断依据。
        """
        import tempfile
        stdout = self._valid_stdout()
        runner = FakeRunner(result=subprocess.CompletedProcess(
            args=["claude"], returncode=0, stdout=stdout, stderr="some warning"))
        with tempfile.TemporaryDirectory() as tmp:
            log = pathlib.Path(tmp) / "nested" / "r1.jsonl"
            invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5, max_turns=5,
                  settings_path="s.json", cwd="/tmp", timeout_s=5.0,
                  env={"HOME": "/home/x", "PATH": "/bin"}, runner=runner,
                  stream_log=log)
            self.assertTrue(log.exists(), "父目录不存在时也必须落盘")
            written = log.read_text(encoding="utf-8")
            self.assertIn(stdout.strip().splitlines()[0], written)
            self.assertEqual(written.count("\n") >= stdout.count("\n"), True,
                             "必须是完整 stream，不是尾部片段")
            self.assertIn("some warning", written, "stderr 也要留")
            self.assertEqual(stat.S_IMODE(log.stat().st_mode), 0o600)

    def test_stream_file_is_created_with_mode_not_chmodded_afterward(self):
        """权限必须在创建调用本身收紧，不能是创建后的第二步。

        「创建后再 chmod」存在一个从默认权限（通常 0644，全用户可读）到 chmod
        生效之间的窗口，那个窗口本身就是暴露面（rmf-08）。

        断言的是**意图**不是实现形状：原版还断言了 `source.count("os.open") == 1`，
        那绑得太死——修 cfr-p12-merged-02 时注释里提到 `os.open` 就会让它失败，
        而安全属性并没有被削弱。现在只要求「无 chmod」+「每个创建调用都带
        0o600」，具体用几次 `os.open`、是否走临时文件+replace 由实现决定。
        """
        from harness import claude_runner

        source = inspect.getsource(claude_runner._persist_stream)
        # 断言只看**代码行**：注释与 docstring 里会解释「为什么不用 chmod」，
        # 对整段源文本做子串检查会被这些解释文字误伤（实测踩过一次）。
        body = source.split('"""')[-1] if source.count('"""') >= 2 else source
        code_lines = [ln for ln in body.splitlines()
                      if not ln.lstrip().startswith("#")]
        code = "\n".join(code_lines)
        self.assertNotIn("chmod", code,
                         "落盘后 chmod 会重新引入默认权限窗口")
        opens = [ln for ln in code_lines if "os.open(" in ln]
        self.assertTrue(opens, "必须用 os.open 显式指定创建权限")
        for line in opens:
            self.assertIn("0o600", line,
                          f"创建调用未带 0o600：{line.strip()}")

    def test_stream_is_persisted_even_when_the_call_times_out(self):
        """超时是最需要事后判因的情形，不能反而什么都不留。"""
        import tempfile

        def boom(*a, **k):
            raise subprocess.TimeoutExpired(cmd="claude", timeout=5.0,
                                            output="partial line\n",
                                            stderr="killed")
        with tempfile.TemporaryDirectory() as tmp:
            log = pathlib.Path(tmp) / "r2.jsonl"
            res = invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5,
                        max_turns=5, settings_path="s.json", cwd="/tmp",
                        timeout_s=5.0, env={"HOME": "/h", "PATH": "/bin"},
                        runner=boom, stream_log=log)
            self.assertEqual(res.exit_code, 124)
            self.assertTrue(log.exists())
            self.assertIn("partial line", log.read_text(encoding="utf-8"))

    def test_cwd_is_passed_through_correctly(self):
        runner = FakeRunner(result=subprocess.CompletedProcess(
            args=["claude"], returncode=0, stdout=self._valid_stdout(),
            stderr=""))
        invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5, max_turns=5,
              settings_path="s.json", cwd="/some/particular/dir",
              timeout_s=5.0, env={"HOME": "/home/x", "PATH": "/bin"},
              runner=runner)
        self.assertEqual(runner.calls[0]["cwd"], "/some/particular/dir")


if __name__ == "__main__":
    unittest.main()


class TestStreamLogPermissionsOnExistingFile(unittest.TestCase):
    """0o600 必须对**既有文件**也成立（评审 cfr-p12-merged-02）。

    `os.open(..., O_CREAT|O_TRUNC, 0o600)` 的 mode 只在**首次创建**时生效；
    打开一个已存在的文件时 mode 被完全忽略，权限保持原样。

    这不是理论风险：stream 路径按 `round_id`/`role`/`attempt` **确定性**生成，
    重跑同一轮、崩溃恢复、或上一次留下的残留文件都会命中同一路径。届时新的
    敏感 stream 会写进一个全用户可读的 inode，而权限位测试（只覆盖新文件）
    仍然全绿——典型的假绿。
    """

    def test_existing_world_readable_file_is_not_written_through(self):
        import stat as _stat
        import tempfile

        stdout = _CLEAN_STDOUT if "_CLEAN_STDOUT" in globals() else None
        with tempfile.TemporaryDirectory() as tmp:
            log = pathlib.Path(tmp) / "r1.jsonl"
            log.write_text("stale", encoding="utf-8")
            log.chmod(0o644)
            self.assertEqual(_stat.S_IMODE(log.stat().st_mode), 0o644)

            runner = FakeRunner(result=subprocess.CompletedProcess(
                args=["claude"], returncode=0,
                stdout=(stdout or self._valid_stdout()), stderr=""))
            invoke(prompt="/x", tools=VALID_TOOLS, grant_usd=0.5, max_turns=5,
                  settings_path="s.json", cwd="/tmp", timeout_s=5.0,
                  env={"HOME": "/h", "PATH": "/bin"}, runner=runner,
                  stream_log=log)

            self.assertEqual(
                _stat.S_IMODE(log.stat().st_mode), 0o600,
                "既有文件的权限没有被收紧——敏感 stream 写进了全用户可读的 inode")

    def _valid_stdout(self):
        return TestInvoke()._valid_stdout()
