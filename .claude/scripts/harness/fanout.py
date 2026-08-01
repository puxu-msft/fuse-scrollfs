"""控制器驱动扇出（ADR-002 D1/D2）。

取代 scrollz-propose.js + Skill(scrollz-round) + TaskOutput 的三级嵌套编排；
每个 finder/judge 现在是控制器直接启动的独立顶层 ``claude -p`` 进程。
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field, replace

from .claude_runner import InvocationResult
from .queue import canonical_key
from .role_invocation import RoleInvocationRequest


_PRIORITY_ORDER = {"T0": 0, "T1": 1, "T2": 2, "T3": 3, "T4": 4}
_SIZE_ORDER = {"S": 0, "M": 1, "L": 2}
_MAX_RANKED_CANDIDATES = 3

# 顺序敏感：UUID 必须先于裸 hex，否则第一段会被提前替换。
_ID_PATTERNS = (
    re.compile(
        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        re.I,
    ),
    re.compile(r"\b[0-9A-HJKMNP-TV-Z]{26}\b"),
    re.compile(r"req_\S+"),
    re.compile(r"trace[-_]?id[=: ]\S+", re.I),
    re.compile(r"\d{10,}"),
    re.compile(r"[0-9a-f]{8,}", re.I),
)


def dedupe_and_rank(
    candidates: list[dict],
    *,
    known_canonical_keys: set[str],
    blocked_lanes: list[str] | None = None,
) -> list[dict]:
    """跨 finder 与跨轮去重，过滤被阻塞 lane，并按优先级和大小排序。"""
    blocked = set(blocked_lanes or ())
    seen = set(known_canonical_keys)
    deduped: list[dict] = []
    for candidate in candidates:
        if not candidate.get("title") or not candidate.get("oracle"):
            continue
        if candidate.get("lane") in blocked:
            continue
        key = canonical_key(
            candidate.get("goal", ""),
            candidate.get("invariant", ""),
            candidate.get("primary_path", ""),
            candidate.get("oracle", ""),
        )
        if key in seen:
            continue
        seen.add(key)
        deduped.append(dict(candidate, canonical_key=key))
    deduped.sort(
        key=lambda candidate: (
            _PRIORITY_ORDER.get(candidate.get("priority"), 9),
            _SIZE_ORDER.get(candidate.get("size"), 9),
        )
    )
    return deduped[:_MAX_RANKED_CANDIDATES]


def normalize_error(err: object) -> str:
    """折叠传输错误中的动态 ID，同时保留错误首尾的语义差异。"""
    text = str(getattr(err, "message", None) or err)
    for pattern in _ID_PATTERNS:
        text = pattern.sub("<id>", text)
    text = re.sub(r"\s+", " ", text).strip()
    if len(text) > 300:
        text = text[:200] + "…" + text[-100:]
    return text


def record_degraded(
    degraded: list[dict], *, role: str, error: str, attempts: int
) -> None:
    """按 role 与规范化错误折叠降级记录。"""
    for record in degraded:
        if record["role"] == role and record["error"] == error:
            record["occurrences"] += 1
            record["attempts"] += attempts
            return
    degraded.append(
        {
            "role": role,
            "agentType": role,
            "error": error,
            "occurrences": 1,
            "attempts": attempts,
        }
    )


@dataclass(frozen=True)
class AttemptRecord:
    role: str
    attempt: int
    status: str
    session_id: str | None = None
    parent_session_id: str | None = None
    cost_usd: float = 0.0
    cost_known: bool = True
    turns: int = 0
    denials: int = 0
    protocol_errors: list = field(default_factory=list)
    payload: dict | None = None
    last_error: str | None = None
    retryable: bool = False
    resumable: bool = False
    subtype: str | None = None


def _has_protocol_error(invocation: InvocationResult, fragment: str) -> bool:
    return any(fragment in error for error in invocation.protocol_errors)


def _classify_retryable(
    invocation: InvocationResult,
    status: str,
    validation_errors: list[str],
) -> bool:
    """按 Phase 5 v5 终态分类表的优先级判定是否值得重试。"""
    if status == "success":
        return False
    if status == "capability_drift":
        return False

    deterministic_protocol_errors = (
        "duplicate init events",
        "duplicate terminal result events",
    )
    if any(
        _has_protocol_error(invocation, fragment)
        for fragment in deterministic_protocol_errors
    ):
        return False
    if _has_protocol_error(invocation, "missing init event"):
        return False
    if invocation.subtype == "error_max_budget_usd":
        return False

    parser_payload_failure = (
        not invocation.ok
        and invocation.subtype == "success"
        and _has_protocol_error(
            invocation, "unparseable or malformed payload"
        )
    )
    if parser_payload_failure:
        return True
    if validation_errors:
        return True

    # 未命中更具体类别的失败是传输抖动、进程中断或其它执行错误。
    return True


def _check_capability_drift(
    invocation: InvocationResult, expected_tools: frozenset[str]
) -> list[str]:
    problems: list[str] = []
    actual_tools = frozenset(invocation.init_tools)
    if actual_tools != expected_tools:
        problems.append(
            f"工具集不等：多={sorted(actual_tools - expected_tools)} "
            f"少={sorted(expected_tools - actual_tools)}"
        )
    if invocation.init_mcp_servers:
        problems.append(f"MCP 未清空：{invocation.init_mcp_servers}")
    if invocation.init_plugins:
        problems.append(f"插件未清空：{invocation.init_plugins}")
    if invocation.init_errors:
        problems.append(f"加载报错：{invocation.init_errors}")
    return problems


def _failed_attempt_error(
    invocation: InvocationResult, validation_errors: list[str]
) -> str:
    details = list(validation_errors) + list(invocation.protocol_errors)
    if invocation.raw_tail:
        details.append(invocation.raw_tail)
    if not details:
        details.append(
            f"invocation failed with exit_code={invocation.exit_code} "
            f"subtype={invocation.subtype!r}"
        )
    return "; ".join(details)


def run_one_attempt(
    *,
    role: str,
    attempt: int,
    request: RoleInvocationRequest,
    invoke_fn,
    validate,
    expected_tools: frozenset[str] | None = None,
) -> AttemptRecord:
    """发起一次角色调用并将所有预期失败规范化成纯数据。"""
    invocation = invoke_fn(request)
    capability_drift: list[str] = []
    if expected_tools and invocation.init_seen:
        capability_drift = _check_capability_drift(invocation, expected_tools)

    validation_errors: list[str] = []
    if invocation.ok and not capability_drift:
        validation_errors = validate(invocation.payload)

    if capability_drift:
        status = "capability_drift"
        last_error = "; ".join(capability_drift)
    elif invocation.ok and not validation_errors:
        status = "success"
        last_error = None
    else:
        status = "failed_transport"
        last_error = _failed_attempt_error(invocation, validation_errors)

    reported_session_id = invocation.session_id
    session_id = reported_session_id or request.session_id or request.resume
    return AttemptRecord(
        role=role,
        attempt=attempt,
        status=status,
        session_id=session_id,
        parent_session_id=request.resume,
        cost_usd=invocation.cost_usd,
        cost_known=invocation.cost_known,
        turns=invocation.turns,
        denials=invocation.denials,
        protocol_errors=list(invocation.protocol_errors),
        payload=invocation.payload if status == "success" else None,
        last_error=last_error,
        retryable=_classify_retryable(
            invocation, status, validation_errors
        ),
        resumable=bool(reported_session_id),
        subtype=invocation.subtype,
    )


def build_continuation_request(
    previous: RoleInvocationRequest, resume_session_id: str
) -> RoleInvocationRequest:
    """从当前 attempt 的骨架请求构造一次 fork 续接请求。"""
    return replace(
        previous,
        prompt=(
            "Continue the previous task from the existing session context. "
            "Return only the required JSON payload."
        ),
        session_id=None,
        resume=resume_session_id,
        fork_session=True,
    )
