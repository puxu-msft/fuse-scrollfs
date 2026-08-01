"""扇出角色调用的唯一请求契约。"""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Callable

from .claude_runner import _extract_json_object, _extract_payload


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
