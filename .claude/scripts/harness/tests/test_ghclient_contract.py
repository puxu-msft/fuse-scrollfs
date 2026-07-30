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
from harness.outbox import ResponseLost, TerminalOperationError
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

    def test_find_issue_by_marker_normalizes_list_result_labels(self):
        """评审 Critical 之后：`find_issue_by_marker` 改走直接列表端点
        （`repos/{slug}/issues`，返回裸数组），不再是 Search 的
        `{"items": [...]}` 包裹形状——此处响应形状同步更新。"""
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(
            [{"number": 3, "body": "x HARNESS-OP:abc",
              "labels": [{"name": "harness"}]}]))
        gh = GhCli(_cfg(), runner=transport)
        found = gh.find_issue_by_marker("HARNESS-OP:abc")
        self.assertEqual(found["labels"], ["harness"])


class TestResponseLostClassification(unittest.TestCase):
    """发现 3：表驱动覆盖确定性 4xx / 5xx / 超时 / 截断 JSON / 普通成功。"""

    def test_mutation_deterministic_4xx_raises_terminal_operation_error(self):
        """评审 Important-1：确定性 422 业务拒绝必须是 TerminalOperationError
        （outbox 据此标记 failed_terminal），而不是普通 RuntimeError（那会
        被 Outbox.execute() 的兜底放过，永远卡在 prepared）。"""
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr='{"message":"Validation Failed"}'
                                              "\ngh: Validation Failed (HTTP 422)")
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(TerminalOperationError):
            gh.create_issue("t", "b", [])

    def test_mutation_401_raises_terminal_operation_error(self):
        """401/403 可能是凭据问题而非请求本身有误，但不改凭据盲目重试
        同一调用不会成功——修凭据本身就是一次人工介入，语义上仍是
        TerminalOperationError（而非『可能已生效、值得 probe』的
        ResponseLost）。"""
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr="gh: Bad credentials (HTTP 401)")
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(TerminalOperationError):
            gh.create_comment(1, "body")

    def test_mutation_403_raises_terminal_operation_error(self):
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr="gh: Forbidden (HTTP 403)")
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(TerminalOperationError):
            gh.replace_labels(1, ["x"])

    def test_mutation_unclassified_failure_raises_terminal_operation_error(self):
        """无 HTTP 状态码、无传输中断标志的未知失败：保守默认为确定性失败，
        不无凭据地当作可能已生效（沿用原语义，只是异常类型从 RuntimeError
        改为 TerminalOperationError）。"""
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr="gh: something inexplicable happened")
        gh = GhCli(_cfg(), runner=transport)
        with self.assertRaises(TerminalOperationError):
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


