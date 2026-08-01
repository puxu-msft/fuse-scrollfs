"""扇出角色调用的唯一请求契约。"""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Callable

from .claude_runner import (DEFAULT_AGENT_MODEL, _extract_json_object,
                            _extract_payload)
from .config import SETTINGS_PATH


@dataclass(frozen=True)
class RequestContext:
    """扇出调用共享的生产环境值。"""

    cwd: str
    settings_path: str
    model: str
    stream_log_dir: str


@dataclass(frozen=True)
class RoleInvocationRequest:
    """完整映射 ``claude_runner.invoke`` 参数并附带路由角色。"""

    role: str
    prompt: str
    tools: str
    grant_usd: float
    max_turns: int
    settings_path: str
    cwd: str
    timeout_s: float
    model: str | None = None
    stream_log: object = None
    session_id: str | None = None
    resume: str | None = None
    fork_session: bool = False
    payload_parser: Callable[[str], dict | None] = _extract_payload


def to_invoke_kwargs(request: RoleInvocationRequest) -> dict:
    """展开为 ``invoke(**kwargs)`` 可接受的字典，不传递路由字段。"""
    values = asdict(request)
    values.pop("role")
    return values


def for_judge(**kwargs) -> RoleInvocationRequest:
    """构造使用宽顶层对象 parser 的 judge 请求。"""
    kwargs.setdefault("payload_parser", _extract_json_object)
    return RoleInvocationRequest(**kwargs)


def build_stream_log_path(stream_log_dir: str, round_id: str, task_role: str,
                          attempt: int) -> str:
    """按与账本 attempt_key 相同的 identity 模板构造日志路径。"""
    attempt_key = f"{round_id}:{task_role}:{attempt}"
    return f"{stream_log_dir}/{attempt_key}.jsonl"


def build_request_context(cfg) -> RequestContext:
    """扇出请求上下文的**唯一生产构造点**（评审 cfr-p12-merged-01）。

    `RequestContext` 本身只是个容器——`cwd="/tmp"`、`settings_path=""`、
    `model=None` 都能构造出来。若测试各自手写一组"看起来对"的字面量，断言的
    就是它自己刚写的东西，「生产值必须是真值」这条要求便没有可执行地基：
    Phase 5/6 重新引入占位值时，测试照样全绿。

    因此生产路径**只能**经由本函数取得上下文，测试也调用它而不是自填期望值。
    四个值各自绑到真实来源：仓库根、settings 常量、规范模型 ID、stream 目录。
    """
    stream_dir = cfg.state_db.parent / "rounds"
    return RequestContext(
        cwd=str(cfg.repo_root),
        settings_path=SETTINGS_PATH,
        model=DEFAULT_AGENT_MODEL,
        stream_log_dir=str(stream_dir),
    )
