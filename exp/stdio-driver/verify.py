#!/home/linuxbrew/.linuxbrew/bin/python3
"""Offline verification gates for the captured stdio-driver evidence."""

from __future__ import annotations

import json
from pathlib import Path

ROOT = Path("/home/xp/src/zipfs/exp/stdio-driver")


def events(path: str) -> list[dict]:
    return [json.loads(line) for line in Path(path).read_bytes().splitlines() if line.strip()]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def main() -> None:
    basic_dir = ROOT / "artifacts/run-20260731T103554.216711Z-basic/basic-multiturn"
    basic = events(str(basic_dir / "wire.out.bin"))
    basic_results = [e for e in basic if e.get("type") == "result"]
    require([e.get("result") for e in basic_results] == ["FIRST", "CODE:MANGO"], "basic results/context mismatch")
    require(len([e for e in basic if e.get("type") == "user"]) == 2, "replayed user count mismatch")
    require(len({e.get("session_id") for e in basic_results}) == 1, "basic session changed")
    require(basic_dir.joinpath("wire.in.bin").read_bytes().count(b"\n") == 2, "basic input is not two JSONL records")

    safe = events(str(ROOT / "artifacts/run-20260731T103643.201557Z-safe-bash/safe-bash/wire.out.bin"))
    require(not any(e.get("type") == "control_request" for e in safe), "safe Bash unexpectedly gated")
    require(any(e.get("type") == "result" and e.get("result") == "SAFE_DONE" for e in safe), "safe Bash result missing")

    deny = events(str(ROOT / "artifacts/run-20260731T103812.032330Z-permission-deny/permission-deny/wire.out.bin"))
    deny_req = next(e for e in deny if e.get("type") == "control_request")
    deny_resp = next(e for e in deny if e.get("type") == "control_response")
    require(deny_req["request"]["subtype"] == "can_use_tool", "deny request is not can_use_tool")
    require(deny_resp["response"]["request_id"] == deny_req["request_id"], "deny request_id mismatch")
    require(deny_resp["response"]["response"]["behavior"] == "deny", "deny behavior mismatch")
    require(not Path("/tmp/stdio-driver-deny-marker.txt").exists(), "denied file exists")

    rewrite = events(str(ROOT / "artifacts/run-20260731T103823.675318Z-permission-rewrite/permission-rewrite/wire.out.bin"))
    rewrite_req = next(e for e in rewrite if e.get("type") == "control_request")
    rewrite_resp = next(e for e in rewrite if e.get("type") == "control_response")
    updated = rewrite_resp["response"]["response"]["updatedInput"]
    require(rewrite_req["request"]["input"]["file_path"] == "/tmp/stdio-driver-rewrite-original.txt", "original rewrite path mismatch")
    require(updated["file_path"] == str(ROOT / "rewrite-effective.txt"), "effective rewrite path mismatch")
    require(not Path("/tmp/stdio-driver-rewrite-original.txt").exists(), "original rewrite target exists")
    require(ROOT.joinpath("rewrite-effective.txt").read_bytes() == b"REWRITTEN", "rewritten bytes mismatch")

    retry_root = ROOT / "artifacts/run-20260731T104743.480383Z-retry"
    failed = events(str(retry_root / "retry-failed-attempt/wire.out.bin"))
    sibling = events(str(retry_root / "retry-sibling/wire.out.bin"))
    retried = events(str(retry_root / "retry-fork/wire.out.bin"))
    failed_result = next(e for e in failed if e.get("type") == "result")
    sibling_result = next(e for e in sibling if e.get("type") == "result")
    retry_result = next(e for e in retried if e.get("type") == "result")
    retry_init = next(e for e in retried if e.get("type") == "system" and e.get("subtype") == "init")
    require(failed_result.get("terminal_reason") == "aborted_streaming", "failed session not interrupted")
    require(sibling_result.get("result") == "SIBLING_OK", "sibling was affected")
    require(retry_result.get("result") == "RETRY:PEAR", "fork did not retain context")
    require(retry_result.get("session_id") == retry_init.get("session_id"), "fork init/result session mismatch")
    require(retry_result.get("session_id") != failed_result.get("session_id"), "fork reused failed session ID")

    agent_main = events(str(ROOT / "artifacts/run-20260731T104216.079069Z-agents/inline-agent/wire.out.bin"))
    init = next(e for e in agent_main if e.get("type") == "system" and e.get("subtype") == "init")
    require("poc-inline" in init.get("agents", []), "inline agent missing from init")
    task_use = next(
        block
        for event in agent_main
        for block in event.get("message", {}).get("content", [])
        if event.get("type") == "assistant" and block.get("type") == "tool_use" and block.get("name") == "Task"
    )
    require(task_use["input"]["subagent_type"] == "poc-inline", "wrong subagent invoked")
    require(any(e.get("type") == "system" and e.get("subtype") == "task_notification" and e.get("summary") == "INLINE_AGENT_OK" for e in agent_main), "inline agent completion missing")
    main_results = [e for e in agent_main if e.get("type") == "result"]
    require(main_results[0].get("result") == "PARENT_DONE", "inline agent parent result missing")
    require(len(main_results) >= 2 and main_results[1].get("result") == "PARENT_DONE", "background Task duplicate-success counterexample missing")

    agent_interrupt = events(str(ROOT / "artifacts/run-20260731T105042.256411Z-agents/inline-agent/wire.out.bin"))
    interrupt_results = [e for e in agent_interrupt if e.get("type") == "result"]
    require(len(interrupt_results) >= 2, "background Task interrupt counterexample missing")
    require(interrupt_results[1].get("origin", {}).get("kind") == "task-notification", "second interrupt result lacks task-notification origin")

    print("verification=pass gates=23")


if __name__ == "__main__":
    main()
