"""调用 claude -p 并解析 stream-json（spec §9.1、§七 B.2）。

启动组合是硬契约：--setting-sources project 屏蔽用户级授权与 hooks/plugins，
--strict-mcp-config 且不给 --mcp-config 等价于零 MCP。

本模块是 headless 子进程的唯一入口——它决定了无人值守 agent 到底拿到多大权限，
因此硬权限契约（Stage 1 工具 allowlist、预算/回合数上限、settings 路径）不能只活
在调用方的测试输入里：`build_argv()` 自己校验，误传/配置污染/把 Stage 1 工具集
改成 "default" 一律在这里被拒绝，而不是被原样传给 claude。
"""

from __future__ import annotations

import json
import math
import os
import subprocess
from dataclasses import dataclass, field
from typing import Callable

from .config import CLAUDE

# 不用正则从 fence 里"抠"JSON：payload 的 body_md 是 Markdown，内部完全可能
# 含代码 fence；一旦某段以 `}` 收尾再跟 ```，正则会把字符串内部的 fence 当成
# 外层结束，截出半个对象（实测反例：body_md="example } ``` remainder"）。
# 改为按首尾边界剥壳，再对中间全文做一次 json.loads。

# Stage 1 允许的工具集：只读探测 + Skill/Workflow 调用，不含任何写能力。
# build_argv() 强制 tools 解析后恰好等于这个集合——多一个、少一个、换成
# "default" 都会被拒绝，而不是原样传给 claude 子进程。
STAGE1_ALLOWED_TOOLS = frozenset({"Read", "Grep", "Glob", "Skill", "Workflow"})


class UnsafeInvocationError(ValueError):
    """build_argv 检测到会突破 Stage 1 权限边界的调用参数。"""


# 需要在子进程环境里清除的凭据通道。除了直接的 token/askpass 变量外，还要清
# gh/git 会读取的配置定位变量——否则即便删了 GH_TOKEN，子进程仍可能通过
# ~/.config/gh/hosts.yml 或 ~/.gitconfig 的 credential helper 拿到等价凭据。
_CREDENTIAL_ENV_VARS = (
    "GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK", "GIT_ASKPASS", "SSH_ASKPASS",
    "GH_CONFIG_DIR", "XDG_CONFIG_HOME", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM",
)
# git 的按序注入配置通道：GIT_CONFIG_COUNT=n 之外还有编号的 KEY_<n>/VALUE_<n>。
_GIT_CONFIG_INDEXED_PREFIXES = ("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_")


def _validate_tools(tools) -> None:
    if not isinstance(tools, str):
        raise UnsafeInvocationError(f"tools 必须是逗号分隔字符串，实际 {tools!r}")
    parsed = frozenset(t.strip() for t in tools.split(",") if t.strip())
    if parsed != STAGE1_ALLOWED_TOOLS:
        raise UnsafeInvocationError(
            "tools 必须恰好等于 Stage 1 allowlist "
            f"{sorted(STAGE1_ALLOWED_TOOLS)}，实际收到 {sorted(parsed)!r}"
            f"（原始输入 tools={tools!r}）")


def _validate_grant_usd(grant_usd) -> None:
    try:
        value = float(grant_usd)
    except (TypeError, ValueError):
        raise UnsafeInvocationError(f"grant_usd 必须是数字，实际 {grant_usd!r}") from None
    if not math.isfinite(value) or value <= 0:
        raise UnsafeInvocationError(f"grant_usd 必须是有限正数，实际 {grant_usd!r}")


def _validate_max_turns(max_turns) -> None:
    if not isinstance(max_turns, int) or max_turns <= 0:
        raise UnsafeInvocationError(f"max_turns 必须是正整数，实际 {max_turns!r}")


def _validate_settings_path(settings_path) -> None:
    if not isinstance(settings_path, str) or not settings_path.strip():
        raise UnsafeInvocationError(f"settings_path 不能为空，实际 {settings_path!r}")


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
    # 解析期发现的协议层异常（重复 init/result、无法解析的行、非法 cost/turns
    # 等）。非空即代表 stream 不干净——即便某个 terminal result 看起来是
    # success，也不得据此判 ok=True。
    protocol_errors: list[str] = field(default_factory=list)