class TestFindIssueByMarkerStronglyConsistentScan(unittest.TestCase):
    """评审 Critical：`find_issue_by_marker` 不得依赖异步索引的 Search API。

    覆盖：改用直接分页扫描（强一致资源端点，非 Search）、扫描窗口有界、
    列表请求瞬时错误的有界退避重试、以及重试耗尽必须原样抛出异常而不是
    悄悄返回 None（避免调用方把『查询失败』误读为『确定未创建』）。
    """

    def test_uses_direct_issue_list_endpoint_not_search(self):
        """必须走 `repos/{slug}/issues`，绝不能是 `search/issues`——
        后者才是本次评审要防的『异步索引，阴性不可信』的来源。"""
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(
            [{"number": 3, "body": "x HARNESS-OP:abc", "labels": []}]))
        gh = GhCli(_cfg(), runner=transport)
        found = gh.find_issue_by_marker("HARNESS-OP:abc")
        self.assertEqual(found["number"], 3)
        argv = transport.calls[0]
        self.assertIn("repos/acme/widgets/issues", argv)
        self.assertNotIn("search/issues", argv)

    def test_finds_marker_on_first_page(self):
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(
            [{"number": 5, "body": "unrelated", "labels": []},
             {"number": 6, "body": "y HARNESS-OP:xyz", "labels": []}]))
        gh = GhCli(_cfg(), runner=transport)
        found = gh.find_issue_by_marker("HARNESS-OP:xyz")
        self.assertEqual(found["number"], 6)
        self.assertEqual(len(transport.calls), 1, "命中第一页不应再翻页")

    def test_scans_multiple_pages_within_bounded_window(self):
        """第一页恰好满员（100 条）但未命中，必须继续翻到第二页。"""
        from harness.ghclient import _RECOVERY_LIST_PAGE_SIZE
        full_page = [{"number": i, "body": "noise", "labels": []}
                     for i in range(_RECOVERY_LIST_PAGE_SIZE)]
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(full_page))
        transport.queue(stdout=json.dumps(
            [{"number": 999, "body": "z HARNESS-OP:deep", "labels": []}]))
        gh = GhCli(_cfg(), runner=transport)
        found = gh.find_issue_by_marker("HARNESS-OP:deep")
        self.assertEqual(found["number"], 999)
        self.assertEqual(len(transport.calls), 2)
        self.assertIn("page=1", transport.calls[0])
        self.assertIn("page=2", transport.calls[1])

    def test_not_found_within_window_returns_none_without_scanning_forever(self):
        """未满页即代表已到最后一页：不再继续翻页，也不视为『不确定』。"""
        transport = FakeGhTransport()
        transport.queue(stdout=json.dumps(
            [{"number": 1, "body": "noise", "labels": []}]))
        gh = GhCli(_cfg(), runner=transport)
        self.assertIsNone(gh.find_issue_by_marker("HARNESS-OP:nope"))
        self.assertEqual(len(transport.calls), 1)

    def test_transient_error_retries_with_bounded_backoff_then_succeeds(self):
        """列表请求先遇到 502，退避后重试成功——必须真的调用了 sleep，
        且用的是有界退避表，而不是无限重试或立即失败。"""
        transport = FakeGhTransport()
        transport.queue(returncode=1, stderr="gh: Bad Gateway (HTTP 502)")
        transport.queue(stdout=json.dumps(
            [{"number": 1, "body": "x HARNESS-OP:retry-ok", "labels": []}]))
        sleeps: list[float] = []
        gh = GhCli(_cfg(), runner=transport, sleep=sleeps.append)
        found = gh.find_issue_by_marker("HARNESS-OP:retry-ok")
        self.assertEqual(found["number"], 1)
        self.assertEqual(len(transport.calls), 2, "必须真的重试了一次")
        self.assertEqual(len(sleeps), 1, "重试前必须退避一次")
        self.assertGreater(sleeps[0], 0)

    def test_retry_exhausted_raises_instead_of_silently_returning_none(self):
        """核心断言（评审 Critical）：重试耗尽后必须原样抛出异常，绝不能
        把『结果未知』悄悄折叠成 None——那会让调用方误判为『确定未创建』
        进而重发 create_issue，制造第二个 Issue。"""
        from harness.ghclient import (TransientReadError,
                                      _RECOVERY_RETRY_BACKOFFS_S)
        transport = FakeGhTransport()
        for _ in range(len(_RECOVERY_RETRY_BACKOFFS_S) + 1):
            transport.queue(returncode=1, stderr="gh: Bad Gateway (HTTP 502)")
        sleeps: list[float] = []
        gh = GhCli(_cfg(), runner=transport, sleep=sleeps.append)
        with self.assertRaises(TransientReadError):
            gh.find_issue_by_marker("HARNESS-OP:unknown-outcome")
        self.assertEqual(len(sleeps), len(_RECOVERY_RETRY_BACKOFFS_S),
                         "重试次数必须等于有界退避表长度，不多不少")

    def test_timeout_during_recovery_scan_raises_not_none(self):
        """超时同样是『结果不确定』，重试耗尽后仍必须抛出而不是返回 None。"""
        from harness.ghclient import (TransientReadError,
                                      _RECOVERY_RETRY_BACKOFFS_S)
        transport = FakeGhTransport()
        for _ in range(len(_RECOVERY_RETRY_BACKOFFS_S) + 1):
            transport.queue_timeout()
        gh = GhCli(_cfg(), runner=transport, sleep=lambda s: None)
        with self.assertRaises(TransientReadError):
            gh.find_issue_by_marker("HARNESS-OP:timeout-case")


if __name__ == "__main__":
    unittest.main()
