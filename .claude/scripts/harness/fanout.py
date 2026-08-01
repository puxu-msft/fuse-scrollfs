"""控制器驱动扇出（ADR-002 D1/D2）。

取代 scrollz-propose.js + Skill(scrollz-round) + TaskOutput 的三级嵌套编排；
每个 finder/judge 现在是控制器直接启动的独立顶层 ``claude -p`` 进程。
"""

from __future__ import annotations

import re
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field, replace

from . import ledger
from .claude_runner import InvocationResult
from .fanout_schema import validate_finder_output, validate_judge_output
from .prompts import AgentDef, build_finder_prompt, build_judge_prompt
from .queue import canonical_key, fingerprint
from .role_invocation import (
    RequestContext,
    RoleInvocationRequest,
    build_stream_log_path,
    for_judge,
)
from .session_identity import derive_session_id


_PRIORITY_ORDER = {"T0": 0, "T1": 1, "T2": 2, "T3": 3, "T4": 4}
_SIZE_ORDER = {"S": 0, "M": 1, "L": 2}
_MAX_RANKED_CANDIDATES = 3
_MAX_AGENT_ATTEMPTS = 3
_DEFAULT_SINGLE_CALL_CAP_USD = 0.3
_MIN_CALL_WINDOW_S = 5.0
_CALL_TEARDOWN_MARGIN_S = 1.0
_STAGE1_EXPECTED_TOOLS = frozenset({"Read", "Grep", "Glob"})
_FINDER_ROLES = (
    "finder:roadmap",
    "finder:code",
    "finder:bench",
    "finder:hygiene",
)
_FINDER_AGENT_NAMES = {
    "finder:roadmap": "harness-finder-roadmap",
    "finder:code": "harness-finder-code",
    "finder:bench": "harness-finder-bench",
    "finder:hygiene": "harness-finder-hygiene",
}
_FINDER_LANES = {
    "finder:roadmap": "roadmap",
    "finder:code": "defect",
    "finder:bench": "perf",
    "finder:hygiene": "hygiene",
}
_JUDGE_AGENT_NAMES = {
    "redline": "harness-judge-redline",
    "completed": "harness-judge-completed",
    "oracle": "harness-judge-oracle",
}
_JUDGE_ORDER = ("redline", "completed", "oracle")
_JUDGE_TYPES = tuple(_JUDGE_AGENT_NAMES[kind] for kind in _JUDGE_ORDER)
_DEFAULT_MAX_TURNS = 10
_DEFAULT_REQUEST_TIMEOUT_S = 60.0

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


@dataclass(frozen=True)
class WaveResult:
    final: dict[str, AttemptRecord]
    all_attempts: list[AttemptRecord]


class BudgetTracker:
    """线程安全的单轮调用预算预留器。"""

    def __init__(self, total_usd: float) -> None:
        if total_usd < 0:
            raise ValueError("total_usd must be non-negative")
        self._remaining = float(total_usd)
        self._lock = threading.Lock()

    def try_reserve(self, amount: float) -> bool:
        if amount <= 0:
            raise ValueError("reservation amount must be positive")
        with self._lock:
            if self._remaining < amount:
                return False
            self._remaining -= amount
            return True

    def settle(self, *, reserved: float, actual: float, cost_known: bool) -> None:
        if not cost_known:
            return
        with self._lock:
            self._remaining += reserved - actual

    def remaining(self) -> float:
        with self._lock:
            return self._remaining


def _unscheduled_record(
    role: str, attempt: int, reason: str
) -> AttemptRecord:
    return AttemptRecord(
        role=role,
        attempt=attempt,
        status="failed_transport",
        last_error=reason,
    )


