"""控制器驱动扇出（ADR-002 D1/D2）。

取代 scrollz-propose.js + Skill(scrollz-round) + TaskOutput 的三级嵌套编排；
每个 finder/judge 现在是控制器直接启动的独立顶层 ``claude -p`` 进程。
"""

from __future__ import annotations

import re

from .queue import canonical_key


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
