"""测试替身：内存 GitHub，支持在写方法上注入『已生效/未生效 + 响应丢失』。

故障注入覆盖范围：所有会进入 outbox（即可能被 lifecycle 重试/重放）的写方法
——`create_issue` / `replace_labels` / `create_comment` / `ensure_label`。
只读方法（`viewer_permission`、`find_issue_by_marker`、`list_labels`、
`get_issue_labels`、`find_comment_by_marker`、`list_open_issues_with_label`）
不支持故障注入：它们没有『已生效/未生效』的语义。
"""

from __future__ import annotations

import subprocess

from harness.outbox import ResponseLost


class FakeGitHub:
    def __init__(self, permission: str = "WRITE"):
        self.permission = permission
        self.issues: dict[int, dict] = {}
        self.comments: dict[int, list[dict]] = {}
        self.labels: set[str] = set()
        self._next_number = 1
        self._faults: dict[str, bool] = {}
        self.calls: list[str] = []
        # 索引延迟模拟（评审 Critical，回归测试专用）：见
        # `simulate_delayed_marker_visibility()` / `find_issue_by_marker_delayed()`。
        self._delayed_visible_after: dict[str, int] = {}

    def fail_next(self, method: str, applied: bool) -> None:
        self._faults[method] = applied

    def _maybe_fail(self, method: str, apply):
        self.calls.append(method)
        if method in self._faults:
            applied = self._faults.pop(method)
            if applied:
                apply()
            raise ResponseLost(f"injected fault on {method}")
        return apply()

    def viewer_permission(self) -> str:
        return self.permission

    def create_issue(self, title: str, body: str, labels: list[str]) -> dict:
        def apply():
            number = self._next_number
            self._next_number += 1
            issue = {"number": number, "title": title, "body": body,
                     "labels": list(labels), "state": "open"}
            self.issues[number] = issue
            self.comments[number] = []
            return issue
        return self._maybe_fail("create_issue", apply)

    def find_issue_by_marker(self, marker: str) -> dict | None:
        """强一致查询：直接扫描内存里的全部 Issue，无任何延迟——这正是
        `GhCli.find_issue_by_marker()` 修复后（改用直接列表端点而非
        Search 索引）想要保证的语义：创建后立即可见，不存在『探测阴性
        ≠ 确定未创建』这类不确定性。生产代码路径（`Publisher`/`round.py`）
        只调用这一个方法作为 natural-key 恢复探测。
        """
        for issue in self.issues.values():
            if marker in issue["body"]:
                return issue
        return None

    def simulate_delayed_marker_visibility(self, marker: str,
                                           calls_until_visible: int) -> None:
        """模拟『对象已创建，但按 marker 探测时暂不可见』（如异步 Search
        索引延迟）：接下来 `calls_until_visible` 次
        `find_issue_by_marker_delayed(marker)` 调用返回 None，即便匹配的
        Issue 已经存在；用于构造对照测试，证明『探测阴性 → 重发
        create_issue』这条路径在探测本身不可靠时确实会产生重复对象
        （评审 Critical：GitHub Search 是异步索引的，阴性结果不能证明
        对象未创建）。

        `find_issue_by_marker()`（生产代码实际调用的方法）**不**受此影响
        ——它模拟的是修复后『直接列表扫描，强一致』的语义。本方法只影响
        `find_issue_by_marker_delayed()`，后者仅供回归测试构造对照组，
        生产代码从不调用它。
        """
        self._delayed_visible_after[marker] = calls_until_visible

    def find_issue_by_marker_delayed(self, marker: str) -> dict | None:
        """仅供回归测试构造对照组：模拟旧版基于 Search 索引、可能延迟可见
        的探测语义。生产代码从不调用这个方法——真实探测入口是
        `find_issue_by_marker()`。
        """
        remaining = self._delayed_visible_after.get(marker, 0)
        if remaining > 0:
            self._delayed_visible_after[marker] = remaining - 1
            return None
        return self.find_issue_by_marker(marker)

    def list_labels(self) -> list[str]:
        return sorted(self.labels)

    def ensure_label(self, name: str, color: str, description: str) -> None:
        def apply():
            self.labels.add(name)
            return None
        return self._maybe_fail("ensure_label", apply)

    def get_issue_labels(self, number: int) -> list[str]:
        return list(self.issues[number]["labels"])

    def replace_labels(self, number: int, labels: list[str]) -> None:
        def apply():
            self.issues[number]["labels"] = list(labels)
            return None
        return self._maybe_fail("replace_labels", apply)

    def create_comment(self, number: int, body: str) -> dict:
        def apply():
            comment = {"id": len(self.comments[number]) + 1, "body": body}
            self.comments[number].append(comment)
            return comment
        return self._maybe_fail("create_comment", apply)

    def find_comment_by_marker(self, number: int, marker: str) -> dict | None:
        for c in self.comments.get(number, []):
            if marker in c["body"]:
                return c
        return None

    def list_open_issues_with_label(self, label: str) -> list[dict]:
        return [i for i in self.issues.values()
                if i["state"] == "open" and label in i["labels"]]


class FakeGhTransport:
    """可注入 `GhCli` 的假 subprocess 执行器：不 fork 真进程、不访问公网。

    按 `queue()` 的顺序逐次消费响应；每次被调用（即一次 `gh` 调用）弹出
    队首一项。用于对**真实 `GhCli`** 做契约测试——喂真实 GitHub 形状的
    JSON、多页分页 stdout、各类 stderr 文案，验证 `GhCli` 生成的 argv 与
    对响应的解析，而不必真的调用 `gh`。

    用法：
        transport = FakeGhTransport()
        transport.queue(stdout='{"...": ...}')
        transport.queue_timeout()
        gh = GhCli(cfg, runner=transport)
    """

    def __init__(self):
        self._queue: list = []
        self.calls: list[list[str]] = []

    def queue(self, stdout: str = "", stderr: str = "", returncode: int = 0) -> None:
        self._queue.append(("result", returncode, stdout, stderr))

    def queue_timeout(self) -> None:
        self._queue.append(("timeout",))

    def __call__(self, argv: list[str], capture_output: bool = True,
                 text: bool = True, env: dict | None = None,
                 timeout: float | None = None) -> subprocess.CompletedProcess:
        self.calls.append(argv)
        if not self._queue:
            raise AssertionError(
                f"FakeGhTransport 队列已空，收到多余调用: {argv!r}")
        item = self._queue.pop(0)
        if item[0] == "timeout":
            raise subprocess.TimeoutExpired(cmd=argv, timeout=timeout or 0)
        _, returncode, stdout, stderr = item
        return subprocess.CompletedProcess(
            args=argv, returncode=returncode, stdout=stdout, stderr=stderr)
