"""harness 入口：round / status / doctor / probe。"""

from __future__ import annotations

import argparse
import json
import os
import sys

from . import db
from .claude_runner import invoke
from .config import load_config
from .ghclient import GhCli
from .gitops import PublishWorktree
from .lifecycle import State
from .outbox import Outbox
from .precheck import inspect_facts
from .queue import Queue
from .round import SETTINGS_PATH, STAGE1_TOOLS, Deps, run_round


def _wire(cfg):
    conn = db.connect(cfg.state_db)
    db.migrate(conn)
    gh = GhCli(cfg)
    worktree = PublishWorktree(cfg.repo_root, cfg.publish_worktree)
    return conn, gh, worktree


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="harness")
    parser.add_argument("command",
                        choices=["round", "status", "doctor", "probe"])
    args = parser.parse_args(argv)
    cfg = load_config()
    conn, gh, worktree = _wire(cfg)

    if args.command == "doctor":
        # `doctor` 是第一道纯只读授权诊断门：绝不 reconcile、绝不
        # fetch/reset 发布工作区，也不能对尚未收敛的 `prepared` root 假绿
        # （评审 Important-2）。生产 round 路径仍走带副作用的
        # `run_prechecks()`，两者刻意分离。
        results = inspect_facts(cfg, gh, worktree, Outbox(conn))
        for r in results:
            print(f"[{'ok ' if r.ok else 'FAIL'}] {r.name}: {r.detail}")
        return 0 if all(r.ok for r in results) else 1

    if args.command == "status":
        rows = conn.execute(
            "SELECT round_id, mode, result, settled_usd, started_at FROM rounds"
            " ORDER BY started_at DESC LIMIT 20").fetchall()
        for row in rows:
            print(dict(row))
        return 0

    if args.command == "probe":
        res = invoke(prompt="回复 OK，不要调用任何工具。", tools=STAGE1_TOOLS,
                     grant_usd=0.10, max_turns=2, settings_path=SETTINGS_PATH,
                     cwd=str(cfg.repo_root), timeout_s=180, env=dict(os.environ))
        print(json.dumps({
            "exit_code": res.exit_code, "init_seen": res.init_seen,
            "init_tools": res.init_tools, "init_mcp_servers": res.init_mcp_servers,
            "init_plugins": res.init_plugins, "init_errors": res.init_errors,
            "cost_usd": res.cost_usd,
        }, ensure_ascii=False, indent=2))
        # 缺 init 事件不得当作「干净」——absence-as-success 是典型假绿
        if not res.init_seen:
            print("负向验证失败：未观察到 system/init 事件，无法证明隔离生效")
            return 1
        expected = set(STAGE1_TOOLS.split(","))
        actual = set(res.init_tools)
        problems = []
        # `res.ok` 只覆盖 stream 协议本身是否干净；进程退出码非 0（例如 claude
        # 因参数错误提前退出）与残留的 protocol_errors 都必须显式再查一遍，
        # 否则「init 事件看着干净」会被误判为整体成功（评审 Important #5）。
        if not res.ok:
            problems.append("res.ok 为 False（stream 不干净或非成功终态）")
        if res.exit_code != 0:
            problems.append(f"进程退出码非 0：{res.exit_code}")
        if res.protocol_errors:
            problems.append(f"协议错误：{res.protocol_errors}")
        if actual != expected:
            problems.append(f"工具集不等：多={sorted(actual - expected)} "
                            f"少={sorted(expected - actual)}")
        if res.init_mcp_servers:
            problems.append(f"MCP 未清空：{res.init_mcp_servers}")
        if res.init_plugins:
            problems.append(f"插件未清空：{res.init_plugins}")
        if res.init_errors:
            problems.append(f"加载报错：{res.init_errors}")
        if problems:
            print("负向验证失败：" + "；".join(problems))
            return 1
        print(f"负向验证通过：工具集恰为 {sorted(expected)}，无 MCP、无插件")
        return 0

    deps = Deps(conn=conn, gh=gh, worktree=worktree, outbox=Outbox(conn),
                queue=Queue(conn), invoke=invoke)
    result = run_round(cfg, deps)
    print(json.dumps(result, ensure_ascii=False))
    if result["result"] in ("published", "no-candidate", "duplicate"):
        return 0
    # 恢复轮只有真正收敛（发布收据核验通过）才算成功；仍处于 inconsistent
    # 或任何未收敛的中间态都必须非零退出，否则一次半途而废的恢复会被
    # systemd 误报为成功（评审 Important #5）。
    if result["result"] == "resumed" and result.get("state") == State.RECEIPT_COMPLETE:
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
