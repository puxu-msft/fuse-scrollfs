from __future__ import annotations
from dataclasses import dataclass


class State:
    CANDIDATE_SELECTED = "candidate-selected"
    ISSUE_CREATED = "issue-created"
    LABELS_SET = "labels-set"
    COMMITTED_LOCAL = "proposal-committed-local"
    PUBLISHED = "proposal-published"
    RECEIPT_COMPLETE = "publication-receipt-complete"
    CLOSED_BY_USER = "closed-by-user"
    INCONSISTENT = "inconsistent"


@dataclass(frozen=True)
class Facts:
    issue_closed_by_user: bool
    outbox_record_present: bool
    issue_present: bool
    labels_match: bool
    local_commit_present: bool
    remote_proposal_present: bool
    receipt_present: bool
    binding_ok: bool


def _binding_conflict(f: Facts) -> bool:
    if not f.binding_ok:
        return True
    if f.receipt_present and not f.remote_proposal_present:
        return True
    if (f.local_commit_present or f.remote_proposal_present or f.receipt_present) \
            and not f.issue_present:
        return True
    if f.remote_proposal_present and not f.local_commit_present:
        return True
    return False


def derive(f: Facts) -> str:
    if f.issue_closed_by_user:
        return State.CLOSED_BY_USER
    if _binding_conflict(f):
        return State.INCONSISTENT
    if f.receipt_present and f.remote_proposal_present:
        return State.RECEIPT_COMPLETE
    if f.remote_proposal_present:
        return State.PUBLISHED
    if f.local_commit_present:
        return State.COMMITTED_LOCAL
    if f.issue_present and f.labels_match:
        return State.LABELS_SET
    if f.issue_present:
        return State.ISSUE_CREATED
    if f.outbox_record_present:
        return State.CANDIDATE_SELECTED
    return State.INCONSISTENT
