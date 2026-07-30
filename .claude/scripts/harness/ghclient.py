"""GitHub 访问层：经 gh CLI，凭据只从控制器进程环境注入。

DTO 规范化：`gh api` 对 Issue 端点返回的 `labels` 字段是
`[{"name": ..., "color": ..., ...}, ...]`（GitHub REST/Search 原始形状）。
本层统一把它压平成 `list[str]`，保留其余原始字段——下游（含崩溃矩阵）只
应看到规范化后的形状，不应各自重新解析标签对象。
"""

from __future__ import annotations

import json
import re
import subprocess
from typing import Callable, Protocol, runtime_checkable

from .config import GH, Config
from .outbox import ResponseLost

# 单次 gh 调用超时：卡住的 gh 不应耗尽整轮 deadline。
DEFAULT_TIMEOUT_S = 30.0

# 传输层中断标志：连接在请求执行期间断开，服务端是否已处理不可知。
# 覆盖：普通超时、连接重置/拒绝/中断、EOF 类、HTTP/2 流重置、broken pipe、
# 服务端主动断开、TLS 握手失败、DNS/路由不可达。
_TRANSPORT_MARKERS = (
    "timeout", "timed out", "time-out", "deadline exceeded",
    "connection reset", "connection refused", "connection aborted",
    "broken pipe", "eof", "server disconnected", "stream error",
    "stream reset", "http2: stream closed", "i/o timeout",
    "tls handshake", "no route to host", "network is unreachable",
)

_HTTP_STATUS_RE = re.compile(r"\(HTTP (\d{3})\)")


class TransientReadError(Exception):
    """只读调用的结果不确定，可安全重试（无副作用，不存在『已生效』的含义）。"""


def _classify_failure(args: list[str], stderr: str, mutation: bool) -> Exception:
    """把一次非零退出的 gh 调用分类为确定性失败还是结果不确定。

    mutation=True（写操作）：
      - 4xx 业务拒绝 → 确定性失败（RuntimeError）
      - 5xx / 传输中断 → 结果不确定，服务端可能已生效（ResponseLost）
    mutation=False（只读）：
      - 4xx 业务拒绝 → 确定性失败（RuntimeError）
      - 5xx / 传输中断 → 可重试的瞬时错误（TransientReadError），
        不使用 ResponseLost——只读没有『已生效』的含义。
    未能归类的错误一律视为确定性失败（保守默认，不无凭据地当作可能已生效）。
    """
    msg = f"gh {' '.join(args)} failed: {stderr}"
    match = _HTTP_STATUS_RE.search(stderr)
    if match:
        code = int(match.group(1))
        if code >= 500:
            return ResponseLost(msg) if mutation else TransientReadError(msg)
        return RuntimeError(msg)
    lower = stderr.lower()
    if any(marker in lower for marker in _TRANSPORT_MARKERS):
        return ResponseLost(msg) if mutation else TransientReadError(msg)
    return RuntimeError(msg)


@runtime_checkable
class GitHubClient(Protocol):
    """GhCli 与 FakeGitHub 共同遵守的契约。"""

    def viewer_permission(self) -> str: ...

    def create_issue(self, title: str, body: str, labels: list[str]) -> dict: ...

    def find_issue_by_marker(self, marker: str) -> dict | None: ...

    def list_labels(self) -> list[str]: ...

    def ensure_label(self, name: str, color: str, description: str) -> None: ...

    def get_issue_labels(self, number: int) -> list[str]: ...

    def replace_labels(self, number: int, labels: list[str]) -> None: ...

    def create_comment(self, number: int, body: str) -> dict: ...

    def find_comment_by_marker(self, number: int, marker: str) -> dict | None: ...

    def list_open_issues_with_label(self, label: str) -> list[dict]: ...