def _record_ledger_attempt(
    conn,
    *,
    round_id: str,
    record: AttemptRecord,
) -> None:
    if conn is None:
        return
    attempt_key = f"{round_id}:{record.role}:{record.attempt}"
    try:
        ledger.record_attempt_started(
            conn,
            round_id=round_id,
            role=record.role,
            attempt=record.attempt,
            session_id=record.session_id or "",
            parent_session_id=record.parent_session_id,
        )
        ledger.record_attempt_finished(
            conn,
            attempt_key=attempt_key,
            status=record.status,
            cost_usd=record.cost_usd,
            turns=record.turns,
        )
    except Exception as exc:
        print(
            f"warning: agent attempt ledger write failed for {attempt_key}: {exc}",
            file=sys.stderr,
        )


def run_wave_scheduled(
    *,
    roles: tuple[str, ...],
    make_request,
    invoke_fn,
    validate,
    budget: BudgetTracker,
    deadline_monotonic: float,
    single_call_cap_usd: float = _DEFAULT_SINGLE_CALL_CAP_USD,
    expected_tools: frozenset[str] | None = None,
    conn=None,
    round_id: str = "",
) -> WaveResult:
    """按波并发执行角色调用，并保留每一次实际尝试。"""
    final: dict[str, AttemptRecord] = {}
    all_attempts: list[AttemptRecord] = []
    pending = list(roles)
    resumable_sessions: dict[str, str] = {}

    for attempt in range(1, _MAX_AGENT_ATTEMPTS + 1):
        if not pending:
            break

        remaining_window = deadline_monotonic - time.monotonic()
        if remaining_window < _MIN_CALL_WINDOW_S:
            for role in pending:
                final[role] = _unscheduled_record(
                    role, attempt, "deadline-exhausted"
                )
            break

        scheduled: list[tuple[str, RoleInvocationRequest]] = []
        for role in pending:
            if not budget.try_reserve(single_call_cap_usd):
                final[role] = _unscheduled_record(
                    role, attempt, "budget-exhausted"
                )
                continue

            base_request = make_request(role, attempt)
            dynamic_timeout = min(
                base_request.timeout_s,
                deadline_monotonic
                - time.monotonic()
                - _CALL_TEARDOWN_MARGIN_S,
            )
            if dynamic_timeout <= 0:
                budget.settle(
                    reserved=single_call_cap_usd,
                    actual=0.0,
                    cost_known=True,
                )
                final[role] = _unscheduled_record(
                    role, attempt, "deadline-exhausted"
                )
                continue
            request = replace(base_request, timeout_s=dynamic_timeout)
            if role in resumable_sessions:
                request = build_continuation_request(
                    request, resumable_sessions[role]
                )
            scheduled.append((role, request))

        if not scheduled:
            break

        def worker(item):
            role, request = item
            record = run_one_attempt(
                role=role,
                attempt=attempt,
                request=request,
                invoke_fn=invoke_fn,
                validate=validate,
                expected_tools=expected_tools,
            )
            return role, record

        with ThreadPoolExecutor(max_workers=len(scheduled)) as pool:
            completed = list(pool.map(worker, scheduled))

        retry_roles: list[str] = []
        for role, record in completed:
            all_attempts.append(record)
            final[role] = record
            budget.settle(
                reserved=single_call_cap_usd,
                actual=record.cost_usd,
                cost_known=record.cost_known,
            )
            _record_ledger_attempt(
                conn, round_id=round_id, record=record
            )
            if record.retryable and attempt < _MAX_AGENT_ATTEMPTS:
                retry_roles.append(role)
                if record.resumable and record.session_id:
                    resumable_sessions[role] = record.session_id
                else:
                    resumable_sessions.pop(role, None)

        pending = retry_roles

    return WaveResult(final=final, all_attempts=all_attempts)


def _agent_for(
    agents: dict[str, AgentDef] | None, agent_name: str
) -> AgentDef:
    if agents is None or agent_name not in agents:
        raise ValueError(f"missing agent definition for {agent_name}")
    return agents[agent_name]


