"""一轮编排（spec §七 Phase A/B/C）。Stage 1 只走「只扫描」模式。"""

from __future__ import annotations

import datetime as dt
import re
import time
import uuid
from dataclasses import dataclass, field
from typing import Callable

from . import fanout, prompts
from .budget import Budget, BudgetError
from .claude_runner import InvocationResult, STAGE1_ALLOWED_TOOLS
from .precheck import PrecheckFailed, assert_all_ok, run_prechecks
from .publish import Publisher
from .queue import Queue, canonical_key, fingerprint
from .role_invocation import (RoleInvocationRequest, build_request_context)

# 单一真相源：允许的工具集只在 claude_runner 里定义一次。这里曾经是第二份硬编码
# 字符串，加 TaskOutput 时两边立刻漂移——被 build_argv 的入口强制拦下（真机实测
# 2026-07-31）。拦住是对的，但正确的形态是让漂移**无法发生**。
STAGE1_TOOLS = ",".join(sorted(STAGE1_ALLOWED_TOOLS))
# 真相源在 config；此处再导出，保持 `from .round import SETTINGS_PATH` 的既有调用点不变。
from .config import SETTINGS_PATH  # noqa: F401
ROUND_DEADLINE_S = 20 * 60
# 结算窗口：为 checkpoint/记账预留的时间，绝不能被 `max(x, 60)` 之类的下限
# 放大——`max(deadline - elapsed, 60)` 在剩余为负时会返回 60，使子进程超时
# 反而超过整轮剩余时间（评审 Important #7）。剩余不足以覆盖这段窗口时，
# 直接不启动模型，走结构化 deadline-exhausted 结果。
CLEANUP_RESERVE_S = 60.0

# candidate DTO 白名单（评审 Critical #2）：在任何 outbox intent 或外部副作用
# 之前，先对模型返回的候选做完整校验。字段集合按 `scrollz-propose.js`
# `pickCandidateFields()` 的真实输出形状确定——`evidence`/`touched_paths`/
# `canonical_key`/`verdicts` 是 Workflow 自身附加的合法字段，必须放行；
# `labels` 不由 Workflow 产出，但即便模型或未来改动夹带了它，也只被**忽略**
# （控制器确定性派生自己的 labels，见 `_derive_labels`），而不必因为多了这个
# 已知但会被丢弃的字段就整体拒绝候选。除以上字段外，任何其他未声明字段一律
# 判定为非法 DTO。
_LANES = frozenset({"roadmap", "defect", "perf", "hygiene"})
_PRIORITIES = frozenset({"T0", "T1", "T2", "T3", "T4"})
_SIZES = frozenset({"S", "M", "L"})
_SLUG_RE = re.compile(r"^[a-z0-9-]+$")
_REQUIRED_CANDIDATE_FIELDS = frozenset({
    "title", "goal", "invariant", "primary_path", "oracle", "slug", "lane",
    "size", "priority", "needs_decision", "body_md",
})
_OPTIONAL_CANDIDATE_FIELDS = frozenset({
    "evidence", "touched_paths", "canonical_key", "verdicts", "labels",
})
_ALLOWED_CANDIDATE_FIELDS = _REQUIRED_CANDIDATE_FIELDS | _OPTIONAL_CANDIDATE_FIELDS
_MAX_SHORT_TEXT = 300
_MAX_LONG_TEXT = 20000
_MAX_SLUG_LEN = 80
_MAX_TOUCHED_PATHS = 50
_MAX_PATH_LEN = 500


@dataclass
class Deps:
    conn: object
    gh: object
    worktree: object
    outbox: object
    queue: Queue
    invoke: Callable[[RoleInvocationRequest], InvocationResult]
    tools: tuple = field(default_factory=tuple)


def _today() -> str:
    return dt.date.today().isoformat()


def _is_bool(value) -> bool:
    return isinstance(value, bool)


def _check_text(candidate: dict, field_name: str, max_len: int,
                errors: list[str]) -> None:
    value = candidate.get(field_name)
    if not isinstance(value, str) or not value.strip():
        errors.append(f"{field_name} 必须是非空字符串")
        return
    if len(value) > max_len:
        errors.append(f"{field_name} 超过长度上限 {max_len}")


