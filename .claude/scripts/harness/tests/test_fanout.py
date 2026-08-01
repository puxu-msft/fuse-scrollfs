import _thread
import sqlite3
import unittest
from dataclasses import asdict

from harness.claude_runner import InvocationResult, UnsafeInvocationError
from harness.fanout import dedupe_and_rank
from harness.role_invocation import RoleInvocationRequest


_C1 = {
    "title": "a",
    "goal": "g1",
    "invariant": "i1",
    "primary_path": "p1",
    "oracle": "o1",
    "priority": "T0",
    "size": "S",
    "lane": "roadmap",
}
_C2 = {
    "title": "b",
    "goal": "g2",
    "invariant": "i2",
    "primary_path": "p2",
    "oracle": "o2",
    "priority": "T2",
    "size": "M",
    "lane": "defect",
}
_C1_DUP = dict(_C1, title="a-dup")


class TestDedupeAndRank(unittest.TestCase):
    def test_dedupes_by_canonical_key_within_batch(self):
        result = dedupe_and_rank(
            [_C1, _C1_DUP, _C2], known_canonical_keys=set()
        )
        self.assertEqual(len(result), 2)

    def test_known_keys_from_previous_rounds_are_excluded(self):
        from harness.queue import canonical_key

        known = {
            canonical_key(
                _C1["goal"],
                _C1["invariant"],
                _C1["primary_path"],
                _C1["oracle"],
            )
        }
        result = dedupe_and_rank([_C1, _C2], known_canonical_keys=known)
        self.assertEqual([candidate["title"] for candidate in result], ["b"])

    def test_ranks_by_priority_then_size(self):
        low_priority_small = dict(_C2, priority="T3", size="S")
        high_priority_large = dict(_C1, priority="T0", size="L")
        result = dedupe_and_rank(
            [low_priority_small, high_priority_large],
            known_canonical_keys=set(),
        )
        self.assertEqual(result[0]["priority"], "T0")

    def test_blocked_lanes_excluded_before_ranking(self):
        result = dedupe_and_rank(
            [_C1, _C2],
            known_canonical_keys=set(),
            blocked_lanes=["roadmap"],
        )
        self.assertEqual([candidate["title"] for candidate in result], ["b"])

    def test_missing_title_or_oracle_dropped(self):
        bad = dict(_C2, title="")
        result = dedupe_and_rank([_C1, bad], known_canonical_keys=set())
        self.assertEqual(len(result), 1)


class TestNormalizeError(unittest.TestCase):
    def test_folds_hex_request_id(self):
        from harness.fanout import normalize_error

        first = normalize_error(
            "API Error: Server error mid-response. req_9f3a2b7c1d"
        )
        second = normalize_error(
            "API Error: Server error mid-response. req_11ee44aa99"
        )
        self.assertEqual(first, second)

    def test_folds_uuid_trace_id(self):
        from harness.fanout import normalize_error

        first = normalize_error(
            "...(trace 9f3a2b7c-1d4e-4f8a-9b2c-1234567890ab)"
        )
        second = normalize_error(
            "...(trace 0c8d51ea-7b62-4a19-8e30-0987654321fe)"
        )
        self.assertEqual(first, second)

    def test_does_not_fold_different_error_kinds(self):
        from harness.fanout import normalize_error

        schema_error = normalize_error("schema validation failed: candidates")
        transport_error = normalize_error(
            "API Error: Server error mid-response. req_9f3a2b7c1d"
        )
        self.assertNotEqual(schema_error, transport_error)

    def test_preserves_tail_difference_after_shared_prefix(self):
        from harness.fanout import normalize_error

        shared_prefix = "x" * 250
        body_error = normalize_error(
            shared_prefix + " MISSING body_md on candidate 1"
        )
        slug_error = normalize_error(
            shared_prefix + " MISSING slug on candidate 2"
        )
        self.assertNotEqual(body_error, slug_error)


