import inspect
import unittest
from dataclasses import fields
from typing import get_type_hints

from harness.claude_runner import (
    DEFAULT_AGENT_MODEL,
    _extract_json_object,
    _extract_payload,
    invoke,
)
from harness.role_invocation import (
    RequestContext,
    RoleInvocationRequest,
    build_stream_log_path,
    for_judge,
    to_invoke_kwargs,
)


class TestRoleInvocationRequest(unittest.TestCase):
    def _request(self, **overrides):
        values = {
            "role": "finder:roadmap",
            "prompt": "find candidates",
            "tools": "Glob,Grep,Read",
            "grant_usd": 0.5,
            "max_turns": 10,
            "settings_path": ".claude/harness-settings.json",
            "cwd": "/home/xp/src/zipfs",
            "timeout_s": 60.0,
            "model": DEFAULT_AGENT_MODEL,
            "session_id": "11111111-1111-4111-8111-111111111111",
            "stream_log": "/home/xp/src/zipfs/.claude/state/rounds/r:f:1.jsonl",
        }
        values.update(overrides)
        return RoleInvocationRequest(**values)

    def test_missing_required_fields_raise_type_error(self):
        with self.assertRaises(TypeError):
            RoleInvocationRequest(role="finder:roadmap")

    def test_to_invoke_kwargs_matches_real_invoke_signature(self):
        request = self._request()
        kwargs = to_invoke_kwargs(request)
        invoke_parameters = set(inspect.signature(invoke).parameters)
        self.assertLessEqual(set(kwargs), invoke_parameters)
        self.assertNotIn("role", kwargs)
        expected = {field.name for field in fields(RoleInvocationRequest)} - {"role"}
        self.assertEqual(set(kwargs), expected)

    def test_default_request_uses_finder_payload_parser(self):
        self.assertIs(self._request().payload_parser, _extract_payload)

    def test_for_judge_uses_json_object_payload_parser(self):
        request = for_judge(**{
            field.name: getattr(self._request(role="judge:redline:" + "a" * 32),
                                field.name)
            for field in fields(RoleInvocationRequest)
            if field.name != "payload_parser"
        })
        self.assertIs(request.payload_parser, _extract_json_object)
        self.assertEqual(request.role, "judge:redline:" + "a" * 32)


class TestRequestContext(unittest.TestCase):
    def test_context_can_hold_production_values_without_placeholders(self):
        context = RequestContext(
            cwd="/home/xp/src/zipfs",
            settings_path=".claude/harness-settings.json",
            model=DEFAULT_AGENT_MODEL,
            stream_log_dir="/home/xp/src/zipfs/.claude/state/rounds",
        )
        self.assertNotIn(context.cwd, ("", "/tmp"))
        self.assertEqual(context.settings_path,
                         ".claude/harness-settings.json")
        self.assertIsNotNone(context.model)
        self.assertNotEqual(context.model, "")
        self.assertNotIn(context.stream_log_dir, ("", "/tmp"))

    def test_model_annotation_is_non_optional_str(self):
        self.assertIs(get_type_hints(RequestContext)["model"], str)

    def test_stream_log_path_identity_matches_ledger_attempt_key(self):
        stream_log_dir = "/state/rounds"
        round_id = "round-1"
        task_role = "judge:oracle:" + "b" * 32
        attempt = 3
        attempt_key = f"{round_id}:{task_role}:{attempt}"
        path = build_stream_log_path(stream_log_dir, round_id, task_role,
                                     attempt)
        self.assertEqual(path, f"{stream_log_dir}/{attempt_key}.jsonl")
        self.assertEqual(path.removeprefix(stream_log_dir + "/")
                         .removesuffix(".jsonl"), attempt_key)


if __name__ == "__main__":
    unittest.main()
