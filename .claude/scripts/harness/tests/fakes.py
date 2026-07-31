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
        # 生产路径读延迟模拟（评审 Critical A）：见
        # `simulate_delayed_marker_visibility()`——现在直接作用于生产代码
        # 实际调用的 `find_issue_by_marker()` 本身。
        self._delayed_visible_after: dict[str, int] = {}
        self.read_calls: list[tuple] = []

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
        """natural-key 恢复探测——生产代码路径（`Publisher`/`round.py`）唯一
        调用的探测方法。默认强一致（无延迟），但支持通过
        `simulate_delayed_marker_visibility()` 在**这同一个生产路径方法**上
        注入暂不可见的阴性读取（评审 Critical A：故障注入必须覆盖生产路径
        本身，不能只有一个专门的、生产代码从不调用的替身方法才会延迟——
        否则测不出『生产路径读延迟导致重复创建』这个真实失效模式）。
        """
        remaining = self._delayed_visible_after.get(marker, 0)
        if remaining > 0:
            self._delayed_visible_after[marker] = remaining - 1
            self.read_calls.append(("find_issue_by_marker", marker))
            return None
        self.read_calls.append(("find_issue_by_marker", marker))
        for issue in self.issues.values():
            if marker in issue["body"]:
                return issue
        return None

    def simulate_delayed_marker_visibility(self, marker: str,
                                           calls_until_visible: int) -> None:
        """模拟『对象已创建，但按 marker 探测时暂不可见』（如异步索引延迟、
        或直接列表端点的最终一致性窗口）：接下来 `calls_until_visible` 次
        `find_issue_by_marker(marker)` 调用返回 None，即便匹配的 Issue 已经
        存在。作用于**生产代码实际调用的方法本身**（评审 Critical A），
        不再只有一个专门的、生产代码从不调用的『delayed』替身方法才会
        延迟——那样测不出生产路径自己读延迟时的真实行为。
        """
        self._delayed_visible_after[marker] = calls_until_visible

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
