#!/usr/bin/env python3
"""Self-test for the coverage-debt entitlement fix in estate-audit.py.

AGENTS.md rule 7: a gate must prove it is entitled to the claim it makes.
The claim here is "this crate is composed by no consumer", which was false for
`theme-kit` and `color-kit` until 2026-10-18: chisel-doc composes both, and the
audit reported them as never composed. Three defects produced the false claim:

  1. Only `estate-integration` was listed in CONSUMERS, so no product repo's
     consumption was visible.
  2. Only the root `[dependencies]`-style tables were read, so a workspace that
     declares versions in `[workspace.dependencies]` (chisel-doc does) showed
     nothing.
  3. Consumption was defined as an exact pin (`=1.2.3`), so a product repo
     pinning a caret range (`theme-kit = "0.1.0"`) did not count, even though
     a caret range is precisely how a product consumes a library.

These tests pin the fixed behaviour. They are pure functions of the manifest
parsing, so they need no network.
"""

import importlib.util
import pathlib
import sys
import unittest

_SPEC = importlib.util.spec_from_file_location(
    "estate_audit",
    pathlib.Path(__file__).resolve().parent / "estate-audit.py",
)
estate_audit = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(estate_audit)


WORKSPACE_TOML = """
[package]
name = "consumer"
version = "0.1.0"

[workspace.dependencies]
theme-kit = "0.1.0"
color-kit = "0.1.0"

[dependencies]
tui-actions = "=0.1.0"
unicode-segmentation = "1.13"

[dev-dependencies]
font-shape = { workspace = true }
input-kit = "0.1.0"

[build-dependencies]
cc = "1"
"""


class FakeCompleted:
    """What `subprocess.run` returns: an object with `.stdout`."""

    def __init__(self, text):
        self.stdout = text
        self.returncode = 0


class FakeGh:
    """Stands in for `gh api`, returning one manifest's base64 content."""

    def __init__(self, text):
        import base64
        import json
        self.stdout = json.dumps(
            {"content": base64.b64encode(text.encode()).decode()}
        )

    def __call__(self, *args, **kwargs):
        return FakeCompleted(self.stdout)


class ConsumerPinTest(unittest.TestCase):
    def setUp(self):
        self._real_run = estate_audit.subprocess.run

    def tearDown(self):
        estate_audit.subprocess.run = self._real_run

    def pins_for(self, toml_text):
        estate_audit.subprocess.run = FakeGh(toml_text)
        return estate_audit.fetch_consumer_pins("WyattAu/example")

    def test_workspace_dependencies_are_read(self):
        """Defect 2: chisel-doc keeps its versions in [workspace.dependencies]."""
        _exact, loose = self.pins_for(WORKSPACE_TOML)
        self.assertIn("theme-kit", loose, "workspace table was not read")
        self.assertIn("color-kit", loose)

    def test_caret_range_counts_as_consumption_but_not_as_a_pin(self):
        """Defect 3: a product pins a caret range; that is consumption, not drift."""
        exact, loose = self.pins_for(WORKSPACE_TOML)
        self.assertIn("theme-kit", loose, "caret range must count as composed")
        self.assertNotIn("theme-kit", exact, "caret range must not count as a pin")
        self.assertIn("tui-actions", exact, "=1.2.3 is a pin")
        self.assertIn("tui-actions", loose, "a pin is also consumption")

    def test_workspace_true_member_still_counts(self):
        """`{ workspace = true }` names the crate even though it has no version."""
        _exact, loose = self.pins_for(WORKSPACE_TOML)
        self.assertIn("font-shape", loose, "workspace = true was dropped")
        self.assertNotIn("font-shape", _exact, "workspace = true is not a pin")

    def test_only_exact_versions_are_pins(self):
        """`exact` gates the drift check, so a caret range must never enter it.

        The coverage check filters on estate membership separately, so a
        third-party crate may pass through `loose` without affecting any
        estate claim.
        """
        exact, loose = self.pins_for(WORKSPACE_TOML)
        self.assertIn("unicode-segmentation", loose, "third-party still parsed")
        self.assertNotIn("unicode-segmentation", exact, "1.13 is not an exact pin")
        self.assertEqual(exact, {"tui-actions": "0.1.0"},
                         "only an =version spec is a pin")
        self.assertIsNone(exact.get("font-shape"), "workspace=true is no pin")

    def test_split_consumer_parses_a_branch_ref(self):
        """CONSUMERS entries carry `owner/repo@ref` for non-default branches."""
        self.assertEqual(
            estate_audit.split_consumer("WyattAu/chisel-doc@rebrand"),
            ("WyattAu/chisel-doc", "rebrand", "Cargo.toml"),
        )
        self.assertEqual(
            estate_audit.split_consumer("WyattAu/estate-integration"),
            ("WyattAu/estate-integration", "HEAD", "Cargo.toml"),
        )

    def test_split_consumer_parses_a_manifest_path(self):
        """`owner/repo@ref subdir` reads that directory's manifest.

        The sqlite-side crate exists precisely because its rusqlite major
        cannot resolve against the root's; without path support the audit
        would only ever read the root manifest and report nine crates as
        uncomposed that are composed one directory over.
        """
        self.assertEqual(
            estate_audit.split_consumer("WyattAu/estate-integration@HEAD sqlite-side"),
            ("WyattAu/estate-integration", "HEAD", "sqlite-side/Cargo.toml"),
        )

    def test_a_product_repo_is_a_tracked_consumer(self):
        """Rule 7: product consumption must be visible, or the coverage claim
        is false for anything a product actually uses."""
        self.assertIn("WyattAu/estate-integration", estate_audit.CONSUMERS)
        self.assertTrue(
            any("chisel-doc" in c for c in estate_audit.CONSUMERS),
            "a product repo must be a tracked consumer, or product consumption "
            "is invisible to the coverage claim",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)