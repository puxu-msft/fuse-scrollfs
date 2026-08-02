import _thread
import sqlite3
import unittest
from dataclasses import asdict

from harness import fanout
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

    def test_four_exact_quarter_reservations_exhaust_budget(self):
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(1.2)
        accepted = [budget.try_reserve(0.3) for _ in range(4)]
        self.assertEqual(accepted, [True, True, True, True])
        self.assertEqual(budget.remaining(), 0.0)

    def test_three_exact_third_reservations_exhaust_budget(self):
        from harness.fanout import BudgetTracker

        budget = BudgetTracker(0.6)
        accepted = [budget.try_reserve(0.2) for _ in range(3)]
        self.assertEqual(accepted, [True, True, True])
        self.assertEqual(budget.remaining(), 0.0)


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
             make_request=None, conn=None, expected_tools=None,
             single_call_cap_usd=0.3):
        import time
        from harness.fanout import BudgetTracker, run_wave_scheduled

        return run_wave_scheduled(
            roles=roles or (self._ROLE,),
            make_request=make_request or self._request,
            invoke_fn=invoke_fn,
            validate=lambda payload: [],
            budget=budget or BudgetTracker(3.0),
            deadline_monotonic=deadline or time.monotonic() + 120.0,
            single_call_cap_usd=single_call_cap_usd,
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

    def test_reserved_cap_is_applied_to_every_scheduled_request(self):
        seen = []
        roles = ("finder:roadmap", "finder:code")

        from harness.fanout import BudgetTracker

        self._run(
            lambda request: (
                seen.append(request)
                or self._invocation(session_id=request.session_id)
            ),
            roles=roles,
            budget=BudgetTracker(0.2),
            make_request=lambda role, attempt: self._request(
                role, attempt, grant_usd=0.3
            ),
            single_call_cap_usd=0.1,
        )
        self.assertEqual(len(seen), 2)
        self.assertTrue(all(request.grant_usd == 0.1 for request in seen))

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


class TestFanoutComposition(unittest.TestCase):
    _CONTEXT = None

    @classmethod
    def setUpClass(cls):
        from harness.prompts import AgentDef
        from harness.role_invocation import RequestContext

        cls._CONTEXT = RequestContext(
            cwd="/real/repo",
            settings_path=".claude/harness-settings.json",
            model="claude-sonnet-5",
            stream_log_dir="/real/repo/.claude/state/rounds",
        )
        cls._AGENTS = {
            name: AgentDef(
                name=name,
                description=name,
                tools=("Glob", "Grep", "Read"),
                body=f"persona for {name}",
            )
            for name in (
                "harness-finder-roadmap",
                "harness-finder-code",
                "harness-finder-bench",
                "harness-finder-hygiene",
                "harness-judge-redline",
                "harness-judge-completed",
                "harness-judge-oracle",
            )
        }

    @staticmethod
    def _candidate(**overrides):
        candidate = {
            "title": "candidate",
            "goal": "goal",
            "invariant": "invariant",
            "primary_path": "src/main.py",
            "oracle": "python -m unittest",
            "evidence": "evidence",
            "touched_paths": ["src/main.py"],
            "size": "S",
            "priority": "T0",
            "needs_decision": False,
            "body_md": "body",
            "slug": "candidate",
            "lane": "defect",
        }
        candidate.update(overrides)
        return candidate

    @staticmethod
    def _invocation(payload, *, request, ok=True, init_tools=None,
                    session_id=None, raw_tail=""):
        return InvocationResult(
            ok=ok,
            payload=payload if ok else None,
            cost_usd=0.01,
            turns=1,
            cost_known=True,
            session_id=session_id or request.session_id,
            subtype="success" if ok else "error_during_execution",
            init_seen=True,
            init_tools=init_tools or ["Glob", "Grep", "Read"],
            raw_tail=raw_tail,
        )

    def _run_finders(self, invoke_fn, **overrides):
        import time
        from harness.fanout import BudgetTracker, run_finders

        values = {
            "round_id": "round-1",
            "invoke_fn": invoke_fn,
            "budget": BudgetTracker(3.0),
            "deadline_monotonic": time.monotonic() + 120.0,
            "blocked_lanes": [],
            "known_canonical_keys": set(),
            "context": self._CONTEXT,
            "agents": self._AGENTS,
        }
        values.update(overrides)
        return run_finders(**values)

    def _judge(self, candidate, invoke_fn, **overrides):
        import time
        from harness.fanout import BudgetTracker, judge_candidate

        values = {
            "round_id": "round-1",
            "candidate": candidate,
            "invoke_fn": invoke_fn,
            "budget": BudgetTracker(3.0),
            "deadline_monotonic": time.monotonic() + 120.0,
            "inflight_paths": ["src/inflight.py"],
            "context": self._CONTEXT,
            "agents": self._AGENTS,
        }
        values.update(overrides)
        return judge_candidate(**values)

    def test_finders_return_ranked_candidate_without_degradation(self):
        candidate = self._candidate()

        def invoke(request):
            finder_candidate = {k: v for k, v in candidate.items() if k != "lane"}
            payload = {"candidates": [finder_candidate]} if request.role == "finder:code" else {"candidates": []}
            return self._invocation(payload, request=request)

        ranked, degraded = self._run_finders(invoke)
        self.assertEqual([c["title"] for c in ranked], [candidate["title"]])
        self.assertEqual(degraded, [])

    def test_failed_finder_does_not_discard_other_finder_candidate(self):
        candidate = self._candidate()

        def invoke(request):
            if request.role == "finder:roadmap":
                return self._invocation(
                    None,
                    request=request,
                    ok=False,
                    session_id="real-roadmap",
                    raw_tail="transient",
                )
            finder_candidate = {k: v for k, v in candidate.items() if k != "lane"}
            payload = {"candidates": [finder_candidate]} if request.role == "finder:code" else {"candidates": []}
            return self._invocation(payload, request=request)

        ranked, degraded = self._run_finders(invoke)
        self.assertEqual(len(ranked), 1)
        self.assertEqual([d["role"] for d in degraded], ["finder:roadmap"])

    def test_redline_reject_short_circuits_other_judges(self):
        calls = []

        def invoke(request):
            calls.append(request.role)
            self.assertTrue(request.role.startswith("judge:redline:"))
            return self._invocation(
                {
                    "verdict": "reject",
                    "reason": "redline",
                    "invariant_at_risk": "risk",
                },
                request=request,
            )

        verdicts, degraded = self._judge(self._candidate(), invoke)
        self.assertEqual(len(calls), 1)
        self.assertEqual(degraded, [])
        self.assertEqual(
            verdicts[0]["skipped_judges"],
            ["harness-judge-completed", "harness-judge-oracle"],
        )

    def test_redline_pass_runs_all_judges_with_no_skips(self):
        def invoke(request):
            kind = request.role.split(":", 2)[1]
            payloads = {
                "redline": {
                    "verdict": "pass",
                    "reason": "safe",
                    "invariant_at_risk": "none",
                },
                "completed": {
                    "verdict": "pass",
                    "reason": "new",
                    "evidence": "none",
                },
                "oracle": {
                    "verdict": "pass",
                    "reason": "strong",
                    "suggested_oracle": "none",
                },
            }
            return self._invocation(payloads[kind], request=request)

        verdicts, degraded = self._judge(self._candidate(), invoke)
        self.assertEqual(len(verdicts), 3)
        self.assertEqual(degraded, [])
        self.assertTrue(all(v["skipped_judges"] == [] for v in verdicts))

    def test_degraded_redline_is_reject_and_top_level_degraded(self):
        def invoke(request):
            return self._invocation(
                None,
                request=request,
                ok=False,
                session_id="real-redline",
                raw_tail="transient",
            )

        verdicts, degraded = self._judge(self._candidate(), invoke)
        self.assertEqual(verdicts[0]["verdict"], "reject")
        self.assertEqual(verdicts[0]["reason"], "judge-unavailable")
        self.assertTrue(verdicts[0]["degraded"])
        self.assertEqual(len(degraded), 1)
        self.assertTrue(degraded[0]["role"].startswith("judge:redline:"))

    def test_different_candidates_get_distinct_judge_session_identity(self):
        seen = []

        def invoke(request):
            seen.append(request)
            return self._invocation(
                {
                    "verdict": "reject",
                    "reason": "redline",
                    "invariant_at_risk": "risk",
                },
                request=request,
            )

        self._judge(self._candidate(goal="goal-a"), invoke)
        self._judge(self._candidate(goal="goal-b"), invoke)
        self.assertNotEqual(seen[0].session_id, seen[1].session_id)

    def test_judge_capability_drift_becomes_degraded_reject(self):
        def invoke(request):
            return self._invocation(
                {
                    "verdict": "pass",
                    "reason": "safe",
                    "invariant_at_risk": "none",
                },
                request=request,
                init_tools=["Bash", "Glob", "Grep", "Read"],
            )

        verdicts, degraded = self._judge(self._candidate(), invoke)
        self.assertEqual(verdicts[0]["reason"], "judge-unavailable")
        self.assertEqual(len(degraded), 1)

    def test_judge_identity_drives_session_ledger_and_stream_log(self):
        from unittest.mock import patch
        from harness.queue import fingerprint
        from harness.role_invocation import build_stream_log_path
        from harness.session_identity import derive_session_id

        candidate_a = self._candidate(goal="goal-a")
        candidate_b = self._candidate(goal="goal-b")
        actual_requests = []
        started = []

        def invoke(request):
            actual_requests.append(request)
            return self._invocation(
                {
                    "verdict": "reject",
                    "reason": "redline",
                    "invariant_at_risk": "risk",
                },
                request=request,
            )

        def record_started(conn, **kwargs):
            started.append(kwargs)

        with patch("harness.fanout.ledger.record_attempt_started", record_started), patch(
            "harness.fanout.ledger.record_attempt_finished"
        ):
            self._judge(candidate_a, invoke, conn=object())
            self._judge(candidate_b, invoke, conn=object())

        for candidate, request, ledger_row in zip(
            (candidate_a, candidate_b), actual_requests, started, strict=True
        ):
            fp = fingerprint(
                candidate["goal"],
                candidate["invariant"],
                candidate["primary_path"],
                candidate["oracle"],
            )
            task_role = f"judge:redline:{fp}"
            self.assertEqual(request.session_id, derive_session_id("round-1", task_role, 1))
            self.assertEqual(
                request.stream_log,
                build_stream_log_path(
                    self._CONTEXT.stream_log_dir, "round-1", task_role, 1
                ),
            )
            self.assertEqual(
                f"{ledger_row['round_id']}:{ledger_row['role']}:{ledger_row['attempt']}",
                f"round-1:{task_role}:1",
            )
        self.assertNotEqual(actual_requests[0].stream_log, actual_requests[1].stream_log)

    def test_request_context_populates_every_role_request(self):
        seen = []

        def finder_invoke(request):
            seen.append(request)
            return self._invocation({"candidates": []}, request=request)

        self._run_finders(finder_invoke)

        def judge_invoke(request):
            seen.append(request)
            kind = request.role.split(":", 2)[1]
            payload = {
                "redline": {"verdict": "pass", "reason": "safe", "invariant_at_risk": "none"},
                "completed": {"verdict": "pass", "reason": "new", "evidence": "none"},
                "oracle": {"verdict": "pass", "reason": "strong", "suggested_oracle": "none"},
            }[kind]
            return self._invocation(payload, request=request)

        self._judge(self._candidate(), judge_invoke)
        self.assertEqual(len(seen), 7)
        for request in seen:
            self.assertEqual(request.cwd, self._CONTEXT.cwd)
            self.assertEqual(request.settings_path, self._CONTEXT.settings_path)
            self.assertEqual(request.model, self._CONTEXT.model)

    def test_judge_retry_uses_distinct_per_attempt_stream_logs(self):
        seen = []

        def invoke(request):
            seen.append(request)
            if len(seen) == 1:
                return self._invocation(
                    None,
                    request=request,
                    ok=False,
                    session_id="real-redline",
                    raw_tail="transient",
                )
            return self._invocation(
                {
                    "verdict": "reject",
                    "reason": "redline",
                    "invariant_at_risk": "risk",
                },
                request=request,
            )

        self._judge(self._candidate(), invoke)
        self.assertEqual(len(seen), 2)
        self.assertNotEqual(seen[0].stream_log, seen[1].stream_log)


class TestRunFanout(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        TestFanoutComposition.setUpClass()
        cls.composition = TestFanoutComposition
        cls._CONTEXT = cls.composition._CONTEXT
        cls._AGENTS = cls.composition._AGENTS

    def _run(self, invoke_fn, **overrides):
        import time
        from harness.fanout import BudgetTracker, run_fanout

        values = {
            "round_id": "round-1",
            "invoke_fn": invoke_fn,
            "budget": BudgetTracker(5.0),
            "deadline_monotonic": time.monotonic() + 120.0,
            "blocked_lanes": [],
            "known_canonical_keys": set(),
            "inflight_paths": [],
            "context": self._CONTEXT,
            "agents": self._AGENTS,
        }
        values.update(overrides)
        return run_fanout(**values)

    def _candidate(self, **overrides):
        return self.composition._candidate(**overrides)

    def _invocation(self, payload, *, request, **overrides):
        return self.composition._invocation(
            payload, request=request, **overrides
        )

    def test_empty_finders_preserve_legacy_top_level_shape(self):
        result = self._run(
            lambda request: self._invocation(
                {"candidates": []}, request=request
            )
        )
        self.assertEqual(result["candidates"], [])
        self.assertEqual(result["rejected"], [])
        self.assertIn("degraded", result)
        self.assertEqual(result["degraded"], [])

    def test_single_call_cap_reaches_finders_and_judges(self):
        candidate = self._candidate()
        seen = []

        def invoke(request):
            seen.append(request)
            if request.role.startswith("finder:"):
                finder_candidate = {
                    k: v for k, v in candidate.items() if k != "lane"
                }
                payload = (
                    {"candidates": [finder_candidate]}
                    if request.role == "finder:code"
                    else {"candidates": []}
                )
            else:
                kind = request.role.split(":", 2)[1]
                payload = {
                    "redline": {"verdict": "pass", "reason": "safe", "invariant_at_risk": "none"},
                    "completed": {"verdict": "pass", "reason": "new", "evidence": "none"},
                    "oracle": {"verdict": "pass", "reason": "strong", "suggested_oracle": "none"},
                }[kind]
            return self._invocation(payload, request=request)

        self._run(invoke, single_call_cap_usd=0.1)
        self.assertEqual(len(seen), 7)
        self.assertTrue(all(request.grant_usd == 0.1 for request in seen))

    def test_first_candidate_passing_all_judges_is_selected(self):
        candidate = self._candidate()

        def invoke(request):
            if request.role.startswith("finder:"):
                finder_candidate = {
                    k: v for k, v in candidate.items() if k != "lane"
                }
                payload = (
                    {"candidates": [finder_candidate]}
                    if request.role == "finder:code"
                    else {"candidates": []}
                )
            else:
                kind = request.role.split(":", 2)[1]
                payload = {
                    "redline": {"verdict": "pass", "reason": "safe", "invariant_at_risk": "none"},
                    "completed": {"verdict": "pass", "reason": "new", "evidence": "none"},
                    "oracle": {"verdict": "pass", "reason": "strong", "suggested_oracle": "none"},
                }[kind]
            return self._invocation(payload, request=request)

        result = self._run(invoke)
        self.assertEqual(len(result["candidates"]), 1)
        self.assertEqual(len(result["candidates"][0]["verdicts"]), 3)
        self.assertEqual(result["rejected"], [])

    def test_degraded_redline_reaches_top_level(self):
        candidate = self._candidate()

        def invoke(request):
            if request.role.startswith("finder:"):
                finder_candidate = {
                    k: v for k, v in candidate.items() if k != "lane"
                }
                payload = (
                    {"candidates": [finder_candidate]}
                    if request.role == "finder:code"
                    else {"candidates": []}
                )
                return self._invocation(payload, request=request)
            return self._invocation(
                None,
                request=request,
                ok=False,
                session_id="real-redline",
                raw_tail="transient",
            )

        result = self._run(invoke)
        self.assertEqual(result["candidates"], [])
        self.assertTrue(
            any(d["role"].startswith("judge:redline:") for d in result["degraded"])
        )

    def test_returns_every_attempt_including_a_failure_superseded_by_retry(self):
        candidate = self._candidate()
        roadmap_calls = 0

        def invoke(request):
            nonlocal roadmap_calls
            if request.role == "finder:roadmap":
                roadmap_calls += 1
                if roadmap_calls == 1:
                    return self._invocation(
                        None,
                        request=request,
                        ok=False,
                        session_id="real-roadmap",
                        raw_tail="transient",
                    )
            if request.role.startswith("finder:"):
                finder_candidate = {
                    key: value for key, value in candidate.items() if key != "lane"
                }
                payload = (
                    {"candidates": [finder_candidate]}
                    if request.role == "finder:code"
                    else {"candidates": []}
                )
            else:
                payload = {
                    "verdict": "reject",
                    "reason": "redline",
                    "invariant_at_risk": "risk",
                }
            return self._invocation(payload, request=request)

        result = self._run(invoke)

        self.assertEqual(roadmap_calls, 2)
        self.assertEqual(len(result["attempts"]), 6)
        roadmap_attempts = [
            record for record in result["attempts"]
            if record.role == "finder:roadmap"
        ]
        self.assertEqual([record.attempt for record in roadmap_attempts], [1, 2])
        self.assertEqual(
            [record.status for record in roadmap_attempts],
            ["failed_transport", "success"],
        )

    def test_settlement_counts_failed_and_successful_attempt_costs(self):
        candidate = self._candidate()
        roadmap_calls = 0

        def invoke(request):
            nonlocal roadmap_calls
            if request.role == "finder:roadmap":
                roadmap_calls += 1
                if roadmap_calls == 1:
                    return InvocationResult(
                        ok=False,
                        payload=None,
                        cost_usd=0.2,
                        turns=1,
                        cost_known=True,
                        session_id="real-roadmap",
                        subtype="error_during_execution",
                        init_seen=True,
                        init_tools=["Glob", "Grep", "Read"],
                        raw_tail="transient",
                    )
            if request.role.startswith("finder:"):
                finder_candidate = {
                    k: v for k, v in candidate.items() if k != "lane"
                }
                payload = (
                    {"candidates": [finder_candidate]}
                    if request.role == "finder:code"
                    else {"candidates": []}
                )
            else:
                payload = {
                    "verdict": "reject",
                    "reason": "redline",
                    "invariant_at_risk": "risk",
                }
            return InvocationResult(
                ok=True,
                payload=payload,
                cost_usd=0.2,
                turns=1,
                cost_known=True,
                session_id=request.session_id,
                subtype="success",
                init_seen=True,
                init_tools=["Glob", "Grep", "Read"],
            )

        result = self._run(invoke)
        self.assertEqual(roadmap_calls, 2)
        self.assertAlmostEqual(result["settlement"].total_cost_usd, 1.2)
        self.assertNotAlmostEqual(result["settlement"].total_cost_usd, 1.0)

    def test_unknown_attempt_cost_makes_settlement_unknown(self):
        calls = 0

        def invoke(request):
            nonlocal calls
            calls += 1
            return InvocationResult(
                ok=True,
                payload={"candidates": []},
                cost_usd=0.01,
                turns=1,
                cost_known=calls != 1,
                session_id=request.session_id,
                subtype="success",
                init_seen=True,
                init_tools=["Glob", "Grep", "Read"],
            )

        result = self._run(invoke)
        self.assertFalse(result["settlement"].cost_known)

    def test_capability_drift_is_aggregated_for_caller(self):
        calls = 0

        def invoke(request):
            nonlocal calls
            calls += 1
            return self._invocation(
                {"candidates": []},
                request=request,
                init_tools=(
                    ["Bash", "Glob", "Grep", "Read"]
                    if calls == 1
                    else ["Glob", "Grep", "Read"]
                ),
            )

        result = self._run(invoke)
        self.assertEqual(len(result["settlement"].capability_drift), 1)

    def test_protocol_errors_keep_role_and_attempt_sources(self):
        from harness.fanout import AttemptRecord, _aggregate_settlement

        records = [
            AttemptRecord(
                role="finder:roadmap",
                attempt=1,
                status="failed_transport",
                protocol_errors=["duplicate init events: 2"],
            ),
            AttemptRecord(
                role="finder:code",
                attempt=2,
                status="failed_transport",
                protocol_errors=["unparseable stream line: bad"],
            ),
        ]
        settlement = _aggregate_settlement(records)
        self.assertEqual(len(settlement.protocol_errors), 2)
        self.assertTrue(settlement.protocol_errors[0].startswith("finder:roadmap:1:"))
        self.assertTrue(settlement.protocol_errors[1].startswith("finder:code:2:"))


if __name__ == "__main__":
    unittest.main()


class TestPerCallTimeoutAccommodatesRealWork(unittest.TestCase):
    """单次调用的超时上限必须容得下真实工作量（Phase 8 真机发现）。

    真机实测（2026-08-02 Task 8.3）：完整扇出一轮里 **4 个 finder 全部失败**，
    7 次尝试无一产出终态 `result`，成本与 turns 均为 0。查 stream 日志才判出
    根因——`_DEFAULT_REQUEST_TIMEOUT_S = 60.0`，而 Task 8.2 单角色冒烟实测
    一个 finder 需要 **78.5 秒**。每个 finder 都在 60 秒被 `subprocess` 杀掉。

    分类逻辑本身是对的：超时路径 `protocol_errors` 为空 → 判 `failed_transport`
    且可重试 → 4 次首尝 + 3 次重试，全部同样超时。**缺陷不在分类，在这个常量。**

    为什么 453 个离线测试全绿也发现不了：所有测试都用**瞬间返回的假件**，
    60 秒这个值从来没被真实工作量检验过。这属于 HANDOVER 记的
    「『我活多久』这类问题离线测试系统性地看不见」。
    """

    def test_default_request_timeout_exceeds_observed_real_finder_duration(self):
        # Task 8.2 真机实测：finder:hygiene 单独跑 78.5 秒（19 turns）。
        # 完整扇出下 4 个 finder 并发，只会更慢，不会更快。
        observed_real_seconds = 78.5
        self.assertGreater(
            fanout._DEFAULT_REQUEST_TIMEOUT_S, observed_real_seconds * 2,
            "单次调用超时上限低于真机实测工作量的两倍——真实 finder 会被"
            "无差别杀掉，且表现为『全部 failed_transport、成本 0』，"
            "从账本上看不出是超时还是别的")

    def test_default_timeout_leaves_room_for_more_than_one_wave(self):
        """至少要能在一轮截止内跑完两波，否则重试机制形同虚设。"""
        from harness import round as round_module

        usable = round_module.ROUND_DEADLINE_S - round_module.CLEANUP_RESERVE_S
        self.assertGreaterEqual(
            usable, fanout._DEFAULT_REQUEST_TIMEOUT_S * 2,
            "单次超时上限太大，一轮截止内放不下两波——首波失败后没有重试余地")


class TestMaxTurnsAccommodatesRealWork(unittest.TestCase):
    """回合上限必须容得下真实工作量（Phase 8 真机发现，与超时同源）。

    真机实测（2026-08-02 Task 8.3 第二次）：`_DEFAULT_MAX_TURNS = 10`，
    **4 个 finder 的首次尝试全部在第 11 轮撞上 `error_max_turns`**，各烧掉约
    $0.5 后一无所得；靠 fork 重试才救回其中两个。Task 8.2 单角色冒烟实测一个
    finder 需要 **19 轮**才能完成。

    这与 `_DEFAULT_REQUEST_TIMEOUT_S = 60` 是**同一类错误**：上限被设成了
    「正常工作量的量级」而不是「异常的量级」。用户 2026-08-02 的原话——
    「预算上限不是为了卡住正常运行，是为了拦截异常」——同样适用于回合数与
    超时：**能在正常运行中被触发的上限是坏上限**。

    离线测试看不见的原因同上：假件瞬间返回，回合数从未被真实工作量检验过。
    """

    def test_default_max_turns_exceeds_observed_real_finder_turns(self):
        # Task 8.2 真机实测：finder:hygiene 单独跑用了 19 turns 才完成。
        observed_real_turns = 19
        self.assertGreater(
            fanout._DEFAULT_MAX_TURNS, observed_real_turns * 1.5,
            "回合上限低于真机实测工作量的 1.5 倍——真实 finder 会在完成前被"
            "error_max_turns 截断，且已花的钱全部作废")
