"""启动硬预检（spec §七 Phase A）：任一失败 fail closed，不起模型、不烧钱。

分两层（评审 Important-2）：
- 第一层 `_inspect_preconditions`：纯读、无副作用（token / 权限 / 工具 / 暂停哨兵）。
- 第二层 `_prepare_publish_worktree`：有副作用（outbox reconcile、worktree
  prune/create/reset）。**只有第一层全部通过才会进入第二层**——否则暂停或权限
  不足时，预检本身就会先去 reconcile、reset 发布工作区，这与"本轮不该发生"矛盾。
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

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
    results = _inspect_preconditions(cfg, gh, tools)
    if any(not r.ok for r in results):
        # 第一层未过：绝不进入有副作用的第二层（reconcile / worktree reset）。
        return results
    results.extend(_prepare_publish_worktree(outbox, worktree, probes or {}))
    return results


def _inspect_preconditions(cfg, gh, tools: tuple[str, ...]) -> list[CheckResult]:
    """第一层：纯读、无副作用。"""
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

    try:
        paused = gh.list_open_issues_with_label(PAUSED_LABEL)
        results.append(CheckResult("not_paused", not paused,
                                   f"存在 {PAUSED_LABEL} 哨兵 Issue"
                                   if paused else "ok"))
    except Exception as exc:
        results.append(CheckResult("not_paused", False, repr(exc)))

    return results


def _prepare_publish_worktree(outbox, worktree, probes: dict) -> list[CheckResult]:
    """第二层：有副作用。第一层全部通过后才会执行到这里。"""
    results: list[CheckResult] = []

    try:
        # 顺序不可调换：先对账，再决定能否 reset 工作区（评审 C-04）
        outbox.reconcile(probes)
    except Exception as exc:
        # reconcile 抛出即代表持久化事实尚不可信，不得再去碰 worktree
        # （评审 Minor-3）：既不能凭它判断 has_unpushed_commit，也不能
        # 让 ensure() 在一个我们没能核实过的状态上执行 reset/clean。
        results.append(CheckResult("outbox_reconcile", False, repr(exc)))
    else:
        # 待推提交必须直查持久化事实，不能从 reconcile 的返回值推导——
        # 已 observed 的 commit operation 不在未决集合里，那样推导恒为 False
        has_unpushed_commit = bool(outbox.unpushed_commits())
        results.append(_check_publish_worktree_clean(worktree, has_unpushed_commit))

    # 只有 failed_terminal 才阻断本轮。**不能**用 open_operations()——
    # 那会让 prepared / failed_retryable 也阻断预检，于是 run_round 直接返回
    # precheck-failed，「恢复优先于新扫描」的路径永远走不到，形成死锁。
    unresolved = outbox.unresolved()
    results.append(CheckResult(
        "outbox_resolved", not unresolved,
        f"存在 {len(unresolved)} 个未决 operation：" +
        ", ".join(f"{o.kind}/{o.natural_key}" for o in unresolved)
        if unresolved else "ok"))

    return results


def _worktree_exists(worktree) -> bool:
    path = getattr(worktree, "path", None)
    if path is None:
        return False
    return (Path(path) / ".git").exists()


def _check_publish_worktree_clean(worktree, has_unpushed_commit: bool) -> CheckResult:
    """评审 Important-1：先判断 dirty，再决定是否 reset/clean，绝不可反过来。

    `PublishWorktree.ensure(allow_reset=True)` 内部会先 `reset --hard` 再
    `clean -fd`；若我们先调用它、再问 `is_clean()`，那必然恒真——真实的脏
    改动在被检查之前就已经被清空了。所以：worktree 已存在且此刻已知会被
    允许 reset（即没有待推提交）时，必须先在任何 reset/clean 之前调用一次
    `is_clean()`；一旦发现脏，直接判失败并**不再调用 `ensure()`**，避免脏
    改动被清理路径悄悄销毁。

    `has_unpushed_commit=True` 时不做这项前置检查——此时 `ensure()` 本就以
    `allow_reset=False` 调用，不会触发 reset/clean，不存在数据被销毁的风险，
    沿用原有「clean or has_unpushed_commit」判定即可（恢复态语义不变）。
    """
    try:
        if _worktree_exists(worktree) and not has_unpushed_commit \
                and not worktree.is_clean():
            return CheckResult("publish_worktree_clean", False,
                               "发布工作区有未提交改动")
        worktree.ensure(allow_reset=not has_unpushed_commit)
        clean = worktree.is_clean()
        return CheckResult(
            "publish_worktree_clean", clean or has_unpushed_commit,
            "发布工作区有未提交改动" if not clean else "ok")
    except Exception as exc:
        return CheckResult("publish_worktree_clean", False, repr(exc))


def assert_all_ok(results: list[CheckResult]) -> None:
    failed = [r for r in results if not r.ok]
    if failed:
        lines = "\n".join(f"  - {r.name}: {r.detail}" for r in failed)
        raise PrecheckFailed(f"预检失败 {len(failed)} 项：\n{lines}")