def _candidates_shape_error(candidates) -> str | None:
    """信任边界处的最外层结构校验（评审 Critical A）：在任何 `.get("lane")`
    之类的解引用之前，先确认 `candidates` 本身是列表、且每个元素都是
    对象。顶层不是列表（例如模型返回 `candidates` 为字符串列表、字典、
    `None`）或列表内混入非对象元素（例如 `None`、字符串）都必须在这里
    就地判定为非法，而不是让 `c.get("lane")` 直接 `AttributeError` 崩溃
    ——那样会让 round 永久悬挂在 `mode=pending, result=None`。
    """
    if not isinstance(candidates, list):
        return (f"candidates 顶层形状非法，必须是列表，实际类型 "
                f"{type(candidates).__name__}")
    for i, c in enumerate(candidates):
        if not isinstance(c, dict):
            return f"candidates[{i}] 不是对象，实际类型 {type(c).__name__}"
    return None


def validate_candidate(candidate: dict) -> list[str]:
    """在任何 outbox intent 或外部副作用之前，对模型返回的 candidate 做完整
    校验（评审 Critical #2）。返回错误列表；非空即拒绝，不得先创建 Issue
    再靠下游正则间接拒绝——那样一个非法 slug 会先产生一个公开 Issue，
    之后每次 resume 都重遇同一非法 candidate。
    """
    errors: list[str] = []
    if not isinstance(candidate, dict):
        return ["candidate 必须是对象"]

    unknown = set(candidate) - _ALLOWED_CANDIDATE_FIELDS
    if unknown:
        errors.append(f"未知字段：{sorted(unknown)}")

    missing = _REQUIRED_CANDIDATE_FIELDS - set(candidate)
    if missing:
        errors.append(f"缺少必需字段：{sorted(missing)}")
        # 必需字段缺失时后续按字段校验大多会重复报错或直接 KeyError，
        # 提前返回即可，不影响「非空即拒绝」的整体判定。
        return errors

    _check_text(candidate, "title", _MAX_SHORT_TEXT, errors)
    _check_text(candidate, "goal", _MAX_LONG_TEXT, errors)
    _check_text(candidate, "invariant", _MAX_LONG_TEXT, errors)
    _check_text(candidate, "primary_path", _MAX_LONG_TEXT, errors)
    _check_text(candidate, "oracle", _MAX_LONG_TEXT, errors)
    _check_text(candidate, "body_md", _MAX_LONG_TEXT, errors)

    lane = candidate.get("lane")
    if lane not in _LANES:
        errors.append(f"lane 不在枚举内：{lane!r}")

    priority = candidate.get("priority")
    if priority not in _PRIORITIES:
        errors.append(f"priority 不在枚举内：{priority!r}")

    size = candidate.get("size")
    if size not in _SIZES:
        errors.append(f"size 不在枚举内：{size!r}")

    slug = candidate.get("slug")
    if not isinstance(slug, str) or not slug or len(slug) > _MAX_SLUG_LEN \
            or not _SLUG_RE.match(slug):
        errors.append(f"slug 非法：{slug!r}（须匹配 ^[a-z0-9-]+$ 且非空）")

    if not _is_bool(candidate.get("needs_decision")):
        errors.append(
            f"needs_decision 必须是布尔值，实际 {candidate.get('needs_decision')!r}")

    touched_paths = candidate.get("touched_paths")
    if touched_paths is not None:
        if not isinstance(touched_paths, list) \
                or len(touched_paths) > _MAX_TOUCHED_PATHS \
                or not all(isinstance(p, str) and len(p) <= _MAX_PATH_LEN
                           for p in touched_paths):
            errors.append("touched_paths 必须是长度受限的字符串列表")

    labels = candidate.get("labels")
    if labels is not None and not (
            isinstance(labels, list) and all(isinstance(l, str) for l in labels)):
        errors.append("labels 若存在必须是字符串列表（其值会被忽略，仅校验形状）")

    return errors