def _base_request(
    *,
    task_role: str,
    attempt: int,
    prompt: str,
    agent: AgentDef,
    context: RequestContext,
    round_id: str,
    judge: bool = False,
) -> RoleInvocationRequest:
    kwargs = {
        "role": task_role,
        "prompt": prompt,
        "tools": ",".join(sorted(agent.tools)),
        "grant_usd": _DEFAULT_SINGLE_CALL_CAP_USD,
        "max_turns": _DEFAULT_MAX_TURNS,
        "settings_path": context.settings_path,
        "cwd": context.cwd,
        "timeout_s": _DEFAULT_REQUEST_TIMEOUT_S,
        "model": context.model,
        "stream_log": build_stream_log_path(
            context.stream_log_dir, round_id, task_role, attempt
        ),
        "session_id": derive_session_id(round_id, task_role, attempt),
    }
    return for_judge(**kwargs) if judge else RoleInvocationRequest(**kwargs)


def run_finders(
    *,
    round_id: str,
    invoke_fn,
    budget: BudgetTracker,
    deadline_monotonic: float,
    blocked_lanes: list[str],
    known_canonical_keys: set[str],
    context: RequestContext,
    agents: dict[str, AgentDef] | None = None,
    conn=None,
    all_records: list[AttemptRecord] | None = None,
) -> tuple[list[dict], list[dict]]:
    prompts_by_role = {
        role: build_finder_prompt(
            _agent_for(agents, _FINDER_AGENT_NAMES[role]),
            blocked_lanes=blocked_lanes,
            known_canonical_keys=sorted(known_canonical_keys),
        )
        for role in _FINDER_ROLES
    }

    def make_request(role: str, attempt: int) -> RoleInvocationRequest:
        agent = _agent_for(agents, _FINDER_AGENT_NAMES[role])
        return _base_request(
            task_role=role,
            attempt=attempt,
            prompt=prompts_by_role[role],
            agent=agent,
            context=context,
            round_id=round_id,
        )

    wave = run_wave_scheduled(
        roles=_FINDER_ROLES,
        make_request=make_request,
        invoke_fn=invoke_fn,
        validate=validate_finder_output,
        budget=budget,
        deadline_monotonic=deadline_monotonic,
        expected_tools=_STAGE1_EXPECTED_TOOLS,
        conn=conn,
        round_id=round_id,
    )
    if all_records is not None:
        all_records.extend(wave.all_attempts)

    candidates: list[dict] = []
    degraded: list[dict] = []
    for role in _FINDER_ROLES:
        record = wave.final[role]
        if record.status == "success" and record.payload is not None:
            candidates.extend(
                dict(candidate, lane=_FINDER_LANES[role])
                for candidate in record.payload["candidates"]
            )
        else:
            record_degraded(
                degraded,
                role=role,
                error=normalize_error(record.last_error or record.status),
                attempts=sum(r.role == role for r in wave.all_attempts),
            )
    return (
        dedupe_and_rank(
            candidates,
            known_canonical_keys=known_canonical_keys,
            blocked_lanes=blocked_lanes,
        ),
        degraded,
    )


def _judge_kind(task_role: str) -> str:
    namespace, kind, _fingerprint = task_role.split(":", 2)
    if namespace != "judge" or kind not in _JUDGE_AGENT_NAMES:
        raise ValueError(f"invalid judge task role: {task_role}")
    return kind


def _judge_verdict(
    judge_type: str,
    payload: dict,
    *,
    degraded: bool,
    skipped_judges: list[str],
) -> dict:
    if degraded:
        verdict = {
            "judge": judge_type,
            "verdict": "reject",
            "reason": "judge-unavailable",
            "degraded": True,
            "skipped_judges": skipped_judges,
        }
        exclusive_field = {
            "harness-judge-redline": "invariant_at_risk",
            "harness-judge-completed": "evidence",
            "harness-judge-oracle": "suggested_oracle",
        }[judge_type]
        verdict[exclusive_field] = None
        return verdict
    return {
        "judge": judge_type,
        **payload,
        "degraded": False,
        "skipped_judges": skipped_judges,
    }


