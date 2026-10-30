#!/usr/bin/env python3
"""Fail if a template's llms.txt carries unresolved generation placeholders.

Loop 25: all ten templates shipped llms.txt with literal `$1` where the repo
name belonged -- three broken URLs each, published for every loop since. No
gate looked at the file, so nothing caught it. This check is the estate-wide
recurrence guard: the estate audit runs it nightly over every template in
estate.yml, and `make llms-links` runs it locally over the local clones.

A placeholder is any occurrence of a shell-style variable in a URL
(`WyattAu/$1/blob/...`). The check is deliberately textual and offline: it
needs no network and cannot be satisfied by a coincidental-looking value.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import tempfile

# `$var`, `${var}`, `$1`, `$@`, `%var%` -- the shapes a generator can leave
# behind. The bare-positional form (`$1`) matters most: it is what loop 25
# shipped, and a pattern requiring a leading letter silently misses it.
PLACEHOLDER = re.compile(
    r"\$\{[^{}]*\}"
    r"|\$[A-Za-z_][A-Za-z0-9_]*"
    r"|\$[0-9@*#?!$]"
    r"|%[A-Za-z_][A-Za-z0-9_]*%"
)
# Only flag placeholders that sit inside a URL; prose mentioning $HOME is fine.
URL_PLACEHOLDER = re.compile(r"https?://[^\s)>\"']*?(?:" + PLACEHOLDER.pattern + r")")


def check_file(path: pathlib.Path) -> list[str]:
    """Return one message per placeholder found in a llms.txt file."""
    problems: list[str] = []
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        for match in URL_PLACEHOLDER.finditer(line):
            problems.append(f"{path}:{lineno}: unresolved placeholder {match.group(0)!r} in URL")
    return problems


def check_remote(owner: str, repo: str, ref: str = "main") -> tuple[list[str], bool]:
    """Check one repo's llms.txt. Returns (problems, skipped).

    A 404 means the repo carries no LLM contract -- most of the estate is
    library repos, and demanding an llms.txt of them would be noise. Skipping
    is therefore normal and counted. Every other fetch failure IS a problem:
    a gate that cannot distinguish "absent" from "unreachable" is not a gate.
    """
    import urllib.error
    import urllib.request

    url = f"https://raw.githubusercontent.com/{owner}/{repo}/{ref}/llms.txt"
    # Connection-level failures are retried before being reported. The CDN
    # edge alternates between clean answers and resets under load (measured
    # 2026-10-22: the same URL returned HTTPError 404 and then URLError
    # "connection reset" on consecutive probes), and a single-attempt fetch
    # turns that flakiness into a spurious gate failure. HTTPErrors are
    # decisive server answers and are never retried.
    body = None
    last_error: Exception | None = None
    for attempt in range(3):
        try:
            request = urllib.request.Request(url)
            with urllib.request.urlopen(request, timeout=30) as response:  # noqa: S310
                body = response.read().decode("utf-8")
            break
        except urllib.error.HTTPError as error:
            last_error = error
            break
        except urllib.error.URLError as error:
            last_error = error.reason
    if body is None:
        if isinstance(last_error, urllib.error.HTTPError):
            if last_error.code == 404:
                return [], True
            return [f"{owner}/{repo}: llms.txt not fetchable ({last_error.code}) at {url}"], False
        return [f"{owner}/{repo}: llms.txt fetch failed ({last_error})"], False

    scratch = pathlib.Path(tempfile.gettempdir()) / f"llms-{repo}.txt"
    scratch.write_text(body, encoding="utf-8")
    try:
        return [p.replace(str(scratch), f"{owner}/{repo}") for p in check_file(scratch)], False
    finally:
        scratch.unlink(missing_ok=True)


def repos_from_manifest(manifest: pathlib.Path) -> list[tuple[str, str]]:
    """(owner, repo) pairs from estate.yml.

    Parsed with the same loader estate-audit.py uses, so the gate and the
    audit can never disagree about which repos exist. Repos whose language
    family has no llms.txt contract are not special-cased here: fetch failures
    are reported, and a template that drops the file should be noticed.
    """
    import yaml  # imported lazily so the file-path mode needs no dependency

    data = yaml.safe_load(manifest.read_text(encoding="utf-8")) or {}
    owner = data.get("owner") or "WyattAu"
    repos = data.get("repos") or {}
    if not isinstance(repos, dict):
        raise ValueError("estate.yml: `repos` must be a mapping of name -> entry")
    return [(owner, str(name)) for name in sorted(repos)]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path, help="llms.txt files to check")
    parser.add_argument(
        "--estate",
        type=pathlib.Path,
        help="check every repo in this estate.yml manifest over the network instead",
    )
    parser.add_argument("--ref", default="main", help="git ref to check for --estate")
    args = parser.parse_args(argv)

    problems: list[str] = []
    if args.estate:
        repos = repos_from_manifest(args.estate)
        if not repos:
            print(f"{args.estate}: no repos parsed from the manifest", file=sys.stderr)
            return 1
        checked = skipped = 0
        for owner, repo in repos:
            repo_problems, was_skipped = check_remote(owner, repo, args.ref)
            problems.extend(repo_problems)
            skipped += int(was_skipped)
            checked += int(not was_skipped)
        print(f"llms.txt scan: {checked} repo(s) with a contract, {skipped} without")
    else:
        if not args.paths:
            parser.error("pass llms.txt paths or --estate MANIFEST")
        for path in args.paths:
            if not path.is_file():
                problems.append(f"{path}: missing")
                continue
            problems.extend(check_file(path))

    if problems:
        print("llms.txt placeholder check FAILED:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        print(
            "\nFix: substitute the real repository name. A placeholder here ships a\n"
            "broken URL to every LLM that reads the file -- loop 25 found three per\n"
            "template, live for thirteen loops.",
            file=sys.stderr,
        )
        return 1

    if not args.estate:
        print(f"llms.txt placeholder check: clean ({len(args.paths)} file(s))")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
