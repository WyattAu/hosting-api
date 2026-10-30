#!/usr/bin/env python3
"""Estate layer checker — enforces the WyattAu L0-L3 layer model.

Rule: a crate's declared layer must be >= the layer of every estate-internal
crate it depends on (directly or transitively). Dependencies must never point
UP the stack:

    L3 -> L0/L1/L2/L3,  L2 -> L0/L1/L2,  L1 -> L0/L1,  L0 -> (proc-macro companions only)

L0 additionally requires zero runtime estate-internal dependencies; the only
estate deps an L0 crate may have are its own proc-macro (codegen companion)
crates, which compile for the host and create no runtime edge.

Crates at L3, and crates with no declared layer, are not restricted (logged
only). Missing declarations are warnings during the rollout.

Usage (from the repo root, as the shared CI job does):

    python3 check-layers.py --cwd . --estate-list estate-crates.txt

Pure stdlib: json + subprocess to `cargo metadata`. No third-party deps.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

VALID_TIERS = ("L0", "L1", "L2", "L3")
TIER_ORDER = {"L0": 0, "L1": 1, "L2": 2, "L3": 3}
CHECKED_KINDS = ("normal", "build")  # dev-dependencies do not count for layering


def warn(msg: str) -> None:
    prefix = "::warning::" if os.environ.get("GITHUB_ACTIONS") == "true" else "warning: "
    print(f"{prefix}{msg}")


def fail(msg: str) -> None:
    prefix = "::error::" if os.environ.get("GITHUB_ACTIONS") == "true" else "error: "
    print(f"{prefix}{msg}")


def load_estate_tiers(path: Path) -> dict[str, str]:
    """Authoritative crate -> tier table (crate names are unversioned, so the
    tier of a *dependency* is looked up here, not from the vendored registry
    manifest which only reflects the last published version)."""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        warn(f"could not read estate tier table {path}: {exc}")
        return {}
    tiers = data.get("tiers", {})
    bad = {t for t in tiers.values() if t not in VALID_TIERS}
    if bad:
        warn(f"invalid tiers in {path}: {sorted(bad)}")
    return {c: t for c, t in tiers.items() if t in VALID_TIERS}


def load_estate_list(path: Path) -> set[str]:
    names = set()
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.split("#", 1)[0].strip()
        if line:
            names.add(line)
    return names


def declared_tier(manifest_path: str) -> str | None:
    """Extract tier from [package.metadata.layers] tier = "Lx" in Cargo.toml.

    Section-aware scan (stdlib only, no tomllib dependency on older runners):
    find the `[package.metadata.layers]` header, then read `tier = "Lx"` from
    the keys that follow it, before the next table header.
    """
    try:
        text = Path(manifest_path).read_text(encoding="utf-8")
    except OSError:
        return None
    header = re.compile(r"^\s*\[package\.metadata\.layers\]\s*$", re.MULTILINE)
    match = header.search(text)
    if not match:
        return None
    rest = text[match.end():]
    section = rest.split("\n[", 1)[0]  # keys until the next table header
    tier = re.search(r'^\s*tier\s*=\s*"(L[0-3])"\s*(?:#.*)?$', section, re.MULTILINE)
    return tier.group(1) if tier else None


def cargo_metadata(cwd: Path) -> dict:
    proc = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        cwd=str(cwd),
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        print(proc.stderr, file=sys.stderr)
        raise SystemExit(2)
    meta = json.loads(proc.stdout)
    if not meta.get("resolve"):
        raise SystemExit(
            "error: cargo metadata returned no resolve graph "
            "(is the workspace resolvable?)"
        )
    return meta


def packages_by_name(meta: dict) -> dict[str, dict]:
    by_name: dict[str, dict] = {}
    for pkg in meta["packages"]:
        # first entry wins; duplicates only differ by version
        by_name.setdefault(pkg["name"], pkg)
    return by_name


def proc_macro_packages(meta: dict) -> set[str]:
    names = set()
    for pkg in meta["packages"]:
        for target in pkg.get("targets", []):
            if "proc-macro" in target.get("kind", []):
                names.add(pkg["name"])
                break
    return names


def transitive_estate_deps(
    meta: dict,
    member_id: str,
    estate: set[str],
    name_of_id: dict[str, str],
) -> set[str]:
    """All estate-internal package names reachable from `member_id` over
    normal/build edges. Path deps and registry deps are treated identically.
    Matching is by package name, resolved through the package id (resolve
    nodes key deps by lib-target name, e.g. `throttle_kit`, while package
    names are hyphenated: `throttle-kit`)."""
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    found: set[str] = set()
    stack = [member_id]
    visited: set[str] = set()
    while stack:
        node_id = stack.pop()
        if node_id in visited:
            continue
        visited.add(node_id)
        node = nodes.get(node_id)
        if node is None:
            continue
        # walk every normal/build edge (estate or not) to find deeper estate crates
        for dep in node.get("deps", []):
            kinds = {(dk.get("kind") or "normal") for dk in dep.get("dep_kinds", [])}
            if not (kinds & set(CHECKED_KINDS)):
                continue
            dep_name = name_of_id.get(dep["pkg"], dep["name"])
            if dep_name in estate:
                found.add(dep_name)
            stack.append(dep["pkg"])
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description="WyattAu estate layer checker")
    parser.add_argument("--cwd", default=".", help="repo/workspace root to check")
    parser.add_argument(
        "--estate-list",
        default=str(Path(__file__).with_name("estate-crates.txt")),
        help="file with one WyattAu crates.io crate name per line",
    )
    parser.add_argument(
        "--estate-tiers",
        default=str(Path(__file__).with_name("estate-tiers.json")),
        help="authoritative crate -> tier table for dependency lookups",
    )
    args = parser.parse_args()

    estate = load_estate_list(Path(args.estate_list))
    if not estate:
        print(f"error: estate list {args.estate_list} is empty", file=sys.stderr)
        return 2
    tier_table = load_estate_tiers(Path(args.estate_tiers))

    meta = cargo_metadata(Path(args.cwd))
    by_id = {p["id"]: p for p in meta["packages"]}
    by_name = packages_by_name(meta)
    proc_macros = proc_macro_packages(meta)

    def tier_of_dep(dep_name: str) -> str | None:
        """Tier of an estate dependency: tiers table first (authoritative),
        then its local manifest if it is vendored in this workspace (covers
        brand-new estate crates not yet in the regenerated table)."""
        if dep_name in tier_table:
            return tier_table[dep_name]
        dep_pkg = by_name.get(dep_name)
        if dep_pkg is not None:
            return declared_tier(dep_pkg["manifest_path"])
        return None

    violations: list[str] = []
    warnings = 0
    checked = 0
    leaf_members = 0

    for member_id in meta["workspace_members"]:
        pkg = by_id.get(member_id)
        if pkg is None:
            continue
        name = pkg["name"]
        tier = declared_tier(pkg["manifest_path"])
        estate_deps = transitive_estate_deps(
            meta, member_id, estate, {p["id"]: p["name"] for p in meta["packages"]}
        )
        estate_deps.discard(name)  # a workspace-internal crate is not its own dep

        if not estate_deps:
            leaf_members += 1
            continue

        if tier is None:
            warnings += 1
            warn(
                f"{name}: no [package.metadata.layers] tier declared "
                f"(estate deps: {', '.join(sorted(estate_deps))}) — rollout warning"
            )
            continue

        checked += 1
        crate_violations: list[str] = []

        if tier == "L3":
            print(f"ok: {name} L3 (composition layer, no estate-layer restriction)")
            continue

        for dep in sorted(estate_deps):
            dep_tier = tier_of_dep(dep)
            is_proc_macro = dep in proc_macros

            if tier == "L0" and not is_proc_macro:
                crate_violations.append(
                    f"{name} declared L0 but has runtime estate dep {dep}: "
                    "L0 crates must have zero runtime estate deps "
                    "(proc-macro companions are the only permitted estate dep)"
                )
            elif dep_tier is None:
                warn(f"{name} {tier} -> estate dep {dep} is undeclared (not enforced)")
            elif TIER_ORDER[tier] < TIER_ORDER[dep_tier]:
                crate_violations.append(
                    f"{name} declared {tier} but depends on estate crate {dep} "
                    f"({dep_tier}): dependency points up the stack "
                    f"({tier} -> {dep_tier})"
                )

        if crate_violations:
            violations.extend(crate_violations)
            for v in crate_violations:
                print(f"VIOLATION: {v}")
        else:
            print(
                f"ok: {name} {tier} (estate deps: {', '.join(sorted(estate_deps))})"
            )

    print()
    if warnings:
        print(f"{warnings} undeclared crate(s) — warnings only during rollout")
    if violations:
        for v in violations:
            fail(v)
        print(
            "\nlayer check FAILED: "
            f"{len(violations)} violation(s). Fix: raise the dependent "
            "crate's layer to cover its deps, or restructure the dependency "
            "downward. There are no exceptions."
        )
        return 1
    print(f"layer check passed ({checked} declared crate(s) checked, "
          f"{leaf_members} member(s) with no estate deps)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
