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
import pathlib
import subprocess
import sys
import uuid
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
# 后台 workflow 等待上限（毫秒）：需大于单次 invocation 的 timeout_s
BG_WAIT_CEILING_MS = 1_200_000

# 无人值守 agent 的模型必须钉死为**规范 ID**，不能用 `sonnet` 这类别名：别名
# 的解析会被父进程的 ANTHROPIC_MODEL 改写。真机实测（2026-07-31）：交互会话里
# `ANTHROPIC_MODEL=opus[1m]` 使 `--model sonnet` 解析成 `sonnet[1m]`——同一句
# "Reply with exactly: OK" 花 $0.1918，而 `--model claude-sonnet-5` 只花
# $0.1439。模型档位由环境决定，成本与行为就都不可预测。
DEFAULT_AGENT_MODEL = "claude-sonnet-5"

# 本模块**自己**要设进子进程环境的 CLAUDE_*/ANTHROPIC_* 变量。除这些之外，
# 父进程的同前缀变量一律清除（见 _sanitize_env）。
_HARNESS_OWNED_CLAUDE_ENV = frozenset({
    "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS",
})

# 模型 API 认证通道：deny-by-default 会把它们一并清掉，子进程随即报
# `Not logged in · Please run /login`（实测 2026-07-31，apiKeySource=none、
# 成本 0、零副作用——fail closed 正确，但轮次跑不起来）。因此必须**显式**放行。
# 写成白名单而不是"少删一点"，是为了让「headless agent 的认证从哪来」这件事
# 在代码里可见：生产环境由 systemd unit 的 EnvironmentFile 提供，而不是碰巧
# 从某个交互式 shell 继承。
#
# 注意 ANTHROPIC_BASE_URL 同时也是一条重定向通道——放行它意味着启动者能改变
# agent 的流量去向。Stage 1 接受这一点（agent 本来就以该身份运行），前提是
# 启动环境受控；若将来要跑在不受控启动环境下，这一条要改成硬编码常量。
_INHERITED_AUTH_ENV = (
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
)

# TaskOutput 是打通 `claude -p` + Workflow 的关键：Workflow 工具**总是**立刻返回
# 一个 run/task ID 并转入后台（工具文档原文："Workflows run in the background —
# this tool returns immediately"）。而 headless 会话在模型结束回合后即退出，后台
# 任务随即被 stopped。真机实测两次（2026-07-31）：模型如实宣布"我会等待完成通知
# 后再取结果"，随后 stop_reason=end_turn，任务被杀。这不是提示词能修的——模型在
# -p 模式下没有"跨回合等待"这个动作。
# TaskOutput(block=True) 让等待变成**同一回合内的一次工具调用**，模型不必结束回合。
# 它是只读工具（只取已有任务的输出），不扩大写能力边界。
STAGE1_ALLOWED_TOOLS = frozenset({"Read", "Grep", "Glob", "Skill", "Workflow",
                                  "TaskOutput"})


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


def _validate_session_identity(session_id: str | None, resume: str | None,
                               fork_session: bool) -> None:
    if session_id is not None and resume is not None:
        raise UnsafeInvocationError("session_id 与 resume 互斥")
    if fork_session and resume is None:
        raise UnsafeInvocationError("fork_session=True 时必须提供 resume")
    for name, value in (("session_id", session_id), ("resume", resume)):
        if value is None:
            continue
        if not isinstance(value, str):
            raise UnsafeInvocationError(f"{name} 必须是 UUID 字符串，实际 {value!r}")
        try:
            uuid.UUID(value)
        except ValueError:
            raise UnsafeInvocationError(
                f"{name} 必须是 UUID 字符串，实际 {value!r}") from None


@dataclass
class InvocationResult:
    ok: bool
    payload: dict | None
    cost_usd: float
    turns: int
    denials: int = 0
    exit_code: int = 0
    raw_tail: str = ""
    # 终态 result 事件里解析到了合法的 total_cost_usd —— 这是「成本已知」的
    # **显式** oracle。不能拿 `turns > 0` 或 `cost > 0` 反推：前者是另一个字段，
    # 后者分不清「真的花了 0」与「没解析到」。失败轮该按实测成本还是按预留满额
    # 计费，取决于这一位（评审 rmf-05）。
    cost_known: bool = False
    session_id: str | None = None
    subtype: str | None = None
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
               settings_path: str, model: str | None = None,
               session_id: str | None = None, resume: str | None = None,
               fork_session: bool = False) -> list[str]:
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
    _validate_session_identity(session_id, resume, fork_session)
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
    if session_id is not None:
        argv += ["--session-id", session_id]
    if resume is not None:
        argv += ["--resume", resume]
    if fork_session:
        argv.append("--fork-session")
    return argv


