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

# canonical key 的规范化必须与 `.claude/workflows/scrollz-propose.js` 的
# `canonicalKey()` **逐字节一致**——key 由 Python 产出、由 JS 比对，两侧算不出同一
# 个串，跨轮去重就静默失效（不报错、不告警，只是每轮重提同一候选）。
#
# JS 那边是 `x.trim().toLowerCase().replace(/\s+/g, ' ')`，这里逐步镜像它。
# 两处**不能**用 Python 的自带语义（跨语言测试各抓出一次真实漂移）：
#   1. 不能用 `re` 的 `\s`：它匹配 `\x1c`–`\x1f`，ECMAScript 的 `\s` 不匹配。
#      而 `\x1f` 恰恰是本模块拼接四个字段用的分隔符本身。
#   2. 不能用 `str.strip()`：JS 的 `trim()` 去掉 `\ufeff`（BOM），Python 的
#      `strip()` 不认它，于是 `"BOM\ufeff"` 两侧分别得到 `bom ` 与 `bom`。
#
# 已知残余差异（记录而非修复）：`toLowerCase()` 与 `str.lower()` 对少数字符不同
# （如 `ß`、土耳其语 `İ`）。出现在这四个字段里的概率极低，且真出现时跨语言测试
# 会抓到——加样本即可，不必现在引入完整的 Unicode 大小写映射。
_JS_SPACE = ("\u0020\t\n\r\f\v\u00a0\u1680\u2000-\u200a"
             "\u2028\u2029\u202f\u205f\u3000\ufeff")
_WS = re.compile(f"[{_JS_SPACE}]+")
_WS_EDGE = re.compile(f"^[{_JS_SPACE}]+|[{_JS_SPACE}]+$")


def _norm(text: str) -> str:
    return _WS.sub(" ", _WS_EDGE.sub("", text).lower())


def canonical_key(goal: str, invariant: str, primary_path: str,
                  oracle: str) -> str:
    """候选的规范化原文 key —— 就是 `fingerprint()` 做 sha256 **之前**的那个 blob。

    控制器**自己算**，绝不消费模型返回的 `canonical_key` 字段（评审 rmf-13）：
    那个字段是 Workflow 附加、再穿过外层模型一次"原样回显"才到达控制器的，
    `validate_candidate()` 对它零校验，而 `remember_canonical_key` 遇空值静默返回。
    三者叠加 = 模型少抄一个字段，跨轮去重就永久失效，且无日志、无计数、无测试
    能发现。更糟的是它可被构造：模型给一个精心挑选的 key，就能**永久抑制**某个
    合法方向。这与"labels 一律由控制器确定性派生"是同一条原则。

    四个入参都在 `_REQUIRED_CANDIDATE_FIELDS` 里，DTO 校验已保证是非空字符串，
    所以"记不住"在结构上不可能发生。
    """
    return "\x1f".join(_norm(x) for x in (goal, invariant, primary_path, oracle))


def fingerprint(goal: str, invariant: str, primary_path: str, oracle: str) -> str:
    blob = canonical_key(goal, invariant, primary_path, oracle)
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

    def remember_canonical_key(self, fp: str, canonical_key: str | None) -> None:
        """记住某提案的 canonical key，供后续轮次跨轮去重（评审 rmf-02）。

        `canonical_key` 缺失时**跳过而不是报错**：少一条去重记忆是退化，把一次
        本该成功的发布变成异常则是事故。

        首写为准（`INSERT OR IGNORE`）：同一 fingerprint 的 canonical key 按定义
        不会变（两者都由同四个字段导出），若真的对不上，那是上游出了问题，此时
        保留最早的记录比让它被静默覆盖更容易查。
        """
        if not canonical_key:
            return
        self.conn.execute(
            "INSERT OR IGNORE INTO proposal_keys(fingerprint, canonical_key,"
            " created_at) VALUES(?,?,?)",
            (fp, canonical_key, time.time()))
        self.conn.commit()

    def known_canonical_keys(self) -> list[str]:
        """在册提案的 canonical key 集合，用于让 finder 跳过已提过的候选。

        只取 `state='proposed'` 的在册提案：已关闭的提案是否可以重提，属于拒绝
        记忆的语义（Stage 1b B2），不在这里替它做决定。

        JOIN 而非直接全表取：`proposal_keys` 是纯追加的，提案被删/被换状态后它的
        行仍在，直接取会把早已不在册的 key 也塞进去重集，从而**永久**屏蔽掉一个
        本该可以重提的方向。
        """
        rows = self.conn.execute(
            "SELECT k.canonical_key FROM proposal_keys AS k"
            " JOIN proposals AS p ON p.fingerprint = k.fingerprint"
            " WHERE p.state = 'proposed'"
            " ORDER BY k.created_at").fetchall()
        return [r[0] for r in rows]

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
