#!/usr/bin/env python3
"""Self-test for check-duplicate-majors.py: the chose-both gate.

The gate's whole point is honest classification, so the tests are the
misclassification cases: a transitive straggler must not block, two
members genuinely requiring different majors must block, workspace
inheritance must resolve to the root's requirement, undecidable
requirements must never produce a blocking guess, and nested graphs (the
sqlite-side pattern) are judged on their own lockfiles.
"""

import importlib.util
import pathlib
import tempfile
import unittest

_spec = importlib.util.spec_from_file_location(
    "check_duplicate_majors",
    pathlib.Path(__file__).resolve().parent / "check-duplicate-majors.py",
)
mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mod)

LOCK_DUPLICATE = """\
[[package]]
name = "alpha"
version = "1.2.0"

[[package]]
name = "alpha"
version = "2.0.1"

[[package]]
name = "beta"
version = "0.3.0"
"""

LOCK_SINGLE = """\
[[package]]
name = "alpha"
version = "2.0.1"
"""


def ws(*deps: str) -> str:
    return "[package]\nname = \"x\"\n\n[dependencies]\n" + "\n".join(deps) + "\n"


class RequirementMajorTest(unittest.TestCase):
    def test_ordinary_requirements_select_a_major(self):
        for req, major in [("2", 2), ("2.1", 2), ("=2.1.3", 2), ("^2", 2), (">=2", 2)]:
            self.assertEqual(mod.requirement_major(req), major, req)

    def test_undecidable_requirements_return_none(self):
        for req in ["*", ">=1, <3", ""]:
            self.assertIsNone(mod.requirement_major(req), req)


class RequirementsTest(unittest.TestCase):
    def test_workspace_true_resolves_to_the_root_requirement(self):
        with tempfile.TemporaryDirectory() as d:
            root = pathlib.Path(d)
            (root / "Cargo.toml").write_text(
                "[workspace.dependencies]\nalpha = \"1\"\n", encoding="utf-8")
            (root / "member").mkdir()
            (root / "member" / "Cargo.toml").write_text(
                ws("alpha = { workspace = true }"), encoding="utf-8")
            reqs = mod.requirements_from_manifests(root)
            self.assertEqual(reqs["alpha"], {1})

    def test_feature_tables_do_not_count_as_requirements(self):
        with tempfile.TemporaryDirectory() as d:
            root = pathlib.Path(d)
            (root / "Cargo.toml").write_text(
                "[package]\nname = \"x\"\n[features]\nalpha = []\n", encoding="utf-8")
            self.assertEqual(mod.requirements_from_manifests(root), {})


class CheckRepoTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.dir.name)
        (self.root / "Cargo.lock").write_text(LOCK_DUPLICATE, encoding="utf-8")

    def tearDown(self):
        self.dir.cleanup()

    def test_chose_both_blocks(self):
        (self.root / "Cargo.toml").write_text(ws("alpha = \"1\""), encoding="utf-8")
        (self.root / "sub").mkdir()
        (self.root / "sub" / "Cargo.toml").write_text(ws("alpha = \"2\""), encoding="utf-8")
        blocking, _ = mod.check_repo(self.root, include_transitive=False)
        self.assertEqual(len(blocking), 1)
        self.assertIn("chose both", blocking[0])

    def test_transitive_straggler_is_informational_only(self):
        # Only major 2 is required directly; major 1 rode in under an
        # upstream. Blocking this would demand fixing someone else's crate.
        (self.root / "Cargo.toml").write_text(ws("alpha = \"2\""), encoding="utf-8")
        blocking, info = mod.check_repo(self.root, include_transitive=True)
        self.assertEqual(blocking, [])
        self.assertEqual(len(info), 1)
        self.assertIn("transitive", info[0])

    def test_undecidable_requirement_never_blocks(self):
        (self.root / "Cargo.toml").write_text(ws("alpha = \">=1, <3\""), encoding="utf-8")
        blocking, _ = mod.check_repo(self.root, include_transitive=False)
        self.assertEqual(blocking, [])

    def test_allowlisted_duplicate_passes_with_reason(self):
        original = dict(mod.ALLOWED)
        mod.ALLOWED[("alpha", "1,2")] = "documented deliberate split"
        try:
            (self.root / "Cargo.toml").write_text(ws("alpha = \"1\""), encoding="utf-8")
            blocking, info = mod.check_repo(self.root, include_transitive=False)
            self.assertEqual(blocking, [])
            self.assertTrue(any("allowed" in line for line in info))
        finally:
            mod.ALLOWED.clear()
            mod.ALLOWED.update(original)

    def test_nested_graph_is_judged_on_its_own_lock(self):
        # The sqlite-side pattern: a nested crate with its own lockfile is a
        # separate graph and must be judged independently.
        side = self.root / "side"
        side.mkdir()
        (side / "Cargo.toml").write_text(ws("alpha = \"2\""), encoding="utf-8")
        (side / "Cargo.lock").write_text(LOCK_SINGLE, encoding="utf-8")
        (self.root / "Cargo.toml").write_text(ws("alpha = \"1\""), encoding="utf-8")
        blocking, _ = mod.check_repo(self.root, include_transitive=False)
        # The root graph still chose both? No: the root's requirement set
        # includes the nested manifest (recursive walk), so the side lock is
        # clean but the root lock blocks.
        self.assertEqual(len(blocking), 1)
        self.assertIn(str(self.root / "Cargo.lock"), blocking[0])

    def test_target_directories_are_skipped(self):
        pkg = self.root / "target" / "package"
        pkg.mkdir(parents=True)
        (pkg / "Cargo.lock").write_text(LOCK_DUPLICATE, encoding="utf-8")
        (self.root / "Cargo.toml").write_text(ws("alpha = \"2\""), encoding="utf-8")
        blocking, _ = mod.check_repo(self.root, include_transitive=False)
        self.assertEqual(blocking, [])


class MainTest(unittest.TestCase):
    def test_exit_codes_follow_findings(self):
        with tempfile.TemporaryDirectory() as d:
            root = pathlib.Path(d)
            (root / "Cargo.toml").write_text(ws("alpha = \"1\""), encoding="utf-8")
            (root / "sub").mkdir()
            (root / "sub" / "Cargo.toml").write_text(ws("alpha = \"2\""), encoding="utf-8")
            (root / "Cargo.lock").write_text(LOCK_DUPLICATE, encoding="utf-8")
            self.assertEqual(mod.main([str(root)]), 1)
            (root / "Cargo.lock").write_text(LOCK_SINGLE, encoding="utf-8")
            self.assertEqual(mod.main([str(root)]), 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
