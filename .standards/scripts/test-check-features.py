#!/usr/bin/env python3
"""Self-test for scripts/check-features.py.

Each fixture is a miniature crate under `scripts/testdata/<name>/src/lib.rs`
plus the rules it must trigger. The fixtures are the *historical* estate
regressions, reproduced verbatim in shape:

  r1_enum_variant      outbox-kit 0.1.0 vs breaker's `timeout` feature
  r1_private_enum      same shape, but not public API -> must stay silent
  r1_nested_module     pub enum in a pub mod — nesting must not hide it
  r1_non_exhaustive    #[non_exhaustive] enum — the sanctioned remedy
  r2_struct_field      a gated field on a pub struct
  r2_non_exhaustive    gated field on #[non_exhaustive] struct -> allowed
  r3_match_arm         a match arm that vanishes under unification
  r4_trait_method      a gated required trait method
  r4_default_method    a gated trait method *with a default body* -> allowed
  r5_bare_suppression  a suppression with no reason -> must be reported
  scoped_binary_only   a member with no lib.rs is not a library -> silent
  scoped_vendored      a vendored dependency's source is not ours -> silent
  clean                ordinary feature usage -> must stay silent

Run:  python3 test-check-features.py
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
CHECKER = HERE / "check-features.py"
TESTDATA = HERE / "testdata"


def run(cwd: Path) -> tuple[int, list[dict]]:
    proc = subprocess.run(
        [sys.executable, str(CHECKER), "--cwd", str(cwd), "--json"],
        capture_output=True,
        text=True,
    )
    if proc.returncode not in (0, 1):
        raise SystemExit(
            "checker crashed on %s (rc=%d)\n%s" % (cwd, proc.returncode, proc.stderr)
        )
    import json

    data = json.loads(proc.stdout)
    return proc.returncode, data["findings"]


def fixture(name: str, dest: Path) -> Path:
    """Copy a fixture tree verbatim, preserving nested workspace layouts."""
    src = TESTDATA / name
    if not src.is_dir():
        raise SystemExit("missing fixture: %s" % src)
    dest.mkdir(parents=True, exist_ok=True)
    shutil.copytree(src, dest, dirs_exist_ok=True)
    return dest


def check(name: str, expect_rules: list[str], expect_clean: bool = False) -> bool:
    """expect_rules: rules that MUST be reported. expect_clean: nothing at all."""
    with tempfile.TemporaryDirectory() as td:
        cwd = fixture(name, Path(td) / name)
        rc, findings = run(cwd)
        got = sorted({f["rule"] for f in findings})

        if expect_clean:
            ok = rc == 0 and not findings
            detail = "expected silence, got rc=%d rules=%s" % (rc, got)
        else:
            missing = [r for r in expect_rules if r not in got]
            unexpected = [r for r in got if r not in expect_rules]
            ok = rc == 1 and not missing and not unexpected
            detail = "expected %s, got %s (missing=%s unexpected=%s)" % (
                sorted(expect_rules),
                got,
                missing,
                unexpected,
            )
            for f in findings:
                detail += "\n      %s:%d [%s] %s" % (
                    f["file"],
                    f["line"],
                    f["rule"],
                    f["message"][:90],
                )

    print("%-20s %s  %s" % (name, "PASS" if ok else "FAIL", detail))
    return ok


CASES = [
    ("r1_enum_variant", ["R1"], False),
    ("r1_private_enum", [], True),
    ("r1_nested_module", ["R1"], False),
    ("r1_non_exhaustive", [], True),
    ("r2_struct_field", ["R2"], False),
    ("r2_non_exhaustive", [], True),
    ("r3_match_arm", ["R3"], False),
    ("r4_trait_method", ["R4"], False),
    ("r4_default_method", [], True),
    ("r5_bare_suppression", ["R5"], False),
    ("scoped_binary_only", [], True),
    ("scoped_vendored", [], True),
    ("clean", [], True),
]


def main() -> int:
    if not TESTDATA.is_dir():
        print("no testdata/ — cannot self-test", file=sys.stderr)
        return 2
    results = [check(*case) for case in CASES]
    failed = results.count(False)
    print()
    if failed:
        print("feature-compat self-test FAILED: %d/%d" % (failed, len(results)))
        return 1
    print("feature-compat self-test passed (%d/%d)" % (len(results), len(results)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
