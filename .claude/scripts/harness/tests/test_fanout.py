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


if __name__ == "__main__":
    unittest.main()