def _strip_fence_and_parse(text: str) -> object | None:
    """剥离可选 JSON fence，并拒绝 fence 后的额外文字。"""
    blob = (text or "").strip()
    if blob.startswith("```"):
        first_newline = blob.find("\n")
        last_fence = blob.rfind("```")
        if first_newline == -1 or last_fence <= first_newline:
            return None
        trailing = blob[last_fence + 3:].strip()
        if trailing:
            return None
        blob = blob[first_newline + 1:last_fence].strip()
    if not blob.startswith(("{", "[")):
        return None
    try:
        return json.loads(blob)
    except json.JSONDecodeError:
        return None


def _extract_payload(text: str) -> dict | None:
    """解析 finder payload；顶层须为含 `candidates: list` 的对象。"""
    data = _strip_fence_and_parse(text)
    if not isinstance(data, dict):
        return None
    candidates = data.get("candidates")
    if not isinstance(candidates, list):
        return None
    return data


def _extract_json_object(text: str) -> dict | None:
    """解析 judge payload；字段级约束由 fanout_schema 负责。"""
    data = _strip_fence_and_parse(text)
    return data if isinstance(data, dict) else None


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


def _parse_terminal_result(event: dict, protocol_errors: list[str],
                           payload_parser: Callable[[str], dict | None]
                           ) -> tuple[float, int, bool, dict | None, bool, str | None]:
    """解析终态事件，返回 (cost, turns, ok, payload, cost_known, subtype)。

    cost/turns 校验独立于 subtype：即便是 error_max_turns 之类的失败事件，
    预算账本仍要记它花了多少钱、跑了多少轮，所以字段本身必须合法，只是
    ok 恒为 False。只有 subtype == success 且 payload 可解析时 ok 才可能为
    True。
    """
    subtype = event.get("subtype")
    cost = _coerce_finite_nonneg_float(event.get("total_cost_usd"),
                                       "total_cost_usd", protocol_errors)
    turns = _coerce_nonneg_int(event.get("num_turns"), "num_turns",
                              protocol_errors)
    if cost is None or turns is None:
        return 0.0, 0, False, None, False, subtype
    if subtype != "success":
        return cost, turns, False, None, True, subtype
    payload = payload_parser(event.get("result", ""))
    if payload is None:
        protocol_errors.append("unparseable or malformed payload in success result")
        return cost, turns, False, None, True, subtype
    return cost, turns, True, payload, True, subtype


def parse_stream_json(lines, *,
                      payload_parser: Callable[[str], dict | None] = _extract_payload
                      ) -> InvocationResult:
    cost, turns = 0.0, 0
    payload: dict | None = None
    result_ok = False
    init_seen = False
    cost_known = False
    session_id: str | None = None
    subtype: str | None = None
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
                session_id = event.get("session_id")
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
                cost, turns, result_ok, payload, cost_known, subtype = (
                    _parse_terminal_result(event, protocol_errors,
                                           payload_parser))
                if session_id is None:
                    session_id = event.get("session_id")
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
                            cost_known=cost_known, session_id=session_id,
                            subtype=subtype,
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
    # 父会话的 CLAUDE_*/ANTHROPIC_* 控制变量一律清除，再由本模块显式设回需要
    # 的那几个。这里必须是**前缀级 deny-by-default**，不能列黑名单：真机实测
    # （2026-07-31）里泄漏进来的是 `ANTHROPIC_MODEL=opus[1m]`（把模型换成溢价
    # 档位）与 `CLAUDE_CODE_ENABLE_TASKS=0`（禁掉后台任务基础设施，多 agent
    # workflow 一起就被 kill），两者都不在任何预想的黑名单上。无人值守 agent
    # 拿到什么模型、有什么运行时能力，不能取决于「谁启动了它」。
    for key in list(safe_env):
        if (key.startswith(("ANTHROPIC_", "CLAUDE_"))
                and key not in _HARNESS_OWNED_CLAUDE_ENV
                and key not in _INHERITED_AUTH_ENV):
            safe_env.pop(key, None)
    safe_env["GIT_TERMINAL_PROMPT"] = "0"
    # 真机实测（2026-07-31 首轮）：workflow 起 4 个并行 finder 时，Workflow
    # 工具走"后台启动"路径并立刻返回 run ID；外层模型宣布"等完成再输出"后
    # **结束了本轮**，会话一退后台任务即被 kill，round 报 invocation-failed。
    # 契约探针当时没暴露这点，因为它只起一个 agent、很快就同步返回。
    # 显式抬高 print 模式的后台等待上限，使 claude -p 真正等到 workflow 收敛。
    # 该值必须大于单次 invocation 的 timeout_s，否则先被这里截断。
    safe_env["CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS"] = str(BG_WAIT_CEILING_MS)
    # 禁止 git 读取系统级 gitconfig（credential.helper 等可能藏在这里）。
    safe_env["GIT_CONFIG_NOSYSTEM"] = "1"
    return safe_env


