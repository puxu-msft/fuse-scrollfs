"""控制器驱动扇出（ADR-002 D1/D2）。

取代 scrollz-propose.js + Skill(scrollz-round) + TaskOutput 的三级嵌套编排；
每个 finder/judge 现在是控制器直接启动的独立顶层 ``claude -p`` 进程。
"""

from __future__ import annotations

from .queue import canonical_key


_PRIORITY_ORDER = {"T0": 0, "T1": 1, "T2": 2, "T3": 3, "T4": 4}
_SIZE_ORDER = {"S": 0, "M": 1, "L": 2}
_MAX_RANKED_CANDIDATES = 3


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
