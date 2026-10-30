#!/usr/bin/env python3
"""Self-test for drift-sweep.py: the round-close drift drain.

Pure functions only -- the cargo invocations are integration territory.
The surfaces that matter: audit-line parsing (dedup across the root and
@HEAD:side consumer spellings), manifest bumping that respects HOLDS,
and store reconciliation that moves or adds without oscillating.
"""

import importlib.util
import pathlib
import tempfile
import unittest

_spec = importlib.util.spec_from_file_location(
    "drift_sweep",
    pathlib.Path(__file__).resolve().parent / "drift-sweep.py",
)
mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mod)

AUDIT_SAMPLE = """\
--- DRIFT (5) ---------------------------------------------------
  cas-kit: WyattAu/estate-integration pins =0.2.1 but crates.io has 0.2.2 (published 2026-10-26)
  cas-kit: WyattAu/estate-integration@HEAD:sqlite-side pins =0.2.1 but crates.io has 0.2.2 (published 2026-10-26)
  webhookkit: WyattAu/estate-integration pins =2.2.1 but crates.io has 2.2.2 (published 2026-10-26)
  vat-rules: WyattAu/estate-integration pins =0.1.1 but crates.io has 0.1.2 (published 2026-10-26)
--- DEBT (0) ----------------------------------------------------
"""


class ParseAuditTest(unittest.TestCase):
    def test_parse_dedups_and_keeps_holds(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            f = pathlib.Path(d) / "audit.out"
            f.write_text(AUDIT_SAMPLE, encoding="utf-8")
            pairs = mod.parse_audit(f)
        # cas-kit appears twice (root + side spellings) and dedups.
        self.assertEqual(pairs, [
            ("cas-kit", "0.2.1", "0.2.2"),
            ("webhookkit", "2.2.1", "2.2.2"),
            ("vat-rules", "0.1.1", "0.1.2"),
        ])


class BumpManifestTest(unittest.TestCase):
    def test_bumps_move_pins_and_respect_holds(self):
        with tempfile.TemporaryDirectory() as d:
            manifest = pathlib.Path(d) / "Cargo.toml"
            manifest.write_text(
                'cas-kit = "=0.2.1"\n'
                'webhookkit = "=2.2.1"\n'
                'vat-rules = { version = "=0.1.1", features = ["x"] }\n',
                encoding="utf-8")
            n = mod.bump_manifest(manifest, [
                ("cas-kit", "0.2.1", "0.2.2"),
                ("webhookkit", "2.2.1", "2.2.2"),
                ("vat-rules", "0.1.1", "0.1.2"),
            ])
            self.assertEqual(n, 2, "the held crate is not bumped")
            text = manifest.read_text(encoding="utf-8")
            self.assertIn('cas-kit = "=0.2.2"', text)
            self.assertIn('webhookkit = "=2.2.1"', text, "hold respected")
            self.assertIn('vat-rules = { version = "=0.1.2", features = ["x"] }', text)

    def test_absent_pins_are_silent(self):
        with tempfile.TemporaryDirectory() as d:
            manifest = pathlib.Path(d) / "Cargo.toml"
            manifest.write_text('other = "=1.0"\n', encoding="utf-8")
            self.assertEqual(mod.bump_manifest(manifest, [("cas-kit", "0.2.1", "0.2.2")]), 0)


class ApplyStoreEntriesTest(unittest.TestCase):
    def test_moves_existing_and_adds_missing(self):
        with tempfile.TemporaryDirectory() as d:
            store = pathlib.Path(d) / "config.toml"
            store.write_text(
                '[[exemptions.cas-kit]]\nversion = "0.2.1"\ncriteria = "safe-to-deploy"\n\n',
                encoding="utf-8")
            mod.apply_store_entries(store, [("cas-kit", "0.2.2"), ("new-crate", "0.1.0")])
            text = store.read_text(encoding="utf-8")
            self.assertIn('[[exemptions.cas-kit]]\nversion = "0.2.2"', text)
            self.assertIn('[[exemptions.new-crate]]\nversion = "0.1.0"', text)

    def test_second_version_adds_alongside_instead_of_oscillating(self):
        # The failure mode measured on config-kit and idempotency-kit: a
        # crate under two graph versions must hold both entries; moving to
        # newest re-flags the old version next pass.
        with tempfile.TemporaryDirectory() as d:
            store = pathlib.Path(d) / "config.toml"
            store.write_text(
                '[[exemptions.two-ver]]\nversion = "0.2.0"\ncriteria = "safe-to-deploy"\n\n',
                encoding="utf-8")
            mod.apply_store_entries(store, [("two-ver", "0.1.0")])
            text = store.read_text(encoding="utf-8")
            self.assertIn('version = "0.2.0"', text)
            self.assertIn('version = "0.1.0"', text)


class PruneStaleTest(unittest.TestCase):
    def test_entries_whose_version_left_the_lock_are_dropped(self):
        with tempfile.TemporaryDirectory() as d:
            store = pathlib.Path(d) / "config.toml"
            lock = pathlib.Path(d) / "Cargo.lock"
            store.write_text(
                '[[exemptions.alpha]]\nversion = "1.0.0"\ncriteria = "safe-to-deploy"\n\n'
                '[[exemptions.alpha]]\nversion = "2.0.0"\ncriteria = "safe-to-deploy"\n\n',
                encoding="utf-8")
            lock.write_text(
                '[[package]]\nname = "alpha"\nversion = "2.0.0"\n', encoding="utf-8")
            dropped = mod.prune_stale_entries(store, lock)
            self.assertEqual(dropped, 1)
            text = store.read_text(encoding="utf-8")
            self.assertIn('version = "2.0.0"', text)
            self.assertNotIn('version = "1.0.0"', text)

    def test_live_versions_are_kept(self):
        with tempfile.TemporaryDirectory() as d:
            store = pathlib.Path(d) / "config.toml"
            lock = pathlib.Path(d) / "Cargo.lock"
            store.write_text(
                '[[exemptions.alpha]]\nversion = "1.0.0"\ncriteria = "safe-to-deploy"\n\n'
                '[[exemptions.alpha]]\nversion = "2.0.0"\ncriteria = "safe-to-deploy"\n\n',
                encoding="utf-8")
            lock.write_text(
                '[[package]]\nname = "alpha"\nversion = "1.0.0"\n\n'
                '[[package]]\nname = "alpha"\nversion = "2.0.0"\n', encoding="utf-8")
            self.assertEqual(mod.prune_stale_entries(store, lock), 0)
            self.assertEqual(store.read_text(encoding="utf-8").count("[[exemptions.alpha]]"), 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
