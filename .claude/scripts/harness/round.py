"""一轮编排（spec §七 Phase A/B/C）。Stage 1 只走「只扫描」模式。"""

from __future__ import annotations

import datetime as dt
import re
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
    invoke: Callable[..., InvocationResult]
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


def _capability_drift_problems(invocation: InvocationResult) -> list[str]:
    """能力漂移校验（评审 Important #4）：Round 0 的 probe 通过不保证以后
    每次 invocation 都没有配置漂移——必须把实际解析到的能力集纳入**每次**
    invocation 的成功谓词。
    """
    problems: list[str] = []
    expected = frozenset(STAGE1_TOOLS.split(","))
    actual = frozenset(invocation.init_tools)
    if actual != expected:
        problems.append(f"工具集不等：多={sorted(actual - expected)} "
                        f"少={sorted(expected - actual)}")
    if invocation.init_mcp_servers:
        problems.append(f"MCP 未清空：{invocation.init_mcp_servers}")
    if invocation.init_plugins:
        problems.append(f"插件未清空：{invocation.init_plugins}")
    if invocation.init_errors:
        problems.append(f"加载报错：{invocation.init_errors}")
    return problems


def run_round(cfg, deps: Deps) -> dict:
    round_id = uuid.uuid4().hex[:12]
    started = time.monotonic()
    budget = Budget(deps.conn, cfg.round_budget_usd, cfg.daily_budget_usd)
    day = _today()

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

    # 单调截止已耗尽（或不足以覆盖结算窗口）时，绝不能用 `max(x, 60)` 之类
    # 的下限把剩余时间反向放大——那会让子进程超时超过整轮剩余时间（评审
    # Important #7）。剩余不足以覆盖 `CLEANUP_RESERVE_S` 时直接不起模型。
    remaining = ROUND_DEADLINE_S - (time.monotonic() - started)
    if remaining <= CLEANUP_RESERVE_S:
        budget.abandon(round_id, day)
        budget.record_outcome(round_id, result="deadline-exhausted")
        return {"round_id": round_id, "mode": "scan", "result": "deadline-exhausted",
                "detail": f"剩余 {remaining:.1f}s 不足以覆盖结算窗口"
                          f" {CLEANUP_RESERVE_S:.0f}s"}
    timeout_s = remaining - CLEANUP_RESERVE_S

    prompt = ("/scrollz-round\n"
              f'{{"blocked_lanes": {blocked_lanes!r}, "known_canonical_keys": [],'
              f' "inflight_paths": []}}').replace("'", '"')

    invocation = deps.invoke(prompt=prompt, tools=STAGE1_TOOLS, grant_usd=grant,
                             max_turns=cfg.max_turns, settings_path=SETTINGS_PATH,
                             cwd=str(cfg.repo_root), timeout_s=timeout_s)

    if not invocation.ok or invocation.payload is None:
        budget.abandon(round_id, day)
        budget.record_outcome(round_id, mode="scan", result="invocation-failed",
                              turns=invocation.turns, denials=invocation.denials,
                              exit_code=invocation.exit_code)
        return {"round_id": round_id, "mode": "scan",
                "result": "invocation-failed", "detail": invocation.raw_tail}

    # 能力漂移 fail-closed（评审 Important #4）：`invocation.ok` 只表示 stream
    # 协议干净，不保证本次真实解析到的 tools/MCP/plugins/加载错误仍等于
    # Stage 1 集合。漂移即判失败并按最坏值记账，不得继续使用其 candidates。
    drift_problems = _capability_drift_problems(invocation)
    if drift_problems:
        budget.abandon(round_id, day)
        budget.record_outcome(round_id, mode="scan", result="capability-drift",
                              turns=invocation.turns, denials=invocation.denials,
                              exit_code=invocation.exit_code)
        return {"round_id": round_id, "mode": "scan", "result": "capability-drift",
                "detail": "；".join(drift_problems)}

    candidates = invocation.payload.get("candidates", [])
    eligible = [c for c in candidates if c.get("lane") not in blocked_lanes]
    if not eligible:
        budget.settle(round_id, day, invocation.cost_usd)
        budget.record_outcome(round_id, mode="scan", result="no-candidate",
                              turns=invocation.turns, denials=invocation.denials,
                              exit_code=invocation.exit_code)
        return {"round_id": round_id, "mode": "scan", "result": "no-candidate"}

    candidate = dict(eligible[0])

    # DTO 校验必须在任何 outbox intent 或外部副作用之前（评审 Critical #2）：
    # 非法 candidate（缺字段、枚举越界、slug 穿越路径、未知字段等）在这里
    # 就地拒绝，产生零 Issue、零 operation、零 git 改动——不能先建 Issue、
    # 再靠 git 路径正则间接拒绝，那样非法 slug 会先产生一个公开 Issue，
    # 且该 Issue 已被 observed，之后每次 resume 都重遇同一非法 slug。
    dto_errors = validate_candidate(candidate)
    if dto_errors:
        budget.settle(round_id, day, invocation.cost_usd)
        budget.record_outcome(round_id, mode="scan", result="invalid-candidate",
                              turns=invocation.turns, denials=invocation.denials,
                              exit_code=invocation.exit_code)
        return {"round_id": round_id, "mode": "scan", "result": "invalid-candidate",
                "detail": "; ".join(dto_errors)}

    candidate["fingerprint"] = fingerprint(
        candidate["goal"], candidate["invariant"],
        candidate["primary_path"], candidate["oracle"])
    if deps.queue.classify(candidate) != "new":
        budget.settle(round_id, day, invocation.cost_usd)
        budget.record_outcome(round_id, mode="scan", result="duplicate",
                              turns=invocation.turns, denials=invocation.denials,
                              exit_code=invocation.exit_code)
        return {"round_id": round_id, "mode": "scan", "result": "duplicate"}

    # 控制器忽略任何输入 labels，确定性派生（评审 Important #3）。
    candidate["labels"] = _derive_labels(candidate)

    publisher = Publisher(deps.outbox, deps.gh, deps.worktree, deps.queue, round_id)
    published = publisher.publish(candidate)
    budget.settle(round_id, day, invocation.cost_usd)
    budget.record_outcome(round_id, mode="scan", result="published",
                          turns=invocation.turns, denials=invocation.denials,
                          exit_code=invocation.exit_code)
    return {"round_id": round_id, "mode": "scan", "result": "published",
            "issue": published["issue"], "state": published["state"]}
