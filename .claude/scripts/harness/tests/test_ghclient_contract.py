"""GhCli 契约测试：注入假 subprocess 执行器，不访问公网。

覆盖评审发现 1（分页解析）、2（labels DTO 规范化）、3（ResponseLost 分类 +
超时）、4（GitHubClient Protocol 契约 + 此前零测试的四个方法）。
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

from harness.config import Config
from harness.ghclient import GhCli, GitHubClient, TransientReadError
from harness.outbox import ResponseLost
from harness.tests.fakes import FakeGhTransport, FakeGitHub


def _cfg() -> Config:
    root = Path("/tmp/ghclient-contract-test")
    return Config(
        repo_root=root,
        state_db=root / "h.db",
        publish_worktree=root / "wt",
        repo_slug="acme/widgets",
        gh_token="fake-token",
        round_budget_usd=1.5,
        daily_budget_usd=20.0,
        max_turns=60,
        proposed_cap=20,
        lane_cap=6,
    )


class TestGitHubClientProtocolConformance(unittest.TestCase):
    """发现 4：Fake 与真实 GhCli 必须共同满足同一份 Protocol 契约。"""

    def test_fake_satisfies_protocol(self):
        self.assertIsInstance(FakeGitHub(), GitHubClient)

    def test_ghcli_satisfies_protocol(self):
        self.assertIsInstance(GhCli(_cfg(), runner=FakeGhTransport()), GitHubClient)


class TestPaginationFlattening(unittest.TestCase):
    """发现 1：`--paginate` 数组端点必须能解析多页 stdout。"""

    def test_list_labels_flattens_two_pages(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(
            [[{"name": "harness", "color": "ededed"}],
             [{"name": "T1", "color": "ffffff"}]]))
        gh = GhCli(_cfg(), runner=transport)
        labels = gh.list_labels()
        self.assertEqual(labels, ["harness", "T1"])
        # 断言真实生成的 argv 带上了 --slurp（而不仅是 --paginate）
        self.assertIn("--slurp", transport.calls[0])
        self.assertIn("--paginate", transport.calls[0])

    def test_list_labels_single_page_still_works(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps([[{"name": "solo", "color": "abcabc"}]]))
        gh = GhCli(_cfg(), runner=transport)
        self.assertEqual(gh.list_labels(), ["solo"])

    def test_list_labels_empty_result(self):
        transport = FakeGhTransport()
        transport.queue(stdout="")
        gh = GhCli(_cfg(), runner=transport)
        self.assertEqual(gh.list_labels(), [])

    def test_find_comment_by_marker_across_two_pages(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(
            [[{"id": 1, "body": "unrelated"}],
             [{"id": 2, "body": "HARNESS-RECEIPT\nop=abc"}]]))
        gh = GhCli(_cfg(), runner=transport)
        found = gh.find_comment_by_marker(42, "op=abc")
        self.assertEqual(found["id"], 2)

    def test_list_open_issues_with_label_across_two_pages(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(
            [[{"number": 1, "state": "open",
               "labels": [{"name": "harness"}]}],
             [{"number": 2, "state": "open",
               "labels": [{"name": "harness"}]}]]))
        gh = GhCli(_cfg(), runner=transport)
        issues = gh.list_open_issues_with_label("harness")
        self.assertEqual([i["number"] for i in issues], [1, 2])
        # 顺带验证展平后的每条 issue 也做了 label DTO 规范化
        self.assertEqual(issues[0]["labels"], ["harness"])


class TestIssueLabelDtoNormalization(unittest.TestCase):
    """发现 2：Issue 的 labels 字段必须被规范化成 list[str]，且不丢其余字段。"""

    def test_create_issue_normalizes_github_shape_labels(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps({
            "number": 7, "title": "t", "body": "b", "state": "open",
            "labels": [{"id": 1, "name": "harness", "color": "ededed"},
                       {"id": 2, "name": "T1", "color": "ffffff"}],
            "html_url": "https://github.com/acme/widgets/issues/7",
        }))
        gh = GhCli(_cfg(), runner=transport)
        issue = gh.create_issue("t", "b", ["harness", "T1"])
        self.assertEqual(issue["labels"], ["harness", "T1"])
        # 未来可能需要的字段（如 html_url）不应被提前丢弃
        self.assertEqual(issue["html_url"],
                         "https://github.com/acme/widgets/issues/7")

    def test_get_issue_labels_normalizes_github_shape(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps({
            "number": 7, "labels": [{"name": "bug", "color": "d73a4a"}],
        }))
        gh = GhCli(_cfg(), runner=transport)
        self.assertEqual(gh.get_issue_labels(7), ["bug"])

    def test_find_issue_by_marker_normalizes_search_result_labels(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps({
            "items": [{"number": 3, "body": "x HARNESS-OP:abc",
                       "labels": [{"name": "harness"}]}],
        }))
        gh = GhCli(_cfg(), runner=transport)
        found = gh.find_issue_by_marker("HARNESS-OP:abc")
        self.assertEqual(found["labels"], ["harness"])


class TestResponseLostClassification(unittest.TestCase):
    """发现 3：表驱动覆盖确定性 4xx / 5xx / 超时 / 截断 JSON / 普通成功。"""

    def test_mutation_deterministic_4xx_raises_runtime_error(self):
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr='{"message":"Validation Failed"}'
                                              "\ngh: Validation Failed (HTTP 422)")
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(RuntimeError):
            gh.create_issue("t", "b", [])

    def test_readonly_deterministic_4xx_raises_runtime_error(self):
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr="gh: Not Found (HTTP 404)")
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(RuntimeError):
            gh.get_issue_labels(999999)

    def test_mutation_5xx_raises_response_lost(self):
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr="gh: Internal Server Error (HTTP 500)")
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(ResponseLost):
            gh.create_issue("t", "b", [])

    def test_readonly_5xx_raises_transient_read_error_not_response_lost(self):
        """只读没有『已生效』的含义，不应该抛 ResponseLost。"""
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr="gh: Bad Gateway (HTTP 502)")
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(TransientReadError):
            gh.get_issue_labels(1)

    def test_mutation_timeout_raises_response_lost(self):
        transport = FakeGhTransport()
        transport.queue_timeout()
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(ResponseLost):
            gh.create_comment(1, "body")

    def test_readonly_timeout_raises_transient_read_error(self):
        transport = FakeGhTransport()
        transport.queue_timeout()
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(TransientReadError):
            gh.get_issue_labels(1)

    def test_mutation_truncated_json_on_success_exit_raises_response_lost(self):
        """退出码 0 但响应截断：写操作视为『服务端可能已成功』。"""
        transport = FakeGhTransport()
        transport.queue(returncode=0, stdout='{"number": 7, "labels": [')
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(ResponseLost):
            gh.create_issue("t", "b", [])

    def test_readonly_truncated_json_on_success_exit_raises_transient(self):
        transport = FakeGhTransport()
        transport.queue(returncode=0, stdout='{"labels": [')
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(TransientReadError):
            gh.get_issue_labels(1)

    def test_transport_reset_marker_without_http_status_classified_as_lost(self):
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr="connection reset by peer")
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(ResponseLost):
            gh.create_comment(1, "body")

    def test_ordinary_success_returns_parsed_result(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(
            {"number": 1, "title": "t", "body": "b", "labels": [], "state": "open"}))
        gh = GhCli(_cfg(), runner=transport)
        issue = gh.create_issue("t", "b", [])
        self.assertEqual(issue["number"], 1)


class TestUncoveredMethodsArgvAndParsing(unittest.TestCase):
    """发现 4：此前零测试的四个方法——argv 生成与响应解析都要有断言。"""

    def test_viewer_permission_argv_and_parsing(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(
            {"data": {"repository": {"viewerPermission": "WRITE"}}}))
        gh = GhCli(_cfg(), runner=transport)
        self.assertEqual(gh.viewer_permission(), "WRITE")
        argv = transport.calls[0]
        self.assertIn("graphql", argv)
        self.assertTrue(any("viewerPermission" in a for a in argv))

    def test_list_labels_argv_targets_repo_labels_endpoint(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps([[]]))
        gh = GhCli(_cfg(), runner=transport)
        gh.list_labels()
        argv = transport.calls[0]
        self.assertIn("repos/acme/widgets/labels", argv)
        self.assertIn("--paginate", argv)

    def test_ensure_label_skips_post_when_label_already_exists(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps([[{"name": "harness"}]]))
        gh = GhCli(_cfg(), runner=transport)
        gh.ensure_label("harness", "ededed", "desc")
        self.assertEqual(len(transport.calls), 1)  # 只有 list_labels 一次调用

    def test_ensure_label_posts_when_missing(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps([[]]))
        transport.queue(stdout="")
        gh = GhCli(_cfg(), runner=transport)
        gh.ensure_label("harness", "ededed", "desc")
        self.assertEqual(len(transport.calls), 2)
        post_argv = transport.calls[1]
        self.assertIn("-X", post_argv)
        self.assertIn("POST", post_argv)
        self.assertIn("name=harness", post_argv)

    def test_list_open_issues_with_label_argv(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps([[]]))
        gh = GhCli(_cfg(), runner=transport)
        gh.list_open_issues_with_label("harness")
        argv = transport.calls[0]
        self.assertIn("repos/acme/widgets/issues", argv)
        self.assertIn("labels=harness", argv)
        self.assertIn("state=open", argv)
        self.assertIn("--paginate", argv)


if __name__ == "__main__":
    unittest.main()
