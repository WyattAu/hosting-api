#!/usr/bin/env python3
"""Walk the exact-pin republish chain after an upstream crate bumps.

The estate pins its own crates exactly (`=x.y.z`), so when a producer
crate publishes a new version, every estate crate that pins the old
version must republish before the composition graphs can follow. Doing
that walk by hand consumed an afternoon per chain (vat-rules 0.1.1: ap-kit
-> fx-kit -> invoice-kit -> vat-return -> statement-camt). This script is
that walk:

  1. query crates.io reverse dependencies of the bumped crate;
  2. keep the dependents the estate owns (crate -> repo via estate.yml);
  3. for each, in dependency order: clone or pull its repo, bump the
     dependency requirement to the new version, bump the crate's own
     patch version, run the tests, and (with --publish) publish;
  4. recurse: the republished crate may itself have estate dependents;
  5. report the chain, the skips, and the failures.

Dry-run by default: without --publish the walk prints the plan and runs
tests but touches neither the registry nor the remotes (it does commit
locally, so a dry run still leaves reviewable state on each branch).

    chain-walk.py --crate recon-kit --to 0.1.1 --estate estate.yml
    chain-walk.py --crate recon-kit --to 0.1.1 --publish

Exit 0 when the chain is walked (published or planned), 1 on any test
failure or publish failure -- a half-walked chain is reported, never
hidden.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
import urllib.request

UA = "estate-chain-walk (wyatt_au@protonmail.com)"
PATCH_BUMP = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


def http_json(url: str) -> dict:
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with urllib.request.urlopen(req, timeout=30) as fh:
        return json.load(fh)


def next_patch(version: str) -> str:
    """0.1.3 -> 0.1.4; 1.0.0 -> 1.0.1. The house chain convention."""
    m = PATCH_BUMP.match(version)
    if not m:
        raise ValueError(f"not a plain x.y.z version: {version!r}")
    return f"{m.group(1)}.{m.group(2)}.{int(m.group(3)) + 1}"


def req_pins_version(req: str, version: str) -> bool:
    """True when the requirement is an exact pin of exactly this version."""
    return req.strip() == f"={version}"


def latest_version(crate: str) -> str | None:
    """The newest non-yanked version, or None."""
    try:
        data = http_json(f"https://crates.io/api/v1/crates/{crate}")
        versions = [v for v in data.get("versions", []) if not v.get("yanked")]
        return versions[0]["num"] if versions else None
    except Exception:
        return None


def repo_for(crate: str, estate: dict) -> str | None:
    for repo, meta in estate["repos"].items():
        if crate in (meta.get("crates") or []):
            return repo
    return None


def reverse_dependents(crate: str) -> list[tuple[str, str]]:
    """(dependent crate, requirement) pairs from crates.io.

    The reverse-deps endpoint names the DEPENDENCY in each entry's
    crate_id (the crate queried); the dependent arrives via version_id,
    resolved against the versions list the same payload carries.
    """
    data = http_json(f"https://crates.io/api/v1/crates/{crate}/reverse_dependencies")
    by_version_id = {
        v.get("id"): (v.get("crate", ""), v.get("num", ""))
        for v in data.get("versions", [])
    }
    out = []
    for dep in data.get("dependencies", []):
        crate_name, _num = by_version_id.get(dep.get("version_id"), ("", ""))
        if crate_name:
            out.append((crate_name, dep.get("req", "")))
    return out


def bump_manifest_dep(manifest: pathlib.Path, crate: str, new: str) -> int:
    """Rewrite `crate = "={old}"`-style requirements to the new version."""
    s = manifest.read_text(encoding="utf-8")
    n = 0
    new_s, count = re.subn(
        r"(?m)^(" + crate + r' = ")=([^"]*)(")',
        r"\g<1>=" + new + r"\g<3>",
        s,
    )
    n += count
    new_s, count = re.subn(
        r"(?m)^(" + crate + r' = \{ version = ")=([^"]*)(")',
        r"\g<1>=" + new + r"\g<3>",
        new_s,
    )
    n += count
    if n:
        manifest.write_text(new_s, encoding="utf-8")
    return n


def bump_version(manifest: pathlib.Path) -> str | None:
    """Bump the manifest's own patch version; returns the new version."""
    s = manifest.read_text(encoding="utf-8")
    m = re.search(r'(?m)^version = "(\d+)\.(\d+)\.(\d+)"$', s)
    if not m:
        return None
    new = f"{m.group(1)}.{m.group(2)}.{int(m.group(3)) + 1}"
    s = s[: m.start()] + f'version = "{new}"' + s[m.end():]
    manifest.write_text(s, encoding="utf-8")
    return new


