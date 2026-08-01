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


class TestBudgetTracker(unittest.TestCase):
    def test_reserve_deducts_available_budget(self):
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(1.0)
        self.assertTrue(budget.try_reserve(0.3))
        self.assertAlmostEqual(budget.remaining(), 0.7)

    def test_failed_reserve_preserves_available_budget(self):
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(0.2)
        self.assertFalse(budget.try_reserve(0.3))
        self.assertAlmostEqual(budget.remaining(), 0.2)

    def test_concurrent_reservations_cannot_overspend(self):
        from concurrent.futures import ThreadPoolExecutor
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(1.0)
        with ThreadPoolExecutor(max_workers=10) as pool:
            accepted = list(pool.map(lambda _: budget.try_reserve(0.3), range(10)))
        self.assertLessEqual(sum(accepted) * 0.3, 1.0)

    def test_settle_returns_unused_reservation(self):
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(1.0)
        self.assertTrue(budget.try_reserve(0.3))
        before = budget.remaining()
        budget.settle(reserved=0.3, actual=0.1, cost_known=True)
        self.assertAlmostEqual(budget.remaining() - before, 0.2)

    def test_settle_charges_spend_above_reservation(self):
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(1.0)
        self.assertTrue(budget.try_reserve(0.3))
        before = budget.remaining()
        budget.settle(reserved=0.3, actual=0.5, cost_known=True)
        self.assertAlmostEqual(budget.remaining() - before, -0.2)

    def test_unknown_cost_keeps_full_reservation_charged(self):
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(1.0)
        self.assertTrue(budget.try_reserve(0.3))
        before = budget.remaining()
        budget.settle(reserved=0.3, actual=0.1, cost_known=False)
        self.assertAlmostEqual(budget.remaining(), before)

    def test_negative_balance_blocks_later_reservations(self):
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(0.6)
        self.assertTrue(budget.try_reserve(0.3))
        self.assertTrue(budget.try_reserve(0.3))
        for _ in range(2):
            budget.settle(reserved=0.3, actual=0.5, cost_known=True)
        self.assertLess(budget.remaining(), 0.0)
        self.assertFalse(budget.try_reserve(0.01))


