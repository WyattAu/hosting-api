#!/usr/bin/env python3
"""Fail if one action is pinned to more than one SHA across the estate.

Loop 26: seven actions were pinned to two different SHAs in different repos
(checkout v4 and v7.0.1, setup-go v5 and v7.0.0, upload-artifact v7.0.1 and
v7.0.2, scorecard-action v2.4.1 and v2.4.4, ...). Every individual pin was
reproducible, so nothing looked wrong; the estate as a whole simply had no
single version of its own automation. 79 pins converged to one per action.

This gate reads the workflow files of every estate repo over the network and
fails on any action with more than one distinct SHA, so the split cannot come
back unnoticed.

Deliberately excluded: actions pinned to a moving branch (`master`, `stable`,
`release/v1` -- e.g. dtolnay/rust-toolchain for the MSRV gate), where two SHAs
mean two branches, not two versions.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
import urllib.error
import urllib.request

UA = "estate-pin-consistency"


def _headers() -> dict[str, str]:
    """Authenticate when a token is available.

    Scanning 197 repos is ~400 API calls; the anonymous limit is 60/hour, so
    an unauthenticated run cannot finish. CI's GITHUB_TOKEN raises that to
    5,000/hour. Without a token the gate would fail on rate limits rather
    than on drift, which is its own kind of lie.
    """
    import os

    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    headers = {"User-Agent": UA, "Accept": "application/vnd.github+json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers
# Reusable workflow references look like `owner/repo/.github/workflows/kit.yml@sha`
# and are pins too -- loop 31 proved the original regex was blind to them, which
# is how six templates sat on kit references the gate could not see.
PINS = re.compile(
    r"uses:\s*([\w.-]+/[\w.-]+(?:/[^\s@]+)?)@([0-9a-f]{40})(?:\s*#\s*(\S+))?"
)
# Refs that track a branch by design; two SHAs there are two branches.
MOVING_REFS = {"master", "stable", "main", "release/v1", "nightly"}


def workflow_paths(paths: list[pathlib.Path]) -> list[pathlib.Path]:
    found: list[pathlib.Path] = []
    for path in paths:
        if path.is_file():
            found.append(path)
        elif path.is_dir():
            found.extend(sorted((path / ".github" / "workflows").glob("*.y*ml")))
    return found


def collect(files: list[pathlib.Path]) -> dict[str, dict[str, list[str]]]:
    """action -> sha -> [repo:file] across every file."""
    pins: dict[str, dict[str, list[str]]] = {}
    for wf in files:
        for action, sha, comment in PINS.findall(wf.read_text(encoding="utf-8")):
            if comment and comment in MOVING_REFS:
                continue
            pins.setdefault(action, {}).setdefault(sha, []).append(wf.name)
    return pins


def fetch_remote(owner: str, repo: str, ref: str = "main") -> list[tuple[str, str]] | None:
    """(filename, source) pairs for one repo's workflows, or None if it has none."""
    url = (
        f"https://api.github.com/repos/{owner}/{repo}/contents/.github/workflows?ref={ref}"
    )
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=_headers()),
                                    timeout=30) as fh:
            listing = json.loads(fh.read())
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise
    bodies: list[tuple[str, str]] = []
    for item in listing:
        # A workflows *directory* entry carries download_url: null and would
        # crash urllib -- skip anything that is not a downloadable file.
        download = item.get("download_url")
        if not download or item.get("type") != "file":
            continue
        with urllib.request.urlopen(
            urllib.request.Request(download, headers=_headers()), timeout=30
        ) as fh:
            bodies.append((item.get("name") or download.rsplit("/", 1)[-1],
                           fh.read().decode("utf-8")))
    return bodies


def repos_from_manifest(manifest: pathlib.Path) -> list[tuple[str, str]]:
    """Loop-owned repos only: those carrying the `# Loop-owned` marker.

    The manifest is the whole estate (~200 repos); the pin invariant is the
    loop's responsibility for the Omni family, the standards repo, and their
    derived e2e repos -- not for every personal repo the owner maintains.
    Enforcing estate-wide would fail on repos this loop has never been asked
    to touch; ignoring them entirely would hide drift in the ones it owns.
    The marker makes the scope explicit and reviewable in one file.
    """
    import yaml

    data = yaml.safe_load(manifest.read_text(encoding="utf-8")) or {}
    owner = data.get("owner") or "WyattAu"
    repos = data.get("repos") or {}
    gated: list[tuple[str, str]] = []
    for name, entry in repos.items():
        if isinstance(entry, dict) and entry.get("gated"):
            gated.append((owner, str(name)))
    if not gated:
        raise ValueError(
            f"{manifest}: no repo carries `gated: true` -- refusing to pass vacuously"
        )
    return sorted(gated)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path, help="local repo roots/files")
    parser.add_argument("--estate", type=pathlib.Path, help="check every repo in this manifest")
    args = parser.parse_args(argv)

    pins: dict[str, dict[str, list[str]]] = {}
    scanned = 0
    if args.estate:
        for owner, repo in repos_from_manifest(args.estate):
            bodies = fetch_remote(owner, repo)
            if bodies is None:
                continue
            scanned += 1
            for name, body in bodies:
                for action, sha, comment in PINS.findall(body):
                    if comment and comment in MOVING_REFS:
                        continue
                    pins.setdefault(action, {}).setdefault(sha, []).append(f"{repo}/{name}")
    else:
        if not args.paths:
            parser.error("pass repo roots/files or --estate MANIFEST")
        files = workflow_paths(args.paths)
        scanned = len(files)
        pins = collect(files)

    split = {a: v for a, v in pins.items() if len(v) > 1}
    distinct = sum(len(v) for v in pins.values())
    if split:
        print("pin consistency FAILED:", file=sys.stderr)
        for action, shas in sorted(split.items()):
            print(f"  {action}:", file=sys.stderr)
            for sha, where in shas.items():
                print(f"    {sha[:12]}... in {len(where)} file(s): {', '.join(where[:3])}",
                      file=sys.stderr)
        print(
            "\nConverge to one SHA per action (one version of the estate's own\n"
            "automation) before shipping. Loop 26 converged 79 such pins.",
            file=sys.stderr,
        )
        return 1

    print(f"pin consistency: {distinct} distinct pins over {len(pins)} actions, "
          f"{scanned} file(s) scanned -- one SHA per action")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
