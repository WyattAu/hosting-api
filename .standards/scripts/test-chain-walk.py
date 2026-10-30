#!/usr/bin/env python3
"""Self-test for chain-walk.py's pure surfaces.

The network and cargo invocations are integration territory; what the
tests own are the decisions the walk makes on their behalf: which
requirements need a republish, how versions bump, how manifests rewrite,
and how the crate->repo map resolves.
"""

import importlib.util
import pathlib
import tempfile
import unittest

_spec = importlib.util.spec_from_file_location(
    "chain_walk",
    pathlib.Path(__file__).resolve().parent / "chain-walk.py",
)
mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mod)


class NextPatchTest(unittest.TestCase):
    def test_patch_bumps_follow_the_house_convention(self):
        self.assertEqual(mod.next_patch("0.1.3"), "0.1.4")
        self.assertEqual(mod.next_patch("1.0.0"), "1.0.1")
        self.assertEqual(mod.next_patch("0.3.0"), "0.3.1")

    def test_non_plain_versions_are_refused(self):
        with self.assertRaises(ValueError):
            mod.next_patch("0.1.0-rc.1")
        with self.assertRaises(ValueError):
            mod.next_patch("latest")


class ReqPinsVersionTest(unittest.TestCase):
    def test_exact_pins_match_only_that_version(self):
        self.assertTrue(mod.req_pins_version("=0.1.0", "0.1.0"))
        self.assertFalse(mod.req_pins_version("=0.1.0", "0.1.1"))
        self.assertFalse(mod.req_pins_version("=0.1.1", "0.1.0"))

    def test_ranges_are_never_treated_as_pins(self):
        self.assertFalse(mod.req_pins_version("^0.1", "0.1.0"))
        self.assertFalse(mod.req_pins_version("0.1", "0.1.0"))


class RepoForTest(unittest.TestCase):
    def test_resolves_through_the_estate_registry(self):
        estate = {"repos": {
            "some-repo": {"crates": ["some-crate", "other-crate"]},
            "empty-repo": {"crates": []},
        }}
        self.assertEqual(mod.repo_for("other-crate", estate), "some-repo")
        self.assertIsNone(mod.repo_for("unknown-crate", estate))

    def test_missing_crates_key_does_not_raise(self):
        estate = {"repos": {"bare": {}}}
        self.assertIsNone(mod.repo_for("anything", estate))


class BumpManifestDepTest(unittest.TestCase):
    def test_plain_and_inline_table_formats_both_rewrite(self):
        with tempfile.TemporaryDirectory() as d:
            manifest = pathlib.Path(d) / "Cargo.toml"
            manifest.write_text(
                'recon-kit = "=0.1.0"\n'
                'ap-kit = { version = "=0.1.1", features = ["x"] }\n',
                encoding="utf-8")
            n = mod.bump_manifest_dep(manifest, "recon-kit", "0.1.1")
            self.assertEqual(n, 1)
            text = manifest.read_text(encoding="utf-8")
            self.assertIn('recon-kit = "=0.1.1"', text)
            self.assertIn('ap-kit = { version = "=0.1.1"', text,
                          "an unrelated crate's req is untouched")

    def test_absent_crate_makes_no_write(self):
        with tempfile.TemporaryDirectory() as d:
            manifest = pathlib.Path(d) / "Cargo.toml"
            before = 'other = "=1.0"\n'
            manifest.write_text(before, encoding="utf-8")
            self.assertEqual(mod.bump_manifest_dep(manifest, "recon-kit", "0.1.1"), 0)
            self.assertEqual(manifest.read_text(encoding="utf-8"), before)


class BumpVersionTest(unittest.TestCase):
    def test_bumps_the_patch_segment(self):
        with tempfile.TemporaryDirectory() as d:
            manifest = pathlib.Path(d) / "Cargo.toml"
            manifest.write_text('[package]\nname = "x"\nversion = "0.1.3"\n', encoding="utf-8")
            self.assertEqual(mod.bump_version(manifest), "0.1.4")
            self.assertIn('version = "0.1.4"', manifest.read_text(encoding="utf-8"))

    def test_missing_version_returns_none(self):
        with tempfile.TemporaryDirectory() as d:
            manifest = pathlib.Path(d) / "Cargo.toml"
            manifest.write_text('[package]\nname = "x"\n', encoding="utf-8")
            self.assertIsNone(mod.bump_version(manifest))


if __name__ == "__main__":
    unittest.main(verbosity=2)