class TestRecordDegraded(unittest.TestCase):
    def test_folds_same_role_same_error(self):
        from harness.fanout import record_degraded

        degraded = []
        record_degraded(
            degraded, role="finder:roadmap", error="e1", attempts=3
        )
        record_degraded(
            degraded, role="finder:roadmap", error="e1", attempts=3
        )
        self.assertEqual(len(degraded), 1)
        self.assertEqual(degraded[0]["occurrences"], 2)
        self.assertEqual(degraded[0]["attempts"], 6)

    def test_does_not_fold_different_roles(self):
        from harness.fanout import record_degraded

        degraded = []
        record_degraded(
            degraded, role="finder:roadmap", error="e1", attempts=3
        )
        record_degraded(
            degraded, role="judge:redline", error="e1", attempts=3
        )
        self.assertEqual(len(degraded), 2)

    def test_writes_agent_type_alias_for_round_describe_degraded(self):
        from harness.fanout import record_degraded

        degraded = []
        record_degraded(
            degraded, role="finder:roadmap", error="e1", attempts=3
        )
        self.assertEqual(degraded[0]["agentType"], "finder:roadmap")
        self.assertEqual(degraded[0]["agentType"], degraded[0]["role"])


class TestRunOneAttempt(unittest.TestCase):
    _REPORTED_SESSION = "33333333-3333-4333-8333-333333333333"
    _REQUESTED_SESSION = "11111111-1111-4111-8111-111111111111"
    _EXPECTED_TOOLS = frozenset({"Read", "Grep"})

    def _request(self, **overrides):
        values = {
            "role": "finder:roadmap",
            "prompt": "find candidates",
            "tools": "Grep,Read",
            "grant_usd": 0.5,
            "max_turns": 10,
            "settings_path": ".claude/harness-settings.json",
            "cwd": "/repo",
            "timeout_s": 60.0,
            "session_id": self._REQUESTED_SESSION,
        }
        values.update(overrides)
        return RoleInvocationRequest(**values)

    def _invocation(self, **overrides):
        values = {
            "ok": True,
            "payload": {"candidates": []},
            "cost_usd": 0.1,
            "turns": 2,
            "cost_known": True,
            "session_id": self._REPORTED_SESSION,
            "subtype": "success",
            "init_seen": True,
            "init_tools": sorted(self._EXPECTED_TOOLS),
        }
        values.update(overrides)
        return InvocationResult(**values)

    def _run(self, invocation, *, validate=lambda payload: [], **overrides):
        from harness.fanout import run_one_attempt

        seen = []

        def invoke_fn(request):
            seen.append(request)
            return invocation

        record = run_one_attempt(
            role="finder:roadmap",
            attempt=1,
            request=self._request(),
            invoke_fn=invoke_fn,
            validate=validate,
            expected_tools=self._EXPECTED_TOOLS,
            **overrides,
        )
        self.assertEqual(seen, [self._request()])
        return record

    def test_success_preserves_payload_and_reported_session(self):
        payload = {"candidates": [{"title": "candidate"}]}
        record = self._run(self._invocation(payload=payload))
        self.assertEqual(record.status, "success")
        self.assertEqual(record.session_id, self._REPORTED_SESSION)
        self.assertIs(record.payload, payload)
        self.assertFalse(record.retryable)
        self.assertTrue(record.resumable)
        self.assertEqual(record.subtype, "success")

    def test_true_timeout_is_retryable_and_not_resumable(self):
        record = self._run(
            self._invocation(
                ok=False,
                payload=None,
                session_id=None,
                subtype=None,
                protocol_errors=[],
                raw_tail="timed out",
            )
        )
        self.assertEqual(record.status, "failed_transport")
        self.assertTrue(record.retryable)
        self.assertFalse(record.resumable)
        self.assertTrue(record.last_error)
        self.assertEqual(record.session_id, self._REQUESTED_SESSION)

    def test_schema_validation_failure_is_retryable(self):
        record = self._run(
            self._invocation(),
            validate=lambda payload: ["candidate shape invalid"],
        )
        self.assertEqual(record.status, "failed_transport")
        self.assertTrue(record.retryable)
        self.assertIn("candidate shape invalid", record.last_error)

    def test_unsafe_invocation_error_propagates(self):
        from harness.fanout import run_one_attempt

        expected = UnsafeInvocationError("bad request")

        def invoke_fn(request):
            raise expected

        with self.assertRaises(UnsafeInvocationError) as caught:
            run_one_attempt(
                role="finder:roadmap",
                attempt=1,
                request=self._request(),
                invoke_fn=invoke_fn,
                validate=lambda payload: [],
                expected_tools=self._EXPECTED_TOOLS,
            )
        self.assertIs(caught.exception, expected)

    def test_capability_drift_is_not_retryable(self):
        record = self._run(
            self._invocation(init_tools=["Bash", "Grep", "Read"])
        )
        self.assertEqual(record.status, "capability_drift")
        self.assertFalse(record.retryable)
        self.assertIn("Bash", record.last_error)

    def test_matching_capabilities_do_not_change_success(self):
        record = self._run(self._invocation())
        self.assertEqual(record.status, "success")

    def test_attempt_record_contains_only_plain_data(self):
        record = self._run(self._invocation())
        values = asdict(record)
        connection = sqlite3.connect(":memory:")
        lock = _thread.allocate_lock()
        try:
            stack = [values]
            while stack:
                value = stack.pop()
                self.assertNotIsInstance(value, sqlite3.Connection)
                self.assertNotIsInstance(value, _thread.LockType)
                if isinstance(value, dict):
                    stack.extend(value.values())
                elif isinstance(value, (list, tuple, set)):
                    stack.extend(value)
        finally:
            connection.close()
            del lock

    def test_reported_session_alone_controls_resumability(self):
        no_report = self._run(
            self._invocation(
                ok=False,
                payload=None,
                session_id=None,
                subtype=None,
                raw_tail="timed out",
            )
        )
        reported = self._run(
            self._invocation(
                ok=False,
                payload=None,
                session_id=self._REPORTED_SESSION,
                subtype=None,
                raw_tail="transport failure",
            )
        )
        self.assertFalse(no_report.resumable)
        self.assertTrue(reported.resumable)

    def test_budget_exhaustion_is_not_retryable(self):
        record = self._run(
            self._invocation(
                ok=False,
                payload=None,
                subtype="error_max_budget_usd",
                raw_tail="budget exhausted",
            )
        )
        self.assertEqual(record.status, "failed_transport")
        self.assertFalse(record.retryable)

    def test_duplicate_terminal_event_is_not_retryable(self):
        record = self._run(
            self._invocation(
                ok=False,
                payload=None,
                protocol_errors=["duplicate terminal result events: 2"],
            )
        )
        self.assertEqual(record.status, "failed_transport")
        self.assertFalse(record.retryable)

    def test_duplicate_init_event_is_not_retryable(self):
        record = self._run(
            self._invocation(
                ok=False,
                payload=None,
                protocol_errors=["duplicate init events: 2"],
            )
        )
        self.assertFalse(record.retryable)

    def test_execution_error_without_protocol_error_is_retryable(self):
        record = self._run(
            self._invocation(
                ok=False,
                payload=None,
                subtype="error_during_execution",
                protocol_errors=[],
                raw_tail="API Error: Server error mid-response",
            )
        )
        self.assertTrue(record.retryable)

    def test_parser_layer_payload_failure_is_retryable(self):
        record = self._run(
            self._invocation(
                ok=False,
                payload=None,
                subtype="success",
                protocol_errors=[
                    "unparseable or malformed payload in success result"
                ],
            )
        )
        self.assertTrue(record.retryable)

    def test_missing_init_cli_failure_is_not_retryable(self):
        record = self._run(
            self._invocation(
                ok=False,
                payload=None,
                session_id=None,
                subtype=None,
                init_seen=False,
                init_tools=[],
                protocol_errors=[
                    "missing init event",
                    "missing terminal result event",
                ],
            )
        )
        self.assertFalse(record.retryable)
        self.assertFalse(record.resumable)

    def test_nonretryable_protocol_failure_precedes_retryable_parser_failure(self):
        from harness.fanout import _classify_retryable

        invocation = self._invocation(
            ok=False,
            payload=None,
            subtype="success",
            protocol_errors=[
                "missing init event",
                "unparseable or malformed payload in success result",
            ],
        )
        self.assertFalse(
            _classify_retryable(invocation, "failed_transport", [])
        )

    def test_missing_init_and_budget_exhaustion_are_unambiguously_nonretryable(self):
        from harness.fanout import _classify_retryable

        invocation = self._invocation(
            ok=False,
            payload=None,
            subtype="error_max_budget_usd",
            protocol_errors=["missing init event"],
        )
        self.assertFalse(
            _classify_retryable(invocation, "failed_transport", [])
        )

    def test_continuation_request_forks_reported_session_with_new_prompt(self):
        from harness.fanout import build_continuation_request

        previous = self._request()
        continued = build_continuation_request(
            previous, self._REPORTED_SESSION
        )
        self.assertEqual(continued.resume, self._REPORTED_SESSION)
        self.assertTrue(continued.fork_session)
        self.assertIsNone(continued.session_id)
        self.assertNotEqual(continued.prompt, previous.prompt)
        self.assertEqual(continued.role, previous.role)
        self.assertEqual(continued.stream_log, previous.stream_log)


if __name__ == "__main__":
    unittest.main()
