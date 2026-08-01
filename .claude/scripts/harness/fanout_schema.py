"""Validate raw finder and judge payloads at the controller boundary."""

from __future__ import annotations


_CANDIDATE_REQUIRED: frozenset[str] = frozenset({
    "title",
    "goal",
    "invariant",
    "primary_path",
    "oracle",
    "evidence",
    "touched_paths",
    "size",
    "priority",
    "needs_decision",
    "body_md",
    "slug",
})
_SIZES: frozenset[str] = frozenset({"S", "M", "L"})
_PRIORITIES: frozenset[str] = frozenset({"T0", "T1", "T2", "T3", "T4"})
_MAX_CANDIDATES = 3
_MAX_SHORT_TEXT = 300
_MAX_LONG_TEXT = 20000

_CANDIDATE_TEXT_LIMITS: dict[str, int] = {
    "title": _MAX_SHORT_TEXT,
    "goal": _MAX_LONG_TEXT,
    "invariant": _MAX_LONG_TEXT,
    "primary_path": _MAX_LONG_TEXT,
    "oracle": _MAX_LONG_TEXT,
    "evidence": _MAX_LONG_TEXT,
    "body_md": _MAX_LONG_TEXT,
    "slug": _MAX_SHORT_TEXT,
}

_JUDGE_SCHEMAS: dict[str, dict[str, frozenset[str]]] = {
    "harness-judge-completed": {
        "required": frozenset({"verdict", "reason", "evidence"}),
        "verdicts": frozenset({"pass", "reject"}),
    },
    "harness-judge-redline": {
        "required": frozenset({"verdict", "reason", "invariant_at_risk"}),
        "verdicts": frozenset({"pass", "reject", "needs_decision"}),
    },
    "harness-judge-oracle": {
        "required": frozenset({"verdict", "reason", "suggested_oracle"}),
        "verdicts": frozenset({"pass", "reject"}),
    },
}


def _check_enum(
    value,
    allowed: frozenset[str],
    field_label: str,
    errors: list[str],
) -> None:
    if not isinstance(value, str):
        errors.append(
            f"{field_label} must be a string, got {type(value).__name__}"
        )
        return
    if value not in allowed:
        errors.append(f"{field_label} must be one of {sorted(allowed)}, got {value!r}")


def _check_text(
    value,
    field_label: str,
    errors: list[str],
    max_length: int | None = None,
) -> None:
    if not isinstance(value, str):
        errors.append(
            f"{field_label} must be a string, got {type(value).__name__}"
        )
        return
    if max_length is not None and len(value) > max_length:
        errors.append(
            f"{field_label} exceeds maximum length {max_length}: {len(value)}"
        )


def _validate_one_candidate(c: dict, errors: list[str], idx: int) -> None:
    label = f"candidates[{idx}]"
    if not isinstance(c, dict):
        errors.append(f"{label} must be an object, got {type(c).__name__}")
        return

    fields = set(c)
    unknown = fields - _CANDIDATE_REQUIRED
    if unknown:
        errors.append(f"{label} has unknown fields: {sorted(unknown)}")

    missing = _CANDIDATE_REQUIRED - fields
    if missing:
        errors.append(f"{label} is missing required fields: {sorted(missing)}")
        return

    for field_name, max_length in _CANDIDATE_TEXT_LIMITS.items():
        _check_text(
            c[field_name], f"{label}.{field_name}", errors, max_length
        )

    touched_paths = c["touched_paths"]
    if not isinstance(touched_paths, list) or not all(
        isinstance(path, str) for path in touched_paths
    ):
        errors.append(f"{label}.touched_paths must be a list of strings")

    _check_enum(c["size"], _SIZES, f"{label}.size", errors)
    _check_enum(c["priority"], _PRIORITIES, f"{label}.priority", errors)

    if not isinstance(c["needs_decision"], bool):
        errors.append(f"{label}.needs_decision must be a boolean")


def validate_finder_output(payload: dict) -> list[str]:
    errors: list[str] = []
    if not isinstance(payload, dict):
        return [f"finder output must be an object, got {type(payload).__name__}"]

    fields = set(payload)
    if fields != {"candidates"}:
        missing = {"candidates"} - fields
        extra = fields - {"candidates"}
        if missing:
            errors.append(f"finder output is missing required fields: {sorted(missing)}")
        if extra:
            errors.append(f"finder output has unknown fields: {sorted(extra)}")
        if missing:
            return errors

    candidates = payload["candidates"]
    if not isinstance(candidates, list):
        errors.append(
            f"finder output candidates must be a list, got "
            f"{type(candidates).__name__}"
        )
        return errors
    if len(candidates) > _MAX_CANDIDATES:
        errors.append(
            f"finder output has {len(candidates)} candidates; maximum is "
            f"{_MAX_CANDIDATES}"
        )

    for idx, candidate in enumerate(candidates):
        _validate_one_candidate(candidate, errors, idx)
    return errors


def validate_judge_output(judge_type: str, payload: dict) -> list[str]:
    schema = _JUDGE_SCHEMAS[judge_type]
    errors: list[str] = []
    if not isinstance(payload, dict):
        return [f"judge output must be an object, got {type(payload).__name__}"]

    required = schema["required"]
    fields = set(payload)
    missing = required - fields
    extra = fields - required
    if missing:
        errors.append(f"judge output is missing required fields: {sorted(missing)}")
    if extra:
        errors.append(f"judge output has unknown fields: {sorted(extra)}")
    if missing:
        return errors

    _check_enum(payload["verdict"], schema["verdicts"], "verdict", errors)
    for field_name in required - {"verdict"}:
        _check_text(payload[field_name], field_name, errors)
    return errors
