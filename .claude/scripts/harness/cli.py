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

# `invoke()` 统一走 `claude_runner._extract_payload()`：只接受形如
# `{"candidates": [...]}` 的对象，且要求「单个 JSON 代码块，不加任何解释」
# （评审 Critical B）。probe 若仍要求模型「回复 OK，不要调用任何工具」，
# 模型正确遵从提示词后 `_extract_payload("OK")` 返回 None，`res.ok` 恒为
# False——probe 在设计上永远不可能通过。这里选择方案一：让 probe 的提示词
# 直接要求返回 parser 能接受的确定性 payload，而不是给 probe 另开一条独立
# 判定路径。理由：probe 本就复用同一个 `invoke()`/`parse_stream_json()`
# 管线来验证「Stage 1 隔离生效」，若再引入第二套响应判定逻辑，反而制造了
# 一条只有 probe 会走、生产 round 永远不会走的代码路径——这条路径本身就可能
# 藏着与真实契约不一致的假绿。让 probe 与生产 round 共用同一份「模型必须吐出
# 单个 JSON 代码块」契约，才是真正验证了这条契约本身可行。
_PROBE_REPLY_JSON = '{"candidates": []}'
PROBE_PROMPT = ("回复恰好一个 JSON 代码块，不要调用任何工具、不要输出任何其他"
                f"文字：\n```json\n{_PROBE_REPLY_JSON}\n```")
# 模型若严格遵从上面的提示词，应当原样回显这个代码块——这是 probe 的
# 「真实接缝」测试要喂给 `parse_stream_json()` 的确切文本，而不是手工构造的
# `payload={"candidates": []}`。
PROBE_EXPECTED_REPLY = f"```json\n{_PROBE_REPLY_JSON}\n```"


def _publish_or_resume_succeeded(result: dict) -> bool:
    """新发布与恢复共用同一个成功谓词（评审 Critical B）：终态必须是
    `publication-receipt-complete` 才算真正收敛。

    修复前：`result="published"` 无条件判定退出 0，即便 `state` 是
    `inconsistent` 或 `proposal-published`（收据未核验通过）；只有
    `resumed` 路径额外检查了 `RECEIPT_COMPLETE`。两条路径的成功语义必须
    一致，否则一次「Issue 已建、收据未核验」的半途而废发布会被 systemd
    误报为成功。
    """
    return result.get("state") == State.RECEIPT_COMPLETE


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
        res = invoke(prompt=PROBE_PROMPT, tools=STAGE1_TOOLS,
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
    if result["result"] in ("no-candidate", "duplicate"):
        return 0
    # 新发布（`published`）与恢复（`resumed`）共用同一个成功谓词（评审
    # Critical B）：终态必须是 `publication-receipt-complete` 才算真正收敛。
    # 修复前 `published` 无条件判 0，即便 `state` 是 `inconsistent` 或
    # `proposal-published`（收据未核验通过）——一次半途而废的发布会被
    # systemd 误报为成功。
    if result["result"] in ("published", "resumed"):
        return 0 if _publish_or_resume_succeeded(result) else 1
    return 1


if __name__ == "__main__":
    sys.exit(main())
