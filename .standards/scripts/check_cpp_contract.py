#!/usr/bin/env python3
"""Check the caller/callee contract of the shared C++ gate matrix.

Runs in the CALLER's context (cwd = caller repo checkout) with the estate
repo checked out at `estate/`. Fails when:

1. no caller workflow `uses:` cpp-kit.yml;
2. the caller passes an input the callee does not declare;
3. `tier` is not fast|full;
4. any gate in the matrix below is missing from the callee;
5. any checkout step in the caller file lacks `persist-credentials: false`
   (estate convention).

Usage: python3 estate/scripts/check_cpp_contract.py [--callee PATH]
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

try:
    import yaml
except ImportError:
    print("check_cpp_contract: PyYAML is required (pip install pyyaml)")
    sys.exit(1)

# The C++ gate matrix: every id here must exist as a callee job.
MATRIX = (
    "contract",
    "format-check",
    "headless-debug",
    "headless-debug-clang",
    "headless-release",
    "tsan",
    "asan-ubsan",
    "vulkan-validation",
    "live-proof",
    "docs",
    "examples",
    "presets",
)


def fail(message: str) -> int:
    print(f"check_cpp_contract FAILED: {message}")
    return 1


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--callee",
        default="estate/.github/workflows/cpp-kit.yml",
        help="path to the shared cpp-kit.yml (default: estate checkout)",
    )
    args = parser.parse_args()

    callee_path = Path(args.callee)
    if not callee_path.is_file():
        return fail(f"callee file not found: {callee_path}")
    callee = yaml.safe_load(callee_path.read_text(encoding="utf-8"))

    # NOTE: YAML 1.1 parses the `on:` key as boolean True.
    on = callee.get("on", callee.get(True, {}))
    if "workflow_call" not in on:
        return fail(f"{callee_path} does not declare on: workflow_call")
    declared_inputs = set((on.get("workflow_call") or {}).get("inputs", {}))

    caller_file = caller_name = caller_job = None
    caller_doc = None
    for path in sorted(Path(".github/workflows").glob("*.yml")):
        doc = yaml.safe_load(path.read_text(encoding="utf-8"))
        for name, job in (doc.get("jobs") or {}).items():
            if not isinstance(job, dict):
                continue
            if "cpp-kit.yml" in str(job.get("uses", "")):
                caller_file, caller_name, caller_job, caller_doc = (
                    path,
                    name,
                    job,
                    doc,
                )
    if caller_job is None:
        return fail("no caller workflow job uses cpp-kit.yml")

    for passed in (caller_job.get("with") or {}):
        if passed not in declared_inputs:
            return fail(
                f"caller passes undeclared input {passed!r} "
                f"(declared: {sorted(declared_inputs)})"
            )
    tier = (caller_job.get("with") or {}).get("tier", "full")
    if tier not in ("fast", "full"):
        return fail(f"tier must be fast|full, got {tier!r}")

    callee_jobs = callee.get("jobs", {})
    for gate in MATRIX:
        if gate not in callee_jobs:
            return fail(f"gate {gate!r} missing from {callee_path}")

    for job_name, job in caller_doc.get("jobs", {}).items():
        if not isinstance(job, dict):
            continue
        for step in job.get("steps", []) or []:
            if not isinstance(step, dict):
                continue
            if str(step.get("uses", "")).startswith("actions/checkout@"):
                with_ = step.get("with") or {}
                if with_.get("persist-credentials") is not False:
                    return fail(
                        f"caller job {job_name!r}: checkout "
                        "without persist-credentials: false"
                    )

    print(f"check_cpp_contract: caller {caller_file}::{caller_name} agrees "
          f"(tier={tier}, {len(MATRIX)} gates)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
