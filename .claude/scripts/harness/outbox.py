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


class TerminalOperationError(Exception):
    """确定性外部失败：重试无意义，需人工介入。

    与 `ResponseLost`（结果不确定，可能已生效，允许重试/probe）互斥：
    这类失败已确定**未生效**且再次以相同 payload 调用会得到相同拒绝
    （例如 GitHub 422 业务校验拒绝）。`Outbox.execute()` 捕获后把
    operation 原子标记为 `failed_terminal` 并原样重新抛出——绝不吞掉，
    也绝不当作可重试的 `failed_retryable`。
    """


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
    # 结果不确定的跨轮次有界观察窗（评审 Critical A）：阴性读取不授权重发，
    # 只记录『已观察次数、首次观察时刻』。持久化在 result_json 的保留信封
    # 里（见 `_UNCERTAIN_KEY`），不新增 DB 列、不改变既有 phase 取值语义。
    uncertain_observations: int = 0
    uncertain_first_seen_at: float | None = None


# result_json 内的保留信封键：把『结果不确定』的观察计数与首次观察时刻
# 存进本来就有的 result_json 列（该列对 failed_retryable 状态原本恒为
# NULL），避免为此新增 DB 列。反序列化时一旦命中该键，`Operation.result`
# 保持 None（该信封不是真实外部结果），与既有语义一致。
_UNCERTAIN_KEY = "__uncertain__"