def _derive_labels(candidate: dict) -> list[str]:
    """确定性派生 labels，忽略任何输入 labels（评审 Important #3）。

    Workflow/skill 明确规定 candidate 不带 labels、由控制器根据
    priority/size/lane/needs_decision 派生；这里**不合并**任何输入 labels，
    只按白名单字段重新构造。
    """
    state_label = ("harness:needs-decision" if candidate.get("needs_decision")
                   else "harness:proposed")
    return ["harness", state_label, candidate["priority"],
            f"size:{candidate['size']}", f"lane:{candidate['lane']}"]


def _capability_drift_problems(
    settlement: fanout.FanoutSettlement,
) -> list[str]:
    """返回扇出层逐次检查并聚合出的能力漂移。"""
    return list(settlement.capability_drift)


def _describe_degraded(degraded: list) -> str:
    """把降级记录压成一行可读摘要，供 round.log 与账本 detail 使用。

    只取 agentType/occurrences/attempts 三项：错误原文已在 workflow 侧按同类
    折叠过（连续的同类传输故障之间没有信息含量），这里再全文带出只会淹掉
    真正有区别的失败。
    """
    parts = []
    for d in degraded:
        if not isinstance(d, dict):
            continue
        parts.append(
            f"{d.get('agentType') or d.get('label') or '?'}"
            f"×{d.get('occurrences', 1)}"
            f"（{d.get('attempts', '?')} 次尝试）")
    return "降级 agent：" + "、".join(parts) if parts else ""


def _settle_failed(budget: Budget, round_id: str, day: str, *,
                   cost_known: bool, cost_usd: float) -> None:
    """失败轮的结算：成本已知按实测，未知才按预留满额（评审 rmf-05）。"""
    if cost_known:
        budget.settle(round_id, day, cost_usd)
    else:
        budget.abandon(round_id, day)


def _format_detail(settlement: fanout.FanoutSettlement, fallback: str) -> str:
    if settlement.protocol_errors:
        return "; ".join(settlement.protocol_errors)
    return fallback


def _build_request_context(cfg):
    return build_request_context(cfg)


def _load_agents(repo_root) -> dict[str, prompts.AgentDef]:
    agent_dir = repo_root / ".claude" / "agents"
    names = (
        "harness-finder-roadmap",
        "harness-finder-code",
        "harness-finder-bench",
        "harness-finder-hygiene",
        "harness-judge-redline",
        "harness-judge-completed",
        "harness-judge-oracle",
    )
    return {
        name: prompts.parse_agent_file(agent_dir / f"{name}.md")
        for name in names
    }


def run_round(cfg, deps: Deps) -> dict:
    round_id = uuid.uuid4().hex[:12]
    started = time.monotonic()
    budget = Budget(deps.conn, cfg.round_budget_usd, cfg.daily_budget_usd)
    day = _today()

    # 单一 finalize 边界（评审 Critical C）：`progress` 记录本轮到目前为止
    # 已知的账本元数据。`Publisher.publish()`、GitHub、Git 或 SQLite 抛出
    # 的任何未预期异常，都会被下面唯一的 `except Exception` 捕获并据此结算
    # 账本——成本已知（invoke 已返回）则记实际值，未知则按该 round 的预留
    # 上限计费（与既有「结果未知按最坏值」语义一致）；随后原样重新抛出，
    # 绝不吞错。修复前只覆盖了显式 `return` 分支，未预期异常会让 round
    # 永久停在 `mode=pending, result=None, ended_at=None,
    # reserved_usd=<本轮预留>` 未释放的状态，若干次故障即可把日预算吃空、
    # harness 静默停摆。
    progress = {"mode": "scan", "cost_known": False, "cost": 0.0,
                "turns": None, "denials": None, "exit_code": None}

    def _finalize_unhandled_exception() -> None:
        row = deps.conn.execute(
            "SELECT reserved_usd, ended_at FROM rounds WHERE round_id=?",
            (round_id,)).fetchone()
        if row is None:
            # 异常发生在本轮任何账本行建立之前（`reserve()`/
            # `open_round_record()` 都还没跑到）：本轮从未占用预算，建一条
            # `reserved_usd=0` 的账本行留痕即可，不需要额外结算。
            budget.open_round_record(round_id, mode=progress["mode"])
        elif row["ended_at"] is None:
            if progress["cost_known"]:
                budget.settle(round_id, day, progress["cost"])
            else:
                # 成本未知：按该 round 自己的预留上限全额计费（worst-case，
                # 与 `abandon()`/`settle_orphaned()` 同一语义）。
                budget.abandon(round_id, day)
        budget.record_outcome(
            round_id, mode=progress["mode"], result="unhandled-exception",
            turns=progress["turns"], denials=progress["denials"],
            exit_code=progress["exit_code"])

    try:
        return _run_round_body(cfg, deps, round_id, started, budget, day, progress)
    except Exception:
        _finalize_unhandled_exception()
        raise


