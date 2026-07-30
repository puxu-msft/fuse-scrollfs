import json, unittest
from harness.claude_runner import build_argv, parse_stream_json


class TestArgv(unittest.TestCase):
    def test_argv_pins_the_exact_isolation_combination(self):
        argv = build_argv(prompt="/scrollz-round", tools="Read,Grep,Glob,Skill",
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

    def test_tools_never_include_write_capabilities_in_stage1(self):
        argv = build_argv(prompt="/scrollz-round", tools="Read,Grep,Glob,Skill",
                          grant_usd=0.5, max_turns=10,
                          settings_path=".claude/harness-settings.json")
        idx = argv.index("--tools")
        self.assertNotIn("Bash", argv[idx + 1])
        self.assertNotIn("Edit", argv[idx + 1])
        self.assertNotIn("Write", argv[idx + 1])


class TestParse(unittest.TestCase):
    def test_extracts_payload_cost_and_turns(self):
        lines = [
            json.dumps({"type": "system", "subtype": "init",
                        "tools": ["Read", "Grep"], "mcp_servers": []}),
            json.dumps({"type": "assistant", "message": {"content": []}}),
            json.dumps({"type": "result", "subtype": "success",
                        "total_cost_usd": 0.42, "num_turns": 12,
                        "result": '```json\n{"candidates": [{"title": "x"}]}\n```'}),
        ]
        res = parse_stream_json(lines)
        self.assertTrue(res.ok)
        self.assertAlmostEqual(res.cost_usd, 0.42)
        self.assertEqual(res.turns, 12)
        self.assertEqual(res.payload["candidates"][0]["title"], "x")

    def test_error_result_is_not_ok_but_still_reports_cost(self):
        lines = [json.dumps({"type": "result", "subtype": "error_max_turns",
                             "total_cost_usd": 0.9, "num_turns": 40,
                             "result": ""})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertAlmostEqual(res.cost_usd, 0.9)

    def test_init_event_exposes_tools_and_mcp_for_negative_verification(self):
        """Round 0 的负向验证依赖这个：必须能看到实际生效的工具集与 MCP 列表。"""
        lines = [json.dumps({"type": "system", "subtype": "init",
                             "tools": ["Read"], "mcp_servers": []})]
        res = parse_stream_json(lines)
        self.assertEqual(res.init_tools, ["Read"])
        self.assertEqual(res.init_mcp_servers, [])

    def test_fence_inside_json_string_does_not_truncate_payload(self):
        """body_md 是 Markdown，内部可能含代码 fence——不得据此截断 payload。"""
        lines = [json.dumps({"type": "result", "subtype": "success",
                             "total_cost_usd": 0.1, "num_turns": 2,
                             "result": '```json\n{"candidates":[{"title":"x",'
                                       '"body_md":"example } ``` remainder"}]}\n```'})]
        res = parse_stream_json(lines)
        self.assertTrue(res.ok)
        self.assertEqual(res.payload["candidates"][0]["title"], "x")
        self.assertIn("```", res.payload["candidates"][0]["body_md"])

    def test_missing_init_event_is_flagged(self):
        """缺 init 事件时不得当作『干净』——absence-as-success 是假绿。"""
        lines = [json.dumps({"type": "result", "subtype": "success",
                             "total_cost_usd": 0.1, "num_turns": 1,
                             "result": '{"candidates": []}'})]
        res = parse_stream_json(lines)
        self.assertFalse(res.init_seen)

    def test_unparseable_payload_is_not_ok(self):
        lines = [json.dumps({"type": "result", "subtype": "success",
                             "total_cost_usd": 0.1, "num_turns": 2,
                             "result": "我觉得可以做 X"})]
        res = parse_stream_json(lines)
        self.assertFalse(res.ok)
        self.assertIsNone(res.payload)


if __name__ == "__main__":
    unittest.main()
