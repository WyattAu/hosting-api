#!/usr/bin/env python3
"""Unit test: every estate.yml repo entry carries the schema the audit
and the review surfaces consume.

Cycle 13 (2026-10-29): `accounting-spec` was added to the manifest by
hand mid-loop. The entry was well-formed, but nothing would have
caught a malformed one network-free: the audit reads area/status at
runtime and an entry missing them either crashes the run or silently
skips the repo's checks. This test pins the entry schema so the
manifest cannot drift into shapes the consumers do not parse.
"""
from __future__ import annotations

import pathlib
import sys

import yaml

HERE = pathlib.Path(__file__).parent
MANIFEST = HERE.parent / "estate.yml"

REQUIRED_FIELDS = ("area", "status", "notes")


def repo_entries(manifest: dict) -> dict:
    """The repos map, wherever the document root puts it."""
    if isinstance(manifest, dict) and isinstance(manifest.get("repos"), dict):
        return manifest["repos"]
    return manifest


def main() -> int:
    raw = yaml.safe_load(MANIFEST.read_text())
    assert isinstance(raw, dict), "estate.yml must parse to a mapping"

    repos = repo_entries(raw)
    assert repos, "estate.yml must declare at least one repo"

    problems: list[str] = []
    for name, entry in sorted(repos.items()):
        if not isinstance(entry, dict):
            problems.append(f"{name}: entry is not a mapping")
            continue
        for field in REQUIRED_FIELDS:
            value = entry.get(field)
            if not isinstance(value, str) or not value.strip():
                problems.append(f"{name}: missing or empty '{field}'")
        if not isinstance(entry.get("crates", []), list):
            problems.append(f"{name}: 'crates' must be a list")
        seen = entry.get("_seen", {})
        if not isinstance(seen, dict):
            problems.append(f"{name}: '_seen' must be a mapping")

    if problems:
        for p in problems:
            print(f"FAIL: {p}", file=sys.stderr)
        return 1

    print(f"estate.yml schema ok: {len(repos)} repos, all carry "
          f"{', '.join(REQUIRED_FIELDS)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
