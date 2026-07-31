import tempfile, time, unittest
from pathlib import Path
from harness import db
from harness.queue import Queue, fingerprint


class TestQueue(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        db.migrate(self.conn)
        self.q = Queue(self.conn)

    def tearDown(self):
        # 先关连接再删目录：否则 SQLite 连接随 GC 关闭会触发
        # ResourceWarning，掩盖真正的资源泄漏信号
        conn = getattr(self, "conn", None)
        if conn is not None:
            conn.close()
        self.tmp.cleanup()

    def test_fingerprint_is_stable_and_order_insensitive_to_whitespace(self):
        a = fingerprint("加 CRC", "块完整性", "crates/scrollz/src/archive.rs",
                        "损坏块必须 fail-closed")
        b = fingerprint("加 CRC ", " 块完整性", "crates/scrollz/src/archive.rs",
                        "损坏块必须 fail-closed")
        self.assertEqual(a, b)

    def test_fingerprint_table_driven_should_be_same(self):
        base = fingerprint("Goal Text", "Invariant", "path/a.rs", "oracle text")
        cases = [
            ("大小写差异", fingerprint("goal text", "invariant", "path/a.rs",
                                     "oracle text")),
            ("tab 折叠", fingerprint("Goal\tText", "Invariant", "path/a.rs",
                                   "oracle text")),
            ("newline 折叠", fingerprint("Goal\nText", "Invariant", "path/a.rs",
                                       "oracle text")),
            ("多空格折叠", fingerprint("Goal   Text", "Invariant", "path/a.rs",
                                     "oracle text")),
            ("Unicode 空白（全角空格）", fingerprint("Goal　Text", "Invariant",
                                                "path/a.rs", "oracle text")),
            ("首尾空白", fingerprint("  Goal Text  ", "Invariant", "path/a.rs",
                                   "oracle text")),
        ]
        for label, other in cases:
            with self.subTest(label=label):
                self.assertEqual(base, other)

    def test_fingerprint_table_driven_should_differ(self):
        goal, invariant, path, oracle = "goal", "invariant", "path/a.rs", "oracle"
        base = fingerprint(goal, invariant, path, oracle)
        # 字段顺序稳定 != 字段可交换：两两交换必须产生不同指纹。
        swaps = [
            ("goal<->invariant", fingerprint(invariant, goal, path, oracle)),
            ("goal<->path", fingerprint(path, invariant, goal, oracle)),
            ("goal<->oracle", fingerprint(oracle, invariant, path, goal)),
            ("invariant<->path", fingerprint(goal, path, invariant, oracle)),
            ("invariant<->oracle", fingerprint(goal, oracle, path, invariant)),
            ("path<->oracle", fingerprint(goal, invariant, oracle, path)),
        ]
        for label, other in swaps:
            with self.subTest(label=label):
                self.assertNotEqual(base, other)

    def test_exact_duplicate_detected(self):
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "defect", "t", "proposed", issue_number=1)
        self.assertEqual(self.q.classify(
            {"fingerprint": fp, "lane": "defect", "title": "t"}), "exact_duplicate")

    def test_rejected_without_ready_condition_blocks_reproposal(self):
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "perf", "t", "rejected",
                      reconsider_when="not_before:2099-01-01")
        self.assertEqual(self.q.classify(
            {"fingerprint": fp, "lane": "perf", "title": "t"}), "rejected_active")

    def test_rejected_with_satisfied_condition_becomes_new(self):
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "perf", "t", "rejected",
                      reconsider_when="not_before:2000-01-01")
        self.assertEqual(self.q.classify(
            {"fingerprint": fp, "lane": "perf", "title": "t"}), "new")

    def test_unparseable_reconsider_when_never_auto_expires(self):
        """自然语言条件不得伪装成自动复议。"""
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "perf", "t", "rejected",
                      reconsider_when="等 fuser 升级之后再说")
        self.assertFalse(self.q.reconsider_ready(fp, {}))
        self.assertEqual(self.q.classify(
            {"fingerprint": fp, "lane": "perf", "title": "t"}), "rejected_active")

    def test_lane_and_total_caps(self):
        for i in range(3):
            self.q.record(f"fp{i}", "hygiene", f"t{i}", "proposed")
        self.assertTrue(self.q.lane_full("hygiene", cap=3))
        self.assertFalse(self.q.lane_full("perf", cap=3))
        self.assertTrue(self.q.total_full(cap=3))
        self.assertFalse(self.q.total_full(cap=4))

    def test_main_sha_changed_predicate(self):
        sha_a = "a" * 40
        sha_b = "b" * 40
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "roadmap", "t", "rejected",
                      reconsider_when=f"main_sha_changed:{sha_a}")
        self.assertFalse(self.q.reconsider_ready(fp, {"main_sha": sha_a}))
        self.assertTrue(self.q.reconsider_ready(fp, {"main_sha": sha_b}))

    def test_main_sha_changed_rejects_non_sha_argument(self):
        """评审复现的绕过：main_sha_changed:not-a-sha 不得被判定为 True。"""
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "roadmap", "t", "rejected",
                      reconsider_when="main_sha_changed:not-a-sha")
        self.assertFalse(self.q.reconsider_ready(fp, {"main_sha": "deadbeef"}))
        # ctx 侧传的 main_sha 若不是合法 SHA，同样不可判定为 True
        fp2 = fingerprint("g2", "i", "p", "o")
        self.q.record(fp2, "roadmap", "t", "rejected",
                      reconsider_when=f"main_sha_changed:{'a' * 40}")
        self.assertFalse(self.q.reconsider_ready(fp2, {"main_sha": "not-a-sha"}))

    def test_main_sha_changed_accepts_64_char_sha256(self):
        sha_a = "a" * 64
        sha_b = "b" * 64
        fp = fingerprint("g3", "i", "p", "o")
        self.q.record(fp, "roadmap", "t", "rejected",
                      reconsider_when=f"main_sha_changed:{sha_a}")
        self.assertFalse(self.q.reconsider_ready(fp, {"main_sha": sha_a}))
        self.assertTrue(self.q.reconsider_ready(fp, {"main_sha": sha_b}))

    def test_dependency_issue_closed_predicate(self):
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "defect", "t", "rejected",
                      reconsider_when="dependency_issue_closed:42")
        self.assertFalse(self.q.reconsider_ready(fp, {"closed_issues": [1, 2]}))
        self.assertTrue(self.q.reconsider_ready(fp, {"closed_issues": [42]}))
        # ctx 侧传字符串形式同样要能匹配（规范成整数集合后再比较）
        self.assertTrue(self.q.reconsider_ready(fp, {"closed_issues": ["42"]}))

    def test_dependency_issue_closed_rejects_malformed_argument(self):
        """评审复现的绕过：空字符串参数 + 空字符串 closed_issues 不得判 True。"""
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "defect", "t", "rejected",
                      reconsider_when="dependency_issue_closed:")
        self.assertFalse(self.q.reconsider_ready(fp, {"closed_issues": [""]}))

        fp2 = fingerprint("g2", "i", "p", "o")
        self.q.record(fp2, "defect", "t", "rejected",
                      reconsider_when="dependency_issue_closed:0")
        self.assertFalse(self.q.reconsider_ready(fp2, {"closed_issues": [0]}))

        fp3 = fingerprint("g3", "i", "p", "o")
        self.q.record(fp3, "defect", "t", "rejected",
                      reconsider_when="dependency_issue_closed:-1")
        self.assertFalse(self.q.reconsider_ready(fp3, {"closed_issues": [-1]}))

    def test_decision_version_gt_predicate(self):
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "roadmap", "t", "rejected",
                      reconsider_when="decision_version_gt:2")
        self.assertFalse(self.q.reconsider_ready(fp, {"decision_version": 2}))
        self.assertTrue(self.q.reconsider_ready(fp, {"decision_version": 3}))

    def test_decision_version_gt_rejects_negative_argument(self):
        """评审复现的绕过：decision_version_gt:-1 + decision_version=0 不得判 True。"""
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "roadmap", "t", "rejected",
                      reconsider_when="decision_version_gt:-1")
        self.assertFalse(self.q.reconsider_ready(fp, {"decision_version": 0}))

    def test_typed_predicates_malformed_argument_whitespace_and_extra_colon(self):
        # 前后有空白、含额外冒号——一律不可判定为 True。
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "roadmap", "t", "rejected",
                      reconsider_when="main_sha_changed: aaaa:bbbb")
        self.assertFalse(self.q.reconsider_ready(fp, {"main_sha": "b" * 40}))

        fp2 = fingerprint("g2", "i", "p", "o")
        self.q.record(fp2, "roadmap", "t", "rejected",
                      reconsider_when=f" main_sha_changed:{'a' * 40} ")
        self.assertFalse(self.q.reconsider_ready(fp2, {"main_sha": "b" * 40}))

    def test_classify_only_produces_declared_reachable_values(self):
        """`classify()` 只声明并只可能返回三个值：new / exact_duplicate /
        rejected_active。`possible_duplicate` 属 Stage 1b 扩展接口，本模块
        故意不实现、不声明，因此不在此处断言其可达（评审 Important：接口
        声明与实现必须一致，不能声明一个永远走不到的分支充数）。"""
        reachable = set()

        fp_new = fingerprint("new-goal", "i", "p", "o")
        reachable.add(self.q.classify(
            {"fingerprint": fp_new, "lane": "defect", "title": "t"}))

        fp_dup = fingerprint("dup-goal", "i", "p", "o")
        self.q.record(fp_dup, "defect", "t", "proposed")
        reachable.add(self.q.classify(
            {"fingerprint": fp_dup, "lane": "defect", "title": "t"}))

        fp_rej = fingerprint("rejected-goal", "i", "p", "o")
        self.q.record(fp_rej, "defect", "t", "rejected",
                      reconsider_when="not_before:2099-01-01")
        reachable.add(self.q.classify(
            {"fingerprint": fp_rej, "lane": "defect", "title": "t"}))

        self.assertEqual(reachable,
                          {"new", "exact_duplicate", "rejected_active"})

    def test_record_update_preserves_created_at_and_issue_number(self):
        """`INSERT OR REPLACE` 会先删后插，重置 created_at 并清空未传入的
        issue_number/reconsider_when（评审 Important）。改用 upsert 后：
        - 首次写入的 created_at 必须在后续更新中保留；
        - 更新调用若不传 issue_number（None），已有值不得被清空；
        - state 未变化时 decided_at 不应被推进。
        """
        fp = fingerprint("g", "i", "p", "o")
        self.q.record(fp, "defect", "t1", "proposed", issue_number=7,
                      reconsider_when="not_before:2030-01-01")
        row1 = self.q._get(fp)
        created_at_1 = row1["created_at"]
        decided_at_1 = row1["decided_at"]

        time.sleep(0.01)
        # 更新：不传 issue_number / reconsider_when，state 不变
        self.q.record(fp, "defect", "t1-updated", "proposed")
        row2 = self.q._get(fp)
        self.assertEqual(row2["created_at"], created_at_1)  # 不可变字段保留
        self.assertEqual(row2["issue_number"], 7)  # 未清空
        self.assertEqual(row2["reconsider_when"], "not_before:2030-01-01")
        self.assertEqual(row2["title"], "t1-updated")  # 可更新字段确实更新了
        self.assertEqual(row2["decided_at"], decided_at_1)  # state 未变，不推进

        time.sleep(0.01)
        # 再更新：state 变化，decided_at 应该推进
        self.q.record(fp, "defect", "t1-updated2", "rejected",
                      reconsider_when="not_before:2031-01-01")
        row3 = self.q._get(fp)
        self.assertEqual(row3["created_at"], created_at_1)
        self.assertGreater(row3["decided_at"], decided_at_1)
        self.assertEqual(row3["state"], "rejected")
        self.assertEqual(row3["reconsider_when"], "not_before:2031-01-01")



