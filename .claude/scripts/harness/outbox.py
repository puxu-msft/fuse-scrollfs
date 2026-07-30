"""所有外部副作用的唯一执行入口（spec §六）。

绕过本模块的直连 gh/git 写调用是缺陷：崩溃矩阵由本模块的 operation 清单生成，
未经登记的副作用不会被测试覆盖。
"""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import time
import uuid
from dataclasses import dataclass
from typing import Callable


class ResponseLost(Exception):
    """外部调用结果不确定：可能已在服务端生效。禁止盲重试。"""


class OperationConflict(Exception):
    """同一 natural key 对应不同 payload：不得复用旧结果。"""


class InjectedFault(Exception):
    """HARNESS_FAULT 触发的确定性崩溃，仅用于恢复验收。"""


def _fault_check(kind: str, phase: str) -> None:
    """测试专用崩溃开关：HARNESS_FAULT=<kind>:<phase>。

    只读进程环境变量，**不接受**任何来自模型输出或仓库文本的输入。
    phase ∈ before-call | after-call | after-observe
    """
    spec = os.environ.get("HARNESS_FAULT")
    if not spec:
        return
    want_kind, _, want_phase = spec.partition(":")
    if want_kind == kind and want_phase == phase:
        raise InjectedFault(f"注入崩溃于 {kind}:{phase}")


@dataclass
class Operation:
    operation_id: str
    round_id: str
    kind: str
    natural_key: str
    payload: dict
    phase: str
    result: dict | None


