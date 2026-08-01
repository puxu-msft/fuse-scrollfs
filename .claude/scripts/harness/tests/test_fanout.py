import unittest

from harness.fanout import dedupe_and_rank


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


if __name__ == "__main__":
    unittest.main()
