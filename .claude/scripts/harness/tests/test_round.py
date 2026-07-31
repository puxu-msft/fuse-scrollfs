import subprocess, tempfile, unittest
from pathlib import Path
from unittest import mock
from harness import db
from harness.claude_runner import InvocationResult
from harness.config import GIT
from harness.gitops import PublishWorktree
from harness.outbox import Outbox
from harness.publish import Publisher
from harness.queue import Queue, canonical_key, fingerprint
from harness import round as round_module
from harness.round import Deps, run_round
from harness.tests.fakes import FakeGitHub

# 与 `scrollz-propose.js` `pickCandidateFields()` 的真实输出形状一致：
# **不含** `labels` 字段——finder/judge 的输出 schema 里也没有这个字段，
# labels 完全由控制器（`round._derive_labels`）根据 lane/priority/size/
# needs_decision 确定性派生。fixture 若沿用旧的、自带完整 labels 的假数据，
# 会掩盖真实 DTO 接缝（评审 Important #3）。
CANDIDATE_PAYLOAD = {
    "candidates": [{
        "title": "archive: 尾日志加 per-record CRC",
        "lane": "defect", "goal": "补 CRC", "invariant": "尾日志完整性",
        "primary_path": "crates/scrollz/src/archive.rs",
        "oracle": "翻转一个字节后读取必须 fail-closed",
        "slug": "tail-journal-crc", "size": "M", "priority": "T1",
        "needs_decision": False, "body_md": "## 意图\n补 CRC\n",
    }]
}

# Stage 1 允许的工具集，用于让 InvocationResult 通过能力漂移校验
# （评审 Important #4）。真实测试里若不传，`init_tools` 默认空列表，
# `_capability_drift_problems()` 会正确判定漂移——这里显式传入干净能力集，
# 代表「本次调用没有配置漂移」的正常场景。
_CLEAN_INIT_TOOLS = sorted(round_module.STAGE1_TOOLS.split(","))


def _clean_invocation(ok, payload, cost, turns, **kw):
    # 「干净的 stream」按定义包含一个可解析的终态 result 事件，因此成本必然
    # 已知。假件默认 cost_known=False 会让失败轮的结算路径测不到真实行为
    # ——真实的「成本未知」只发生在超时/进程被杀（那种情形直接构造
    # InvocationResult，不走本 helper）。
    kw.setdefault("cost_known", True)
    kw.setdefault("init_seen", True)
    kw.setdefault("init_tools", _CLEAN_INIT_TOOLS)
    kw.setdefault("init_mcp_servers", [])
    kw.setdefault("init_plugins", [])
    kw.setdefault("init_errors", [])
    return InvocationResult(ok, payload, cost, turns, **kw)


class Cfg:
    def __init__(self, root):
        self.repo_root = root
        # 与真实 Config 一致：state_db 决定 stream 日志落在哪。测试假件漏掉真实
        # 配置字段时，接缝缺陷只会在真机才暴露——这里就是一次（新增 stream_log
        # 落盘后 24 个测试同时 AttributeError）。
        self.state_db = Path(root) / ".claude/state/harness.db"
        self.gh_token = "tok"
        self.round_budget_usd = 1.0
        self.daily_budget_usd = 5.0
        self.max_turns = 20
        self.proposed_cap = 20
        self.lane_cap = 6


def run(cwd, *a):
    return subprocess.run([GIT, *a], cwd=cwd, capture_output=True, text=True,
                          check=True).stdout.strip()