def build_argv(prompt: str, tools: str, grant_usd: float, max_turns: int,
               settings_path: str, model: str | None = None) -> list[str]:
    """构造 `claude -p` 的固定 argv。

    权限防线注记（真实契约探针实测，2026-07-31）：真正拦截工具调用的是
    `--settings` 指向的配置里的 `permissions.allow` 列表，不是 `deny` 列表。
    `--permission-mode dontAsk` 会拒绝一切不在 `allow` 里的工具——旧配置只写
    了 `deny`、`allow` 留空，导致入口 skill 连 `Workflow` 都调不动，重试到烧
    完预算。因此 `harness-settings.json` 的 `permissions.allow` 必须显式列出
    Stage 1 允许的每个工具名，清空 allow **不是**更安全的做法，而是会让一切
    工具调用都被拒。

    `--verbose` 是硬性必需项：`--output-format=stream-json` 与 `--print` 同时
    使用时，claude 会直接报错 `requires --verbose` 拒绝启动（`claude --help`
    文档没有记录这条约束，只有真跑才会暴露）。
    """
    _validate_tools(tools)
    _validate_grant_usd(grant_usd)
    _validate_max_turns(max_turns)
    _validate_settings_path(settings_path)
    argv = [
        CLAUDE, "-p", prompt,
        "--setting-sources", "project",
        "--settings", settings_path,
        "--strict-mcp-config",
        "--tools", tools,
        "--permission-mode", "dontAsk",
        "--max-turns", str(max_turns),
        "--max-budget-usd", f"{grant_usd:.2f}",
        "--output-format", "stream-json",
        "--verbose",
    ]
    if model:
        argv += ["--model", model]
    return argv


def _extract_payload(text: str) -> dict | None:
    """从 result.result 里剥出 JSON payload。

    上游契约是「只输出单个 JSON 代码块，不加任何解释」：闭合 fence 之后如果
    还有说明文字，说明模型违反了契约，必须判失败，而不是静默剥壳成功。
    顶层必须是对象且含 `candidates: list`——数组或缺字段的对象都不再被
    无条件包装/放行。
    """
    blob = (text or "").strip()
    if blob.startswith("```"):
        first_newline = blob.find("\n")
        last_fence = blob.rfind("```")
        if first_newline == -1 or last_fence <= first_newline:
            return None
        trailing = blob[last_fence + 3:].strip()
        if trailing:
            # 闭合 fence 之后还有内容：不是「单个代码块，不加解释」
            return None
        blob = blob[first_newline + 1:last_fence].strip()
    if not blob.startswith(("{", "[")):
        return None
    try:
        data = json.loads(blob)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict):
        return None
    candidates = data.get("candidates")
    if not isinstance(candidates, list):
        return None
    return data


def _coerce_finite_nonneg_float(value, field_name: str,
                                protocol_errors: list[str]) -> float | None:
    if value is None:
        protocol_errors.append(f"missing {field_name}")
        return None
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        protocol_errors.append(f"non-numeric {field_name}: {value!r}")
        return None
    if not math.isfinite(parsed) or parsed < 0:
        protocol_errors.append(f"invalid {field_name}: {value!r}")
        return None
    return parsed


def _coerce_nonneg_int(value, field_name: str,
                       protocol_errors: list[str]) -> int | None:
    if value is None:
        protocol_errors.append(f"missing {field_name}")
        return None
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        protocol_errors.append(f"non-numeric {field_name}: {value!r}")
        return None
    if parsed < 0:
        protocol_errors.append(f"invalid {field_name}: {value!r}")
        return None
    return parsed


def _parse_terminal_result(event: dict,
                           protocol_errors: list[str]) -> tuple[float, int, bool, dict | None]:
    """解析单个 terminal result 事件，返回 (cost, turns, ok, payload)。

    cost/turns 校验独立于 subtype：即便是 error_max_turns 之类的失败事件，
    预算账本仍要记它花了多少钱、跑了多少轮，所以字段本身必须合法，只是
    ok 恒为 False。只有 subtype == success 且 payload 可解析时 ok 才可能为
    True。
    """
    cost = _coerce_finite_nonneg_float(event.get("total_cost_usd"),
                                       "total_cost_usd", protocol_errors)
    turns = _coerce_nonneg_int(event.get("num_turns"), "num_turns",
                              protocol_errors)
    if cost is None or turns is None:
        return 0.0, 0, False, None
    if event.get("subtype") != "success":
        return cost, turns, False, None
    payload = _extract_payload(event.get("result", ""))
    if payload is None:
        protocol_errors.append("unparseable or malformed payload in success result")
        return cost, turns, False, None
    return cost, turns, True, payload