class TestCanonicalKeyMemory(unittest.TestCase):
    """跨轮去重的记忆（评审 rmf-02）。

    `known_canonical_keys` 此前被硬编码为 `[]`，而它是 workflow 里 `seen` 集合
    的唯一外部来源——等于把跨轮去重整个关掉。控制器其实**已经收到**
    `canonical_key`（Workflow 的 `pickCandidateFields` 会输出），却把它列进
    "放行但不用"的可选字段后丢弃；而 `proposals` 只存 sha256 摘要，摘要不可逆，
    不补存就永远反推不出 canonical key。

    最坏形态是：每 2 小时花掉一整轮的钱跑完全部 finder 与 judge，最后在最后一步
    被判 duplicate 丢弃，退出码 0，systemd 记成功，仓库零产出，而且**没有任何
    机制让下一轮的结果不一样**。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.conn = db.connect(Path(self.tmp.name) / "h.db")
        db.migrate(self.conn)
        self.addCleanup(self.conn.close)
        self.q = Queue(self.conn)

    def _publish(self, fp, key, state="proposed"):
        self.q.record(fp=fp, lane="perf", title="t", state=state,
                      issue_number=1)
        self.q.remember_canonical_key(fp, key)

    def test_remembered_key_comes_back(self):
        self._publish("fp1", "goal\x1finv\x1fpath\x1foracle")
        self.assertEqual(self.q.known_canonical_keys(),
                         ["goal\x1finv\x1fpath\x1foracle"])

    def test_only_live_proposals_contribute(self):
        """已关闭的提案不进去重集：是否重提由 1b 的拒绝记忆决定，不是这里。"""
        self._publish("fp1", "live-key", state="proposed")
        self._publish("fp2", "closed-key", state="closed-by-user")
        self.assertEqual(self.q.known_canonical_keys(), ["live-key"])

    def test_remember_is_idempotent_and_first_write_wins(self):
        self._publish("fp1", "original")
        self.q.remember_canonical_key("fp1", "changed-later")
        self.assertEqual(self.q.known_canonical_keys(), ["original"])

    def test_key_without_a_proposal_row_does_not_leak(self):
        self.q.remember_canonical_key("orphan", "orphan-key")
        self.assertEqual(self.q.known_canonical_keys(), [])

    def test_missing_canonical_key_is_skipped_not_fatal(self):
        """候选没带 canonical_key 时只是少一条记忆，不得阻断发布。"""
        self.q.record(fp="fp9", lane="perf", title="t",
                      state="proposed", issue_number=9)
        self.q.remember_canonical_key("fp9", None)
        self.assertEqual(self.q.known_canonical_keys(), [])


if __name__ == "__main__":
    unittest.main()
