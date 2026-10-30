#!/usr/bin/env python3
"""Flag graphs where this repo's own manifests require two majors of one crate.

The estate's rule, learned from rusqlite (0.32 and 0.40 cannot share one
graph because of the native sqlite3 link): one major per name per graph.

Scope discipline, because most lockfile duplicates are not ours to fix.
A crate name appearing under two locked majors is classified by what the
repo's manifests require:

  chose both     some member requires major A and another requires major B.
                 This is blocking: the fix is ours, and it is a merge of the
                 two requirements onto the newer major.
  transitive     only one major is required directly; the second arrives
                 under an upstream's requirement. Informational with --all.
                 It disappears when upstreams unify, not when we will it.

    check-duplicate-majors.py [paths...]     # default: the current tree
    check-duplicate-majors.py --all          # also list transitive-only

Exit 1 only on chose-both findings. Intentional duplicates are carried in
ALLOWED with a reason, never inline-suppressed at the call site.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

SKIP_DIRS = {"target", "node_modules", "vendor", ".git", "dist", "build"}

PACKAGE_RE = re.compile(r'\[\[package\]\]\s*\nname = "([^"]+)"\s*\nversion = "([^"]+)"')
DEP_LINE_RE = re.compile(r'^([a-zA-Z0-9_-]+)\s*=\s*(.+?)(?:#.*)?$')
SECTION_RE = re.compile(r"^\[(?:dev-|build-)?dependencies(?:\.[^\]]+)?\]$")
MAJOR_RE = re.compile(r"^\s*[=><~^ ]*v?(\d+)")
INLINE_VERSION_RE = re.compile(r'version\s*=\s*"([^"]+)"')

# (crate name, sorted major list) -> reason this duplicate is deliberate.
ALLOWED: dict[tuple[str, str], str] = {}


def parse_lock(text: str) -> dict[str, set[str]]:
    """crate name -> the exact versions locked in this graph."""
    found: dict[str, set[str]] = {}
    for name, version in PACKAGE_RE.findall(text):
        found.setdefault(name, set()).add(version.split("+")[0])
    return found


def requirement_major(req: str) -> int | None:
    """The major a semver requirement selects, or None when not decidable.

    `2`, `2.1`, `=2.1.3`, `^2`, `>=2` select major 2. Ranges, `*`, and
    anything undecidable return None so the caller never blocks on a guess.
    Inline tables (`{ version = "1" ... }`) yield their version field.
    """
    if req.strip().startswith("{"):
        found = INLINE_VERSION_RE.search(req)
        req = found.group(1) if found else ""
    req = req.strip().strip("\"").strip()
    if "," in req or "*" in req or not req:
        return None
    match = MAJOR_RE.match(req)
    return int(match.group(1)) if match else None


def requirements_from_manifests(repo: pathlib.Path) -> dict[str, set[int | None]]:
    """crate name -> majors the repo's own manifests select.

    `workspace = true` inherits the root's requirement, so the root
    [workspace.dependencies] table is resolved first and substituted.
    """
    root = repo / "Cargo.toml"
    ws_deps: dict[str, str] = {}
    if root.exists():
        try:
            in_ws = False
            for line in root.read_text(encoding="utf-8").splitlines():
                stripped = line.strip()
                if stripped.startswith("["):
                    in_ws = stripped == "[workspace.dependencies]"
                    continue
                if in_ws:
                    m = DEP_LINE_RE.match(stripped)
                    if m and not m.group(2).strip().startswith("{ path"):
                        ws_deps[m.group(1)] = m.group(2).strip().strip('"')
        except OSError:
            pass

    majors: dict[str, set[int | None]] = {}
    for toml in repo.rglob("Cargo.toml"):
        if any(part in SKIP_DIRS for part in toml.parts):
            continue
        try:
            text = toml.read_text(encoding="utf-8")
        except OSError:
            continue
        in_deps = False
        for line in text.splitlines():
            stripped = line.strip()
            if stripped.startswith("["):
                in_deps = bool(SECTION_RE.match(stripped))
                continue
            if not in_deps:
                continue
            m = DEP_LINE_RE.match(stripped)
            if not m:
                continue
            name, value = m.group(1), m.group(2).strip().strip('"')
            if "workspace = true" in value:
                value = ws_deps.get(name, "")
            majors.setdefault(name, set()).add(requirement_major(value))
    return majors


def duplicate_majors(versions: set[str]) -> bool:
    return len({v.split(".")[0] for v in versions}) > 1


def check_repo(repo: pathlib.Path, include_transitive: bool) -> tuple[list[str], list[str]]:
    """(blocking findings, informational lines) for one repo tree."""
    blocking: list[str] = []
    info: list[str] = []
    locks = [
        p for p in repo.rglob("Cargo.lock")
        if not any(part in SKIP_DIRS for part in p.parts)
    ]
    required = requirements_from_manifests(repo)
    for lock in locks:
        locked = parse_lock(lock.read_text(encoding="utf-8", errors="ignore"))
        for name, versions in sorted(locked.items()):
            if not duplicate_majors(versions):
                continue
            locked_majors = {int(v.split(".")[0]) for v in versions}
            key = (name, ",".join(str(m) for m in sorted(locked_majors)))
            reason = ALLOWED.get(key)
            if reason:
                info.append(f"{lock}: {name} majors {key[1]} allowed: {reason}")
                continue
            selected = {m for req_majors in required.get(name, set()) for m in [req_majors]}
            if None not in selected and len(selected & locked_majors) >= 2:
                blocking.append(
                    f"{lock}: {name} is required under majors "
                    f"{sorted(selected & locked_majors)} and locked as {sorted(versions)}: "
                    f"the repo chose both. Merge the requirements onto one major."
                )
            elif include_transitive:
                info.append(f"{lock}: {name} majors {sorted(versions)} (transitive only)")
    return blocking, info


def repos_from(paths: list[pathlib.Path]) -> list[pathlib.Path]:
    """Each lock's owning tree, with nested trees judged on their own locks."""
    locks_seen: set[pathlib.Path] = set()
    out: list[pathlib.Path] = []
    candidates: list[pathlib.Path] = []
    for path in paths:
        if path.is_file():
            candidates.append(path.parent)
        elif path.is_dir():
            candidates.extend(
                lock.parent for lock in path.rglob("Cargo.lock")
                if not any(part in SKIP_DIRS for part in lock.parts)
            )
    for repo in sorted(set(candidates)):
        repo_locks = {
            p for p in repo.rglob("Cargo.lock")
            if not any(part in SKIP_DIRS for part in p.parts)
        }
        if repo_locks <= locks_seen:
            continue
        locks_seen |= repo_locks
        out.append(repo)
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path,
                        help="repos or lock files to check (default: current tree)")
    parser.add_argument("--all", action="store_true",
                        help="also list transitive-only duplicates (informational)")
    args = parser.parse_args(argv)

    roots = args.paths or [pathlib.Path(".")]
    blocking_total: list[str] = []
    info_total: list[str] = []
    for repo in repos_from(roots):
        blocking, info = check_repo(repo, args.all)
        blocking_total.extend(blocking)
        info_total.extend(info)

    for line in info_total:
        print(f"info: {line}")
    if blocking_total:
        print(f"\n{len(blocking_total)} chose-both duplicate major(s):")
        for line in blocking_total:
            print(f"  {line}")
        return 1
    print(
        f"no graphs where this repo chose both majors "
        f"({len(info_total)} transitive-only, shown with --all)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
