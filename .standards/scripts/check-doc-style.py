#!/usr/bin/env python3
"""Check every Markdown document in a tree for the em-dash character.

DOC-STYLE.md rule 1: the em-dash does not appear in any document; the count
must be zero, and the check is the whole-tree grep this script wraps. The
character renders inconsistently across terminals and fonts, and a colon,
comma, semicolon, or parentheses always carries the same sense.

Usage:
    check-doc-style.py [paths ...]          # check these files/directories
    check-doc-style.py --root .             # check a whole tree

Exits 0 when no document carries the character, 1 otherwise (one finding
per offending line, with the line number so the fix is navigable).

Fenced code blocks are not skipped: an em-dash inside sample prose is still
an em-dash in a document, and DOC-STYLE's own check greps the whole file.

Advisory until the backlog graduates: the sweep commit that landed the rule
fixed 824 pre-existing instances across the estate (2026-10-22); three files
in engineering-standards still carry 41 under an in-flight rebrand and are
tracked in the estate ledger rather than exempted here.
"""

from __future__ import annotations

import argparse
import pathlib
import sys

EM_DASH = "\u2014"


def check_file(path: pathlib.Path) -> list[str]:
    """One finding per line carrying the em-dash."""
    findings: list[str] = []
    try:
        text = path.read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError) as error:
        return [f"{path}: unreadable ({error})"]
    for number, line in enumerate(text.splitlines(), start=1):
        if EM_DASH in line:
            findings.append(f"{path}:{number}: em-dash in: {line.strip()[:100]}")
    return findings


SKIP_DIRS = {"target", "node_modules", ".git", "vendor", "dist", "build", "reports"}
# "reports" holds generated output: trendkit's weekly report records external
# data verbatim (a dash inside a crates.io one-line description is that
# project's punctuation, not this estate's prose), so rewriting it would
# falsify the artifact. The generator is the thing to fix, not its output.



def files_from(roots: list[pathlib.Path]) -> list[pathlib.Path]:
    """Every .md file under the roots, deduplicated and sorted.

    Build directories (target, node_modules, dist, ...) are skipped: cargo
    package snapshots the README into target/package at the version it was
    published at, and the published snapshot is not a source document.
    """
    seen: set[pathlib.Path] = set()
    for root in roots:
        if root.is_file() and root.suffix == ".md":
            seen.add(root)
        elif root.is_dir():
            for p in root.rglob("*.md"):
                if p.is_file() and not any(part in SKIP_DIRS for part in p.parts):
                    seen.add(p)
    return sorted(seen)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path,
                        help="files or directories to check (default: current tree)")
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path("."),
                        help="tree root when no paths are given")
    args = parser.parse_args(argv)

    roots = args.paths or [args.root]
    files = files_from(roots)
    findings: list[str] = []
    for path in files:
        findings.extend(check_file(path))

    if findings:
        print(f"DOC-STYLE rule 1 FAILED: {len(findings)} em-dash line(s):\n")
        for finding in findings:
            print(f"  {finding}")
        print(
            "\nFix: a colon, comma, semicolon, or parentheses carries the same"
            "\nsense; see DOC-STYLE.md rule 1. Table cells and list lead-ins"
            "\nnormally take a colon."
        )
        return 1

    print(f"DOC-STYLE rule 1: clean ({len(files)} document(s) checked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())