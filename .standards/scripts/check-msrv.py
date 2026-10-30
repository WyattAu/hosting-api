#!/usr/bin/env python3
"""Check that published-crate repositories declare a canonical rust-version.

Every estate-authored crate declares `rust-version` (58 of 58 at the time
this gate was written), but a declaration nobody compiles against is a
promise, not a property. This gate keeps the declaration present, canonical,
and visible:

  missing          a published-crate repo without rust-version. Blocking:
                   the fleet standard predates this gate; there is no
                   excuse for a new gap.
  non-canonical    a patch-suffixed value ("1.95.0") where the fleet style
                   is major.minor. Blocking for estate-authored repos: it
                   is a one-character fix that keeps the fleet greppable.
  above floor      a value newer than the fleet floor (the mode of all
                   declared values, unless --floor says otherwise).
                   Informational: a crate that genuinely needs a newer
                   toolchain is allowed to say so, but the list is what
                   makes raising the fleet floor a decision instead of an
                   accident.

Forks and dormant repos are reported informationally only: their
manifests follow upstream conventions, and study 45/46's rule is that we
do not restyle other people's trees.

    check-msrv.py --estate estate.yml [--floor 1.85]

Exit 1 on missing or non-canonical declarations in estate-authored repos.
"""

from __future__ import annotations

import argparse
import collections
import pathlib
import re
import sys

import yaml  # pyyaml; the estate gates already require it

SKIP_DIRS = {"target", "node_modules", "vendor", ".git"}
RUST_VERSION_RE = re.compile(r'^rust-version\s*=\s*"([^"]+)"', re.M)
# forks keep upstream's manifest conventions; orphans are upstream trees we
# mirror without adapting; dormant repos are frozen. None are ours to restyle.
SKIP_STATUSES = {"fork", "dormant", "orphan"}


def declared_msrv(repo: pathlib.Path) -> str | None:
    """The first rust-version in the tree, or None."""
    for toml in sorted(repo.rglob("Cargo.toml")):
        if any(part in SKIP_DIRS for part in toml.parts):
            continue
        try:
            match = RUST_VERSION_RE.search(toml.read_text(encoding="utf-8"))
        except OSError:
            continue
        if match:
            return match.group(1)
    return None


def canonical(value: str) -> bool:
    """Fleet style is major.minor; major.minor.patch is a style deviation."""
    parts = value.split(".")
    return len(parts) == 2


def newer(value: str, floor: str) -> bool:
    def key(v: str) -> tuple[int, int]:
        parts = v.split(".")
        return (int(parts[0]), int(parts[1]) if len(parts) > 1 else 0)

    return key(value) > key(floor)


def mode_value(values: list[str]) -> str | None:
    if not values:
        return None
    (value, _), = collections.Counter(values).most_common(1)
    return value


def run(estate_path: pathlib.Path, floor: str | None, repo_root: pathlib.Path) -> int:
    estate = yaml.safe_load(estate_path.read_text(encoding="utf-8"))["repos"]
    checked: list[tuple[str, str]] = []
    missing: list[str] = []
    noncanonical: list[str] = []
    fork_notes: list[str] = []
    for repo, meta in estate.items():
        status = meta.get("status")
        if not (meta.get("crates") or []):
            continue
        directory = repo_root / repo
        if not directory.is_dir():
            continue
        value = declared_msrv(directory)
        if status in SKIP_STATUSES:
            if value is not None:
                fork_notes.append(f"{repo}: {value} (fork/dormant: upstream convention)")
            continue
        if value is None:
            missing.append(repo)
            continue
        checked.append((repo, value))
        if not canonical(value):
            noncanonical.append(
                f"{repo}: rust-version = \"{value}\" is not fleet style; "
                f"drop the patch segment (\"{value.rsplit('.', 1)[0]}\")"
            )

    floor_value = floor or mode_value([v for _, v in checked]) or "1.85"
    above = sorted(
        (repo, value) for repo, value in checked
        if canonical(value) and newer(value, floor_value)
    )

    for line in fork_notes:
        print(f"info: {line}")
    for repo, value in above:
        print(f"info: {repo}: rust-version {value} is above the fleet floor {floor_value}")

    if missing or noncanonical:
        for repo in missing:
            print(f"MISSING: {repo} declares no rust-version")
        for line in noncanonical:
            print(f"NON-CANONICAL: {line}")
        return 1

    print(
        f"msrv: {len(checked)} estate-authored repo(s) declare rust-version, "
        f"all canonical; fleet floor {floor_value}"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--estate", type=pathlib.Path, default=pathlib.Path("estate.yml"))
    parser.add_argument("--floor", default=None,
                        help="fleet floor override (default: the mode of declared values)")
    parser.add_argument("--repo-root", type=pathlib.Path, default=pathlib.Path("."),
                        help="directory containing the estate's repo checkouts")
    args = parser.parse_args(argv)
    return run(args.estate, args.floor, args.repo_root)


if __name__ == "__main__":
    sys.exit(main())
