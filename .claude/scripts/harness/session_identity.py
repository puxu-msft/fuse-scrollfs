from __future__ import annotations

import re
import uuid

ROLES = frozenset(
    {
        "finder:roadmap",
        "finder:code",
        "finder:bench",
        "finder:hygiene",
    }
)

_JUDGE_ROLE = re.compile(r"^judge:(redline|completed|oracle):[0-9a-f]{32}$")
_HARNESS_SESSION_NAMESPACE = uuid.UUID("f1c7578d-28ee-4f61-976a-a93821e59c6f")


def derive_session_id(round_id: str, role: str, attempt: int) -> str:
    """Return the deterministic UUID v5 for a logical role attempt."""
    if role not in ROLES and _JUDGE_ROLE.fullmatch(role) is None:
        raise ValueError(f"invalid harness role: {role!r}")
    if type(attempt) is not int or attempt < 1:
        raise ValueError("attempt must be an integer starting at 1")
    return str(uuid.uuid5(_HARNESS_SESSION_NAMESPACE, f"{round_id}:{role}:{attempt}"))
