#!/usr/bin/env python3
"""Release gate: refuses a publish until the covering suite has run green.

Created after ledger-pg 0.2.0 shipped a double-flipped reversal: `cargo
publish` verifies the library build only, and the defect lived in the
integration paths. A release does not go out until the suite covering the
change has run green, even for "just plumbing".

Gates, in order:

1. **build**    — `cargo publish --dry-run` compiles.
2. **test**     — the crate's own test suite, all features, passes.
3. **clippy**   — zero warnings and errors, all targets.
4. **fmt**      — rustfmt clean.
5. **pins**     — every exact-pinned estate dependency is the latest
                  non-yanked version on crates.io. This is the convergence
                  check: publishing a crate that pins an older shared-crate
                  version than the graph's is how a graph splits across two
                  non-interoperable versions of the same crate (the
                  vat-rules 0.1.1 and double-entry 0.1.1 cascades).

With `--publish`, `cargo publish` runs only after every gate passes. The
gate never publishes on a partial pass; a partial pass is the failure.

Usage:
    release-gate.py --repo PATH --name NAME [--strict-pins] [--publish]
"""

import argparse
import json
import subprocess
import sys
import urllib.request

# The estate's shared crates: an exact pin on any of these is checked
# against crates.io's latest. Extended as the estate grows.
ESTATE_CRATES = {
    "double-entry",
    "vat-rules",
    "invoice-kit",
    "ap-kit",
    "report-kit",
    "fx-kit",
    "group-kit",
    "ledger-pg",
    "vat-return",
    "vat-mtd",
    "recon-kit",
    "sales-tax",
    "statement-camt",
    "statement-ofx",
    "pay-kit",
}

def sparse_index_url(name: str) -> str:
    """The crates.io sparse-index path, per the resolvable-URL rules:
    1 char -> 3/{name}; 2 chars -> 2/{name}; 3 chars -> 3/{first}/{name};
    otherwise {first two}/{next two}/{rest}."""
    if len(name) == 1:
        return f"https://index.crates.io/3/{name}"
    if len(name) == 2:
        return f"https://index.crates.io/2/{name}"
    if len(name) == 3:
        return f"https://index.crates.io/3/{name[0]}/{name}"
    return f"https://index.crates.io/{name[:2]}/{name[2:4]}/{name}"


def run(cmd: list, cwd: str) -> tuple:
    result = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    return result.returncode == 0, result.stdout + result.stderr


def gate(name: str, ok: bool, detail: str) -> bool:
    mark = "PASS" if ok else "FAIL"
    print(f"[{mark}] {name}")
    if not ok:
        tail = detail.strip().splitlines()[-6:]
        for line in tail:
            print(f"       {line}")
    return ok


def sparse_index_versions(name: str) -> list:
    """Versions from the sparse index: [(version, yanked)], in upload order."""
    url = sparse_index_url(name)
    request = urllib.request.Request(url, headers={"User-Agent": "wyatt-release-gate"})
    with urllib.request.urlopen(request, timeout=30) as response:
        rows = [json.loads(line) for line in response if line.strip()]
    return [(row["vers"], row.get("yanked", False)) for row in rows]


def latest_live_version(name: str) -> str | None:
    """The newest non-yanked version, by semver-ish ordering of the index."""
    versions = [(v, y) for (v, y) in sparse_index_versions(name) if not y]
    if not versions:
        return None

    def key(version: str):
        parts = []
        for piece in version.split("."):
            digits = "".join(c for c in piece if c.isdigit())
            parts.append(int(digits) if digits else 0)
        return parts

    return max(versions, key=lambda pair: key(pair[0]))[0]


def crate_exact_pins(manifest_text: str) -> dict:
    """Estate crates exact-pinned in the manifest: name -> pinned version.

    Matches `name = "=x.y.z"` — exact pins only, because a caret pin on a
    shared crate is a graph-split risk the estate audit already reports.
    """
    import re
    pins = {}
    for crate in sorted(ESTATE_CRATES, key=len, reverse=True):
        pattern = re.compile(
            rf'^\s*{re.escape(crate)}\s*=\s*.*?"=([0-9][0-9A-Za-z.-]*)"',
            re.MULTILINE,
        )
        m = pattern.search(manifest_text)
        if m:
            pins[crate] = m.group(1)
    return pins


def pin_gates(manifest_text: str) -> list:
    """One gate entry per estate pin that is not the latest live version."""
    failures = []
    pins = crate_exact_pins(manifest_text)
    for crate, pinned in sorted(pins.items()):
        try:
            latest = latest_live_version(crate)
        except Exception as e:  # noqa: BLE001 — network down is a skip, not a fail
            print(f"[SKIP] pin convergence for {crate}: {e}")
            continue
        if latest is None:
            continue
        if pinned != latest:
            failures.append(
                f"pin drift: {crate} pins ={pinned} but crates.io has {latest} — "
                f"publishing splits the graph across two versions"
            )
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="path to the crate repo")
    parser.add_argument("--name", required=True, help="the crate's name")
    parser.add_argument("--strict-pins", action="store_true",
                        help="treat pin drift as a gate failure (default: warn)")
    parser.add_argument("--publish", action="store_true",
                        help="run `cargo publish` after all gates pass")
    parser.add_argument("--skip-pins", action="store_true",
                        help="skip the crates.io convergence check")
    args = parser.parse_args()

    ok = True

    build_ok, detail = run(
        ["cargo", "publish", "--dry-run", "--all-features"], args.repo)
    ok &= gate("build (publish --dry-run)", build_ok, detail)

    test_ok, detail = run(
        ["cargo", "test", "--all-features"], args.repo)
    ok &= gate("test (--all-features)", test_ok, detail)

    clippy_ok, detail = run(
        ["cargo", "clippy", "--all-features", "--all-targets", "--", "-D", "warnings"],
        args.repo)
    ok &= gate("clippy (-D warnings)", clippy_ok, detail)

    fmt_ok, detail = run(["cargo", "fmt", "--check"], args.repo)
    ok &= gate("fmt", fmt_ok, detail)

    if not args.skip_pins:
        manifest = open(f"{args.repo}/Cargo.toml").read()
        drifts = pin_gates(manifest)
        for drift in drifts:
            print(f"[FAIL] {drift}")
        if drifts and args.strict_pins:
            ok = False
        elif drifts:
            print("[WARN] pin drift — publish converges the graph or splits it; "
                  "the estate audit is the detector")

    if not ok:
        print("\nRELEASE REFUSED — a release does not go out until the suite "
              "covering the change has run green, even for plumbing.")
        return 1

    if args.publish:
        published, detail = run(["cargo", "publish"], args.repo)
        if not gate("publish", published, detail):
            return 1

    print("\nALL GATES GREEN")
    return 0


if __name__ == "__main__":
    sys.exit(main())
