#!/usr/bin/env python3
"""Topological batch publisher for the estate's crates.

The estate publishes multi-crate dependency chains (yesterday's
mp4-core -> id3-core -> audiobook-core dance was ordered by hand from a
laptop). This script does it in dependency order, with retries for
registry-index propagation, and skips crates already at their manifest
version on crates.io.

    # Show what would be published, in order
    python3 scripts/crates.py publish --dry-run

    # Publish everything that needs it
    python3 scripts/crates.py publish --token $CRATES_IO_TOKEN

    # Publish one repo directory's workspace, waiting between publishes
    python3 scripts/crates.py publish --repo ../mp4-core --token ...

Works from any estate repo directory: the graph comes from
`cargo metadata` over the repo's own workspace, so a single-crate repo
publishes just itself.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path


def cargo_metadata(repo: str) -> dict:
    proc = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=repo, capture_output=True, text=True,
    )
    if proc.returncode != 0:
        print(proc.stderr, file=sys.stderr)
        raise SystemExit(2)
    return json.loads(proc.stdout)


def crates_io_version(name: str) -> str | None:
    req = urllib.request.Request(
        f"https://crates.io/api/v1/crates/{name}",
        headers={"User-Agent": "wyatt-estate-publisher"},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            data = json.load(r)
        return data["crate"].get("max_stable_version")
    except Exception:
        return None


def topological(packages: list[dict]) -> list[dict]:
    """Dependency order over the workspace's internal edges only."""
    by_name = {p["name"]: p for p in packages}
    deps: dict[str, set[str]] = {p["name"]: set() for p in packages}
    for p in packages:
        for dep in p["dependencies"]:
            d = dep["name"]
            if d in by_name and d != p["name"]:
                deps[p["name"]].add(d)
    order, done = [], set()
    def visit(name: str) -> None:
        if name in done:
            return
        for d in sorted(deps[name]):
            visit(d)
        done.add(name)
        order.append(name)
    for name in sorted(deps):
        visit(name)
    return [by_name[n] for n in order]


def publish(repo: str, name: str, token: str | None, dry_run: bool) -> bool:
    cmd = ["cargo", "publish", "-p", name]
    if token:
        cmd += ["--token", token]
    if dry_run:
        cmd += ["--dry-run", "--allow-dirty"]
    for attempt in range(1, 4):
        proc = subprocess.run(cmd, cwd=repo, capture_output=True, text=True)
        out = proc.stdout + proc.stderr
        if proc.returncode == 0:
            return True
        # Already-published is terminal success. A dirty tree is terminal
        # FAILURE: the caller must commit the manifest/lock first, or the
        # publish silently does not happen (this exact bug shipped 0.1.1
        # an hour late on October 23).
        # Already-published is terminal success; registry races retry.
        # Everything else — including a dirty-tree refusal, whose message
        # mentions --allow-dirty — is a FAILURE. A substring match once
        # treated that refusal as success and reported a publish that never
        # happened (ui-text-edit 0.1.2, October 2026).
        if "already exists" in out or "already uploaded" in out:
            print(f"  {name}: already on crates.io")
            return True
        print(f"  {name}: attempt {attempt} failed, retrying in 20s", file=sys.stderr)
        time.sleep(20)
    print(f"  {name}: FAILED after 3 attempts", file=sys.stderr)
    print(out[-2000:], file=sys.stderr)
    return False


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("command", choices=["publish"])
    ap.add_argument("--repo", default=".", help="repo/workspace directory")
    ap.add_argument("--token", default=None, help="crates.io token (else cargo credentials)")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--settle", type=int, default=30, help="seconds to wait after each publish for index propagation")
    args = ap.parse_args()

    meta = cargo_metadata(args.repo)
    packages = meta["packages"]
    order = topological(packages)

    # Skip members that are path-only helpers (publish = false) and members
    # already at their published version.
    todo = []
    for p in order:
        # cargo metadata encodes `publish = false` as an empty list.
        if p.get("publish") == []:
            continue
        live = crates_io_version(p["name"])
        if live == p["version"]:
            print(f"  {p['name']} {p['version']}: already on crates.io, skipping")
            continue
        todo.append(p)

    if not todo:
        print("nothing to publish")
        return
    print(f"publish order: {' -> '.join(p['name'] for p in todo)}")
    if args.dry_run:
        for p in todo:
            print(f"  would publish {p['name']} {p['version']}")
        return

    failed = []
    for p in todo:
        print(f"publishing {p['name']} {p['version']} ...")
        if not publish(args.repo, p["name"], args.token, args.dry_run):
            failed.append(p["name"])
            continue
        print(f"  published {p['name']} {p['version']}; settling {args.settle}s for index propagation")
        time.sleep(args.settle)
    if failed:
        print(f"FAILED: {failed}", file=sys.stderr)
        raise SystemExit(1)
    print("all published")


if __name__ == "__main__":
    main()