def parse_stream_json(lines) -> InvocationResult:
    cost, turns = 0.0, 0
    payload: dict | None = None
    result_ok = False
    init_seen = False
    init_count = 0
    result_count = 0
    init_tools: list[str] = []
    init_mcp: list = []
    init_plugins: list = []
    init_errors: list = []
    tail: list[str] = []
    protocol_errors: list[str] = []

    for line in lines:
        line = line.strip()
        if not line:
            continue
        tail.append(line)
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            # 非法行不再静默忽略：截断/代理错误页/损坏行都会在这里留痕，
            # 即便前面已经有一个看起来成功的 result 也不得据此判 ok=True。
            protocol_errors.append(f"unparseable stream line: {line[:200]!r}")
            continue

        if event.get("type") == "system" and event.get("subtype") == "init":
            init_count += 1
            if init_count == 1:
                init_seen = True
                init_tools = event.get("tools", [])
                init_mcp = event.get("mcp_servers", [])
                init_plugins = event.get("plugins", [])
                init_errors = (event.get("plugin_errors", [])
                               + event.get("mcp_server_errors", []))
            # 第二次及以后的 init：不覆盖已记录的（可能更危险的）第一次结果，
            # 后面统一记为协议错误——一个"更干净"的后续 init 不得掩盖此前
            # 那次的真实能力集。
        elif event.get("type") == "result":
            result_count += 1
            if result_count == 1:
                cost, turns, result_ok, payload = _parse_terminal_result(
                    event, protocol_errors)
            # 第二个及以后的 terminal result：无论 success 还是 error，都不
            # 得让它把 ok/payload 重新粘回去或维持旧值——见下方 init_count/
            # result_count 汇总校验。

    if init_count == 0:
        protocol_errors.append("missing init event")
    elif init_count > 1:
        protocol_errors.append(f"duplicate init events: {init_count}")
    if result_count == 0:
        protocol_errors.append("missing terminal result event")
    elif result_count > 1:
        protocol_errors.append(f"duplicate terminal result events: {result_count}")

    ok = result_ok and init_seen and not protocol_errors

    return InvocationResult(ok=ok, payload=payload, cost_usd=cost, turns=turns,
                            raw_tail="\n".join(tail[-5:]), init_seen=init_seen,
                            init_tools=init_tools, init_mcp_servers=init_mcp,
                            init_plugins=init_plugins, init_errors=init_errors,
                            protocol_errors=protocol_errors)


def _sanitize_env(env: dict) -> dict:
    """从传入环境出发删凭据，保留 HOME/PATH 等 claude 运行所需的变量。"""
    safe_env = dict(env)
    for key in _CREDENTIAL_ENV_VARS:
        safe_env.pop(key, None)
    for key in list(safe_env):
        if key == "GIT_CONFIG_COUNT" or key.startswith(_GIT_CONFIG_INDEXED_PREFIXES):
            safe_env.pop(key, None)
    safe_env["GIT_TERMINAL_PROMPT"] = "0"
    # 禁止 git 读取系统级 gitconfig（credential.helper 等可能藏在这里）。
    safe_env["GIT_CONFIG_NOSYSTEM"] = "1"
    return safe_env


def invoke(prompt: str, tools: str, grant_usd: float, max_turns: int,
           settings_path: str, cwd: str, timeout_s: float,
           env: dict | None = None, model: str | None = None,
           runner: Callable[..., subprocess.CompletedProcess] = subprocess.run
           ) -> InvocationResult:
    argv = build_argv(prompt, tools, grant_usd, max_turns, settings_path,
                      model=model)
    # 从完整环境出发再删凭据：只给 GIT_TERMINAL_PROMPT 会丢掉 HOME/PATH 等
    # claude 运行所必需的变量（评审 C-06）
    safe_env = _sanitize_env(env if env is not None else os.environ)
    try:
        proc = runner(argv, cwd=cwd, capture_output=True, text=True,
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
