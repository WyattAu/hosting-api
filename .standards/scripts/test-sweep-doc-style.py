#!/usr/bin/env python3
"""Self-test for sweep-doc-style.py: the DOC-STYLE rule 1 sweep.

The dangerous half of a sweep is what it touches by mistake, so most cases
here assert that something is REFUSED rather than converted. The paired-dash
case is the one that produced a mangled sentence during the first manual
sweep, and the placeholder-cell case is how a table says "not supported".
"""

import importlib.util
import pathlib
import tempfile
import unittest

_spec = importlib.util.spec_from_file_location(
    "sweep_doc_style",
    pathlib.Path(__file__).resolve().parent / "sweep-doc-style.py",
)
mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mod)

EM = "\u2014"


class ClassifyTest(unittest.TestCase):
    def test_bold_lead_in_takes_a_colon(self):
        verdict, value = mod.classify(f"- **`Composer`** {EM} the state machine.")
        self.assertEqual(verdict, "auto")
        self.assertEqual(value, "- **`Composer`**: the state machine.")

    def test_table_cell_takes_a_colon(self):
        verdict, value = mod.classify(f"| `error` | `A2lError` {EM} typed |")
        self.assertEqual((verdict, value), ("auto", "| `error` | `A2lError`: typed |"))

    def test_heading_takes_a_colon(self):
        verdict, value = mod.classify(f"# Security Policy {EM} a2l-parse")
        self.assertEqual((verdict, value), ("auto", "# Security Policy: a2l-parse"))

    def test_prose_takes_a_colon(self):
        verdict, value = mod.classify(f"the result {EM} measured, not assumed.")
        self.assertEqual((verdict, value), ("auto", "the result: measured, not assumed."))

    def test_paired_dashes_are_refused(self):
        line = f"Nothing dropped {EM} losing a boundary is worst {EM} and the span"
        verdict, reason = mod.classify(line)
        self.assertEqual(verdict, "manual")
        self.assertIn("paired", reason)

    def test_unspaced_dash_is_refused(self):
        self.assertEqual(mod.classify(f"plan{EM}ship")[0], "manual")

    def test_leading_dash_is_refused(self):
        self.assertEqual(mod.classify(f"- {EM} continuation of a wrapped sentence")[0], "manual")

    def test_placeholder_cell_is_refused(self):
        verdict, reason = mod.classify(f"| Prometheus | yes | {EM} |")
        self.assertEqual(verdict, "manual")
        self.assertIn("empty table cell", reason)

    def test_line_without_dash_is_untouched(self):
        self.assertEqual(mod.classify("plain line")[0], "auto")


class SweepTextTest(unittest.TestCase):
    def test_clean_text_is_unchanged_and_preserves_trailing_newline(self):
        text = "# Title\n\nA colon: fine.\n"
        new, manual = mod.sweep_text(text)
        self.assertEqual((new, manual), (text, []))

    def test_manual_lines_are_left_byte_identical_and_reported(self):
        line = f"a {EM} b {EM} c"
        text = f"# T\n{line}\n"
        new, manual = mod.sweep_text(text)
        self.assertIn(line, new)
        self.assertEqual(manual[0][0], 2)
        self.assertEqual(manual[0][1], "paired dashes")

    def test_missing_trailing_newline_is_not_added(self):
        new, _ = mod.sweep_text(f"x {EM} y")
        self.assertEqual(new, "x: y")


class SweepFileTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.dir.name)

    def tearDown(self):
        self.dir.cleanup()

    def test_dry_run_leaves_file_untouched(self):
        p = self.root / "a.md"
        p.write_text(f"x {EM} y\n", encoding="utf-8")
        before = p.read_bytes()
        mod.main([str(self.root)])
        self.assertEqual(p.read_bytes(), before)

    def test_write_applies_and_resweep_is_a_noop(self):
        p = self.root / "a.md"
        p.write_text(f"x {EM} y\n", encoding="utf-8")
        self.assertEqual(mod.main([str(self.root), "--write"]), 0)
        self.assertEqual(p.read_text(encoding="utf-8"), "x: y\n")
        self.assertEqual(mod.main([str(self.root), "--write"]), 0)

    def test_write_with_manual_cases_exits_one(self):
        p = self.root / "a.md"
        p.write_text(f"x {EM} y {EM} z\n", encoding="utf-8")
        self.assertEqual(mod.main([str(self.root), "--write"]), 1)

    def test_build_directories_are_skipped(self):
        (self.root / "target" / "package").mkdir(parents=True)
        (self.root / "target" / "package" / "snap.md").write_text(
            f"old {EM} published copy\n", encoding="utf-8")
        self.assertEqual(mod.main([str(self.root)]), 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
