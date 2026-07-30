"""Stage 1 发布编排（spec §七 Phase B 的段 1 之后半段）。

顺序固定：建 Issue（label 随建）→ 写卡 + 本地 commit → push main → 写发布收据。
每步经 outbox；重入时按 §5.0 派生状态跳过已完成步骤。
"""

from __future__ import annotations

import json

from . import lifecycle
from .gitops import PublishWorktree
from .lifecycle import Facts, State
from .outbox import Outbox
from .queue import Queue

RECEIPT_MARKER = "HARNESS-RECEIPT"
OP_MARKER = "HARNESS-OP:"


class Publisher:
    def __init__(self, outbox: Outbox, gh, worktree: PublishWorktree,
                 queue: Queue, round_id: str):
        self.outbox = outbox
        self.gh = gh
        self.wt = worktree
        self.queue = queue
        self.round_id = round_id
        self.last_operation_id: str | None = None

    # ---- 事实采集 ---------------------------------------------------------

    def collect_facts(self, operation_id: str, issue: dict | None,
                      rel_path: str | None, expected_labels: list[str]) -> Facts:
        issue_present = issue is not None
        closed = bool(issue and issue.get("state") == "closed")
        labels_match = bool(
            issue and sorted(issue.get("labels", [])) == sorted(expected_labels))
        local = self.wt.local_has_operation(operation_id) if rel_path else False
        remote = self.wt.remote_has_operation(operation_id, rel_path) \
            if rel_path else False
        receipt = bool(issue and self.gh.find_comment_by_marker(
            issue["number"], OP_MARKER + operation_id))
        return Facts(
            issue_closed_by_user=closed,
            outbox_record_present=True,
            issue_present=issue_present,
            labels_match=labels_match,
            local_commit_present=local or remote,
            remote_proposal_present=remote,
            receipt_present=receipt,
            binding_ok=True,
        )

    # ---- 发布 -------------------------------------------------------------

    def resume(self, operation_id: str) -> dict:
        """按持久化 payload 续做一个未完成的发布，不重新扫描（评审 C-02）。

        传入的可能是子 operation（commit_proposal / push_main /
        publication_receipt），必须先解析到 root，否则会把子 operation 的
        payload 当 candidate 用，缺 fingerprint/body_md 等字段直接崩。
        """
        row = self.outbox.conn.execute(
            "SELECT * FROM operations WHERE operation_id=?",
            (operation_id,)).fetchone()
        if row is None:
            raise KeyError(f"未知 operation {operation_id}")
        root = self.outbox.root_of(self.outbox._row_to_op(row))
        return self.publish(root.payload)

    def publish(self, candidate: dict, stop_after: str | None = None) -> dict:
        """幂等发布。**每一步都无条件走 `outbox.execute`**——不再用
        「artifact 已存在就跳过」的分支，那样 operation 会永远停在 prepared，
        后续轮次将永久认为仍需恢复（评审 R5-C-03）。重入由 execute 的
        probe-before-call 统一处理。
        """
        op = self.outbox.prepare(
            self.round_id, "publish_proposal", candidate["fingerprint"],
            {k: candidate[k] for k in
             ("fingerprint", "title", "slug", "lane", "labels", "body_md")})
        self.last_operation_id = op.operation_id
        marker = OP_MARKER + op.operation_id
        body = f"{candidate['body_md']}\n\n<!-- {marker} -->\n"

        issue = self.outbox.execute(
            op,
            call=lambda: self.gh.create_issue(
                candidate["title"], body, candidate["labels"]),
            probe=lambda: self.gh.find_issue_by_marker(marker))
        number = issue["number"]
        rel_path = f"docs/proposals/{number}-{candidate['slug']}.md"
        self.queue.record(candidate["fingerprint"], candidate["lane"],
                          candidate["title"], "proposed", issue_number=number)
        if stop_after == "issue":
            return {"issue": number, "state": State.ISSUE_CREATED}

        # 重入时把绑定装回工作区对象；进程重启后这两个字段不存在于内存
        self.wt.operation_path = rel_path
        self.wt.operation_sha = self.outbox.commit_sha(op)
        self.wt.ensure(allow_reset=self.wt.operation_sha is None)

        commit_op = self.outbox.prepare(
            self.round_id, "commit_proposal", op.operation_id,
            {"issue": number, "path": rel_path})

        def do_commit():
            self.wt.write_proposal(
                rel_path, self._card(candidate, number, op.operation_id))
            sha = self.wt.commit(
                f"docs(proposals): #{number} {candidate['title']}",
                op.operation_id, rel_path)
            self.outbox.set_commit_sha(op, sha)
            self.outbox.set_commit_sha(commit_op, sha)
            return {"sha": sha}

        def probe_commit():
            """已存在的提交（含崩在 after-call 的情形）必须被认出来。"""
            sha = self.outbox.commit_sha(commit_op) or \
                self.wt.operation_commit_sha(op.operation_id)
            if not sha:
                return None
            self.outbox.set_commit_sha(op, sha)
            self.outbox.set_commit_sha(commit_op, sha)
            return {"sha": sha}

        self.outbox.execute(commit_op, call=do_commit, probe=probe_commit)
        self.wt.operation_sha = self.outbox.commit_sha(op)
        if stop_after == "commit":
            return {"issue": number, "state": State.COMMITTED_LOCAL}

        push_op = self.outbox.prepare(
            self.round_id, "push_main", op.operation_id,
            {"issue": number, "path": rel_path})
        self.outbox.execute(
            push_op,
            call=lambda: (self.wt.push(), {"pushed": True})[1],
            probe=lambda: ({"pushed": True}
                           if self.wt.remote_has_operation(
                               op.operation_id, rel_path) else None))
        if stop_after == "push":
            return {"issue": number, "state": State.PUBLISHED}

        receipt_op = self.outbox.prepare(
            self.round_id, "publication_receipt", op.operation_id,
            {"issue": number, "path": rel_path})
        receipt_body = (f"{RECEIPT_MARKER}\n"
                        f"round={self.round_id}\n"
                        f"{marker}\n"
                        f"proposal={rel_path}\n"
                        f"state={State.PUBLISHED}\n")
        self.outbox.execute(
            receipt_op,
            call=lambda: self.gh.create_comment(number, receipt_body),
            probe=lambda: self.gh.find_comment_by_marker(number, marker))

        issue = self.gh.find_issue_by_marker(marker)
        facts = self.collect_facts(op.operation_id, issue, rel_path,
                                   candidate["labels"])
        state = lifecycle.derive(facts)
        if state == State.RECEIPT_COMPLETE:
            # 只有收据校验通过，整条发布事务才算收敛
            for finished in (commit_op, push_op, receipt_op, op):
                self.outbox.settle(finished)
        return {"issue": number, "state": state}

    @staticmethod
    def _card(candidate: dict, number: int, operation_id: str) -> str:
        return (f"# 提案 #{number}：{candidate['title']}\n\n"
                f"> 由 scrollz harness 自动生成。lane={candidate['lane']}\n"
                f"> {OP_MARKER}{operation_id}\n\n"
                f"{candidate['body_md']}\n")
