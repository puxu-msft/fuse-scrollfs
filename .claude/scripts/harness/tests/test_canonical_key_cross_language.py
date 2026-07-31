"""canonical key 的跨语言一致性（评审 rmf-13）。

`known_canonical_keys` 由 Python 产出、由 `.claude/workflows/scrollz-propose.js`
的 `canonicalKey()` 比对。两份实现必须对同一输入产出**逐字节相同**的串，否则跨轮
去重静默失效——不报错、不告警，只是每轮重新提出同一个候选。

这条测试存在的理由是一次真实漂移：Python 原先用 `re` 的 `\\s`，它匹配 `\\x1c`–`\\x1f`；
而 ECMAScript 的 `\\s` **不**匹配这几个字符。`\\x1f` 恰恰是拼接四个字段用的分隔符
本身，于是同一输入 `a\\x1fb` 两侧算出不同结果。

这不是理论风险：`STAGE1_TOOLS` 在同一批改动里刚因为"第二份硬编码真相"漂移过一次。
凡是同一语义有两份实现，就必须有一条测试把它们钉在一起。
"""

import json
import subprocess
import unittest
from pathlib import Path

from harness.queue import canonical_key

REPO = Path(__file__).resolve().parents[4]
WORKFLOW = REPO / ".claude/workflows/scrollz-propose.js"

# 刻意挑选的样本：普通文本、各类空白、以及 JS 与 Python 的 `\s` 定义有分歧的区间。
SAMPLES = [
    ("补 CRC", "尾日志完整性", "crates/scrollz/src/archive.rs", "翻转一字节即 fail"),
    ("  前后空白  ", "多个   空格", "tab\t分隔", "换行\n与\r\n"),
    # 分隔符本身出现在字段里——两侧 `\s` 定义分歧的核心样本
    ("a\x1fb", "c\x1cd", "e\x1df", "g\x1eh"),
    # 非 ASCII 空白：全角空格、不换行空格、零宽 BOM
    ("全角　空格", "不换行 空格", "BOM﻿", "en quad"),
    ("MiXeD CaSe", "UPPER", "Path/With/Case.RS", "Oracle Text"),
    ("", "", "", ""),
]


class TestCanonicalKeyCrossLanguage(unittest.TestCase):
    def test_python_and_js_agree_on_every_sample(self):
        node = subprocess.run(["node", "--version"], capture_output=True,
                              text=True)
        if node.returncode != 0:                     # pragma: no cover
            self.skipTest("node 不可用，无法做跨语言比对")

        source = WORKFLOW.read_text(encoding="utf-8")
        start = source.index("function canonicalKey")
        end = source.index("\n}", start) + 2
        js_fn = source[start:end]

        script = js_fn + "\nconst rows = " + json.dumps(SAMPLES) + ";\n" + (
            "console.log(JSON.stringify(rows.map("
            "([g, i, p, o]) => canonicalKey("
            "{goal: g, invariant: i, primary_path: p, oracle: o}))));\n")

        proc = subprocess.run(["node", "--input-type=module", "-e", script],
                              capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0,
                         f"node 执行失败：{proc.stderr[-400:]}")
        js_keys = json.loads(proc.stdout)
        py_keys = [canonical_key(*row) for row in SAMPLES]

        for sample, js, py in zip(SAMPLES, js_keys, py_keys):
            with self.subTest(sample=sample):
                self.assertEqual(
                    py, js,
                    "Python 与 JS 的 canonical key 不一致——跨轮去重会静默失效。\n"
                    f"  输入: {sample!r}\n  python: {py!r}\n  js:     {js!r}")


if __name__ == "__main__":
    unittest.main()