class TestRound(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.remote = root / "remote.git"
        self.local = root / "local"
        subprocess.run([GIT, "init", "--bare", "-b", "main", str(self.remote)],
                       check=True, capture_output=True)
        subprocess.run([GIT, "clone", str(self.remote), str(self.local)],
                       check=True, capture_output=True)
        run(self.local, "config", "user.email", "h@e.com")
        run(self.local, "config", "user.name", "h")
        (self.local / "README.md").write_text("seed\n")
        run(self.local, "add", "README.md")
        run(self.local, "commit", "-m", "seed")
        run(self.local, "push", "origin", "main")

        self.conn = db.connect(root / "h.db")
        db.migrate(self.conn)
        self.addCleanup(self.conn.close)
        self.gh = FakeGitHub("WRITE")
        self.cfg = Cfg(self.local)
        self.wt = PublishWorktree(self.local, self.local / ".worktree/_publish")

    def tearDown(self):
        self.tmp.cleanup()

    def _deps(self, invocation):
        return Deps(conn=self.conn, gh=self.gh, worktree=self.wt,
                    outbox=Outbox(self.conn), queue=Queue(self.conn),
                    invoke=lambda **kw: invocation, tools=("/usr/bin/git",))

    def test_successful_round_publishes_and_settles_budget(self):
        inv = _clean_invocation(True, CANDIDATE_PAYLOAD, 0.30, 8)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "published")
        self.assertEqual(len(self.gh.issues), 1)
        row = self.conn.execute("SELECT settled_usd FROM rounds").fetchone()
        self.assertAlmostEqual(row["settled_usd"], 0.30)

    def test_empty_candidates_is_a_clean_noop_round(self):
        inv = _clean_invocation(True, {"candidates": []}, 0.05, 3)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "no-candidate")
        self.assertEqual(len(self.gh.issues), 0)

    def test_failed_invocation_charges_full_reservation(self):
        inv = InvocationResult(False, None, 0.0, 0, exit_code=1)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "invocation-failed")
        day_row = self.conn.execute("SELECT settled_usd FROM budget_days").fetchone()
        self.assertAlmostEqual(day_row["settled_usd"], 1.0,
                               msg="结果未知必须按最坏上限计费")

    def test_needs_decision_candidate_gets_that_label_not_proposed(self):
        payload = {"candidates": [dict(CANDIDATE_PAYLOAD["candidates"][0],
                                       needs_decision=True)]}
        inv = _clean_invocation(True, payload, 0.2, 5)
        run_round(self.cfg, self._deps(inv))
        number = next(iter(self.gh.issues))
        labels = self.gh.get_issue_labels(number)
        self.assertIn("harness:needs-decision", labels)
        self.assertNotIn("harness:proposed", labels)

    def test_published_issue_label_set_is_exactly_derived_not_input(self):
        """控制器必须忽略任何输入 labels，确定性构造
        `[harness, <状态>, <priority>, size:<size>, lane:<lane>]`（评审
        Important #3）。这里故意在 candidate 里夹带一组*错误*的输入 labels
        （包含一个不该存在的 `harness:paused` 状态位与一个虚构辅助
        label），断言最终 label **全集相等**于确定性派生结果，而不是
        只断言状态 label 存在——若实现仍合并输入 labels，这里必然带出
        多余的 `bogus-label`，测试会红。
        """
        candidate = dict(CANDIDATE_PAYLOAD["candidates"][0],
                         labels=["harness:paused", "bogus-label"])
        inv = _clean_invocation(True, {"candidates": [candidate]}, 0.2, 5)
        run_round(self.cfg, self._deps(inv))
        number = next(iter(self.gh.issues))
        labels = self.gh.get_issue_labels(number)
        expected = {"harness", "harness:proposed", "T1", "size:M", "lane:defect"}
        self.assertEqual(set(labels), expected)

    def test_lane_cap_blocks_that_lane_in_next_round(self):
        q = Queue(self.conn)
        for i in range(6):
            q.record(f"fp{i}", "defect", f"t{i}", "proposed")
        inv = _clean_invocation(True, CANDIDATE_PAYLOAD, 0.1, 3)
        deps = self._deps(inv)
        result = run_round(self.cfg, deps)
        self.assertEqual(result["result"], "no-candidate")
        self.assertEqual(len(self.gh.issues), 0, "lane 已满不得再发布该 lane")

    def test_open_root_resumes_even_when_daily_budget_is_exhausted(self):
        """Critical #1 正控：发布在任意相位崩溃 + 日预算刚好耗尽后，下一轮
        仍能恢复并最终收敛。

        复现评审实测反例：旧实现先为新 round 预留预算、再检查
        `open_roots()`——上一轮「模型已花钱、发布未完成」崩溃后，其预留
        仍占满日预算，下一轮直接返回 `budget-exhausted`，恢复路径永远
        到不了。这里先用一个独立的旧 round_id 把日预算刚好占满（模拟
        「已预留但从未结算」的崩溃残留），再制造一个未结 root，断言新一轮
        仍然走 resume 分支、成功收敛，且旧 round 的悬挂预留被正确结算
        （不再占用当日预算）。
        """
        from harness.budget import Budget

        budget = Budget(self.conn, self.cfg.round_budget_usd,
                        self.cfg.daily_budget_usd)
        day = round_module._today()
        # 用足量的旧 round 把日预算刚好占满（每个 round_budget_usd=1.0，
        # daily_budget_usd=5.0，占 5 个）。
        n_fill = int(self.cfg.daily_budget_usd / self.cfg.round_budget_usd)
        for i in range(n_fill):
            budget.reserve(f"orphan-{i}", day)
        with self.assertRaises(Exception):
            budget.reserve("would-overflow", day)

        candidate = dict(CANDIDATE_PAYLOAD["candidates"][0])
        candidate["fingerprint"] = fingerprint(
            candidate["goal"], candidate["invariant"],
            candidate["primary_path"], candidate["oracle"])
        candidate["labels"] = round_module._derive_labels(candidate)
        Publisher(Outbox(self.conn), self.gh, self.wt, Queue(self.conn),
                 "orphan-0").publish(candidate, stop_after="issue")
        self.assertTrue(Outbox(self.conn).open_roots(),
                        "前置状态必须先造出一个未结 root")

        sentinel_invoke = mock.Mock(
            side_effect=AssertionError("恢复轮不得起模型"))
        deps = Deps(conn=self.conn, gh=self.gh, worktree=self.wt,
                   outbox=Outbox(self.conn), queue=Queue(self.conn),
                   invoke=sentinel_invoke, tools=("/usr/bin/git",))

        result = run_round(self.cfg, deps)

        sentinel_invoke.assert_not_called()
        self.assertEqual(result["mode"], "resume")
        self.assertEqual(result["result"], "resumed",
                         "日预算耗尽不得阻断恢复路径")

    def test_precheck_failure_aborts_before_spending(self):
        self.gh.permission = "READ"
        inv = _clean_invocation(True, CANDIDATE_PAYLOAD, 0.3, 8)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "precheck-failed")
        rows = self.conn.execute("SELECT COUNT(*) AS n FROM budget_days").fetchone()
        self.assertEqual(rows["n"], 0, "预检失败不得预留预算")

    def test_capability_drift_fails_closed_and_charges_full_reservation(self):
        """能力漂移 fail-closed（评审 Important #4）：`invocation.ok=True`
        （stream 协议干净）不保证本次真实解析到的 tools 仍等于 Stage 1
        集合。这里构造一个 `ok=True` 但 init 里额外带有 `Bash` 与一个 shell
        MCP 的 invocation——复现评审实测反例：旧实现只看 `invocation.ok`，
        照样建 Issue 并发布。修复后必须判失败、不产生任何 Issue，且按
        最坏值全额计费。
        """
        inv = InvocationResult(
            True, CANDIDATE_PAYLOAD, 0.15, 4,
            init_seen=True,
            init_tools=list(round_module.STAGE1_TOOLS.split(",")) + ["Bash"],
            init_mcp_servers=[{"name": "shell"}],
            init_plugins=[], init_errors=[])
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "capability-drift")
        self.assertEqual(len(self.gh.issues), 0,
                         "能力漂移时绝不能继续发布")
        day_row = self.conn.execute(
            "SELECT settled_usd FROM budget_days").fetchone()
        self.assertAlmostEqual(day_row["settled_usd"], self.cfg.round_budget_usd,
                               msg="能力漂移结果未知，必须按最坏上限计费")

    def test_open_root_resumes_without_invoking_model(self):
        """恢复优先于新扫描：有未结 root 时绝不起模型（评审 C-02）。

        先用一次 stop_after="issue" 的 publish() 制造一个 observed-但-未-settled
        的 publish_proposal root（Issue 已建、卡片未提交/未 push/收据未写），
        这正是「建 Issue 响应丢失」之后真实会留下的持久化状态。随后传入的
        `invoke` 是被调用即失败的哨兵——一旦 run_round 在有未结 root 时仍去
        起模型，本测试必须变红。
        """
        candidate = dict(CANDIDATE_PAYLOAD["candidates"][0])
        candidate["fingerprint"] = fingerprint(
            candidate["goal"], candidate["invariant"],
            candidate["primary_path"], candidate["oracle"])
        # 这里直接调用 Publisher.publish()（绕过 run_round/_derive_labels），
        # 所以必须自行提供 labels——publish.py 仍按 candidate["labels"] 读取，
        # 真实调用路径下这一步由 round._derive_labels() 完成。
        candidate["labels"] = round_module._derive_labels(candidate)
        prior_publisher = Publisher(Outbox(self.conn), self.gh, self.wt,
                                    Queue(self.conn), "prior-round")
        prior_publisher.publish(candidate, stop_after="issue")

        # 制造后的状态必须真的是「未结 root」，否则下面的断言是在测一个
        # 从未发生过的场景（core-must-match-the-claim）。
        outbox = Outbox(self.conn)
        self.assertTrue(outbox.open_roots(), "前置状态必须先造出一个未结 root")

        sentinel_invoke = mock.Mock(
            side_effect=AssertionError("有未结 root 时不得起模型"))
        deps = Deps(conn=self.conn, gh=self.gh, worktree=self.wt,
                   outbox=Outbox(self.conn), queue=Queue(self.conn),
                   invoke=sentinel_invoke, tools=("/usr/bin/git",))

        result = run_round(self.cfg, deps)

        sentinel_invoke.assert_not_called()
        self.assertEqual(result["mode"], "resume")
        self.assertEqual(result["result"], "resumed")
        row = self.conn.execute(
            "SELECT settled_usd FROM rounds WHERE round_id=?",
            (result["round_id"],)).fetchone()
        self.assertAlmostEqual(row["settled_usd"], 0.0,
                               msg="恢复轮不应产生新的模型花费")

    def test_prompt_uses_known_canonical_keys_not_known_fingerprints(self):
        """控制器 prompt 传给 Workflow 的字段名必须是 `known_canonical_keys`
        （评审 Minor #8）：skill/Workflow 读的是 `known_canonical_keys`，若
        控制器仍传旧的 `known_fingerprints`，1a 阶段传空数组时无外部行为
        差异，但 1b 接入已知 key 后会静默失效——这里做一条字面契约测试，
        直接断言 prompt JSON 里含有正确的键名、不含旧键名。
        """
        import json as _json

        seen_prompts = []

        def capturing_invoke(**kw):
            seen_prompts.append(kw.get("prompt"))
            return _clean_invocation(True, {"candidates": []}, 0.01, 1)

        deps = self._deps(None)
        deps.invoke = capturing_invoke
        run_round(self.cfg, deps)

        self.assertEqual(len(seen_prompts), 1)
        prompt = seen_prompts[0]
        _, _, json_blob = prompt.partition("\n")
        args = _json.loads(json_blob)
        self.assertIn("known_canonical_keys", args)
        self.assertNotIn("known_fingerprints", args)

    def test_known_canonical_keys_are_actually_carried_into_the_prompt(self):
        """接线断言（评审 rmf-02）：在册提案的 key 必须真的进 prompt。

        上一条测试只断言了**键名**，`[]` 也能通过——它挡不住「跨轮去重被硬编码
        关闭」这个缺陷，事实上该缺陷正是在它全绿的情况下上线的。这里断言**值**。

        key 里刻意放撇号、双引号与 \x1f 分隔符：prompt 曾用 `repr()` 再把 `'`
        换成 `"` 来拼 JSON，这类字符会直接产出非法 JSON，而下游是要当 JSON 解析的。
        """
        import json as _json

        nasty = "goal's \"quoted\"\x1finv\x1fpath\x1foracle"
        self.deps_queue_seed = None
        deps = self._deps(None)
        deps.queue.record(fp="fp-known", lane="perf", title="t",
                          state="proposed", issue_number=7)
        deps.queue.remember_canonical_key("fp-known", nasty)

        seen = []

        def capturing_invoke(**kw):
            seen.append(kw.get("prompt"))
            return _clean_invocation(True, {"candidates": []}, 0.01, 1)

        deps.invoke = capturing_invoke
        run_round(self.cfg, deps)

        _, _, blob = seen[0].partition("\n")
        args = _json.loads(blob)   # 非法 JSON 会在这里炸
        self.assertEqual(args["known_canonical_keys"], [nasty])

    def test_duplicate_candidate_still_teaches_the_dedup_memory(self):
        """被判重复的候选也必须进去重集，否则系统学不会（rmf-02 修复的自身缺口）。

        `remember_canonical_key` 原先只在**发布成功**后调用。于是在本修复之前就
        已存在的提案（如真机的 Issue #1）永远进不了去重集：它每轮都会被重新提出、
        每轮都在最后一步被判 duplicate 丢弃、每轮都不会被记住——**永久卡死**，
        每 2 小时烧掉一整轮的钱且仓库零产出。

        这条是「修复本身要复核有无引入新缺口」的实例：`known_canonical_keys` 接上
        了，但喂给它的写入点覆盖不全，整体活性并没有恢复。
        """
        deps = self._deps(None)
        dup_fp = None

        cand = dict(CANDIDATE_PAYLOAD["candidates"][0])
        # 刻意给一个**错误**的 canonical_key：控制器必须自算、无视模型给的值。
        # 模型可控的 key 是一条持久抑制通道——给一个精心挑选的值就能永久屏蔽
        # 某个合法方向（评审 rmf-13）。
        cand["canonical_key"] = "attacker-supplied-key"
        expected = canonical_key(cand["goal"], cand["invariant"],
                                 cand["primary_path"], cand["oracle"])

        def invoke_dup(**kw):
            return _clean_invocation(True, {"candidates": [cand]}, 0.01, 1)

        deps.invoke = invoke_dup
        # 先让它发布一次，拿到 fingerprint
        first = run_round(self.cfg, deps)
        self.assertEqual(first["result"], "published")
        self.assertEqual(deps.queue.known_canonical_keys(), [expected])
        self.assertNotIn("attacker-supplied-key",
                         deps.queue.known_canonical_keys(),
                         "控制器消费了模型给的 canonical_key——那是持久抑制通道")

        # 清掉记忆，模拟「修复前就已存在的提案」：proposals 有行，proposal_keys 没有
        deps.queue.conn.execute("DELETE FROM proposal_keys")
        deps.queue.conn.commit()
        self.assertEqual(deps.queue.known_canonical_keys(), [])

        second = run_round(self.cfg, deps)
        self.assertEqual(second["result"], "duplicate")
        self.assertEqual(deps.queue.known_canonical_keys(), [expected],
                         "被判重复的候选没有进去重集——下一轮还会重来，永久卡死")

    def test_failed_invocation_settles_at_known_cost_not_full_reserve(self):
        """成本已知时不得按预留满额计费（评审 rmf-05）。

        `abandon()` 的语义是「结果未知按最坏值计费」，在成本**真的**未知时正确。
        但 `invocation-failed` 分支里成本往往是已知的——终态 result 事件的
        cost/turns 解析独立于 subtype，代码自己前两行刚把它存进 progress。

        为什么这条重要：预算观察期的判据是「复核 budget_days 实际花费，据此定
        真实日上限」。真机今日 $41.07 里只有 $5.57 来自实测，其余约 86% 是
        reserved_usd 的原样回填。**偏置方向恰好是「看起来花得比实际多」**——
        它会让人把上限定得过高，正好是这次观察想避免的错误方向。
        """
        deps = self._deps(None)

        def failing_invoke(**kw):
            # ok=False 但成本已知：终态事件解析到了 cost 与 turns
            return _clean_invocation(False, None, 0.37, 4)

        deps.invoke = failing_invoke
        out = run_round(self.cfg, deps)
        self.assertEqual(out["result"], "invocation-failed")

        row = self.conn.execute(
            "SELECT reserved_usd, settled_usd FROM rounds WHERE round_id=?",
            (out["round_id"],)).fetchone()
        self.assertAlmostEqual(row["settled_usd"], 0.37, places=6,
                               msg="按预留满额计费，账本被虚构花费污染")
        self.assertNotAlmostEqual(row["settled_usd"], row["reserved_usd"],
                                  places=6)

    def test_failed_invocation_with_unknown_cost_still_charges_worst_case(self):
        """成本**真的**未知时（超时、进程被杀）仍按最坏值——这是保守的正确行为。"""
        deps = self._deps(None)

        def timing_out(**kw):
            return InvocationResult(False, None, 0.0, 0, exit_code=124,
                                    raw_tail="timeout", init_seen=False)

        deps.invoke = timing_out
        out = run_round(self.cfg, deps)
        row = self.conn.execute(
            "SELECT reserved_usd, settled_usd FROM rounds WHERE round_id=?",
            (out["round_id"],)).fetchone()
        self.assertAlmostEqual(row["settled_usd"], row["reserved_usd"], places=6)

    def test_degraded_round_with_no_candidate_is_not_a_clean_noop(self):
        """全降级轮不得与「仓库确实没东西可提」混为一谈（评审 rmf-03）。

        修复传输故障隔离**之前**，一个 agent 挂掉会让整轮 aborted →
        invocation-failed → exit 1，systemd 看得见。修复**之后**同样的故障变成
        `candidates: []` → `no-candidate` → exit 0，systemd 记成功。
        隔离是对的，但它把「响亮的失败」换成了「安静的成功」——2 小时节拍下
        这可以持续一整天而无人察觉。
        """
        payload = {"candidates": [],
                   "degraded": [{"agentType": "harness-judge-redline",
                                 "label": "harness-judge-redline",
                                 "error": "API Error: Server error mid-response",
                                 "occurrences": 3, "attempts": 9}]}
        deps = self._deps(_clean_invocation(True, payload, 0.42, 3))
        out = run_round(self.cfg, deps)

        self.assertEqual(out["result"], "no-candidate-degraded")
        self.assertIn("harness-judge-redline", out["detail"])
        row = self.conn.execute(
            "SELECT result FROM rounds WHERE round_id=?",
            (out["round_id"],)).fetchone()
        self.assertEqual(row["result"], "no-candidate-degraded",
                         "降级证据没进账本——事后无法与真正的空轮区分")

    def test_clean_no_candidate_round_stays_a_clean_noop(self):
        """没有降级时，空轮仍是干净的空轮——不得把静默换成噪声。"""
        deps = self._deps(_clean_invocation(True, {"candidates": [],
                                                   "degraded": []}, 0.05, 3))
        out = run_round(self.cfg, deps)
        self.assertEqual(out["result"], "no-candidate")

    def test_published_round_still_surfaces_partial_degradation(self):
        """部分降级但发布成功：仍算成功，但降级证据必须带出来。"""
        payload = {"candidates": list(CANDIDATE_PAYLOAD["candidates"]),
                   "degraded": [{"agentType": "harness-finder-bench",
                                 "label": "perf", "error": "boom",
                                 "occurrences": 1, "attempts": 3}]}
        deps = self._deps(_clean_invocation(True, payload, 0.5, 4))
        out = run_round(self.cfg, deps)
        self.assertEqual(out["result"], "published")
        self.assertIn("harness-finder-bench", out.get("degraded_detail", ""))

    def test_remaining_time_budget_is_passed_to_invoke_as_timeout(self):
        """单调截止：剩余时间被真实传给 invoke，且必须小于整轮截止。

        用受控的 `time.monotonic()` 序列模拟「round 开始后已经过 200 秒」，
        断言 `invoke` 实际收到的 `timeout_s` 等于
        `ROUND_DEADLINE_S - 200 - CLEANUP_RESERVE_S`——不是整轮剩余时间本身，
        而是再扣掉显式结算窗口之后的值，证明确实为 checkpoint/结算预留了
        时间窗，不是把整轮时间全部塞给模型（评审 Important #7）。
        """
        seen_timeouts = []

        def capturing_invoke(**kw):
            seen_timeouts.append(kw.get("timeout_s"))
            return _clean_invocation(True, {"candidates": []}, 0.01, 1)

        deps = self._deps(None)
        deps.invoke = capturing_invoke

        elapsed = 200.0
        times = [1_000_000.0, 1_000_000.0 + elapsed]
        state = {"n": 0}

        def fake_monotonic():
            idx = min(state["n"], len(times) - 1)
            state["n"] += 1
            return times[idx]

        with mock.patch.object(round_module.time, "monotonic",
                               side_effect=fake_monotonic):
            run_round(self.cfg, deps)

        self.assertEqual(len(seen_timeouts), 1, "invoke 必须恰好被调用一次")
        timeout_s = seen_timeouts[0]
        self.assertIsNotNone(
            timeout_s, "timeout_s 必须真的把 remaining_time 传给 invoke，"
                      "而不是被忽略或传 None")
        self.assertGreater(timeout_s, 0)
        self.assertLessEqual(timeout_s, round_module.ROUND_DEADLINE_S)
        self.assertLess(
            timeout_s, round_module.ROUND_DEADLINE_S,
            "必须为 checkpoint/结算预留时间窗，不能把整轮时间全部给模型")
        self.assertAlmostEqual(
            timeout_s,
            round_module.ROUND_DEADLINE_S - elapsed
            - round_module.CLEANUP_RESERVE_S,
            msg="timeout_s 必须等于 round_deadline 减去已流逝的单调时间、"
               "再减去结算窗口，而不是任意常量")

    def test_deadline_exhausted_does_not_invoke_and_charges_full_reservation(self):
        """截止已耗尽（剩余不足以覆盖结算窗口）时必须显式判失败退出，绝不能
        用 `max(x, 60)` 之类的下限把剩余时间反向放大后仍去起模型（评审
        Important #7 实测反例：已过 1301 秒，`invoke()` 仍收到
        `timeout_s=60.0`——本测试是它的正控，先验证同样构造能被本测试抓到）。
        """
        sentinel_invoke = mock.Mock(
            side_effect=AssertionError("截止耗尽时不得起模型"))
        deps = self._deps(None)
        deps.invoke = sentinel_invoke

        elapsed = 1301.0  # > ROUND_DEADLINE_S(1200) - CLEANUP_RESERVE_S(60)
        times = [1_000_000.0, 1_000_000.0 + elapsed]
        state = {"n": 0}

        def fake_monotonic():
            idx = min(state["n"], len(times) - 1)
            state["n"] += 1
            return times[idx]

        with mock.patch.object(round_module.time, "monotonic",
                               side_effect=fake_monotonic):
            result = run_round(self.cfg, deps)

        sentinel_invoke.assert_not_called()
        self.assertEqual(result["result"], "deadline-exhausted")
        row = self.conn.execute(
            "SELECT settled_usd FROM budget_days").fetchone()
        self.assertAlmostEqual(row["settled_usd"], self.cfg.round_budget_usd,
                               msg="结果未知必须按最坏上限计费")