def _run_round_body(cfg, deps: Deps, round_id: str, started: float,
                    budget: Budget, day: str, progress: dict) -> dict:
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
        budget.open_round_record(round_id, mode="scan")
        budget.record_outcome(round_id, result="precheck-failed")
        return {"round_id": round_id, "mode": "scan", "result": "precheck-failed",
                "detail": str(exc)}

    # 恢复优先于新扫描、且必须在任何新预算预留之前检查（评审 Critical #1）：
    # `run_prechecks()` 内部已经对账（`outbox.reconcile(probes)`），这里读到
    # 的 `open_roots()` 已是对账后的真实状态。若在这里才发现还有未结的
    # operation，说明上一轮很可能「模型已花钱、发布未完成」就崩溃了——它的
    # 预留仍占满日预算。绝不能先为新 round 预留预算再检查，否则恢复路径
    # 永远到不了 budget-exhausted 分支。
    #
    # 按 root 聚合：同时有多个未结子 operation 时也只恢复一次，
    # 且不会把子 operation 的 payload 误当作 candidate（评审 C-02）
    open_roots = deps.outbox.open_roots()
    if open_roots:
        orphan_root = open_roots[0]
        # 结算旧 round 悬挂的预留：按它自己持久化的 started_at 所在日历日
        # 结算（不是「今天」），模型成本无法确认时按原 grant 最坏值收费。
        # 已结算过则是安全的 no-op；未知 round（理论上不会发生，root 必然
        # 来自某次 reserve）同样容错跳过，不阻断本轮恢复。
        try:
            budget.settle_orphaned(orphan_root.round_id)
        except BudgetError:
            pass
        # 恢复轮不得创建新的模型预算预留：只建一条 reserved_usd=0 的账本行。
        budget.open_round_record(round_id, mode="resume")
        progress["mode"] = "resume"
        progress["cost_known"] = True
        progress["cost"] = 0.0
        publisher = Publisher(deps.outbox, deps.gh, deps.worktree,
                              deps.queue, round_id)
        resumed = publisher.resume(orphan_root.operation_id)
        budget.settle(round_id, day, 0.0)
        budget.record_outcome(round_id, result="resumed")
        return {"round_id": round_id, "mode": "resume", "result": "resumed",
                "issue": resumed["issue"], "state": resumed["state"],
                "remaining_roots": len(open_roots) - 1}

    try:
        grant = budget.reserve(round_id, day)
    except BudgetError as exc:
        budget.open_round_record(round_id, mode="scan")
        budget.record_outcome(round_id, result="budget-exhausted")
        return {"round_id": round_id, "mode": "scan", "result": "budget-exhausted",
                "detail": str(exc)}

    blocked_lanes = [lane for lane in ("roadmap", "defect", "perf", "hygiene")
                     if deps.queue.lane_full(lane, cfg.lane_cap)]
    if deps.queue.total_full(cfg.proposed_cap):
        blocked_lanes = ["roadmap", "defect", "perf", "hygiene"]

    deadline_monotonic = started + ROUND_DEADLINE_S - CLEANUP_RESERVE_S
    if time.monotonic() >= deadline_monotonic:
        remaining = deadline_monotonic - time.monotonic()
        budget.abandon(round_id, day)
        budget.record_outcome(round_id, result="deadline-exhausted")
        return {"round_id": round_id, "mode": "scan", "result": "deadline-exhausted",
                "detail": f"调用截止时间已耗尽（剩余 {remaining:.1f}s）"}

    known_keys = set(deps.queue.known_canonical_keys())
    agents = _load_agents(cfg.repo_root)
    context = _build_request_context(cfg)
    call_budget = fanout.BudgetTracker(total_usd=grant)
    single_call_cap_usd = fanout.split_budget_cap(grant, 7)

    def _invoke(request: RoleInvocationRequest) -> InvocationResult:
        # 此闭包在 worker 线程运行。这里只允许模型调用；SQLite 写入统一在
        # run_fanout 返回后由当前主线程执行。
        return deps.invoke(request)

    fanout_result = fanout.run_fanout(
        round_id=round_id,
        invoke_fn=_invoke,
        budget=call_budget,
        deadline_monotonic=deadline_monotonic,
        blocked_lanes=blocked_lanes,
        known_canonical_keys=known_keys,
        inflight_paths=[],
        context=context,
        single_call_cap_usd=single_call_cap_usd,
        agents=agents,
        conn=deps.conn,
    )
    settlement = fanout_result["settlement"]
    attempts = fanout_result["attempts"]
    for record in attempts:
        invocation_id = f"{round_id}:{record.role}:{record.attempt}"
        budget.record_invocation(round_id, invocation_id, record.cost_usd)

    if not attempts:
        _settle_failed(
            budget,
            round_id,
            day,
            cost_known=settlement.cost_known,
            cost_usd=settlement.total_cost_usd,
        )
        budget.record_outcome(
            round_id,
            mode="scan",
            result="invocation-failed",
            turns=settlement.total_turns,
            denials=settlement.total_denials,
            exit_code=settlement.exit_code,
        )
        return {
            "round_id": round_id,
            "mode": "scan",
            "result": "invocation-failed",
            "detail": _format_detail(settlement, "no invocation was scheduled"),
        }

    progress["turns"] = settlement.total_turns
    progress["denials"] = settlement.total_denials
    progress["exit_code"] = settlement.exit_code
    progress["cost_known"] = settlement.cost_known
    progress["cost"] = settlement.total_cost_usd

    drift_problems = _capability_drift_problems(settlement)
    if drift_problems:
        _settle_failed(
            budget,
            round_id,
            day,
            cost_known=settlement.cost_known,
            cost_usd=settlement.total_cost_usd,
        )
        budget.record_outcome(
            round_id,
            mode="scan",
            result="capability-drift",
            turns=settlement.total_turns,
            denials=settlement.total_denials,
            exit_code=settlement.exit_code,
        )
        return {
            "round_id": round_id,
            "mode": "scan",
            "result": "capability-drift",
            "detail": _format_detail(settlement, "；".join(drift_problems)),
        }

    candidates = fanout_result["candidates"]
    # 结构校验必须先于任何解引用（评审 Critical A）：`candidates` 顶层非
    # 列表，或列表内混入非对象元素时，下面 `c.get("lane")` 会直接
    # `AttributeError` 崩溃——那样 round 会永久悬挂在
    # `mode=pending, result=None, reserved_usd` 未释放的状态。在这里就地
    # 判定为结构化的 `invalid-candidate` 结算路径，写完整账本、释放预留。
    shape_error = _candidates_shape_error(candidates)
    if shape_error is not None:
        _settle_failed(
            budget,
            round_id,
            day,
            cost_known=settlement.cost_known,
            cost_usd=settlement.total_cost_usd,
        )
        budget.record_outcome(round_id, mode="scan", result="invalid-candidate",
                              turns=settlement.total_turns, denials=settlement.total_denials,
                              exit_code=settlement.exit_code)
        return {"round_id": round_id, "mode": "scan", "result": "invalid-candidate",
                "detail": _format_detail(settlement, shape_error)}

    # 降级证据（某个 finder/judge 反复失败后被跳过）。它必须被读出来并进账本
    # ——否则一轮花了真金白银、只是裁决通道挂了，对外表现却与「仓库确实没东西
    # 可提」完全不可区分（评审 rmf-03）。
    degraded = fanout_result["degraded"]
    degraded_detail = _describe_degraded(degraded)

    eligible = [c for c in candidates if c.get("lane") not in blocked_lanes]
    if not eligible:
        # 有降级 = 不是干净的空轮。用不同的 result 值让它在账本与退出码上都可见；
        # 没有降级时保持 `no-candidate`，不把静默换成噪声。
        result = "no-candidate-degraded" if degraded else "no-candidate"
        _settle_failed(
            budget,
            round_id,
            day,
            cost_known=settlement.cost_known,
            cost_usd=settlement.total_cost_usd,
        )
        budget.record_outcome(round_id, mode="scan", result=result,
                              turns=settlement.total_turns, denials=settlement.total_denials,
                              exit_code=settlement.exit_code)
        out = {"round_id": round_id, "mode": "scan", "result": result}
        detail = _format_detail(settlement, degraded_detail)
        if detail:
            out["detail"] = detail
        return out

    candidate = dict(eligible[0])

    # DTO 校验必须在任何 outbox intent 或外部副作用之前（评审 Critical #2）：
    # 非法 candidate（缺字段、枚举越界、slug 穿越路径、未知字段等）在这里
    # 就地拒绝，产生零 Issue、零 operation、零 git 改动——不能先建 Issue、
    # 再靠 git 路径正则间接拒绝，那样非法 slug 会先产生一个公开 Issue，
    # 且该 Issue 已被 observed，之后每次 resume 都重遇同一非法 slug。
    dto_errors = validate_candidate(candidate)
    if dto_errors:
        _settle_failed(
            budget,
            round_id,
            day,
            cost_known=settlement.cost_known,
            cost_usd=settlement.total_cost_usd,
        )
        budget.record_outcome(round_id, mode="scan", result="invalid-candidate",
                              turns=settlement.total_turns, denials=settlement.total_denials,
                              exit_code=settlement.exit_code)
        return {"round_id": round_id, "mode": "scan", "result": "invalid-candidate",
                "detail": _format_detail(settlement, "; ".join(dto_errors))}

    candidate["fingerprint"] = fingerprint(
        candidate["goal"], candidate["invariant"],
        candidate["primary_path"], candidate["oracle"])
    if deps.queue.classify(candidate) != "new":
        # 被判重复的候选也要进去重集，否则系统学不会：本修复之前就已存在的提案
        # （真机的 Issue #1，其 canonical key 已无法回填——提案卡只有 body_md
        # 五节，fingerprint 又不可逆）会每轮被重新提出、每轮在这里被丢弃、每轮
        # 都不被记住，形成永久卡死：每 2 小时烧掉一整轮的钱且仓库零产出。
        # 只在这里记不影响正确性：能走到这里说明它确实已在 proposals 里在册。
        # 这里的四个字段已过 DTO 校验（`_REQUIRED_CANDIDATE_FIELDS`），必然存在。
        deps.queue.remember_canonical_key(
            candidate["fingerprint"],
            canonical_key(candidate["goal"], candidate["invariant"],
                          candidate["primary_path"], candidate["oracle"]))
        _settle_failed(
            budget,
            round_id,
            day,
            cost_known=settlement.cost_known,
            cost_usd=settlement.total_cost_usd,
        )
        budget.record_outcome(round_id, mode="scan", result="duplicate",
                              turns=settlement.total_turns, denials=settlement.total_denials,
                              exit_code=settlement.exit_code)
        out = {"round_id": round_id, "mode": "scan", "result": "duplicate"}
        detail = _format_detail(settlement, "")
        if detail:
            out["detail"] = detail
        return out

    # 控制器忽略任何输入 labels，确定性派生（评审 Important #3）。
    candidate["labels"] = _derive_labels(candidate)

    publisher = Publisher(deps.outbox, deps.gh, deps.worktree, deps.queue, round_id)
    # canonical key 的记忆已移进 Publisher.publish()，与 queue.record() 挨着写
    # ——分开写必然存在「Issue 已建、提案已在册、key 还没记」的崩溃窗口。
    published = publisher.publish(candidate)
    _settle_failed(
        budget,
        round_id,
        day,
        cost_known=settlement.cost_known,
        cost_usd=settlement.total_cost_usd,
    )
    budget.record_outcome(round_id, mode="scan", result="published",
                          turns=settlement.total_turns, denials=settlement.total_denials,
                          exit_code=settlement.exit_code)
    out_extra = {"degraded_detail": degraded_detail} if degraded else {}
    out = {**out_extra,
           "round_id": round_id, "mode": "scan", "result": "published",
           "issue": published["issue"], "state": published["state"]}
    detail = _format_detail(settlement, "")
    if detail:
        out["detail"] = detail
    return out