def _persist_stream(stream_log, stdout: str, stderr: str) -> None:
    """把完整 stream 落盘供事后判因。

    真机实测（2026-07-31）：一轮 $10 的 round 报 `invocation-failed`，而进程只
    保留了 5 行 `raw_tail`——无从判断是 payload 提取失败、协议异常还是预算耗尽。
    钱花了，诊断依据没有。`raw_tail` 是给告警看的摘要，不能当作事后取证的全部。

    落盘失败**不得**影响本轮结论：诊断信息缺失是遗憾，把一轮本来成功的调用变成
    异常则是事故。因此这里吞掉 IO 错误，但写进 stderr 让它可见，而不是静默。
    """
    if stream_log is None:
        return
    tmp_path = None
    try:
        path = pathlib.Path(stream_log)
        path.parent.mkdir(parents=True, exist_ok=True)
        # 写同目录临时文件再原子 replace，**不是**直接打开目标路径（评审
        # cfr-p12-merged-02）：`os.open(..., O_CREAT|O_TRUNC, 0o600)` 的 mode
        # 只在**首次创建**时生效，打开一个**已存在**的文件时 mode 被完全忽略、
        # 权限保持原样。而 stream 路径按 round/role/attempt 确定性生成，重跑
        # 同一轮、崩溃恢复、上次留下的残留文件都会命中同一路径——届时新的敏感
        # stream 会写进一个 0644 的 inode，而只覆盖新文件的权限测试仍然全绿。
        #
        # 也**不能**改用「写完再 chmod」：那会重新引入从默认权限到 chmod 生效
        # 之间的窗口，正是 rmf-08 修掉的东西。临时文件由本进程新建，mode 必然
        # 生效；`os.replace` 是原子的，且保留新 inode 的权限位。
        tmp_path = path.with_name(f".{path.name}.{os.getpid()}.tmp")
        fd = os.open(str(tmp_path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(stdout or "")
            if stderr:
                fh.write("\n===== stderr =====\n")
                fh.write(stderr)
        os.replace(str(tmp_path), str(path))
        tmp_path = None
    except OSError as exc:
        print(f"harness: 无法写入 stream 日志 {stream_log}: {exc}",
              file=sys.stderr)
    finally:
        # 中途失败时不留半截临时文件；清理本身失败也不得影响本轮结论。
        if tmp_path is not None:
            try:
                os.unlink(str(tmp_path))
            except OSError:
                pass


def invoke(prompt: str, tools: str, grant_usd: float, max_turns: int,
           settings_path: str, cwd: str, timeout_s: float,
           env: dict | None = None, model: str | None = None,
           runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
           stream_log=None, session_id: str | None = None,
           resume: str | None = None, fork_session: bool = False,
           payload_parser: Callable[[str], dict | None] = _extract_payload
           ) -> InvocationResult:
    argv = build_argv(prompt, tools, grant_usd, max_turns, settings_path,
                      model=model, session_id=session_id, resume=resume,
                      fork_session=fork_session)
    # 从完整环境出发再删凭据：只给 GIT_TERMINAL_PROMPT 会丢掉 HOME/PATH 等
    # claude 运行所必需的变量（评审 C-06）
    safe_env = _sanitize_env(env if env is not None else os.environ)
    try:
        proc = runner(argv, cwd=cwd, capture_output=True, text=True,
                     timeout=timeout_s, env=safe_env)
    except subprocess.TimeoutExpired as exc:
        # 超时是最需要事后判因的情形，不能反而什么都不留：把 claude 已经吐出的
        # 部分 stream 一并落盘。
        partial_out = exc.output or ""
        partial_err = exc.stderr or ""
        if isinstance(partial_out, bytes):
            partial_out = partial_out.decode("utf-8", "replace")
        if isinstance(partial_err, bytes):
            partial_err = partial_err.decode("utf-8", "replace")
        _persist_stream(stream_log, partial_out, partial_err)
        return InvocationResult(False, None, 0.0, 0, exit_code=124,
                                raw_tail=str(exc)[-500:])
    _persist_stream(stream_log, proc.stdout, proc.stderr)
    result = parse_stream_json(proc.stdout.splitlines(),
                               payload_parser=payload_parser)
    result.exit_code = proc.returncode
    if proc.returncode != 0:
        # 退出码非 0 时即便 stdout 里恰好有 success result 也不得判 ok
        result.ok = False
        if not result.raw_tail:
            result.raw_tail = proc.stderr[-500:]
    return result
