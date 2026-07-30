"""一轮编排（spec §七 Phase A/B/C）。Stage 1 只走「只扫描」模式。"""

from __future__ import annotations

import datetime as dt
import time
import uuid
from dataclasses import dataclass, field
from typing import Callable

from .budget import Budget, BudgetError
from .claude_runner import InvocationResult
from .precheck import PrecheckFailed, assert_all_ok, run_prechecks
from .publish import Publisher
from .queue import Queue, fingerprint

STAGE1_TOOLS = "Read,Grep,Glob,Skill,Workflow"
SETTINGS_PATH = ".claude/harness-settings.json"
ROUND_DEADLINE_S = 20 * 60


@dataclass
class Deps:
    conn: object
    gh: object
    worktree: object
    outbox: object
    queue: Queue
    invoke: Callable[..., InvocationResult]
    tools: tuple = field(default_factory=tuple)


def _today() -> str:
    return dt.date.today().isoformat()


def run_round(cfg, deps: Deps) -> dict:
    round_id = uuid.uuid4().hex[:12]
    started = time.monotonic()

    # probes 必须真的传进去：reconcile({}) 什么都对不了账（评审 C-02）
    probes = {
        "publish_proposal": lambda op: deps.gh.find_issue_by_marker(
            "HARNESS-OP:" + op.operation_id),
        "publication_receipt": lambda op: deps.gh.find_comment_by_marker(
            op.payload["issue"], "HARNESS-OP:" + op.natural_key),
        "commit_proposal": lambda op: (
            {"sha": deps.outbox.commit_sha(op)}
            if deps.outbox.commit_sha(op) else None),
        "push_main": lambda op: (
            {"pushed": True} if deps.worktree.remote_has_operation(
                op.natural_key, op.payload["path"]) else None),
    }
    results = run_prechecks(cfg, deps.gh, deps.worktree, deps.outbox,
                            tools=deps.tools or (), probes=probes)
    try:
        assert_all_ok(results)
    except PrecheckFailed as exc:
        return {"round_id": round_id, "mode": "scan", "result": "precheck-failed",
                "detail": str(exc)}

    budget = Budget(deps.conn, cfg.round_budget_usd, cfg.daily_budget_usd)
    day = _today()
    try:
        grant = budget.reserve(round_id, day)
    except BudgetError as exc:
        return {"round_id": round_id, "mode": "scan", "result": "budget-exhausted",
                "detail": str(exc)}

    # 恢复优先于新扫描：还有未结的 operation 时不得再起模型（评审 C-02）。
    # 否则「建 Issue 响应丢失 + 搜索索引暂不可见」会让下一轮另开一个候选，
    # 旧 operation 永久悬置，同一提案出现两个 Issue。
    # 按 root 聚合：同时有多个未结子 operation 时也只恢复一次，
    # 且不会把子 operation 的 payload 误当作 candidate（评审 C-02）
    open_roots = deps.outbox.open_roots()
    if open_roots:
        publisher = Publisher(deps.outbox, deps.gh, deps.worktree,
                              deps.queue, round_id)
        resumed = publisher.resume(open_roots[0].operation_id)
        budget.settle(round_id, day, 0.0)
        return {"round_id": round_id, "mode": "resume", "result": "resumed",
                "issue": resumed["issue"], "state": resumed["state"],
                "remaining_roots": len(open_roots) - 1}

    blocked_lanes = [lane for lane in ("roadmap", "defect", "perf", "hygiene")
                     if deps.queue.lane_full(lane, cfg.lane_cap)]
    if deps.queue.total_full(cfg.proposed_cap):
        blocked_lanes = ["roadmap", "defect", "perf", "hygiene"]

    remaining = max(ROUND_DEADLINE_S - (time.monotonic() - started), 60.0)
    prompt = ("/scrollz-round\n"
              f'{{"blocked_lanes": {blocked_lanes!r}, "known_fingerprints": [],'
              f' "inflight_paths": []}}').replace("'", '"')

    invocation = deps.invoke(prompt=prompt, tools=STAGE1_TOOLS, grant_usd=grant,
                             max_turns=cfg.max_turns, settings_path=SETTINGS_PATH,
                             cwd=str(cfg.repo_root), timeout_s=remaining)

    if not invocation.ok or invocation.payload is None:
        budget.abandon(round_id, day)
        return {"round_id": round_id, "mode": "scan",
                "result": "invocation-failed", "detail": invocation.raw_tail}

    candidates = invocation.payload.get("candidates", [])
    eligible = [c for c in candidates if c.get("lane") not in blocked_lanes]
    if not eligible:
        budget.settle(round_id, day, invocation.cost_usd)
        return {"round_id": round_id, "mode": "scan", "result": "no-candidate"}

    candidate = dict(eligible[0])
    candidate["fingerprint"] = fingerprint(
        candidate.get("goal", ""), candidate.get("invariant", ""),
        candidate.get("primary_path", ""), candidate.get("oracle", ""))
    if deps.queue.classify(candidate) != "new":
        budget.settle(round_id, day, invocation.cost_usd)
        return {"round_id": round_id, "mode": "scan", "result": "duplicate"}

    state_label = ("harness:needs-decision" if candidate.get("needs_decision")
                   else "harness:proposed")
    candidate["labels"] = [
        l for l in candidate.get("labels", [])
        if not l.startswith("harness:")] + [state_label]
    if "harness" not in candidate["labels"]:
        candidate["labels"].append("harness")

    publisher = Publisher(deps.outbox, deps.gh, deps.worktree, deps.queue, round_id)
    published = publisher.publish(candidate)
    budget.settle(round_id, day, invocation.cost_usd)
    return {"round_id": round_id, "mode": "scan", "result": "published",
            "issue": published["issue"], "state": published["state"]}
