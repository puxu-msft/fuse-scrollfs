import unittest

from harness.fanout_schema import (
    validate_finder_output,
    validate_judge_output,
)


VALID_CANDIDATE = {
    "title": "Reduce duplicate harness work",
    "goal": "Avoid proposing work that is already complete",
    "invariant": "Every proposal is actionable",
    "primary_path": ".claude/scripts/harness/round.py",
    "oracle": "A completed fingerprint is rejected",
    "evidence": "The queue already contains the fingerprint",
    "touched_paths": [
        ".claude/scripts/harness/round.py",
        ".claude/scripts/harness/tests/test_round.py",
    ],
    "size": "M",
    "priority": "T1",
    "needs_decision": False,
    "body_md": "## Goal\nAvoid duplicate work.\n",
    "slug": "avoid-duplicate-work",
}

VALID_JUDGE_OUTPUTS = {
    "harness-judge-completed": {
        "verdict": "pass",
        "reason": "No completed equivalent exists",
        "evidence": "Queue lookup returned no match",
    },
    "harness-judge-redline": {
        "verdict": "needs_decision",
        "reason": "The candidate touches a frozen boundary",
        "invariant_at_risk": "The disk format must remain stable",
    },
    "harness-judge-oracle": {
        "verdict": "reject",
        "reason": "The proposed oracle cannot detect the failure",
        "suggested_oracle": "Exercise the controller boundary",
    },
}


def finder_payload(candidate=None):
    return {"candidates": [dict(candidate or VALID_CANDIDATE)]}


class TestFinderOutputSchema(unittest.TestCase):
    def test_valid_single_candidate_and_empty_list_pass(self):
        self.assertEqual(validate_finder_output(finder_payload()), [])
        self.assertEqual(validate_finder_output({"candidates": []}), [])

    def test_top_level_shape_errors_are_collected(self):
        cases = (
            {},
            {"candidates": "not-a-list"},
            {"candidates": [dict(VALID_CANDIDATE) for _ in range(4)]},
        )
        for payload in cases:
            with self.subTest(payload=payload):
                self.assertTrue(validate_finder_output(payload))

    def test_missing_and_unknown_candidate_fields_are_rejected(self):
        missing = dict(VALID_CANDIDATE)
        missing.pop("goal")
        unknown = dict(VALID_CANDIDATE, labels=["harness:proposed"])

        self.assertTrue(validate_finder_output(finder_payload(missing)))
        self.assertTrue(validate_finder_output(finder_payload(unknown)))

    def test_string_and_enum_values_are_validated(self):
        cases = (
            ("title", 42),
            ("size", "XL"),
            ("priority", "T9"),
        )
        for field, value in cases:
            with self.subTest(field=field, value=value):
                candidate = dict(VALID_CANDIDATE, **{field: value})
                self.assertTrue(validate_finder_output(finder_payload(candidate)))

    def test_touched_paths_and_needs_decision_types_are_validated(self):
        cases = (
            ("touched_paths", "round.py"),
            ("touched_paths", ["round.py", 42]),
            ("needs_decision", 1),
        )
        for field, value in cases:
            with self.subTest(field=field, value=value):
                candidate = dict(VALID_CANDIDATE, **{field: value})
                self.assertTrue(validate_finder_output(finder_payload(candidate)))

    def test_top_level_additional_property_is_rejected(self):
        payload = finder_payload()
        payload["degraded"] = []
        self.assertTrue(validate_finder_output(payload))

    def test_unhashable_enum_values_are_collected_instead_of_raised(self):
        cases = (("size", []), ("priority", {"value": "T1"}))
        for field, value in cases:
            with self.subTest(field=field):
                candidate = dict(VALID_CANDIDATE, **{field: value})
                errors = validate_finder_output(finder_payload(candidate))
                self.assertTrue(errors)
                self.assertTrue(any(field in error for error in errors))

    def test_candidate_text_length_limits_are_enforced(self):
        cases = (("title", "x" * 301), ("body_md", "x" * 20001))
        for field, value in cases:
            with self.subTest(field=field):
                candidate = dict(VALID_CANDIDATE, **{field: value})
                self.assertTrue(validate_finder_output(finder_payload(candidate)))


class TestJudgeOutputSchema(unittest.TestCase):
    def test_each_judge_accepts_its_own_schema(self):
        for judge_type, payload in VALID_JUDGE_OUTPUTS.items():
            with self.subTest(judge_type=judge_type):
                self.assertEqual(validate_judge_output(judge_type, payload), [])

    def test_judge_specific_fields_cannot_be_exchanged(self):
        redline = dict(VALID_JUDGE_OUTPUTS["harness-judge-redline"])
        redline["suggested_oracle"] = redline.pop("invariant_at_risk")
        oracle = dict(VALID_JUDGE_OUTPUTS["harness-judge-oracle"])
        oracle["evidence"] = oracle.pop("suggested_oracle")

        self.assertTrue(validate_judge_output("harness-judge-redline", redline))
        self.assertTrue(validate_judge_output("harness-judge-oracle", oracle))

    def test_missing_field_and_invalid_verdict_are_rejected(self):
        missing = dict(VALID_JUDGE_OUTPUTS["harness-judge-completed"])
        missing.pop("evidence")
        invalid = dict(VALID_JUDGE_OUTPUTS["harness-judge-completed"], verdict="maybe")

        self.assertTrue(validate_judge_output("harness-judge-completed", missing))
        self.assertTrue(validate_judge_output("harness-judge-completed", invalid))

    def test_unhashable_verdict_is_collected_instead_of_raised(self):
        for value in ([], {"value": "pass"}):
            with self.subTest(value=value):
                payload = dict(
                    VALID_JUDGE_OUTPUTS["harness-judge-completed"],
                    verdict=value,
                )
                errors = validate_judge_output(
                    "harness-judge-completed", payload,
                )
                self.assertTrue(errors)
                self.assertTrue(any("verdict" in error for error in errors))

    def test_non_verdict_string_fields_are_validated(self):
        payload = dict(
            VALID_JUDGE_OUTPUTS["harness-judge-completed"],
            reason=["not", "a", "string"],
        )
        self.assertTrue(validate_judge_output("harness-judge-completed", payload))

    def test_unknown_judge_type_raises_key_error(self):
        with self.assertRaises(KeyError):
            validate_judge_output("harness-judge-unknown", {})


if __name__ == "__main__":
    unittest.main()
