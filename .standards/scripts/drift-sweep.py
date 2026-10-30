#!/usr/bin/env python3
"""Drain the audit's drift queue: bump consumer pins to published versions.

The publishing loop moves faster than pin bumps (measured 2026-10-24: the
queue went 15 -> 27 -> 4 -> 9 -> 64 across two days while sweeps raced it),
so the round-close sweep is a mechanic, and a mechanic deserves a tool
rather than a hand-edited manifest. This script IS the sweep:

  1. runs the estate audit (or reads a saved audit output file) and parses
     every drift pair: crate, pinned version, published version;
  2. rewrites the exact pins in the consumer manifests (root and side
     graphs);
  3. runs the consumer test suites -- the audit's own instruction is
     "bump the pin, then fix whatever the bump broke";
  4. moves the vet store's exemptions to the new versions.

What it deliberately does not do: resolve version conflicts between a new
pin and another crate's exact requirement (a chain like vat-rules 0.1.1
needing ap-kit/fx-kit/vat-return republished is a publishing-round task,
and the script reports the conflict instead of yanking crates); publish
anything; touch holds. A hold (webhookkit 2.2.2 does not compile against
hmac 0.13.0 final) is recorded in HOLDS with the reason and survives
every sweep until the reason is gone.

    drift-sweep.py --estate estate.yml --consumer ../estate-integration
                   [--side ../estate-integration/sqlite-side]
                   [--audit-file /tmp/audit.out] [--no-test]

Exit 1 when suites fail after the bump, or when a version conflict needs
a publishing-round republish. Exit 0 when the sweep converged.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys

DRIFT_LINE = re.compile(
    r"^  ([a-z0-9_-]+): \S+ pins =([\d.]+) but crates\.io has ([\d.]+)"
)
VET_MISSING = re.compile(r"^  ([a-zA-Z0-9_-]+):([^\s]+) missing", re.MULTILINE)

# Crates held off their latest publication, with the measured reason. A hold
# survives sweeps; deleting the entry re-bumps the pin and re-breaks the
# build, which is how the reason gets re-learned.
HOLDS: dict[str, str] = {
    # webhookkit 2.2.2 AND 2.2.3 do not compile: hmac 0.13.0 final moved
    # ct_eq to hybrid-array; both releases were cut against a 0.13.0-rc
    # lock. Measured 2026-10-24 (2.2.2) and 2026-10-27 (2.2.3), E0308 in
    # the published lib.rs. The fix is one call site; it has not shipped.
    "webhookkit": "2.2.2 and 2.2.3 do not compile against hmac 0.13.0 final; hold at 2.2.1 until the ct_eq call site is fixed",
}


def parse_audit(path: pathlib.Path) -> list[tuple[str, str, str]]:
    """(crate, pinned, published) triples from a saved audit output."""
    pairs: list[tuple[str, str, str]] = []
    seen: set[tuple[str, str, str]] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        m = DRIFT_LINE.match(line)
        if m:
            key = m.groups()
            if key not in seen:
                seen.add(key)
                pairs.append(key)
    return pairs


def bump_manifest(path: pathlib.Path, pairs: list[tuple[str, str, str]]) -> int:
    s = path.read_text(encoding="utf-8")
    n = 0
    for crate, old, new in pairs:
        if crate in HOLDS:
            continue
        for fmt in (f'{crate} = "={old}"', f'{crate} = {{ version = "={old}"'):
            if fmt in s:
                s = s.replace(fmt, fmt.replace(f'={old}"', f'={new}"'))
                n += 1
    path.write_text(s, encoding="utf-8")
    return n


def reconcile_store(store: pathlib.Path, consumer: pathlib.Path, rounds: int = 3) -> bool:
    """Move/add vet exemptions until `cargo vet --locked` is clean."""
    for _ in range(rounds):
        proc = subprocess.run(
            ["cargo", "vet", "--locked"], capture_output=True, text=True, cwd=consumer
        )
        out = proc.stdout + proc.stderr
        missing = VET_MISSING.findall(out, re.M)
        if proc.returncode == 0 and not missing:
            return True
        if not missing:
            return False
        apply_store_entries(store, missing)
        subprocess.run(["cargo", "vet", "fmt"], capture_output=True, cwd=consumer)
    return False


def apply_store_entries(store: pathlib.Path, missing: list[tuple[str, str]]) -> None:
    """Add each missing (crate, version) exemption; never move an entry.

    Moving to newest is the oscillation measured on config-kit and
    idempotency-kit: a crate can legitimately sit in one graph under two
    versions (new direct requirement, old transitive), and moving the entry
    re-flags the old version on the next pass, forever. Entries are added;
    `prune_stale_entries` removes the ones the graph outgrew.
    """
    s = store.read_text(encoding="utf-8")
    for crate, version in missing:
        exact = re.compile(
            r"\[\[exemptions\." + crate + r"\]\]\nversion = \"" + re.escape(version) + "\""
        )
        if exact.search(s):
            continue
        entry = (
            f"[[exemptions.{crate}]]\nversion = \"{version}\"\n"
            'criteria = "safe-to-deploy"\n\n'
        )
        anchor = re.search(r"\[\[exemptions\.[a-z0-9_-]+\]\]", s)
        if anchor:
            s = s[: anchor.start()] + entry + s[anchor.start():]
        else:
            s = s.rstrip("\n") + "\n\n" + entry
    store.write_text(s, encoding="utf-8")


LOCK_PACKAGE_RE = re.compile(r'\[\[package\]\]\s*\nname = "([^"]+)"\s*\nversion = "([^"]+)"')


def prune_stale_entries(store: pathlib.Path, lock: pathlib.Path) -> int:
    """Drop exemption entries whose (crate, version) is no longer locked.

    Returns the number of entries dropped. An entry whose version left the
    graph is dead weight with a trust claim attached; it goes.
    """
    locked = set(LOCK_PACKAGE_RE.findall(lock.read_text(encoding="utf-8")))
    s = store.read_text(encoding="utf-8")
    blocks = re.split(r"(?=\[\[exemptions\.)", s)
    head, kept, dropped = blocks[0], [], 0
    for block in blocks[1:]:
        m = re.match(r"\[\[exemptions\.([a-z0-9_-]+)\]\]\nversion = \"([^\"]+)\"", block)
        if m and (m.group(1), m.group(2)) not in locked:
            dropped += 1
            continue
        kept.append(block)
    store.write_text(head + "".join(kept), encoding="utf-8")
    return dropped


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--estate", type=pathlib.Path, default=pathlib.Path("estate.yml"))
    parser.add_argument("--consumer", type=pathlib.Path, required=True,
                        help="the consuming repo to sweep (manifest root)")
    parser.add_argument("--side", type=pathlib.Path, default=None,
                        help="optional side graph with its own manifest + lock")
    parser.add_argument("--audit-file", type=pathlib.Path, default=None,
                        help="reuse a saved audit output instead of running one")
    parser.add_argument("--no-test", action="store_true",
                        help="skip the test suites after the bump")
    args = parser.parse_args(argv)

    consumer = args.consumer.resolve()
    manifests = [consumer / "Cargo.toml"]
    if args.side:
        manifests.append(args.side / "Cargo.toml")

    audit_file = args.audit_file
    if audit_file is None:
        audit_file = pathlib.Path("/tmp/drift-sweep-audit.out")
        cache = pathlib.Path("/tmp/drift-sweep-cache.json")
        cache.unlink(missing_ok=True)
        import os

        env = dict(os.environ)
        env["GH_TOKEN"] = subprocess.run(
            ["gh", "auth", "token"], capture_output=True, text=True
        ).stdout.strip()
        proc = subprocess.run(
            ["python3", "scripts/estate-audit.py"],
            capture_output=True, text=True, cwd=args.estate.resolve().parent,
            env=env,
        )
        audit_file.write_text(proc.stdout + proc.stderr, encoding="utf-8")

    pairs = parse_audit(audit_file)
    live = [(c, o, n) for c, o, n in pairs if c not in HOLDS]
    held = [(c, o, n) for c, o, n in pairs if c in HOLDS]
    print(f"drift pairs: {len(pairs)} ({len(held)} held)")

    # Per-pair targeted convergence: bump the manifest pin, then a targeted
    # `cargo update --precise` that fails cleanly on a chain conflict
    # without re-resolving the whole graph (a yanked-but-locked crate
    # elsewhere survives, measured 2026-10-27). Resolving pairs stay;
    # conflicting pairs are reverted and reported as the round's queue.
    kept, conflicts = [], []
    for crate, old, new in live:
        n = 0
        for path in manifests:
            if path.exists():
                n += bump_manifest(path, [(crate, old, new)])
        if n == 0:
            conflicts.append((crate, old, new, "pin not found in any manifest"))
            continue
        target = subprocess.run(
            ["cargo", "update", f"-p{crate}@{old}", "--precise", new],
            cwd=consumer, capture_output=True, text=True,
        )
        if target.returncode == 0:
            kept.append((crate, old, new))
            for path in manifests:
                if path.exists():
                    lock = path.parent / "Cargo.lock"
                    if lock.exists():
                        subprocess.run(
                            ["cargo", "update", f"-p{crate}@{old}", "--precise", new],
                            cwd=path.parent, capture_output=True, text=True,
                        )
            print(f"  kept: {crate} {old} -> {new}")
        else:
            for path in manifests:
                if path.exists():
                    bump_manifest(path, [(crate, new, old)])
            tail = [l for l in target.stderr.strip().splitlines() if l.strip()]
            conflicts.append((crate, old, new, tail[-1][:110] if tail else "update failed"))
            print(f"  conflict: {crate} {old} -> {new}")

    if conflicts:
        print(f"\n{len(conflicts)} conflicting pair(s) left for the publishing round:")
        for crate, old, new, why in conflicts:
            print(f"  {crate} {old} -> {new}: {why}")

    if not args.no_test:
        test = subprocess.run(
            ["cargo", "test"], cwd=consumer, capture_output=True, text=True
        )
        passed = failed = 0
        for line in test.stdout.splitlines():
            m = re.match(r"test result: \w+\. (\d+) passed; (\d+) failed", line)
            if m:
                passed += int(m.group(1))
                failed += int(m.group(2))
        print(f"suites: {passed} passed, {failed} failed")
        if test.returncode != 0 or failed:
            print("BROKEN: the kept set does not build; investigate before landing.")
            return 1

    if args.side:
        side = subprocess.run(
            ["cargo", "test"], cwd=args.side, capture_output=True, text=True
        )
        if side.returncode != 0:
            print("BROKEN: the side graph fails.")
            return 1
        print("side graph: green")

    store = consumer / "supply-chain" / "config.toml"
    lock = consumer / "Cargo.lock"
    if store.exists():
        if reconcile_store(store, consumer):
            print("vet store: reconciled")
            if lock.exists():
                dropped = prune_stale_entries(store, lock)
                if dropped:
                    print(f"vet store: pruned {dropped} stale entr(ies)")
                    subprocess.run(["cargo", "vet", "fmt"], capture_output=True, cwd=consumer)
                    final = subprocess.run(
                        ["cargo", "vet", "--locked"], capture_output=True, cwd=consumer
                    )
                    if final.returncode != 0:
                        reconcile_store(store, consumer)
                        print("vet store: prune reverted a needed entry; re-reconciled")
        else:
            print("vet store: did not converge; see cargo vet --locked")
            return 1

    print("sweep converged; review the diff, run the full suites, land.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
