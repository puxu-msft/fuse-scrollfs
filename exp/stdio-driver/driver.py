#!/home/linuxbrew/.linuxbrew/bin/python3
"""Drive real Claude Code stream-json sessions over stdin/stdout pipes.

The script uses only the Python standard library. It records exact stdin/stdout
bytes under artifacts/ and removes GitHub credentials from child environments.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import queue
import select
import subprocess
import sys
import threading
import time
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

ROOT = Path("/home/xp/src/zipfs/exp/stdio-driver")
ARTIFACTS = ROOT / "artifacts"
CLAUDE = "/home/xp/.local/bin/claude"
DEFAULT_TIMEOUT = 180.0


def compact_json(value: Any) -> bytes:
    return (json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")


def user_turn(text: str) -> dict[str, Any]:
    return {
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}],
        },
    }


def control_response(request_id: str, inner: dict[str, Any]) -> dict[str, Any]:
    return {
        "type": "control_response",
        "response": {
            "request_id": request_id,
            "subtype": "success",
            "response": inner,
        },
    }


def child_env() -> dict[str, str]:
    env = dict(os.environ)
    for name in (
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_PID",
    ):
        env.pop(name, None)
    return env


def utc_stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")


@dataclass
class Invocation:
    name: str
    max_budget: float
    tools: str = ""
    session_id: str | None = None
    resume: str | None = None
    fork_session: bool = False
    agents: dict[str, Any] | None = None
    replay_user_messages: bool = True
    extra_args: list[str] = field(default_factory=list)
    artifact_dir: Path | None = None
    proc: subprocess.Popen[bytes] | None = field(init=False, default=None)
    events: list[dict[str, Any]] = field(init=False, default_factory=list)
    results: list[dict[str, Any]] = field(init=False, default_factory=list)
    _stdout_queue: queue.Queue[bytes | None] = field(init=False, default_factory=queue.Queue)
    _stdout_file: Any = field(init=False, default=None)
    _stderr_file: Any = field(init=False, default=None)
    _stdin_file: Any = field(init=False, default=None)
    _threads: list[threading.Thread] = field(init=False, default_factory=list)

    def argv(self) -> list[str]:
        argv = [
            CLAUDE,
            "--print",
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--max-budget-usd",
            f"{self.max_budget:.6f}",
            "--tools",
            self.tools,
            "--setting-sources",
            "",
        ]
        if self.replay_user_messages:
            argv.append("--replay-user-messages")
        if self.session_id:
            argv.extend(["--session-id", self.session_id])
        if self.resume:
            argv.extend(["--resume", self.resume])
        if self.fork_session:
            argv.append("--fork-session")
        if self.agents is not None:
            argv.extend(["--agents", json.dumps(self.agents, separators=(",", ":"))])
        argv.extend(self.extra_args)
        return argv

    def start(self) -> "Invocation":
        if self.artifact_dir is None:
            self.artifact_dir = ARTIFACTS / f"{utc_stamp()}-{self.name}"
        self.artifact_dir.mkdir(parents=True, exist_ok=False)
        self._stdin_file = (self.artifact_dir / "wire.in.bin").open("wb")
        self._stdout_file = (self.artifact_dir / "wire.out.bin").open("wb")
        self._stderr_file = (self.artifact_dir / "stderr.bin").open("wb")
        metadata = {
            "name": self.name,
            "started_at": datetime.now(timezone.utc).isoformat(),
            "argv": self.argv(),
            "cwd": "/tmp",
            "removed_env_names": [
                "GH_TOKEN",
                "GITHUB_TOKEN",
                "CLAUDECODE",
                "CLAUDE_CODE_CHILD_SESSION",
                "CLAUDE_CODE_ENTRYPOINT",
                "CLAUDE_CODE_SESSION_ID",
                "CLAUDE_PID",
            ],
        }
        (self.artifact_dir / "metadata.json").write_text(
            json.dumps(metadata, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        self.proc = subprocess.Popen(
            self.argv(),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd="/tmp",
            env=child_env(),
            bufsize=0,
        )
        self._threads = [
            threading.Thread(target=self._pump_stdout, name=f"{self.name}-stdout", daemon=True),
            threading.Thread(target=self._pump_stderr, name=f"{self.name}-stderr", daemon=True),
        ]
        for thread in self._threads:
            thread.start()
        return self

    def _pump_stdout(self) -> None:
        assert self.proc is not None and self.proc.stdout is not None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                break
            self._stdout_file.write(line)
            self._stdout_file.flush()
            with (self.artifact_dir / "received-index.jsonl").open("ab") as index:
                index.write(compact_json({
                    "received_at": datetime.now(timezone.utc).isoformat(),
                    "length": len(line),
                    "base64": base64.b64encode(line).decode("ascii"),
                }))
            self._stdout_queue.put(line)
        self._stdout_queue.put(None)

    def _pump_stderr(self) -> None:
        assert self.proc is not None and self.proc.stderr is not None
        while True:
            chunk = self.proc.stderr.read(65536)
            if not chunk:
                break
            self._stderr_file.write(chunk)
            self._stderr_file.flush()

    def send(self, value: dict[str, Any]) -> bytes:
        assert self.proc is not None and self.proc.stdin is not None
        payload = compact_json(value)
        self._stdin_file.write(payload)
        self._stdin_file.flush()
        with (self.artifact_dir / "sent-index.jsonl").open("ab") as index:
            index.write(compact_json({
                "sent_at": datetime.now(timezone.utc).isoformat(),
                "length": len(payload),
                "base64": base64.b64encode(payload).decode("ascii"),
                "utf8_repr": repr(payload.decode("utf-8")),
            }))
        self.proc.stdin.write(payload)
        self.proc.stdin.flush()
        return payload

    def _record_event(self, raw: bytes) -> dict[str, Any] | None:
        try:
            event = json.loads(raw)
        except json.JSONDecodeError:
            return None
        self.events.append(event)
        if event.get("type") == "result":
            self.results.append(event)
        return event

    def wait_event(
        self,
        predicate: Callable[[dict[str, Any]], bool],
        on_control: Callable[[dict[str, Any]], dict[str, Any] | None] | None = None,
        timeout: float = DEFAULT_TIMEOUT,
    ) -> dict[str, Any]:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{self.name}: matching event not seen in {timeout}s")
            try:
                raw = self._stdout_queue.get(timeout=remaining)
            except queue.Empty as exc:
                raise TimeoutError(f"{self.name}: no stdout in {timeout}s") from exc
            if raw is None:
                code = self.proc.poll() if self.proc else None
                raise RuntimeError(f"{self.name}: stdout closed before matching event; exit={code}")
            event = self._record_event(raw)
            if event is None:
                continue
            if event.get("type") == "control_request" and on_control is not None:
                answer = on_control(event)
                if answer is not None:
                    self.send(answer)
            if predicate(event):
                return event

    def wait_result(
        self,
        on_control: Callable[[dict[str, Any]], dict[str, Any] | None] | None = None,
        timeout: float = DEFAULT_TIMEOUT,
    ) -> dict[str, Any]:
        return self.wait_event(lambda event: event.get("type") == "result", on_control=on_control, timeout=timeout)

    def wait_control_request(
        self,
        subtype: str = "can_use_tool",
        timeout: float = DEFAULT_TIMEOUT,
    ) -> dict[str, Any]:
        return self.wait_event(
            lambda event: event.get("type") == "control_request" and event.get("request", {}).get("subtype") == subtype,
            timeout=timeout,
        )

    def interrupt(self, timeout: float = 30.0) -> tuple[dict[str, Any], dict[str, Any]]:
        interrupt_id = str(uuid.uuid4())
        request = {"type": "control_request", "request_id": interrupt_id, "request": {"subtype": "interrupt"}}
        self.send(request)
        response = self.wait_event(
            lambda event: event.get("type") == "control_response" and event.get("response", {}).get("request_id") == interrupt_id,
            timeout=timeout,
        )
        return request, response

    def drain_available(self, quiet_period: float = 0.5, max_wait: float = 10.0) -> None:
        deadline = time.monotonic() + max_wait
        quiet_deadline = time.monotonic() + quiet_period
        while time.monotonic() < deadline:
            timeout = max(0.0, min(quiet_deadline - time.monotonic(), deadline - time.monotonic()))
            if timeout <= 0:
                return
            try:
                raw = self._stdout_queue.get(timeout=timeout)
            except queue.Empty:
                return
            if raw is None:
                return
            event = self._record_event(raw)
            if event is None:
                quiet_deadline = time.monotonic() + quiet_period
                continue
            quiet_deadline = time.monotonic() + quiet_period

    def alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def finish(self, close_stdin: bool = True, timeout: float = 30.0, interrupt_before_close: bool = False) -> int:
        assert self.proc is not None
        interrupt_exchange = None
        if interrupt_before_close and close_stdin and self.alive() and self.proc.stdin is not None and not self.proc.stdin.closed:
            try:
                interrupt_exchange = self.interrupt(timeout=5.0)
            except (TimeoutError, RuntimeError):
                interrupt_exchange = None
        if close_stdin and self.proc.stdin is not None and not self.proc.stdin.closed:
            self.proc.stdin.close()
        try:
            code = self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            try:
                code = self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                code = self.proc.wait(timeout=5)
        for thread in self._threads:
            thread.join(timeout=2)
        while True:
            try:
                raw = self._stdout_queue.get_nowait()
            except queue.Empty:
                break
            if raw is not None:
                self._record_event(raw)
        summary = {
            "finished_at": datetime.now(timezone.utc).isoformat(),
            "exit_code": code,
            "finish_interrupt_exchange": interrupt_exchange,
            "result_count": len(self.results),
            "results": self.results,
            "event_type_counts": {},
        }
        for event in self.events:
            kind = str(event.get("type"))
            summary["event_type_counts"][kind] = summary["event_type_counts"].get(kind, 0) + 1
        (self.artifact_dir / "summary.json").write_text(
            json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        for handle in (self._stdin_file, self._stdout_file, self._stderr_file):
            if handle is not None:
                handle.close()
        return code


def run_basic(root: Path) -> dict[str, Any]:
    inv = Invocation("basic-multiturn", 0.30, tools="", artifact_dir=root / "basic-multiturn").start()
    sent1 = inv.send(user_turn("Remember codeword MANGO. Reply exactly FIRST."))
    result1 = inv.wait_result()
    alive1 = inv.alive()
    sent2 = inv.send(user_turn("What codeword did I tell you? Reply exactly CODE:<word>."))
    result2 = inv.wait_result()
    alive2 = inv.alive()
    inv.finish(close_stdin=True)
    return {
        "artifact_dir": str(inv.artifact_dir),
        "sent": [sent1.decode(), sent2.decode()],
        "results": [result1, result2],
        "alive_after_each_result": [alive1, alive2],
        "result_event_count": sum(1 for e in inv.events if e.get("type") == "result"),
        "replayed_user_count": sum(1 for e in inv.events if e.get("type") == "user"),
    }


def run_safe_bash(root: Path) -> dict[str, Any]:
    inv = Invocation(
        "safe-bash", 0.30, tools="Bash", extra_args=["--permission-prompt-tool", "stdio"], artifact_dir=root / "safe-bash"
    ).start()
    sent = inv.send(user_turn("Use Bash once to run `printf SAFE_OK`. Then reply exactly SAFE_DONE."))
    result = inv.wait_result(on_control=lambda _: None)
    inv.finish()
    controls = [e for e in inv.events if e.get("type") == "control_request"]
    return {"artifact_dir": str(inv.artifact_dir), "sent": sent.decode(), "result": result, "control_requests": controls}


def run_permission_deny(root: Path) -> dict[str, Any]:
    target = Path("/tmp/stdio-driver-deny-marker.txt")
    if target.exists():
        raise RuntimeError(f"refusing to overwrite existing {target}")
    inv = Invocation(
        "permission-deny", 0.30, tools="Write", extra_args=["--permission-prompt-tool", "stdio"], artifact_dir=root / "permission-deny"
    ).start()
    prompt = f"Use the Write tool once with file_path {target} and content HELLO. If the tool is denied, reply exactly DENIED."
    sent = inv.send(user_turn(prompt))
    captured: list[dict[str, Any]] = []

    def deny(event: dict[str, Any]) -> dict[str, Any] | None:
        if event.get("request", {}).get("subtype") != "can_use_tool":
            return None
        captured.append(event)
        return control_response(event["request_id"], {"behavior": "deny", "message": "PoC deny"})

    result = inv.wait_result(on_control=deny)
    inv.finish()
    return {
        "artifact_dir": str(inv.artifact_dir),
        "sent": sent.decode(),
        "result": result,
        "captured": captured,
        "target_exists": target.exists(),
    }


def run_permission_rewrite(root: Path) -> dict[str, Any]:
    original = Path("/tmp/stdio-driver-rewrite-original.txt")
    effective = ROOT / "rewrite-effective.txt"
    for target in (original, effective):
        if target.exists():
            raise RuntimeError(f"refusing to overwrite existing {target}")
    inv = Invocation(
        "permission-rewrite", 0.30, tools="Write", extra_args=["--permission-prompt-tool", "stdio"], artifact_dir=root / "permission-rewrite"
    ).start()
    prompt = f"Call Write exactly once to create {original} with content ORIGINAL. Then report which path was written."
    sent = inv.send(user_turn(prompt))
    captured: list[dict[str, Any]] = []
    sent_responses: list[dict[str, Any]] = []

    def rewrite(event: dict[str, Any]) -> dict[str, Any] | None:
        if event.get("request", {}).get("subtype") != "can_use_tool":
            return None
        captured.append(event)
        answer = control_response(event["request_id"], {
            "behavior": "allow",
            "updatedInput": {"file_path": str(effective), "content": "REWRITTEN"},
        })
        sent_responses.append(answer)
        return answer

    result = inv.wait_result(on_control=rewrite)
    inv.finish()
    return {
        "artifact_dir": str(inv.artifact_dir),
        "sent": sent.decode(),
        "result": result,
        "captured": captured,
        "control_responses": sent_responses,
        "original_exists": original.exists(),
        "effective_exists": effective.exists(),
        "effective_content": effective.read_text(encoding="utf-8") if effective.exists() else None,
    }


def run_retry(root: Path) -> dict[str, Any]:
    failed_sid = str(uuid.uuid4())
    sibling_sid = str(uuid.uuid4())
    failed = Invocation(
        "retry-failed-attempt",
        0.30,
        tools="Write",
        session_id=failed_sid,
        extra_args=["--permission-prompt-tool", "stdio"],
        artifact_dir=root / "retry-failed-attempt",
    ).start()
    sibling = Invocation("retry-sibling", 0.30, tools="", session_id=sibling_sid, artifact_dir=root / "retry-sibling").start()
    failed.send(user_turn("Remember retry code PEAR. Use Write once to create /tmp/stdio-driver-interrupt.txt containing PEAR, then reply RETRY_FINISHED."))
    sibling.send(user_turn("Reply exactly SIBLING_OK."))
    outcomes: dict[str, Any] = {}

    def interrupt_failed() -> None:
        try:
            control = failed.wait_control_request(timeout=180)
            interrupt_request, interrupt_response = failed.interrupt(timeout=30)
            result = failed.wait_result(timeout=180)
            outcomes["failed"] = {
                "control_request": control,
                "interrupt_request": interrupt_request,
                "interrupt_response": interrupt_response,
                "result": result,
            }
        except Exception as exc:  # recorded as evidence, not swallowed
            outcomes["failed"] = {"exception": type(exc).__name__, "message": str(exc)}
        finally:
            outcomes["failed"]["artifact_dir"] = str(failed.artifact_dir)
            outcomes["failed"]["exit_code"] = failed.finish()

    def collect_sibling() -> None:
        try:
            outcomes["sibling"] = {"result": sibling.wait_result(timeout=180)}
        except Exception as exc:  # recorded as evidence, not swallowed
            outcomes["sibling"] = {"exception": type(exc).__name__, "message": str(exc)}
        finally:
            outcomes["sibling"]["artifact_dir"] = str(sibling.artifact_dir)
            outcomes["sibling"]["exit_code"] = sibling.finish()

    threads = [threading.Thread(target=interrupt_failed), threading.Thread(target=collect_sibling)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    retry = Invocation("retry-fork", 0.30, tools="", resume=failed_sid, fork_session=True, artifact_dir=root / "retry-fork").start()
    retry.send(user_turn("What retry code was in the interrupted task? Reply exactly RETRY:<code>."))
    retry_result = retry.wait_result(timeout=180)
    retry.finish()
    retry_init = next((e for e in retry.events if e.get("type") == "system" and e.get("subtype") == "init"), None)
    outcomes["retry"] = {
        "artifact_dir": str(retry.artifact_dir),
        "result": retry_result,
        "init": retry_init,
        "original_failed_session_id": failed_sid,
        "sibling_session_id": sibling_sid,
    }
    return outcomes


def run_agents(root: Path) -> dict[str, Any]:
    agents = {
        "poc-inline": {
            "description": "Returns the requested marker for the stdio PoC.",
            "prompt": "You are the inline PoC agent. When invoked, reply exactly INLINE_AGENT_OK.",
            "tools": [],
        }
    }
    inv = Invocation("inline-agent", 0.30, tools="Task", agents=agents, artifact_dir=root / "inline-agent").start()
    sent = inv.send(user_turn("Use the poc-inline agent exactly once. Ask it for its marker, then reply exactly PARENT_DONE."))
    result = inv.wait_result(timeout=240)
    inv.drain_available(quiet_period=1.5, max_wait=15.0)
    inv.finish(interrupt_before_close=True)
    agent_uses = []
    for event in inv.events:
        message = event.get("message")
        if not isinstance(message, dict):
            continue
        for block in message.get("content", []):
            if isinstance(block, dict) and block.get("type") == "tool_use" and block.get("name") in ("Agent", "Task"):
                agent_uses.append(block)
    return {
        "artifact_dir": str(inv.artifact_dir),
        "sent": sent.decode(),
        "result": result,
        "agent_tool_uses": agent_uses,
        "all_result_events": inv.results,
    }


SCENARIOS: dict[str, Callable[[Path], dict[str, Any]]] = {
    "basic": run_basic,
    "safe-bash": run_safe_bash,
    "permission-deny": run_permission_deny,
    "permission-rewrite": run_permission_rewrite,
    "retry": run_retry,
    "agents": run_agents,
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("scenario", choices=[*SCENARIOS, "all"])
    args = parser.parse_args()
    run_root = ARTIFACTS / f"run-{utc_stamp()}-{args.scenario}"
    run_root.mkdir(parents=True, exist_ok=False)
    chosen = list(SCENARIOS) if args.scenario == "all" else [args.scenario]
    aggregate: dict[str, Any] = {
        "run_root": str(run_root),
        "started_at": datetime.now(timezone.utc).isoformat(),
        "scenarios": {},
    }
    exit_code = 0
    for name in chosen:
        try:
            aggregate["scenarios"][name] = SCENARIOS[name](run_root)
        except Exception as exc:
            aggregate["scenarios"][name] = {"exception": type(exc).__name__, "message": str(exc)}
            exit_code = 1
            break
        finally:
            (run_root / "aggregate.json").write_text(
                json.dumps(aggregate, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
            )
    aggregate["finished_at"] = datetime.now(timezone.utc).isoformat()
    (run_root / "aggregate.json").write_text(
        json.dumps(aggregate, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(aggregate, ensure_ascii=False, indent=2))
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