def judge_candidate(
    *,
    round_id: str,
    candidate: dict,
    invoke_fn,
    budget: BudgetTracker,
    deadline_monotonic: float,
    inflight_paths: list[str],
    context: RequestContext,
    agents: dict[str, AgentDef] | None = None,
    conn=None,
    all_records: list[AttemptRecord] | None = None,
) -> tuple[list[dict], list[dict]]:
    fp = fingerprint(
        candidate["goal"],
        candidate["invariant"],
        candidate["primary_path"],
        candidate["oracle"],
    )
    degraded_records: list[dict] = []
    verdicts: list[dict] = []

    def run_judges(kinds: tuple[str, ...]) -> WaveResult:
        task_roles = tuple(f"judge:{kind}:{fp}" for kind in kinds)

        def make_request(task_role: str, attempt: int) -> RoleInvocationRequest:
            kind = _judge_kind(task_role)
            agent_name = _JUDGE_AGENT_NAMES[kind]
            agent = _agent_for(agents, agent_name)
            return _base_request(
                task_role=task_role,
                attempt=attempt,
                prompt=build_judge_prompt(
                    agent, candidate, inflight_paths=inflight_paths
                ),
                agent=agent,
                context=context,
                round_id=round_id,
                judge=True,
            )

        validation_context = threading.local()

        def validate(payload: dict) -> list[str]:
            task_role = validation_context.task_role
            judge_type = _JUDGE_AGENT_NAMES[_judge_kind(task_role)]
            return validate_judge_output(judge_type, payload)

        def run_role(request: RoleInvocationRequest) -> InvocationResult:
            validation_context.task_role = request.role
            return invoke_fn(request)

        return run_wave_scheduled(
            roles=task_roles,
            make_request=make_request,
            invoke_fn=run_role,
            validate=validate,
            budget=budget,
            deadline_monotonic=deadline_monotonic,
            expected_tools=_STAGE1_EXPECTED_TOOLS,
            conn=conn,
            round_id=round_id,
        )

    redline_wave = run_judges(("redline",))
    if all_records is not None:
        all_records.extend(redline_wave.all_attempts)
    redline_role = f"judge:redline:{fp}"
    redline_record = redline_wave.final[redline_role]
    redline_failed = redline_record.status != "success"
    if redline_failed:
        record_degraded(
            degraded_records,
            role=redline_role,
            error=normalize_error(redline_record.last_error or redline_record.status),
            attempts=sum(r.role == redline_role for r in redline_wave.all_attempts),
        )
    skipped = list(_JUDGE_TYPES[1:]) if redline_failed or redline_record.payload["verdict"] == "reject" else []
    verdicts.append(
        _judge_verdict(
            _JUDGE_AGENT_NAMES["redline"],
            redline_record.payload or {},
            degraded=redline_failed,
            skipped_judges=skipped,
        )
    )
    if verdicts[0]["verdict"] == "reject":
        return verdicts, degraded_records

    other_wave = run_judges(("completed", "oracle"))
    if all_records is not None:
        all_records.extend(other_wave.all_attempts)
    for kind in ("completed", "oracle"):
        task_role = f"judge:{kind}:{fp}"
        record = other_wave.final[task_role]
        failed = record.status != "success"
        if failed:
            record_degraded(
                degraded_records,
                role=task_role,
                error=normalize_error(record.last_error or record.status),
                attempts=sum(r.role == task_role for r in other_wave.all_attempts),
            )
        verdicts.append(
            _judge_verdict(
                _JUDGE_AGENT_NAMES[kind],
                record.payload or {},
                degraded=failed,
                skipped_judges=[],
            )
        )
    return verdicts, degraded_records