class TestRunWaveScheduled(unittest.TestCase):
    _ROLE = "finder:roadmap"

    def _request(self, role, attempt, **overrides):
        values = {
            "role": role,
            "prompt": "find candidates",
            "tools": "Glob,Grep,Read",
            "grant_usd": 0.3,
            "max_turns": 10,
            "settings_path": ".claude/harness-settings.json",
            "cwd": "/repo",
            "timeout_s": 60.0,
            "session_id": f"session-{role}-{attempt}",
            "stream_log": f"log-{role}-{attempt}.jsonl",
        }
        values.update(overrides)
        return RoleInvocationRequest(**values)

    @staticmethod
    def _invocation(*, ok=True, session_id="reported-session", cost_usd=0.1,
                    cost_known=True, init_tools=None, raw_tail=""):
        return InvocationResult(
            ok=ok,
            payload={"candidates": []} if ok else None,
            cost_usd=cost_usd,
            turns=1,
            cost_known=cost_known,
            session_id=session_id,
            subtype="success" if ok else "error_during_execution",
            init_seen=True,
            init_tools=init_tools or ["Glob", "Grep", "Read"],
            raw_tail=raw_tail,
        )

    def _run(self, invoke_fn, *, roles=None, budget=None, deadline=None,
             make_request=None, conn=None, expected_tools=None):
        import time
        from harness.fanout import BudgetTracker, run_wave_scheduled

        return run_wave_scheduled(
            roles=roles or (self._ROLE,),
            make_request=make_request or self._request,
            invoke_fn=invoke_fn,
            validate=lambda payload: [],
            budget=budget or BudgetTracker(3.0),
            deadline_monotonic=deadline or time.monotonic() + 120.0,
            single_call_cap_usd=0.3,
            expected_tools=expected_tools,
            conn=conn,
            round_id="round-1",
        )

    def test_first_wave_success_records_every_role_and_attempt_number(self):
        seen_attempts = []
        roles = ("finder:roadmap", "finder:code")

        def make_request(role, attempt):
            seen_attempts.append((role, attempt))
            return self._request(role, attempt)

        result = self._run(
            lambda request: self._invocation(session_id=request.session_id),
            roles=roles,
            make_request=make_request,
        )
        self.assertEqual(set(result.final), set(roles))
        self.assertTrue(all(r.status == "success" for r in result.final.values()))
        self.assertEqual(len(result.all_attempts), 2)
        self.assertEqual(seen_attempts, [(role, 1) for role in roles])

    def test_retryable_resumable_failure_forks_from_reported_session(self):
        seen = []

        def invoke(request):
            seen.append(request)
            if len(seen) == 1:
                return self._invocation(
                    ok=False, session_id="real-parent", raw_tail="transient"
                )
            return self._invocation(session_id="real-child")

        result = self._run(invoke)
        self.assertEqual(result.final[self._ROLE].attempt, 2)
        self.assertEqual(len(result.all_attempts), 2)
        self.assertEqual(seen[1].resume, "real-parent")
        self.assertTrue(seen[1].fork_session)

    def test_retryable_nonresumable_failure_starts_fresh_attempt(self):
        seen = []

        def invoke(request):
            seen.append(request)
            if len(seen) == 1:
                return self._invocation(
                    ok=False, session_id=None, raw_tail="timed out"
                )
            return self._invocation(session_id="real-second")

        result = self._run(invoke)
        self.assertEqual(result.final[self._ROLE].attempt, 2)
        self.assertIsNone(seen[1].resume)
        self.assertFalse(seen[1].fork_session)
        self.assertEqual(seen[1].session_id, f"session-{self._ROLE}-2")
        self.assertNotEqual(seen[0].session_id, seen[1].session_id)

    def test_all_failed_attempts_charge_actual_cost_and_are_returned(self):
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(1.0)
        result = self._run(
            lambda request: self._invocation(
                ok=False,
                session_id="real-parent",
                cost_usd=0.5,
                raw_tail="transient",
            ),
            budget=budget,
        )
        self.assertEqual(len(result.all_attempts), 2)
        self.assertAlmostEqual(budget.remaining(), 0.0)

    def test_exhausted_deadline_does_not_start_calls(self):
        import time

        calls = []
        result = self._run(
            lambda request: calls.append(request),
            deadline=time.monotonic() - 1.0,
        )
        self.assertEqual(calls, [])
        self.assertEqual(result.all_attempts, [])
        self.assertIn("deadline-exhausted", result.final[self._ROLE].last_error)

    def test_budget_shortage_prevents_later_wave(self):
        from harness.fanout import BudgetTracker

        calls = []
        budget = BudgetTracker(0.3)
        result = self._run(
            lambda request: (
                calls.append(request)
                or self._invocation(
                    ok=False,
                    session_id="real-parent",
                    cost_known=False,
                    raw_tail="transient",
                )
            ),
            budget=budget,
        )
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(result.all_attempts), 1)
        self.assertIn("budget-exhausted", result.final[self._ROLE].last_error)

    def test_capability_drift_is_not_retried(self):
        calls = []
        result = self._run(
            lambda request: (
                calls.append(request)
                or self._invocation(init_tools=["Bash", "Glob", "Grep", "Read"])
            ),
            expected_tools=frozenset({"Glob", "Grep", "Read"}),
        )
        self.assertEqual(len(calls), 1)
        self.assertEqual(result.final[self._ROLE].status, "capability_drift")

    def test_timeout_shrinks_with_available_deadline(self):
        import time

        observed = []
        for seconds in (90.0, 30.0):
            self._run(
                lambda request: (
                    observed.append(request.timeout_s)
                    or self._invocation(session_id=request.session_id)
                ),
                deadline=time.monotonic() + seconds,
            )
        self.assertNotEqual(observed[0], observed[1])
        self.assertGreater(observed[0], observed[1])
        self.assertTrue(all(0.0 < timeout <= 60.0 for timeout in observed))

    def test_ledger_failures_are_visible_but_nonblocking(self):
        class BrokenConnection:
            def execute(self, *args, **kwargs):
                raise sqlite3.OperationalError("ledger unavailable")

        result = self._run(
            lambda request: self._invocation(session_id=request.session_id),
            conn=BrokenConnection(),
        )
        self.assertEqual(result.final[self._ROLE].status, "success")

    def test_fork_uses_new_attempt_stream_log(self):
        seen = []

        def invoke(request):
            seen.append(request)
            if len(seen) == 1:
                return self._invocation(
                    ok=False, session_id="real-parent", raw_tail="transient"
                )
            return self._invocation(session_id="real-child")

        self._run(invoke)
        self.assertNotEqual(seen[0].stream_log, seen[1].stream_log)
        self.assertTrue(str(seen[1].stream_log).endswith("-2.jsonl"))


if __name__ == "__main__":
    unittest.main()