def git(repo: pathlib.Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True)


def sync_repo(root: pathlib.Path, repo: str) -> pathlib.Path | None:
    """Clone the repo under root if absent; pull if present. None on failure."""
    path = root / repo
    if path.is_dir():
        git(path, "fetch", "origin")
        git(path, "reset", "--hard", f"origin/{'main'}")
        return path
    proc = subprocess.run(
        ["git", "clone", "--depth", "1", f"https://github.com/WyattAu/{repo}", str(path)],
        capture_output=True, text=True,
    )
    return path if proc.returncode == 0 else None


def main(argv: list[str] | None = None) -> int:
    import argparse
    import yaml

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crate", required=True, help="the bumped producer crate")
    parser.add_argument("--to", required=True, help="the new published version")
    parser.add_argument("--estate", type=pathlib.Path, default=pathlib.Path("estate.yml"))
    parser.add_argument("--repos-root", type=pathlib.Path,
                        default=pathlib.Path("."), help="where repo checkouts live")
    parser.add_argument("--publish", action="store_true",
                        help="publish each walked crate (default: dry run)")
    parser.add_argument("--skip-tests", action="store_true")
    args = parser.parse_args(argv)

    estate = yaml.safe_load(args.estate.read_text(encoding="utf-8"))
    old = None
    try:
        data = http_json(f"https://crates.io/api/v1/crates/{args.crate}")
        versions = [v for v in data.get("versions", []) if not v.get("yanked")]
        if len(versions) >= 2:
            old = versions[1]["num"]
    except Exception:
        pass
    if old is None:
        print(f"cannot determine the previous version of {args.crate}; pass it via a future flag")
        return 1
    print(f"chain: {args.crate} {old} -> {args.to}")

    # 1. Which estate crates depend on it, and at what requirement?
    dependents: list[tuple[str, str]] = []
    for dep_crate, req in reverse_dependents(args.crate):
        repo = repo_for(dep_crate, estate)
        if repo and req_pins_version(req, old):
            dependents.append((dep_crate, req, repo))
    if not dependents:
        print("no estate-owned crate pins the old version; the chain is empty")
        return 0

    # 2. Walk in order: crates whose own dependents come later re-queue
    #    themselves via the recursion below.
    published: list[str] = []
    planned: list[str] = []
    failed: list[tuple[str, str]] = []
    queue = sorted(dependents, key=lambda t: repo_for(t[0], estate) or t[0])
    while queue:
        dep_crate, _req, repo = queue.pop(0)
        path = sync_repo(args.repos_root.resolve(), repo)
        if path is None:
            failed.append((dep_crate, f"repo {repo} unavailable"))
            continue
        manifest = path / "Cargo.toml"
        bump_manifest_dep(manifest, args.crate, args.to)
        new_version = bump_version(manifest)
        if new_version is None:
            failed.append((dep_crate, "no plain x.y.z version in the manifest"))
            continue
        git(path, "add", "-A")
        git(path, "commit", "-q", "-s", "-m",
            f"release: {new_version} on {args.crate} {args.to}\n\n"
            f"chain-walk: the exact pin held the composition graph back.")
        test = None
        if not args.skip_tests:
            test = subprocess.run(["cargo", "test"], cwd=path, capture_output=True, text=True)
        if test is not None and test.returncode != 0:
            failed.append((dep_crate, "tests failed on the new dependency"))
            continue
        if args.publish:
            pub = subprocess.run(
                ["cargo", "publish", "--allow-dirty"], cwd=path, capture_output=True, text=True
            )
            if pub.returncode != 0:
                failed.append((dep_crate, f"publish failed: {pub.stderr.strip()[-90:]}"))
                continue
            published.append(f"{dep_crate} {new_version}")
            print(f"  published: {dep_crate} {new_version}")
            # Recurse: the freshly published crate may have its own estate
            # dependents whose exact pins now trail it.
            latest = latest_version(dep_crate)
            for dep2, req2 in reverse_dependents(dep_crate):
                repo2 = repo_for(dep2, estate)
                if repo2 and latest and req_pins_version(req2, latest) and latest != new_version:
                    queue.append((dep2, req2, repo2))
        else:
            planned.append(f"{dep_crate} {new_version}")
            print(f"  planned: {dep_crate} {new_version} (dry run; local commit left for review)")

    print(f"\nchain: {len(published)} published, {len(planned)} planned, {len(failed)} failed")
    for crate, why in failed:
        print(f"  FAILED: {crate}: {why}")
    if not args.publish:
        print("dry run: local commits exist in the touched repos; push or reset them")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
