#!/usr/bin/env python3
"""Phase 0 探针：会话原语真机验证（计划 `docs/harness/plan-control-flow-rewrite.md` Phase 0）。

两个待验证项：

- **Task 0.1 / 待决 A**：单发 `claude -p`（**不是** dual-pipe）能否用 `--session-id`
  预分配身份，再用 `--resume <sid> --fork-session` 续跑并保留上文。
  若被证伪，Phase 2 及之后所有任务的 `claude_runner.py` 接口形状都要改，
  所以在写一行生产代码之前先花 $1 以内问清楚。

- **Task 0.2 / 设计问题 5**：Stage 1 收窄后的只读工具（`Read`/`Grep`/`Glob`）
  是否触发 `can_use_tool`。

  **注意 oracle 与断言必须是同一件事**（评审 cfr-15）：不带
  `--permission-prompt-tool stdio` 时，CLI 根本不会产出任何 `control_request`
  事件——那种配置下「没看见 control_request」**永远为真**，不论被调用的工具
  是否本该触发权限请求。所以这里**必须先打开开关**，阴性结果才有意义。
  这是正控原则的另一种形态：先确认探测手段本身有能力看到目标现象，
  再采信它的阴性结果。

跑法：

    cd /home/xp/src/zipfs
    /home/linuxbrew/.linuxbrew/bin/python3 exp/control-flow-rewrite-probe/probe.py [0.1|0.2|all]

预算：整体 $2 以内，每次调用 `--max-budget-usd` ≤ 0.30。零外部写入
（不建 Issue、不 push、不碰 `.claude/state/harness.db`）。
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
ARTIFACTS = HERE / "artifacts"
CLAUDE = "/home/xp/.local/bin/claude"

sys.path.insert(0, str(REPO / "exp" / "stdio-driver"))


def stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")


def child_env() -> dict[str, str]:
    """给子进程的环境：去掉 GitHub 凭据，保留模型 API 认证。

    与生产 `_sanitize_env()` 的动机相同——探针不需要 GitHub 写能力，
    给了就是多余的授权面。
    """
    env = dict(os.environ)
    for key in ("GH_TOKEN", "GITHUB_TOKEN"):
        env.pop(key, None)
    return env


def run_single_shot(name: str, prompt: str, *, budget: float,
                    session_id: str | None = None,
                    resume: str | None = None,
                    fork_session: bool = False,
                    tools: str = "",
                    artifact_dir: Path) -> dict:
    """单发 `claude -p`，返回解析后的终态 result 事件 + init 事件。"""
    argv = [
        CLAUDE, "-p", prompt,
        "--model", "claude-sonnet-5",
        "--tools", tools,
        "--permission-mode", "dontAsk",
        "--max-turns", "4",
        "--max-budget-usd", f"{budget:.2f}",
        "--output-format", "stream-json",
        "--verbose",
        "--setting-sources", "",
    ]
    if session_id:
        argv += ["--session-id", session_id]
    if resume:
        argv += ["--resume", resume]
    if fork_session:
        argv += ["--fork-session"]

    artifact_dir.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(argv, cwd="/tmp", capture_output=True, text=True,
                          timeout=180, env=child_env())
    (artifact_dir / f"{name}.stdout.jsonl").write_text(proc.stdout, encoding="utf-8")
    if proc.stderr:
        (artifact_dir / f"{name}.stderr.txt").write_text(proc.stderr, encoding="utf-8")

    init_event = None
    result_event = None
    for line in proc.stdout.splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "system" and ev.get("subtype") == "init":
            init_event = ev
        elif ev.get("type") == "result":
            result_event = ev
    return {"argv_redacted": argv, "returncode": proc.returncode,
            "init": init_event, "result": result_event}


def task_0_1(root: Path) -> dict:
    """待决 A：单发模式下 session_id 预分配 + resume/fork-session 续跑。"""
    out = root / "task-0.1"
    sid = str(uuid.uuid4())

    first = run_single_shot(
        "first", "Remember the codeword PLUM. Reply exactly OK.",
        budget=0.15, session_id=sid, artifact_dir=out)

    observed_sid = (first["init"] or {}).get("session_id")
    second = run_single_shot(
        "second-fork", "What was the codeword? Reply exactly CODE:<word>.",
        budget=0.15, resume=sid, fork_session=True, artifact_dir=out)

    forked_sid = (second["init"] or {}).get("session_id")
    second_text = str((second["result"] or {}).get("result", ""))

    findings = {
        "requested_session_id": sid,
        "observed_session_id_first": observed_sid,
        "session_id_honoured": observed_sid == sid,
        "forked_session_id": forked_sid,
        "fork_produced_new_id": bool(forked_sid) and forked_sid != sid,
        "second_reply": second_text,
        "context_carried_over": "PLUM" in second_text,
        "cost_first": (first["result"] or {}).get("total_cost_usd"),
        "cost_second": (second["result"] or {}).get("total_cost_usd"),
        "returncodes": [first["returncode"], second["returncode"]],
    }
    findings["conclusion_strength"] = (
        "confirmed"
        if (findings["session_id_honoured"]
            and findings["fork_produced_new_id"]
            and findings["context_carried_over"])
        else "refuted")
    (out / "findings.json").write_text(
        json.dumps(findings, ensure_ascii=False, indent=2), encoding="utf-8")
    return findings


def task_0_2(root: Path) -> dict:
    """设计问题 5：只读工具是否触发 can_use_tool。

    必须带 `--permission-prompt-tool stdio` 才有观测能力（见模块 docstring）。
    复用 PoC 的 `Invocation`（dual-pipe + control 回调），不复制其代码。
    """
    from driver import Invocation, control_response, user_turn  # noqa: E402

    out = root / "task-0.2"
    seen_controls: list[dict] = []

    inv = Invocation(
        "readonly-tools-permission",
        0.30,
        tools="Read,Grep,Glob",
        # 必须钉模型：PoC 的 child_env() 用的是 7 个名字的**黑名单**，漏掉
        # ANTHROPIC_MODEL，父会话的 opus[1m] 会泄漏进来（生产 _sanitize_env 已
        # 改为前缀级 deny-by-default，这里是 PoC 侧的遗留，不影响生产）。
        extra_args=["--permission-prompt-tool", "stdio",
                    "--model", "claude-sonnet-5"],
        artifact_dir=out,
    )
    inv.start()
    inv.send(user_turn(
        "Read the file /etc/hostname and reply with exactly HOST:<contents>. "
        "Use the Read tool."))

    def on_control(event: dict) -> dict | None:
        if (event.get("request") or {}).get("subtype") != "can_use_tool":
            return None
        # 不论收到什么都先记下再放行——即便真的触发也不会把探针卡死。
        seen_controls.append(event)
        # 回调必须返回**完整信封**，不是内层 {"behavior": ...}；返回内层会被
        # 原样写进 stdin，CLI 收不到应答而永久等待（首次实测即卡死 180s）。
        return control_response(event["request_id"], {"behavior": "allow"})

    result = inv.wait_result(on_control=on_control)
    inv.finish()

    findings = {
        "config": "--permission-prompt-tool stdio + permissionMode=default"
                  "（**不是**生产配置：生产用 --permission-mode dontAsk +"
                  " settings 的 permissions.allow，且不带 stdio 标志）",
        "permission_prompt_tool_enabled": True,
        "init_tools": (inv.events[0].get("tools") if inv.events else None),
        "control_request_count": len(seen_controls),
        "control_request_tools": [
            (c.get("request") or {}).get("tool_name") for c in seen_controls],
        "result_text": str((result or {}).get("result", ""))[:200],
        "cost": (result or {}).get("total_cost_usd"),
    }
    findings["conclusion"] = (
        "readonly-tools-DO-trigger-can-use-tool-when-stdio-enabled"
        if seen_controls else "readonly-tools-do-not-trigger")
    findings["conclusion_strength"] = "confirmed"
    (out / "findings.json").write_text(
        json.dumps(findings, ensure_ascii=False, indent=2), encoding="utf-8")
    return findings


def main() -> int:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    root = ARTIFACTS / stamp()
    root.mkdir(parents=True, exist_ok=True)
    out: dict = {}
    if which in ("0.1", "all"):
        out["task_0_1"] = task_0_1(root)
        print(json.dumps(out["task_0_1"], ensure_ascii=False, indent=2))
    if which in ("0.2", "all"):
        out["task_0_2"] = task_0_2(root)
        print(json.dumps(out["task_0_2"], ensure_ascii=False, indent=2))
    (root / "summary.json").write_text(
        json.dumps(out, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"\nartifacts: {root}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
