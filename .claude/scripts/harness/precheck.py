"""启动硬预检（spec §七 Phase A）：任一失败 fail closed，不起模型、不烧钱。"""

from __future__ import annotations

import os
from dataclasses import dataclass

from .config import CLAUDE, FLOCK, GH, GIT, PYTHON

DEFAULT_TOOLS = (PYTHON, CLAUDE, GH, GIT, FLOCK)
PAUSED_LABEL = "harness:paused"


class PrecheckFailed(Exception):
    pass


@dataclass(frozen=True)
class CheckResult:
    name: str
    ok: bool
    detail: str


def run_prechecks(cfg, gh, worktree, outbox,
                  tools: tuple[str, ...] = DEFAULT_TOOLS,
                  probes: dict | None = None) -> list[CheckResult]:
    results: list[CheckResult] = []

    token_ok = bool(getattr(cfg, "gh_token", ""))
    results.append(CheckResult("gh_token", token_ok,
                               "GH_TOKEN 为空" if not token_ok else "ok"))

    try:
        perm = gh.viewer_permission()
        ok = perm in ("WRITE", "MAINTAIN", "ADMIN")
        results.append(CheckResult("viewer_permission", ok,
                                   f"viewerPermission={perm}，需 >= WRITE"))
    except Exception as exc:
        results.append(CheckResult("viewer_permission", False, repr(exc)))

    for tool in tools:
        ok = os.path.isfile(tool) and os.access(tool, os.X_OK)
        results.append(CheckResult(f"tool:{tool}", ok,
                                   f"{tool} 不存在或不可执行" if not ok else "ok"))

    # 顺序不可调换：先对账，再决定能否 reset 工作区（评审 C-04）
    outbox.reconcile(probes or {})
    # 待推提交必须直查持久化事实，不能从 reconcile 的返回值推导——
    # 已 observed 的 commit operation 不在未决集合里，那样推导恒为 False
    has_unpushed_commit = bool(outbox.unpushed_commits())
    try:
        worktree.ensure(allow_reset=not has_unpushed_commit)
        clean = worktree.is_clean()
        results.append(CheckResult(
            "publish_worktree_clean", clean or has_unpushed_commit,
            "发布工作区有未提交改动" if not clean else "ok"))
    except Exception as exc:
        results.append(CheckResult("publish_worktree_clean", False, repr(exc)))

    # 只有 failed_terminal 才阻断本轮。**不能**用 open_operations()——
    # 那会让 prepared / failed_retryable 也阻断预检，于是 run_round 直接返回
    # precheck-failed，「恢复优先于新扫描」的路径永远走不到，形成死锁。
    unresolved = outbox.unresolved()
    results.append(CheckResult(
        "outbox_resolved", not unresolved,
        f"存在 {len(unresolved)} 个未决 operation：" +
        ", ".join(f"{o.kind}/{o.natural_key}" for o in unresolved)
        if unresolved else "ok"))

    try:
        paused = gh.list_open_issues_with_label(PAUSED_LABEL)
        results.append(CheckResult("not_paused", not paused,
                                   f"存在 {PAUSED_LABEL} 哨兵 Issue"
                                   if paused else "ok"))
    except Exception as exc:
        results.append(CheckResult("not_paused", False, repr(exc)))

    return results


def assert_all_ok(results: list[CheckResult]) -> None:
    failed = [r for r in results if not r.ok]
    if failed:
        lines = "\n".join(f"  - {r.name}: {r.detail}" for r in failed)
        raise PrecheckFailed(f"预检失败 {len(failed)} 项：\n{lines}")