def _valid_candidate(**overrides) -> dict:
    base = dict(CANDIDATE_PAYLOAD["candidates"][0])
    base.update(overrides)
    return base


class TestValidateCandidate(unittest.TestCase):
    """`validate_candidate()` 单元测试：候选 DTO 校验必须在任何 outbox
    intent 或外部副作用之前完成（评审 Critical #2）。"""

    def test_valid_candidate_has_no_errors(self):
        self.assertEqual(round_module.validate_candidate(_valid_candidate()), [])

    def test_missing_required_field_rejected(self):
        candidate = _valid_candidate()
        del candidate["oracle"]
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("缺少必需字段" in e for e in errors))

    def test_unknown_field_rejected(self):
        candidate = _valid_candidate(mystery_field="x")
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("未知字段" in e for e in errors))

    def test_bad_lane_rejected(self):
        candidate = _valid_candidate(lane="not-a-lane")
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("lane" in e for e in errors))

    def test_bad_priority_rejected(self):
        candidate = _valid_candidate(priority="T9")
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("priority" in e for e in errors))

    def test_bad_size_rejected(self):
        candidate = _valid_candidate(size="XL")
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("size" in e for e in errors))

    def test_path_traversal_slug_rejected(self):
        """真实反例：`slug="../escape"` 曾先建 Issue，之后才被 git 路径
        正则间接拒绝——本校验必须在任何 outbox intent 之前就地拒绝。"""
        candidate = _valid_candidate(slug="../escape")
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("slug" in e for e in errors))

    def test_empty_slug_rejected(self):
        candidate = _valid_candidate(slug="")
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("slug" in e for e in errors))

    def test_uppercase_slug_rejected(self):
        candidate = _valid_candidate(slug="Tail-Journal-CRC")
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("slug" in e for e in errors))

    def test_non_bool_needs_decision_rejected(self):
        candidate = _valid_candidate(needs_decision="yes")
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("needs_decision" in e for e in errors))

    def test_overlong_title_rejected(self):
        candidate = _valid_candidate(title="x" * (round_module._MAX_SHORT_TEXT + 1))
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("title" in e for e in errors))

    def test_empty_title_rejected(self):
        candidate = _valid_candidate(title="   ")
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("title" in e for e in errors))

    def test_touched_paths_wrong_shape_rejected(self):
        candidate = _valid_candidate(touched_paths=["ok", 123])
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("touched_paths" in e for e in errors))

    def test_touched_paths_too_many_rejected(self):
        candidate = _valid_candidate(
            touched_paths=[f"p{i}" for i in range(round_module._MAX_TOUCHED_PATHS + 1)])
        errors = round_module.validate_candidate(candidate)
        self.assertTrue(any("touched_paths" in e for e in errors))

    def test_valid_optional_fields_accepted(self):
        candidate = _valid_candidate(
            evidence="见 archive.rs:120", touched_paths=["a.rs", "b.rs"],
            canonical_key="k", verdicts=[{"judge": "x"}])
        self.assertEqual(round_module.validate_candidate(candidate), [])


