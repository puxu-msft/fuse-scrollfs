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
            """已存在的提交必须被现场核验，不能只信 SQLite 缓存的 SHA。

            `wt.operation_commit_sha()` 对**当前** worktree 跑 `git log`，
            只有该 SHA 仍在当前分支历史里可达时才算数——这就是"commit 对象
            确实存在"的现场核验（评审 Critical 修法 1）。查不到时一律返回
            None，绝不退回未经核验的 SQLite 缓存值，否则一个已随 worktree
            丢失的旧 SHA 会被当成"已提交"，进而在 push 阶段把裸 HEAD
            当成"已推送"（评审复现的正是这条链路）。
            """
            sha = self.wt.operation_commit_sha(op.operation_id)
            if not sha:
                return None
            self.outbox.set_commit_sha(op, sha)
            self.outbox.set_commit_sha(commit_op, sha)
            return {"sha": sha}

        # `outbox.execute()` 对 phase 已是 observed/settled 的 operation 会
        # 直接短路返回缓存结果，不会重新探测——这是它的既定契约（本模块不改
        # `outbox.py`）。但缓存里的 SHA 可能已不再是当前仓库里真实存在的
        # 对象：崩在本地 commit 之后、push 之前时 `_publish` worktree 可能
        # 被删或丢失，新建的 worktree 从 origin/main 重新检出，旧 SHA 不再
        # 可达（评审 Critical）。因此在委托给 `outbox.execute()` 之前，先
        # 显式核验一次：只有当 SQLite 认为已提交、但现场核验不到该提交时，
        # 才需要绕过短路、从 root 的完整 payload 重新生成提案提交（评审
        # Critical 修法 3）。
        #
        # 这里不会造成远端重复提案：若该 operation 已经推送成功，`ensure()`
        # 重建的 worktree 会从 origin/main 检出，而 origin/main 此时已包含
        # 那个提交，`operation_commit_sha()` 会在新 worktree 里重新找到它
        # （它现在是主线历史的一部分），从而不会触发再生成。
        cached_sha = self.outbox.commit_sha(commit_op)
        if cached_sha and not self.wt.operation_commit_sha(op.operation_id):
            do_commit()
        else:
            self.outbox.execute(commit_op, call=do_commit, probe=probe_commit)
        self.wt.operation_sha = self.outbox.commit_sha(op)
        if stop_after == "commit":
            return {"issue": number, "state": State.COMMITTED_LOCAL}

        push_op = self.outbox.prepare(
            self.round_id, "push_main", op.operation_id,
            {"issue": number, "path": rel_path})
        # push 之前必须确认当前 HEAD 确实包含该 operation 绑定的提交——
        # 不能仅凭"上面刚做过核验"就假定成立，任何遗漏路径都不能把一个
        # 不含提案卡的 HEAD 当作"待推送"去 push（评审 Critical 修法 2）。
        if not self.wt.operation_commit_sha(op.operation_id):
            raise RuntimeError(
                f"push 前置核验失败：operation {op.operation_id} 的提交在"
                f"当前 worktree 历史中不可达，拒绝 push 一个不含提案卡的 HEAD")
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