class GhCli:
    def __init__(self, cfg: Config, runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
                 timeout: float = DEFAULT_TIMEOUT_S):
        self.cfg = cfg
        self.slug = cfg.repo_slug
        # 可注入的 subprocess 执行器：测试喂真实 GitHub 形状的响应/分页 stdout/
        # 各种 stderr，且不访问公网。默认用真的 subprocess.run。
        self._runner = runner
        self._timeout = timeout

    def _run(self, args: list[str], parse: bool = True, mutation: bool = False):
        env = {"GH_TOKEN": self.cfg.gh_token, "PATH": "/usr/bin:/bin",
               "HOME": str(self.cfg.repo_root)}
        try:
            proc = self._runner([GH, *args], capture_output=True, text=True,
                                 env=env, timeout=self._timeout)
        except subprocess.TimeoutExpired as e:
            msg = f"gh {' '.join(args)} 超时（{self._timeout}s）"
            if mutation:
                raise ResponseLost(msg) from e
            raise TransientReadError(msg) from e
        if proc.returncode != 0:
            raise _classify_failure(args, proc.stderr.strip(), mutation)
        if not parse or not proc.stdout.strip():
            return None
        try:
            return json.loads(proc.stdout)
        except json.JSONDecodeError as e:
            # 退出码 0 但响应不可解析（截断等）：对写操作而言服务端可能已成功。
            msg = f"gh {' '.join(args)} 响应无法解析: {e}"
            if mutation:
                raise ResponseLost(msg) from e
            raise TransientReadError(msg) from e

    def _run_array(self, args: list[str], mutation: bool = False) -> list:
        """`--paginate` 数组端点的统一入口。

        `gh api --paginate` 对数组端点会把每一页各输出一份独立 JSON 文档，
        拼接后不是合法 JSON，`json.loads()` 在超过一页时会抛
        `JSONDecodeError`。加 `--slurp` 让 gh 自己把各页包成一个外层数组，
        这里再展平成单个列表。
        """
        pages = self._run([*args, "--slurp"], mutation=mutation)
        if not pages:
            return []
        flattened: list = []
        for page in pages:
            flattened.extend(page)
        return flattened

    @staticmethod
    def _label_names(labels: list) -> list[str]:
        return [l["name"] if isinstance(l, dict) else l for l in labels]

    @classmethod
    def _normalize_issue(cls, issue: dict) -> dict:
        normalized = dict(issue)
        normalized["labels"] = cls._label_names(issue.get("labels", []))
        return normalized

    def viewer_permission(self) -> str:
        data = self._run(["api", "graphql", "-f", (
            'query={repository(owner:"%s",name:"%s"){viewerPermission}}'
            % tuple(self.slug.split("/")))])
        return data["data"]["repository"]["viewerPermission"]

    def create_issue(self, title: str, body: str, labels: list[str]) -> dict:
        args = ["api", f"repos/{self.slug}/issues", "-X", "POST",
                "-f", f"title={title}", "-f", f"body={body}"]
        for label in labels:
            args += ["-f", "labels[]=" + label]
        return self._normalize_issue(self._run(args, mutation=True))

    def find_issue_by_marker(self, marker: str) -> dict | None:
        data = self._run(["api", "-X", "GET", "search/issues", "-f",
                          f'q=repo:{self.slug} in:body "{marker}"'])
        items = data.get("items", [])
        return self._normalize_issue(items[0]) if items else None

    def list_labels(self) -> list[str]:
        pages = self._run_array(["api", f"repos/{self.slug}/labels", "--paginate"])
        return self._label_names(pages)

    def ensure_label(self, name: str, color: str, description: str) -> None:
        if name in self.list_labels():
            return
        self._run(["api", f"repos/{self.slug}/labels", "-X", "POST",
                   "-f", f"name={name}", "-f", f"color={color}",
                   "-f", f"description={description}"], mutation=True)

    def get_issue_labels(self, number: int) -> list[str]:
        issue = self._run(["api", f"repos/{self.slug}/issues/{number}"])
        return self._label_names(issue["labels"])

    def replace_labels(self, number: int, labels: list[str]) -> None:
        args = ["api", f"repos/{self.slug}/issues/{number}/labels", "-X", "PUT"]
        for label in labels:
            args += ["-f", "labels[]=" + label]
        self._run(args, mutation=True)

    def create_comment(self, number: int, body: str) -> dict:
        return self._run(["api", f"repos/{self.slug}/issues/{number}/comments",
                          "-X", "POST", "-f", f"body={body}"], mutation=True)

    def find_comment_by_marker(self, number: int, marker: str) -> dict | None:
        comments = self._run_array(
            ["api", f"repos/{self.slug}/issues/{number}/comments", "--paginate"])
        for c in comments:
            if marker in c["body"]:
                return c
        return None

    def list_open_issues_with_label(self, label: str) -> list[dict]:
        issues = self._run_array(
            ["api", "-X", "GET", f"repos/{self.slug}/issues",
             "-f", f"labels={label}", "-f", "state=open", "--paginate"])
        return [self._normalize_issue(i) for i in issues]
