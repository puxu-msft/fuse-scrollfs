"""Freeze canonical-key normalization after the JavaScript seam retired.

The former cross-language test compared Python with ``scrollz-propose.js``.
ADR-002 made :func:`harness.queue.canonical_key` the only implementation for
within-round and cross-round deduplication.  These assertions preserve the
same boundary samples and their current Python outputs without a Node process.
"""

import unittest

from harness.queue import _norm, canonical_key


class TestCanonicalKeyNormalization(unittest.TestCase):
    def test_control_characters_are_preserved_not_folded(self):
        self.assertEqual(_norm("a\x1fb"), "a\x1fb")
        self.assertEqual(_norm("c\x1cd"), "c\x1cd")
        self.assertEqual(_norm("e\x1df"), "e\x1df")
        self.assertEqual(_norm("g\x1eh"), "g\x1eh")

    def test_ascii_whitespace_is_stripped_and_folded(self):
        self.assertEqual(_norm("  前后空白  "), "前后空白")
        self.assertEqual(_norm("多个   空格"), "多个 空格")
        self.assertEqual(_norm("tab\t分隔"), "tab 分隔")
        self.assertEqual(_norm("换行\n与\r\n"), "换行 与")

    def test_ecmascript_non_ascii_spaces_are_folded(self):
        self.assertEqual(_norm("全角　空格"), "全角 空格")
        self.assertEqual(_norm("不换行 空格"), "不换行 空格")
        self.assertEqual(_norm("en quad"), "en quad")

    def test_bom_is_stripped_at_edge(self):
        self.assertEqual(_norm("BOM﻿"), "bom")

    def test_case_is_lowered(self):
        self.assertEqual(_norm("MiXeD CaSe"), "mixed case")

    def test_empty_string_normalizes_to_empty(self):
        self.assertEqual(_norm(""), "")

    def test_canonical_key_joins_normalized_fields_with_separator(self):
        self.assertEqual(
            canonical_key("Goal", "Invariant", "path/To/File.rs", "Oracle"),
            "goal\x1finvariant\x1fpath/to/file.rs\x1foracle",
        )

    def test_canonical_key_is_deterministic(self):
        first = canonical_key("g", "i", "p", "o")
        second = canonical_key("g", "i", "p", "o")
        self.assertEqual(first, second)


if __name__ == "__main__":
    unittest.main()