def _hash(payload: dict) -> str:
    blob = json.dumps(payload, sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()


class Outbox:
    SUB_KINDS = ("commit_proposal", "push_main", "publication_receipt")

    def __init__(self, conn: sqlite3.Connection):
        self.conn = conn

    def get(self, kind: str, natural_key: str) -> Operation | None:
        row = self.conn.execute(
            "SELECT * FROM operations WHERE kind=? AND natural_key=?",
            (kind, natural_key)).fetchone()
        return self._row_to_op(row) if row else None

    def prepare(self, round_id: str, kind: str, natural_key: str,
                payload: dict) -> Operation:
        # 冻结隐式父指针：子 operation 的 natural_key 必须等于 root 的 operation_id
        if kind in self.SUB_KINDS:
            parent = self.conn.execute(
                "SELECT 1 FROM operations WHERE operation_id=? AND kind=?",
                (natural_key, "publish_proposal")).fetchone()
            if parent is None:
                raise OperationConflict(
                    f"{kind} 的 natural_key 必须是 publish_proposal 的 "
                    f"operation_id，收到 {natural_key!r}")
        existing = self.get(kind, natural_key)
        if existing is not None:
            row = self.conn.execute(
                "SELECT payload_hash FROM operations WHERE operation_id=?",
                (existing.operation_id,)).fetchone()
            if row["payload_hash"] != _hash(payload):
                raise OperationConflict(
                    f"{kind}/{natural_key} 的 payload 与既有记录不一致，拒绝复用")
            return existing
        now = time.time()
        op_id = uuid.uuid4().hex
        self.conn.execute(
            "INSERT INTO operations(operation_id, round_id, kind, natural_key,"
            " payload_json, payload_hash, phase, created_at, updated_at)"
            " VALUES(?,?,?,?,?,?,'prepared',?,?)",
            (op_id, round_id, kind, natural_key,
             json.dumps(payload, ensure_ascii=False), _hash(payload), now, now))
        return Operation(op_id, round_id, kind, natural_key, payload,
                         "prepared", None)

    def execute(self, op: Operation,
                call: Callable[[], dict | None],
                probe: Callable[[], dict | None]) -> dict | None:
        if op.phase in ("observed", "settled"):
            return op.result
        # 重入（prepared / failed_retryable）：先 probe 再决定是否 call。
        # 「artifact 已存在」不等于「事务已收敛」——崩在 after-call 时副作用
        # 已落地而 op 仍是 prepared，直接重发会失败（如 git commit 报
        # nothing to commit），probe-first 把它正式推进到 observed。
        probed_first = probe()
        if probed_first is not None:
            self._mark(op, "observed", probed_first)
            return probed_first
        _fault_check(op.kind, "before-call")
        try:
            result = call()
            _fault_check(op.kind, "after-call")
        except ResponseLost:
            probed = probe()
            if probed is not None:
                self._mark(op, "observed", probed)
                return probed
            self._mark(op, "failed_retryable", None)
            raise
        self._mark(op, "observed", result)
        _fault_check(op.kind, "after-observe")
        return result

    def unresolved(self) -> list[Operation]:
        """reconcile 之后仍无法判定的 operation。预检据此 fail closed。"""
        rows = self.conn.execute(
            "SELECT * FROM operations WHERE phase='failed_terminal'").fetchall()
        return [self._row_to_op(r) for r in rows]

    def reconcile(self, probes: dict) -> list[Operation]:
        rows = self.conn.execute(
            "SELECT * FROM operations WHERE phase IN"
            " ('prepared','failed_retryable')").fetchall()
        still_open = []
        for row in rows:
            op = self._row_to_op(row)
            probe = probes.get(op.kind)
            if probe is None:
                still_open.append(op)
                continue
            observed = probe(op)
            if observed is not None:
                self._mark(op, "observed", observed)
            else:
                still_open.append(op)
        return still_open

    def open_operations(self) -> list[Operation]:
        rows = self.conn.execute(
            "SELECT * FROM operations WHERE phase IN"
            " ('prepared','failed_retryable')").fetchall()
        return [self._row_to_op(r) for r in rows]

    def root_of(self, op: Operation) -> Operation:
        if op.kind == "publish_proposal":
            return op
        row = self.conn.execute(
            "SELECT * FROM operations WHERE operation_id=? AND kind=?",
            (op.natural_key, "publish_proposal")).fetchone()
        if row is None:
            raise KeyError(f"{op.kind}/{op.natural_key} 找不到所属 publish_proposal")
        return self._row_to_op(row)

    def open_roots(self) -> list[Operation]:
        """尚未收敛的发布事务：root 只在收据校验通过后才 settled。"""
        rows = self.conn.execute(
            "SELECT * FROM operations WHERE kind='publish_proposal'"
            " AND phase NOT IN ('settled','failed_terminal')").fetchall()
        roots = {r["operation_id"]: self._row_to_op(r) for r in rows}
        for op in self.open_operations():
            root = self.root_of(op)
            roots.setdefault(root.operation_id, root)
        return list(roots.values())

    def unpushed_commits(self) -> list[Operation]:
        rows = self.conn.execute(
            "SELECT c.* FROM operations c"
            " WHERE c.kind='commit_proposal' AND c.commit_sha IS NOT NULL"
            " AND NOT EXISTS ("
            "   SELECT 1 FROM operations p"
            "   WHERE p.kind='push_main' AND p.natural_key=c.natural_key"
            "     AND p.phase IN ('observed','settled'))").fetchall()
        return [self._row_to_op(r) for r in rows]

    def set_commit_sha(self, op: Operation, sha: str) -> None:
        self.conn.execute(
            "UPDATE operations SET commit_sha=?, updated_at=? WHERE operation_id=?",
            (sha, time.time(), op.operation_id))

    def commit_sha(self, op: Operation) -> str | None:
        row = self.conn.execute(
            "SELECT commit_sha FROM operations WHERE operation_id=?",
            (op.operation_id,)).fetchone()
        return row["commit_sha"] if row else None

    def settle(self, op: Operation) -> None:
        self._mark(op, "settled", op.result)

    def _mark(self, op: Operation, phase: str, result: dict | None) -> None:
        self.conn.execute(
            "UPDATE operations SET phase=?, result_json=?, updated_at=?"
            " WHERE operation_id=?",
            (phase, json.dumps(result, ensure_ascii=False) if result is not None
             else None, time.time(), op.operation_id))
        op.phase = phase
        op.result = result

    @staticmethod
    def _row_to_op(row: sqlite3.Row) -> Operation:
        return Operation(
            operation_id=row["operation_id"], round_id=row["round_id"],
            kind=row["kind"], natural_key=row["natural_key"],
            payload=json.loads(row["payload_json"]), phase=row["phase"],
            result=json.loads(row["result_json"]) if row["result_json"] else None)
