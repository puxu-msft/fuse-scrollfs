import unittest
import uuid

from harness.session_identity import derive_session_id


class TestSessionIdentity(unittest.TestCase):
    def test_same_inputs_are_deterministic(self):
        first = derive_session_id("round-1", "finder:roadmap", 1)
        second = derive_session_id("round-1", "finder:roadmap", 1)
        self.assertEqual(first, second)

    def test_result_is_valid_uuid(self):
        result = derive_session_id("round-1", "finder:code", 1)
        self.assertEqual(str(uuid.UUID(result)), result)

    def test_each_identity_component_changes_result(self):
        base = derive_session_id("round-1", "finder:roadmap", 1)
        cases = {
            "round_id": derive_session_id("round-2", "finder:roadmap", 1),
            "role": derive_session_id("round-1", "finder:code", 1),
            "attempt": derive_session_id("round-1", "finder:roadmap", 2),
        }
        for component, result in cases.items():
            with self.subTest(component=component):
                self.assertNotEqual(base, result)

    def test_unknown_finder_role_is_rejected(self):
        with self.assertRaises(ValueError):
            derive_session_id("round-1", "finder:unknown", 1)

    def test_judge_fingerprint_distinguishes_candidates(self):
        first = derive_session_id(
            "round-1", "judge:redline:0123456789abcdef0123456789abcdef", 1
        )
        second = derive_session_id(
            "round-1", "judge:redline:fedcba9876543210fedcba9876543210", 1
        )
        self.assertNotEqual(first, second)

    def test_malformed_judge_role_is_rejected(self):
        with self.assertRaises(ValueError):
            derive_session_id("round-1", "judge:redline:short", 1)

    def test_attempt_must_be_positive_integer(self):
        for attempt in (0, -1, 1.0, "1", True):
            with self.subTest(attempt=attempt):
                with self.assertRaises(ValueError):
                    derive_session_id("round-1", "finder:hygiene", attempt)


if __name__ == "__main__":
    unittest.main()
