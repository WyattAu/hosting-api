#!/usr/bin/env python3
"""Self-test for check-doc-style.py: the em-dash gate.

The gate is one rule, so the test is short and concrete: clean files pass,
em-dash files fail with navigable findings, fenced code blocks are NOT
skipped (an em-dash in sample prose is still in a document), non-UTF8 files
report instead of crashing, and directories are walked recursively.
"""

import importlib.util
import pathlib
import tempfile
import unittest

_spec = importlib.util.spec_from_file_location(
    "check_doc_style",
    pathlib.Path(__file__).resolve().parent / "check-doc-style.py",
)
mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mod)


class DocStyleTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.dir.name)

    def tearDown(self):
        self.dir.cleanup()

    def write(self, name: str, text: str) -> pathlib.Path:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def check(self):
        return mod.check_file

    def test_clean_file_passes(self):
        p = self.write("clean.md", "# Title\n\nA colon: carries the sense.\n")
        self.assertEqual(mod.check_file(p), [])

    def test_em_dash_fails_with_line_number(self):
        p = self.write("bad.md", "# Title\n\nWords \u2014 more words.\n")
        findings = mod.check_file(p)
        self.assertEqual(len(findings), 1)
        self.assertTrue(findings[0].startswith(f"{p}:3:"), findings[0])

    def test_fenced_code_blocks_are_not_skipped(self):
        # Deliberately different from the llms-links gate: DOC-STYLE's own
        # check greps the whole file, and an em-dash in sample prose is still
        # in a document.
        p = self.write("fenced.md", "```\ncomment \u2014 stays\n```\n")
        self.assertNotEqual(mod.check_file(p), [])

    def test_unreadable_file_reports_instead_of_crashing(self):
        p = self.root / "binary.md"
        p.write_bytes(b"\xff\xfe\x00broken")
        findings = mod.check_file(p)
        self.assertEqual(len(findings), 1)
        self.assertIn("unreadable", findings[0])

    def test_files_from_walks_directories_and_filters_md(self):
        self.write("a.md", "x")
        (self.root / "b.txt").write_text("x", encoding="utf-8")
        (self.root / "sub").mkdir()
        self.write("sub/c.md", "x")
        files = mod.files_from([self.root])
        names = [f.name for f in files]
        self.assertEqual(names, ["a.md", "c.md"], "non-markdown files excluded")


if __name__ == "__main__":
    unittest.main(verbosity=2)