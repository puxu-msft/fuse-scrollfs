"""测试替身：内存 GitHub，支持在任意调用上注入『已生效/未生效 + 响应丢失』。"""

from __future__ import annotations

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
        for issue in self.issues.values():
            if marker in issue["body"]:
                return issue
        return None

    def list_labels(self) -> list[str]:
        return sorted(self.labels)

    def ensure_label(self, name: str, color: str, description: str) -> None:
        self.labels.add(name)

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
