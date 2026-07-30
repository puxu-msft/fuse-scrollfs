"""调用 claude -p 并解析 stream-json（spec §9.1、§七 B.2）。

启动组合是硬契约：--setting-sources project 屏蔽用户级授权与 hooks/plugins，
--strict-mcp-config 且不给 --mcp-config 等价于零 MCP。
"""

from __future__ import annotations

import json
import os
import subprocess
from dataclasses import dataclass, field

from .config import CLAUDE

# 不用正则从 fence 里"抠"JSON：payload 的 body_md 是 Markdown，内部完全可能
# 含代码 fence；一旦某段以 `}` 收尾再跟 ```，正则会把字符串内部的 fence 当成
# 外层结束，截出半个对象（实测反例：body_md="example } ``` remainder"）。
# 改为按首尾边界剥壳，再对中间全文做一次 json.loads。


@dataclass
class InvocationResult:
    ok: bool
    payload: dict | None
    cost_usd: float
    turns: int
    denials: int = 0
    exit_code: int = 0
    raw_tail: str = ""
    init_seen: bool = False          # 未见 init 事件时不得宣称「无 Bash、无 MCP」
    init_tools: list[str] = field(default_factory=list)
    init_mcp_servers: list = field(default_factory=list)
    init_plugins: list = field(default_factory=list)
    init_errors: list = field(default_factory=list)


def build_argv(prompt: str, tools: str, grant_usd: float, max_turns: int,
               settings_path: str) -> list[str]:
    return [
        CLAUDE, "-p", prompt,
        "--setting-sources", "project",
        "--settings", settings_path,
        "--strict-mcp-config",
        "--tools", tools,
        "--permission-mode", "dontAsk",
        "--max-turns", str(max_turns),
        "--max-budget-usd", f"{grant_usd:.2f}",
        "--output-format", "stream-json",
    ]


def _extract_payload(text: str) -> dict | None:
    blob = (text or "").strip()
    if blob.startswith("```"):
        first_newline = blob.find("\n")
        last_fence = blob.rfind("```")
        if first_newline != -1 and last_fence > first_newline:
            blob = blob[first_newline + 1:last_fence].strip()
    if not blob.startswith(("{", "[")):
        return None
    try:
        data = json.loads(blob)
    except json.JSONDecodeError:
        return None
    return data if isinstance(data, dict) else {"candidates": data}


def parse_stream_json(lines) -> InvocationResult:
    cost, turns, ok, payload = 0.0, 0, False, None
    init_seen = False
    init_tools: list[str] = []
    init_mcp: list = []
    init_plugins: list = []
    init_errors: list = []
    tail: list[str] = []
    for line in lines:
        line = line.strip()
        if not line:
            continue
        tail.append(line)
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "system" and event.get("subtype") == "init":
            init_seen = True
            init_tools = event.get("tools", [])
            init_mcp = event.get("mcp_servers", [])
            init_plugins = event.get("plugins", [])
            init_errors = (event.get("plugin_errors", [])
                           + event.get("mcp_server_errors", []))
        elif event.get("type") == "result":
            cost = float(event.get("total_cost_usd", 0.0))
            turns = int(event.get("num_turns", 0))
            if event.get("subtype") == "success":
                payload = _extract_payload(event.get("result", ""))
                ok = payload is not None
    return InvocationResult(ok=ok, payload=payload, cost_usd=cost, turns=turns,
                            raw_tail="\n".join(tail[-5:]), init_seen=init_seen,
                            init_tools=init_tools, init_mcp_servers=init_mcp,
                            init_plugins=init_plugins, init_errors=init_errors)


def invoke(prompt: str, tools: str, grant_usd: float, max_turns: int,
           settings_path: str, cwd: str, timeout_s: float,
           env: dict | None = None) -> InvocationResult:
    argv = build_argv(prompt, tools, grant_usd, max_turns, settings_path)
    # 从完整环境出发再删凭据：只给 GIT_TERMINAL_PROMPT 会丢掉 HOME/PATH 等
    # claude 运行所必需的变量（评审 C-06）
    safe_env = dict(env if env is not None else os.environ)
    for key in ("GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK",
                "GIT_ASKPASS", "SSH_ASKPASS"):
        safe_env.pop(key, None)
    safe_env["GIT_TERMINAL_PROMPT"] = "0"
    try:
        proc = subprocess.run(argv, cwd=cwd, capture_output=True, text=True,
                              timeout=timeout_s, env=safe_env)
    except subprocess.TimeoutExpired as exc:
        return InvocationResult(False, None, 0.0, 0, exit_code=124,
                                raw_tail=str(exc)[-500:])
    result = parse_stream_json(proc.stdout.splitlines())
    result.exit_code = proc.returncode
    if proc.returncode != 0:
        # 退出码非 0 时即便 stdout 里恰好有 success result 也不得判 ok
        result.ok = False
        if not result.raw_tail:
            result.raw_tail = proc.stderr[-500:]
    return result