def _hash(payload: dict) -> str:
    blob = json.dumps(payload, sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()


class Outbox:
    SUB_KINDS = ("commit_proposal", "push_main", "publication_receipt")

    # 崩溃矩阵机器可派生的来源（spec §十四 14.2）：Stage 1 的全部 operation
    # kind 与 `_fault_check()` 认识的全部相位。测试必须从这两个常量的笛卡尔
    # 积生成覆盖表，不得再手写清单——否则生产新增 operation 而测试忘记同步
    # 时，测试仍会全绿（评审 Critical B）。
    OPERATION_KINDS = ("publish_proposal", "commit_proposal",
                       "push_main", "publication_receipt")
    FAULT_PHASES = ("before-call", "after-call", "after-observe")

    # 结果不确定的跨轮次有界观察窗（评审 Critical A）：连续这么多次『阴性
    # 读取』（探测不到已生效的证据）之后仍不确定，才转 `failed_terminal`
    # 交人工介入。窗口内的任何阴性读取都**不得**授权重发 `call()`——那正是
    # 会把『索引/端点暂不可见』误判为『确定未创建』、进而对同一提案发出
    # 第二次 `create_issue` 的失效模式。
    UNCERTAIN_WINDOW = 3

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
                # 去向决定（评审 Important-1 附带项）：payload 冲突同样是需要
                # 人工介入、重试无意义的确定性失败——盲目复用旧结果不安全，
                # 而反复 prepare() 只会一直撞同一个 OperationConflict。选择
                # 复用 `failed_terminal`（而非另设哨兵状态），使其自动纳入
                # 既有的 `unresolved()` / 预检 `outbox_resolved` 闸门，无需
                # 再让 precheck/reconcile 认识一个新状态。
                #
                # 只标记冲突所在的 operation 及其 root 事务，且仅当尚未
                # `settled`——已收敛的发布不得被这类冲突倒着改写历史（后到
                # 的冲突 payload 不能追溯性地推翻一次已核验的成功发布）。
                if existing.phase != "settled":
                    self._mark(existing, "failed_terminal", None)
                    root = self.root_of(existing)
                    if root.operation_id != existing.operation_id \
                            and root.phase != "settled":
                        self._mark(root, "failed_terminal", None)
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
        if op.phase == "failed_retryable":
            # 评审 Critical A：本 operation 此前已经历过一次 ResponseLost
            # （唯一会把 phase 置为 failed_retryable 的路径），且这次重入的
            # 探测**再次**是阴性。任何阴性读取都不得立即授权重发底层
            # `call()`——一次探测阴性无法证明『确实未创建』，可能只是探测
            # 本身暂不可靠（索引/端点的最终一致性窗口）。因此这里只累计
            # 观察次数，继续等待；只有跨轮次窗口耗尽仍拿不到确凿证据，才
            # 转 `failed_terminal` 交人工介入，绝不猜测式地重发。
            updated = self.record_uncertain_observation(op)
            if updated.uncertain_observations >= self.UNCERTAIN_WINDOW:
                self._mark(op, "failed_terminal", None)
                raise TerminalOperationError(
                    f"{op.kind}/{op.natural_key} 结果历经"
                    f" {updated.uncertain_observations} 次跨轮次阴性读取仍"
                    f"不确定，判定为需人工介入（拒绝盲目重发）")
            raise ResponseLost(
                f"{op.kind}/{op.natural_key} 仍处于观察窗内"
                f"（第 {updated.uncertain_observations} 次阴性读取），暂不重发")
        _fault_check(op.kind, "before-call")
        # 持久化『mutation 边界已越过』（根因修复，2026-07-31）：在真正调用
        # 外部 `call()` 之前，把 op 从 `prepared` 转成 `failed_retryable`
        # 信封（0 次观察）。选型：复用既有 `failed_retryable`/uncertain 信封
        # 而非新增 `call_started` phase——`db.py` 不在本次改动白名单内，新增
        # phase 需要改 CHECK 约束枚举（DB 迁移），复用信封则无需触碰 db.py。
        #
        # 动机：`prepared` 同时表示『打算调用』与『尚未调用』，`execute()`
        # 只在 `phase=='failed_retryable'` 时才会走『阴性读取只累计观察、
        # 绝不重发 call()』的分支；若崩溃/未捕获异常发生在 call() 成功
        # 之后、`_mark(observed)` 之前，op 若仍停在 `prepared`，下一轮的
        # 通用重入 probe 一旦阴性（探测延迟/最终一致性窗口，或恢复 probe
        # 自身抛异常导致 `record_uncertain_observation()` 根本执行不到）
        # 就会被直接放行去重新调用 `call()`，造成重复副作用（如二次
        # `create_issue`）。提前到这里持久化，使得此后任何路径的阴性读取
        # 都必须先过『累计观察、窗口耗尽才转 failed_terminal』这道闸门。
        self._mark_call_boundary_crossed(op)
        try:
            result = call()
            _fault_check(op.kind, "after-call")
        except ResponseLost:
            probed = probe()
            if probed is not None:
                self._mark(op, "observed", probed)
                return probed
            self.record_uncertain_observation(op)
            raise
        except TerminalOperationError:
            # 确定性失败：重试无意义，需人工介入。原子标记后原样重新抛出——
            # 绝不吞掉（不然调用方会误以为本次调用成功返回了 None），也绝
            # 不降级为 failed_retryable（那会让恢复路径每轮重试同一个必然
            # 失败的动作，见评审 Important-1）。
            self._mark(op, "failed_terminal", None)
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
                continue
            if op.phase == "failed_retryable":
                # 与 `execute()` 同一策略：结果曾经不确定、这次探测又是阴性，
                # 只累计观察次数。**这条分支不能少**——若 reconcile 只是把 op
                # 放回 still_open 而不喂观察计数，一个始终经 reconcile 而从不
                # 经 execute 被重访的 operation 会永远开着、撞不到窗口上限，
                # 于是"窗口耗尽转人工"这道闸门被整条路径绕过。
                updated = self.record_uncertain_observation(op)
                if updated.uncertain_observations >= self.UNCERTAIN_WINDOW:
                    self._mark(op, "failed_terminal", None)
                    continue
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

    def _mark_call_boundary_crossed(self, op: Operation) -> None:
        """在调用外部 `call()` 之前持久化『mutation 边界已越过』（根因修复，
        2026-07-31）：把 op 从 `prepared` 转成 `failed_retryable`、
        `uncertain_observations=0` 的信封。

        与 `record_uncertain_observation()` 共用同一个保留信封格式，仅
        `observations` 初值为 0、`first_seen_at` 为 None（尚无阴性观察，
        只是『即将/已经调用外部副作用，结果尚不确定』）。这样此后任何路径
        （`execute()` 重入、`reconcile()`）读到该 op 时，`phase` 都已经是
        `failed_retryable`，会强制先走『先 probe，阴性只累计观察、绝不
        授权重发 `call()`』这条路径——即使本次崩溃发生在真正调用 `call()`
        成功之后、`_mark(observed)` 之前（真实进程崩溃、或恢复 probe 自身
        抛出异常导致 `record_uncertain_observation()` 未被执行到），也不会
        因为 op 仍显示为 `prepared` 而被误判为『从未越过 mutation 边界』
        进而重新发起一次 `call()`。

        `test_prepared_ops_are_not_counted_as_uncertain` 固化的语义不受
        影响：那个用例里的 op 从未进入 `execute()`（只 `prepare()` 过），
        本方法从未被调用，phase 仍是 `prepared`。
        """
        now = time.time()
        envelope = {_UNCERTAIN_KEY: True, "observations": 0,
                   "first_seen_at": None}
        self.conn.execute(
            "UPDATE operations SET phase='failed_retryable', result_json=?,"
            " updated_at=? WHERE operation_id=?",
            (json.dumps(envelope, ensure_ascii=False), now, op.operation_id))
        op.phase = "failed_retryable"
        op.result = None
        op.uncertain_observations = 0
        op.uncertain_first_seen_at = None

    def record_uncertain_observation(self, op: Operation) -> Operation:
        """结果不确定后的阴性读取只增加观察计数，绝不授权重发（评审
        Critical A）。持久化『已观察次数、首次观察时刻』，跨轮次累计——
        `Outbox.execute()`/`reconcile()` 崩溃重启后仍能读回同一计数，不会
        把窗口重置为 0 从而无限期地"每轮都当作第一次"。

        phase 维持 `failed_retryable`（DB CHECK 约束枚举不变，不新增
        db.py 迁移）：这类 operation 本就需要下一轮 reconcile/execute 重新
        尝试判定，与既有『可重试』语义一致；真正决定"窗口耗尽转
        failed_terminal"的判定逻辑由调用方按 `uncertain_observations` 与
        自己的窗口上限比较后调用 `_mark(op, 'failed_terminal', None)`。
        """
        now = time.time()
        first_seen = (op.uncertain_first_seen_at
                      if op.uncertain_first_seen_at is not None else now)
        count = op.uncertain_observations + 1
        envelope = {_UNCERTAIN_KEY: True, "observations": count,
                   "first_seen_at": first_seen}
        self.conn.execute(
            "UPDATE operations SET phase='failed_retryable', result_json=?,"
            " updated_at=? WHERE operation_id=?",
            (json.dumps(envelope, ensure_ascii=False), now, op.operation_id))
        op.phase = "failed_retryable"
        op.result = None
        op.uncertain_observations = count
        op.uncertain_first_seen_at = first_seen
        return op

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
        raw_result = (json.loads(row["result_json"])
                     if row["result_json"] else None)
        # 保留信封解包：`result_json` 里若是 `record_uncertain_observation()`
        # 写入的信封（而非真实外部调用结果），拆成 `uncertain_observations`/
        # `uncertain_first_seen_at`，`result` 保持 None——语义上这从来不是
        # 一次成功的外部结果。
        if isinstance(raw_result, dict) and raw_result.get(_UNCERTAIN_KEY):
            return Operation(
                operation_id=row["operation_id"], round_id=row["round_id"],
                kind=row["kind"], natural_key=row["natural_key"],
                payload=json.loads(row["payload_json"]), phase=row["phase"],
                result=None,
                uncertain_observations=raw_result["observations"],
                uncertain_first_seen_at=raw_result["first_seen_at"])
        return Operation(
            operation_id=row["operation_id"], round_id=row["round_id"],
            kind=row["kind"], natural_key=row["natural_key"],
            payload=json.loads(row["payload_json"]), phase=row["phase"],
            result=raw_result)
