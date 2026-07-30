import itertools, unittest
from harness.lifecycle import Facts as F, State, derive


def facts(**kw):
    base = dict(issue_closed_by_user=False, outbox_record_present=True,
                issue_present=False, labels_match=False, local_commit_present=False,
                remote_proposal_present=False, receipt_present=False, binding_ok=True)
    base.update(kw)
    return F(**base)


class TestDerive(unittest.TestCase):
    def test_canonical_progression(self):
        self.assertEqual(derive(facts()), State.CANDIDATE_SELECTED)
        self.assertEqual(derive(facts(issue_present=True)), State.ISSUE_CREATED)
        self.assertEqual(derive(facts(issue_present=True, labels_match=True)), State.LABELS_SET)
        self.assertEqual(derive(facts(issue_present=True, labels_match=True,
                                      local_commit_present=True)), State.COMMITTED_LOCAL)
        self.assertEqual(derive(facts(issue_present=True, labels_match=True,
                                      local_commit_present=True,
                                      remote_proposal_present=True)), State.PUBLISHED)
        self.assertEqual(derive(facts(issue_present=True, labels_match=True,
                                      local_commit_present=True, remote_proposal_present=True,
                                      receipt_present=True)), State.RECEIPT_COMPLETE)

    def test_user_close_wins(self):
        self.assertEqual(derive(facts(issue_closed_by_user=True, issue_present=True,
                                      labels_match=True, local_commit_present=True,
                                      remote_proposal_present=True, receipt_present=True)),
                         State.CLOSED_BY_USER)

    def test_binding_conflict(self):
        self.assertEqual(derive(facts(issue_present=True, remote_proposal_present=True,
                                      binding_ok=False)), State.INCONSISTENT)

    def test_receipt_without_publication(self):
        self.assertEqual(derive(facts(issue_present=True, labels_match=True,
                                      local_commit_present=True, receipt_present=True)),
                         State.INCONSISTENT)

    def test_artifacts_without_issue(self):
        for kw in ({"local_commit_present": True}, {"remote_proposal_present": True},
                   {"receipt_present": True}):
            with self.subTest(kw=kw):
                self.assertEqual(derive(facts(**kw)), State.INCONSISTENT)

    def test_no_outbox_no_issue(self):
        self.assertEqual(derive(facts(outbox_record_present=False)), State.INCONSISTENT)

    def test_exhaustive_total_function(self):
        names = ("issue_closed_by_user", "outbox_record_present", "issue_present",
                 "labels_match", "local_commit_present", "remote_proposal_present",
                 "receipt_present", "binding_ok")
        known = {getattr(State, n) for n in dir(State) if n.isupper()}
        seen = set()
        for combo in itertools.product([False, True], repeat=len(names)):
            result = derive(F(**dict(zip(names, combo))))
            self.assertIn(result, known, msg=f"未知状态 {result} @ {combo}")
            seen.add(result)
        self.assertEqual(seen, known, f"不可达状态: {known - seen}")


if __name__ == "__main__":
    unittest.main()
