#!/usr/bin/env python3
"""Convert em-dashes in Markdown to the DOC-STYLE rule 1 house style.

DOC-STYLE rule 1: the em-dash does not appear in any document. The accepted
sweep (2026-10-22, 824 instances estate-wide) established the conversion:
a space-delimited em-dash becomes a colon, because a colon, comma, semicolon,
or parentheses always carries the same sense. This tool applies exactly that
rule and refuses the shapes where a blind replacement would corrupt meaning.

Refused (reported with file:line for hand handling):

  paired dashes     two or more on one line: the second is usually a
                    parenthetical close, and "A: B: C" is not a sentence.
  unspaced dash     word\u2014word has no space to anchor the replacement.
  leading dash      a line beginning with the dash is a wrapped continuation,
                    so the colon belongs mid-sentence, not at line start.
  placeholder cell  "| \u2014 |" means "not supported", not punctuation; the
                    fix is n/a or a word, which only a human can choose.

Dry run by default; --write applies the safe conversions. Exit 1 when manual
cases remain after a --write run, so a caller cannot mistake a partial sweep
for a finished one.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

EM = "\u2014"
SKIP_DIRS = {"target", "node_modules", ".git", "vendor", "dist", "build", "reports"}
# "reports" holds generated output: trendkit's weekly report records external
# data verbatim (a dash inside a crates.io one-line description is that
# project's punctuation, not this estate's prose), so rewriting it would
# falsify the artifact. The generator is the thing to fix, not its output.


# A dash used as the content of a table cell: "| — ..." or "| — |".
PLACEHOLDER_CELL = re.compile(r"\|\s*" + EM)
LEADING = re.compile(r"^\s*(?:[-*+]\s+|>\s+|\d+\.\s+)?" + EM)
UNSPACED = re.compile(r"\w" + EM + r"\w")
SPACED = " " + EM + " "


def classify(line: str) -> tuple[str, str]:
    """('auto', rewritten) or ('manual', reason)."""
    if EM not in line:
        return "auto", line
    if line.count(EM) >= 2:
        return "manual", "paired dashes"
    if UNSPACED.search(line):
        return "manual", "unspaced dash"
    if LEADING.match(line):
        return "manual", "leading dash (wrapped continuation)"
    if PLACEHOLDER_CELL.search(line):
        return "manual", "dash in an empty table cell"
    rewritten = line.replace(SPACED, ": ")
    if EM in rewritten:
        return "manual", "dash not space-delimited"
    return "auto", rewritten


def sweep_text(text: str) -> tuple[str, list[tuple[int, str, str]]]:
    """(new text, [(line number, reason, original line)])."""
    lines = text.splitlines()
    out: list[str] = []
    manual: list[tuple[int, str, str]] = []
    for number, line in enumerate(lines, start=1):
        verdict, value = classify(line)
        if verdict == "manual":
            manual.append((number, value, line))
            out.append(line)
        else:
            out.append(value)
    new = "\n".join(out)
    if text.endswith("\n"):
        new += "\n"
    return new, manual


def markdown_files(roots: list[pathlib.Path]) -> list[pathlib.Path]:
    seen: set[pathlib.Path] = set()
    for root in roots:
        if root.is_file() and root.suffix == ".md":
            seen.add(root)
        elif root.is_dir():
            seen.update(
                p for p in root.rglob("*.md")
                if p.is_file() and not any(part in SKIP_DIRS for part in p.parts)
            )
    return sorted(seen)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="+", type=pathlib.Path,
                        help="files or directories to sweep")
    parser.add_argument("--write", action="store_true",
                        help="apply the safe conversions (default: dry run)")
    args = parser.parse_args(argv)

    changed_files = 0
    converted = 0
    manual_total: list[tuple[pathlib.Path, int, str, str]] = []
    for path in markdown_files(args.paths):
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError) as error:
            print(f"{path}: unreadable ({error})")
            continue
        if EM not in text:
            continue
        new, manual = sweep_text(text)
        delta = text.count(EM) - new.count(EM)
        if delta:
            changed_files += 1
            converted += delta
            if args.write:
                path.write_text(new, encoding="utf-8")
        manual_total.extend((path, n, reason, line) for n, reason, line in manual)

    verb = "converted" if args.write else "would convert"
    print(f"{verb} {converted} em-dash(es) across {changed_files} file(s)")
    if manual_total:
        print(f"\n{len(manual_total)} line(s) need a human:")
        for path, number, reason, line in manual_total:
            print(f"  {path}:{number}: [{reason}] {line.strip()[:110]}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