class TestRunRoundRejectsInvalidCandidate(unittest.TestCase):
    """`run_round()` 端到端：非法 candidate 必须在任何 outbox intent 或外部
    副作用之前被拒绝——产生零 Issue、零 operation、零 git 改动（评审
    Critical #2）。表驱动覆盖每一类非法输入。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.remote = root / "remote.git"
        self.local = root / "local"
        subprocess.run([GIT, "init", "--bare", "-b", "main", str(self.remote)],
                       check=True, capture_output=True)
        subprocess.run([GIT, "clone", str(self.remote), str(self.local)],
                       check=True, capture_output=True)
        run(self.local, "config", "user.email", "h@e.com")
        run(self.local, "config", "user.name", "h")
        (self.local / "README.md").write_text("seed\n")
        run(self.local, "add", "README.md")
        run(self.local, "commit", "-m", "seed")
        run(self.local, "push", "origin", "main")

        self.conn = db.connect(root / "h.db")
        db.migrate(self.conn)
        self.addCleanup(self.conn.close)
        self.gh = FakeGitHub("WRITE")
        self.cfg = Cfg(self.local)
        self.wt = PublishWorktree(self.local, self.local / ".worktree/_publish")

    def tearDown(self):
        self.tmp.cleanup()

    def _deps(self, invocation):
        return Deps(conn=self.conn, gh=self.gh, worktree=self.wt,
                    outbox=Outbox(self.conn), queue=Queue(self.conn),
                    invoke=lambda **kw: invocation, tools=("/usr/bin/git",))

    def _assert_zero_side_effects(self, overrides: dict):
        candidate = _valid_candidate(**overrides)
        inv = _clean_invocation(True, {"candidates": [candidate]}, 0.1, 3)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "invalid-candidate")
        self.assertEqual(len(self.gh.issues), 0, "非法 candidate 不得产生 Issue")
        self.assertEqual(len(Outbox(self.conn).open_operations()), 0,
                         "非法 candidate 不得产生 operation")
        head_after = run(self.local, "rev-parse", "HEAD")
        head_before = run(self.remote, "rev-parse", "HEAD")
        self.assertEqual(head_after, head_before, "非法 candidate 不得产生 git 改动")

    def test_path_traversal_slug_produces_zero_side_effects(self):
        """真实反例复现：`slug="../escape"` 且缺 labels（本就不该有）时，
        必须零 Issue、零 operation、零 git 改动。"""
        self._assert_zero_side_effects({"slug": "../escape"})

    def test_missing_field_produces_zero_side_effects(self):
        candidate = _valid_candidate()
        del candidate["invariant"]
        inv = _clean_invocation(True, {"candidates": [candidate]}, 0.1, 3)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "invalid-candidate")
        self.assertEqual(len(self.gh.issues), 0)

    def test_unknown_field_produces_zero_side_effects(self):
        self._assert_zero_side_effects({"extra_field": "sneaky"})

    def test_bad_lane_produces_zero_side_effects(self):
        self._assert_zero_side_effects({"lane": "../escape"})

    def test_bad_priority_produces_zero_side_effects(self):
        self._assert_zero_side_effects({"priority": "URGENT"})

    def test_bad_size_produces_zero_side_effects(self):
        self._assert_zero_side_effects({"size": "HUGE"})

    def test_non_bool_needs_decision_produces_zero_side_effects(self):
        self._assert_zero_side_effects({"needs_decision": "yes"})

    def _assert_zero_side_effects_for_payload(self, payload: dict):
        """真实反例复现（评审 Critical A）：`candidates` 顶层形状非法（字符串
        列表/字典/None）或元素非对象（None）时，旧实现在
        `eligible = [c for c in candidates if c.get("lane") ...]` 处直接
        `AttributeError` 崩溃，round 永久悬挂在 `mode=pending, result=None`、
        `reserved_usd` 未释放。修复后必须走结构化 `invalid-candidate` 结算：
        零 Issue、零 operation、零 git 改动，且账本完整结算。
        """
        inv = _clean_invocation(True, payload, 0.1, 3)
        result = run_round(self.cfg, self._deps(inv))
        self.assertEqual(result["result"], "invalid-candidate")
        self.assertEqual(len(self.gh.issues), 0, "非法顶层形状不得产生 Issue")
        self.assertEqual(len(Outbox(self.conn).open_operations()), 0,
                         "非法顶层形状不得产生 operation")
        head_after = run(self.local, "rev-parse", "HEAD")
        head_before = run(self.remote, "rev-parse", "HEAD")
        self.assertEqual(head_after, head_before, "非法顶层形状不得产生 git 改动")
        row = self._round_row_via_status()
        self.assertIsNotNone(row["ended_at"], "账本必须已完整结算（预留已释放）")

    def _round_row_via_status(self):
        return self.conn.execute(
            "SELECT ended_at, reserved_usd, settled_usd FROM rounds"
            " ORDER BY started_at DESC LIMIT 1").fetchone()

    def test_candidates_as_list_of_strings_produces_zero_side_effects(self):
        self._assert_zero_side_effects_for_payload(
            {"candidates": ["not-an-object"]})

    def test_candidates_as_dict_produces_zero_side_effects(self):
        self._assert_zero_side_effects_for_payload(
            {"candidates": {"lane": "defect"}})

    def test_candidates_as_none_produces_zero_side_effects(self):
        self._assert_zero_side_effects_for_payload({"candidates": None})

    def test_candidates_element_none_produces_zero_side_effects(self):
        self._assert_zero_side_effects_for_payload({"candidates": [None]})

    def test_charges_full_reservation_on_invalid_candidate(self):
        """非法 candidate 仍然消耗了一次真实模型调用，必须按实际花费入账，
        不能因为拒绝发布就悄悄免单。"""
        candidate = _valid_candidate(slug="../escape")
        inv = _clean_invocation(True, {"candidates": [candidate]}, 0.42, 3)
        run_round(self.cfg, self._deps(inv))
        row = self.conn.execute("SELECT settled_usd FROM rounds").fetchone()
        self.assertAlmostEqual(row["settled_usd"], 0.42)


class TestRoundLedgerFinalize(unittest.TestCase):
    """统一、幂等的 round finalize 路径：所有返回分支都必须写入
    mode/result/turns/denials/exit_code（评审 Important #6）——不能像修复前
    那样，成功发布后该行仍是
    `mode=pending, result=None, turns=None, exit_code=None`。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.remote = root / "remote.git"
        self.local = root / "local"
        subprocess.run([GIT, "init", "--bare", "-b", "main", str(self.remote)],
                       check=True, capture_output=True)
        subprocess.run([GIT, "clone", str(self.remote), str(self.local)],
                       check=True, capture_output=True)
        run(self.local, "config", "user.email", "h@e.com")
        run(self.local, "config", "user.name", "h")
        (self.local / "README.md").write_text("seed\n")
        run(self.local, "add", "README.md")
        run(self.local, "commit", "-m", "seed")
        run(self.local, "push", "origin", "main")

        self.conn = db.connect(root / "h.db")
        db.migrate(self.conn)
        self.addCleanup(self.conn.close)
        self.gh = FakeGitHub("WRITE")
        self.cfg = Cfg(self.local)
        self.wt = PublishWorktree(self.local, self.local / ".worktree/_publish")

    def tearDown(self):
        self.tmp.cleanup()

    def _deps(self, invocation):
        return Deps(conn=self.conn, gh=self.gh, worktree=self.wt,
                    outbox=Outbox(self.conn), queue=Queue(self.conn),
                    invoke=lambda **kw: invocation, tools=("/usr/bin/git",))

    def _round_row(self, round_id):
        return self.conn.execute(
            "SELECT mode, result, turns, denials, exit_code FROM rounds"
            " WHERE round_id=?", (round_id,)).fetchone()

    def test_successful_publish_round_is_fully_settled(self):
        inv = InvocationResult(True, CANDIDATE_PAYLOAD, 0.30, 8,
                               init_seen=True,
                               init_tools=list(round_module.STAGE1_TOOLS.split(",")),
                               init_mcp_servers=[], init_plugins=[],
                               init_errors=[], denials=2, exit_code=0)
        result = run_round(self.cfg, self._deps(inv))
        row = self._round_row(result["round_id"])
        self.assertEqual(row["mode"], "scan")
        self.assertEqual(row["result"], "published")
        self.assertEqual(row["turns"], 8)
        self.assertEqual(row["denials"], 2)
        self.assertEqual(row["exit_code"], 0)

    def test_precheck_failed_round_is_fully_settled(self):
        self.gh.permission = "READ"
        inv = _clean_invocation(True, CANDIDATE_PAYLOAD, 0.3, 8)
        result = run_round(self.cfg, self._deps(inv))
        row = self._round_row(result["round_id"])
        self.assertEqual(row["mode"], "scan")
        self.assertEqual(row["result"], "precheck-failed")

    def test_resumed_round_is_fully_settled(self):
        candidate = dict(CANDIDATE_PAYLOAD["candidates"][0])
        candidate["fingerprint"] = fingerprint(
            candidate["goal"], candidate["invariant"],
            candidate["primary_path"], candidate["oracle"])
        candidate["labels"] = round_module._derive_labels(candidate)
        Publisher(Outbox(self.conn), self.gh, self.wt, Queue(self.conn),
                 "prior-round").publish(candidate, stop_after="issue")

        sentinel_invoke = mock.Mock(
            side_effect=AssertionError("恢复轮不得起模型"))
        deps = Deps(conn=self.conn, gh=self.gh, worktree=self.wt,
                   outbox=Outbox(self.conn), queue=Queue(self.conn),
                   invoke=sentinel_invoke, tools=("/usr/bin/git",))
        result = run_round(self.cfg, deps)
        row = self._round_row(result["round_id"])
        self.assertEqual(row["mode"], "resume")
        self.assertEqual(row["result"], "resumed")

    def test_invalid_candidate_round_is_fully_settled(self):
        candidate = _valid_candidate(slug="../escape")
        inv = _clean_invocation(True, {"candidates": [candidate]}, 0.1, 3)
        result = run_round(self.cfg, self._deps(inv))
        row = self._round_row(result["round_id"])
        self.assertEqual(row["result"], "invalid-candidate")
        self.assertEqual(row["turns"], 3)

    def test_unexpected_exception_during_publish_is_still_fully_settled(self):
        """评审 Critical C 复现：`create_issue()` 抛一个普通 transport
        `RuntimeError`（不是 outbox 认识的 `ResponseLost`/
        `TerminalOperationError`）。修复前只有显式 `return` 分支才写账本，
        这里的异常会一路冒穿 `run_round()`，round 永久停在
        `mode=pending, result=None, ended_at=None, reserved_usd=1.0`——
        有限次数的故障会积累成永久预算占用，最终把日预算吃空。

        修复后：异常仍必须继续向上传播（不得被吞掉），且账本必须已完整
        结算——`ended_at` 非空、预留已释放、`result="unhandled-exception"`、
        `turns`/`exit_code` 取自本次真实调用。
        """
        inv = _clean_invocation(True, CANDIDATE_PAYLOAD, 0.25, 6, exit_code=0)
        deps = self._deps(inv)
        with mock.patch.object(
                self.gh, "create_issue",
                side_effect=RuntimeError("transport 故障，非 outbox 已知异常")):
            with self.assertRaises(RuntimeError):
                run_round(self.cfg, deps)

        row = self.conn.execute(
            "SELECT mode, result, turns, exit_code, ended_at, reserved_usd,"
            " settled_usd FROM rounds ORDER BY started_at DESC LIMIT 1"
        ).fetchone()
        self.assertEqual(row["result"], "unhandled-exception")
        self.assertIsNotNone(row["ended_at"], "异常路径也必须完整结算账本")
        self.assertAlmostEqual(row["settled_usd"], 0.25,
                               msg="成本已知（invoke 已返回）必须记实际值")
        self.assertEqual(row["turns"], 6)
        self.assertEqual(row["exit_code"], 0)

        day_row = self.conn.execute(
            "SELECT reserved_usd, settled_usd FROM budget_days").fetchone()
        self.assertAlmostEqual(day_row["reserved_usd"], 0.0,
                               msg="异常路径也必须释放预留，不能永久占用日预算")
        self.assertAlmostEqual(day_row["settled_usd"], 0.25)

    def test_unexpected_exception_before_invoke_charges_worst_case(self):
        """成本未知（异常发生在 `invoke()` 返回之前，例如 prechecks 之后、
        `budget.reserve()` 之后的某个未预期故障）时，必须按该 round 的
        预留上限全额计费（与既有 `abandon()` worst-case 语义一致），而不是
        记 0 元悄悄免单。"""
        def _boom(*a, **kw):
            raise RuntimeError("reserve 之后、invoke 之前的未预期故障")

        deps = self._deps(None)
        deps.queue.lane_full = _boom  # type: ignore[method-assign]

        with self.assertRaises(RuntimeError):
            run_round(self.cfg, deps)

        row = self.conn.execute(
            "SELECT result, ended_at, settled_usd FROM rounds"
            " ORDER BY started_at DESC LIMIT 1").fetchone()
        self.assertEqual(row["result"], "unhandled-exception")
        self.assertIsNotNone(row["ended_at"])
        self.assertAlmostEqual(row["settled_usd"], self.cfg.round_budget_usd,
                               msg="成本未知必须按预留上限全额计费")


if __name__ == "__main__":
    unittest.main()
