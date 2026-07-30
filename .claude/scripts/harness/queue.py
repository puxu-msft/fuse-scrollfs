"""队列治理（spec §十二）。

两级去重（Stage 1a 范围）：精确指纹硬拦，`classify()` 只产出
`"new"` / `"exact_duplicate"` / `"rejected_active"` 三种。语义相似度判定
（`possible_duplicate`）已明确划归 Stage 1b（`docs/harness/plan-stage1b.md`
B2），不在本模块接口范围内，故意不声明、不实现。
reconsider_when 必须是 typed 谓词，否则「自动失效」无从实现。
"""

from __future__ import annotations

import datetime as dt
import hashlib
import re
import sqlite3
import time

_WS = re.compile(r"\s+")
_HEX_SHA = re.compile(r"^[0-9a-f]{40}([0-9a-f]{24})?$")  # 40 位 SHA-1 或 64 位 SHA-256
_POSITIVE_INT = re.compile(r"^[1-9][0-9]*$")
_NONNEG_INT = re.compile(r"^(0|[1-9][0-9]*)$")


def _norm(text: str) -> str:
    return _WS.sub(" ", text.strip().lower())


def fingerprint(goal: str, invariant: str, primary_path: str, oracle: str) -> str:
    blob = "\x1f".join(_norm(x) for x in (goal, invariant, primary_path, oracle))
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()[:32]


class Queue:
    def __init__(self, conn: sqlite3.Connection):
        self.conn = conn

    def record(self, fp: str, lane: str, title: str, state: str,
               issue_number: int | None = None,
               reconsider_when: str | None = None) -> None:
        """插入或更新一条 proposal。

        `fingerprint` 与 `created_at` 是不可变字段——同一 fingerprint 的更新
        必须保留首次写入时的 `created_at`（评审 Important）。`INSERT OR
        REPLACE` 会先删后插，等价于重置这些字段并抹掉未在本次调用中传入的
        `issue_number`/`reconsider_when`；改用 `ON CONFLICT DO UPDATE`，只在
        `state` 真的发生变化时才推进 `decided_at`。
        """
        now = time.time()
        self.conn.execute(
            "INSERT INTO proposals(fingerprint, lane, title, state,"
            " issue_number, reconsider_when, decided_at, created_at)"
            " VALUES(?,?,?,?,?,?,?,?)"
            " ON CONFLICT(fingerprint) DO UPDATE SET"
            "   lane=excluded.lane,"
            "   title=excluded.title,"
            "   issue_number=COALESCE(excluded.issue_number, proposals.issue_number),"
            "   reconsider_when=COALESCE(excluded.reconsider_when,"
            "                            proposals.reconsider_when),"
            "   decided_at=CASE WHEN proposals.state != excluded.state"
            "                   THEN excluded.decided_at ELSE proposals.decided_at END,"
            "   state=excluded.state",
            (fp, lane, title, state, issue_number, reconsider_when, now, now))

    def _get(self, fp: str) -> sqlite3.Row | None:
        return self.conn.execute(
            "SELECT * FROM proposals WHERE fingerprint=?", (fp,)).fetchone()

    def classify(self, candidate: dict) -> str:
        """返回 `"new"` / `"exact_duplicate"` / `"rejected_active"` 之一。

        （`possible_duplicate` 属 Stage 1b 扩展接口，见模块 docstring，本
        方法故意不产出。）
        """
        row = self._get(candidate["fingerprint"])
        if row is None:
            return "new"
        if row["state"] in ("rejected", "closed-by-user"):
            return "new" if self.reconsider_ready(
                candidate["fingerprint"], candidate.get("ctx", {})) \
                else "rejected_active"
        return "exact_duplicate"

    def reconsider_ready(self, fp: str, ctx: dict) -> bool:
        row = self._get(fp)
        if row is None or not row["reconsider_when"]:
            return False
        cond = row["reconsider_when"]
        kind, _, arg = cond.partition(":")
        if kind == "not_before":
            try:
                return dt.date.today() >= dt.date.fromisoformat(arg)
            except ValueError:
                return False
        if kind == "main_sha_changed":
            # arg 须是合法十六进制 Git SHA（40 位 SHA-1 或 64 位 SHA-256），
            # 否则视为不可机器判定：不允许用任意字符串绕过判定。
            if not _HEX_SHA.match(arg):
                return False
            main_sha = ctx.get("main_sha")
            if not main_sha or not _HEX_SHA.match(str(main_sha)):
                return False
            return main_sha != arg
        if kind == "dependency_issue_closed":
            # arg 须是大于 0 的正整数 Issue 号；ctx 侧同样规范成整数集合再比较，
            # 避免空字符串、负数、带符号写法之类的绕过。
            if not _POSITIVE_INT.match(arg):
                return False
            closed_ids: set[int] = set()
            for n in ctx.get("closed_issues", []):
                try:
                    closed_ids.add(int(str(n)))
                except (TypeError, ValueError):
                    continue
            return int(arg) in closed_ids
        if kind == "decision_version_gt":
            # arg 须是非负整数；版本号 0 语义为「尚未产生任何决策版本」。
            if not _NONNEG_INT.match(arg):
                return False
            try:
                return int(ctx.get("decision_version", 0)) > int(arg)
            except (TypeError, ValueError):
                return False
        # 无法机器判定：只能人工复议
        return False

    def lane_full(self, lane: str, cap: int) -> bool:
        n = self.conn.execute(
            "SELECT COUNT(*) AS n FROM proposals WHERE lane=? AND state='proposed'",
            (lane,)).fetchone()["n"]
        return n >= cap

    def total_full(self, cap: int) -> bool:
        n = self.conn.execute(
            "SELECT COUNT(*) AS n FROM proposals WHERE state='proposed'"
        ).fetchone()["n"]
        return n >= cap
