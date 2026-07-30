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
import time
from typing import Callable, Protocol, runtime_checkable

from .config import GH, Config
from .outbox import ResponseLost, TerminalOperationError

# 单次 gh 调用超时：卡住的 gh 不应耗尽整轮 deadline。
DEFAULT_TIMEOUT_S = 30.0

# 自然键恢复（评审 Critical）：GitHub Search 是异步索引的，一次或数次
# `search/issues` 无结果**不能**证明 Issue 未创建——探测阴性可能只是索引
# 尚未追上。`find_issue_by_marker()` 因此改用 `GET /repos/{slug}/issues`
# 直接分页扫描：这条路径不经过 Search 索引，是强一致的（写入后立即可见）。
#
# 扫描窗口有界（默认最近 500 个 Issue/PR，5 页 × 100/页）：自然键恢复要
# 找的必然是**本次调用刚创建**的对象，几乎总在第一页；多留几页只是为了
# 兜住『同时有其他人在建 Issue』的正常竞争。这个有界窗口不是『尽力而为
# 之后放弃』的妥协——只要窗口内没找到，语义就从『不确定』变为『在最近
# 活动范围内确定未创建』，而不再是『Search 索引没追上』那种不确定性。
_RECOVERY_LIST_PAGE_SIZE = 100
_RECOVERY_LIST_MAX_PAGES = 5

# 列表端点本身仍可能遭遇 5xx / 超时等传输层瞬时错误——那与『索引延迟』是
# 两类不同的不确定性，但同样不能被误读成『确认未创建』。用有界退避重试
# 吸收瞬时抖动；重试耗尽后必须原样抛出异常（而不是悄悄返回 None），让
# 调用方（`Outbox.execute()`）把这一轮判定为『结果未知』，绝不因为一次
# 探测失败就重新调用 `create_issue()` 开出第二个对象。
_RECOVERY_RETRY_BACKOFFS_S = (1.0, 2.0, 4.0)

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
      - 4xx 业务拒绝 → 确定性失败（`TerminalOperationError`）：重试用**同一
        payload**必然得到同一拒绝，需人工介入（改 payload 或修凭据），
        outbox 据此把 operation 标记为 `failed_terminal` 并阻断本轮。
        含 401/403：这两者可能是凭据问题而非请求本身有误，但无论哪种，
        **不改变凭据**盲目重试都不会让同一调用成功——修复凭据本身就是一次
        人工介入动作，与「确定性失败需要人工介入」的语义一致，因此仍归入
        `TerminalOperationError` 而非 `ResponseLost`（`ResponseLost` 的语义
        是『服务端可能已生效、值得 probe 补救』，401/403 从未生效，probe
        补救无意义）。
      - 5xx / 传输中断 → 结果不确定，服务端可能已生效（`ResponseLost`）
    mutation=False（只读）：
      - 4xx 业务拒绝 → 确定性失败（`RuntimeError`，维持原语义不变——只读
        路径不进 outbox，不需要 `failed_terminal` 状态机）
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
        return TerminalOperationError(msg) if mutation else RuntimeError(msg)
    lower = stderr.lower()
    if any(marker in lower for marker in _TRANSPORT_MARKERS):
        return ResponseLost(msg) if mutation else TransientReadError(msg)
    return TerminalOperationError(msg) if mutation else RuntimeError(msg)


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
                 timeout: float = DEFAULT_TIMEOUT_S,
                 sleep: Callable[[float], None] = time.sleep):
        self.cfg = cfg
        self.slug = cfg.repo_slug
        # 可注入的 subprocess 执行器：测试喂真实 GitHub 形状的响应/分页 stdout/
        # 各种 stderr，且不访问公网。默认用真的 subprocess.run。
        self._runner = runner
        self._timeout = timeout
        # 可注入的退避 sleep：测试用它跳过真实等待，验证退避次数/顺序。
        self._sleep = sleep

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
        """自然键恢复（评审 Critical）：直接分页扫描 Issue 列表，不经过
        Search 索引。

        GitHub Search 是异步索引的：`search/issues` 阴性结果**不能**证明
        Issue 未创建，只能证明『索引尚未追上』。旧实现在 `create_issue`
        响应丢失后，靠这一探测阴性重新调用 `create_issue`，会对同一提案
        开出两个 Issue——这正是整套 outbox 设计要防止的事。

        改用 `GET /repos/{slug}/issues?state=all&sort=created&direction=desc`
        按页扫描：这是对资源本身的直接查询（非搜索索引），GitHub REST 的
        主资源端点由主库直接支持，创建后立即强一致可见——不同于文档明确
        标注『索引可能滞后一分钟』的 Search API。扫描窗口有界
        （`_RECOVERY_LIST_MAX_PAGES` 页 × `_RECOVERY_LIST_PAGE_SIZE` 个/页，
        按创建时间倒序）——自然键恢复要找的必然是**最近**创建的对象，几乎
        总在第一页；窗口内未命中即确定性地判定『在最近活动范围内未创建』，
        不是『放弃』。

        列表请求本身若遇到 5xx/超时等瞬时传输错误，用有界退避重试吸收
        抖动；重试耗尽后**原样向上抛出异常**，绝不悄悄当作『未找到』——
        否则调用方会把这次不确定误读为确定阴性，进而重发 `create_issue`。

        未在本调用内的窗口中找到，**不代表永久放弃**：`Outbox.reconcile()`
        每轮都会重新调用本方法（评审点 3 的『有上限的延迟重试』因此落在
        跨轮次的既有基础设施上，而不是在这里另起一个阻塞式重试循环）——
        单次调用内部只重试传输层错误，不重试『真实查无』。
        """
        for page in range(1, _RECOVERY_LIST_MAX_PAGES + 1):
            items = self._list_issues_page_with_retry(page)
            for item in items:
                if marker in (item.get("body") or ""):
                    return self._normalize_issue(item)
            if len(items) < _RECOVERY_LIST_PAGE_SIZE:
                break  # 已到最后一页，窗口内确定未命中
        return None

    def _list_issues_page_with_retry(self, page: int) -> list[dict]:
        """单页 Issue 列表查询，遇 5xx/超时等瞬时错误时按有界退避重试。

        重试耗尽后原样抛出 `TransientReadError`——调用方（`find_issue_
        by_marker` 的上层，最终是 `Outbox.reconcile()`/`Outbox.execute()`
        的 probe 回调）必须能区分『查询失败，结果未知』与『查过了，确实
        没有』，绝不能把前者悄悄折叠成后者。
        """
        args = ["api", "-X", "GET", f"repos/{self.slug}/issues",
                "-f", "state=all", "-f", "sort=created", "-f", "direction=desc",
                "-f", f"per_page={_RECOVERY_LIST_PAGE_SIZE}", "-f", f"page={page}"]
        attempt = 0
        while True:
            try:
                data = self._run(args)
                return data or []
            except TransientReadError:
                if attempt >= len(_RECOVERY_RETRY_BACKOFFS_S):
                    raise
                self._sleep(_RECOVERY_RETRY_BACKOFFS_S[attempt])
                attempt += 1

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
