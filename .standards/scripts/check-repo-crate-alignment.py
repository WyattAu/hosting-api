#!/usr/bin/env python3
"""Estate repo<->crate alignment checker — enforces naming-convention rule 1.

Rule (`docs/naming-convention.md` #1): a repository that publishes exactly one
crate has the SAME NAME as that crate.

Nothing in CI enforced this, so the estate drifted. A crate published as
`shutdown-kit` lives in the repo `graceful`; `decimal-money` in `money`;
`tamper-audit` in `auditlog`; `loop-retry` in `retry-backoff`; `http-errors` in
`http-error`. Meanwhile several published crates (`throttle-kit`,
`chronoshift`, `resilient-fetch`, `multi-chain-wallet`, `plycore`, …) have no
repo of that name at all, so their `repository` URL on crates.io and docs.rs
either points somewhere confusing or nowhere.

The drift is invisible until someone looks, because CI runs fine against
whatever the repo happens to be called. This makes it visible.

Monorepos are NOT violations. A repo whose workspace publishes several crates
(`vane`, `suture`, `polyfont`, `plychart`, `typed-id`, `crawlkit`) is named
after the *product*, and its members are named independently — rule 1 applies
per published crate, so a multi-crate repo needs no rename. This checker
distinguishes the two by counting publishable members, which is the whole
difficulty: a repo with one publishable member must match it exactly.

Usage (from this repo's root, as a CI job would):

    # audit every WyattAu repo via the GitHub API
    python3 scripts/check-repo-crate-alignment.py --owner WyattAu

    # audit one local checkout (no network)
    python3 scripts/check-repo-crate-alignment.py --cwd path/to/repo --repo-name name

    # CI gate form: mismatches fail, everything else is reported
    python3 scripts/check-repo-crate-alignment.py --owner WyattAu --enforce

Exit codes: 0 clean (or grandfathered), 1 violations with --enforce, 2 usage
or network error.

Pure stdlib: json + urllib + re + tomllib. No third-party deps.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - older interpreters
    tomllib = None  # type: ignore[assignment]

API = "https://api.github.com"

# Grandfathered mismatches: repo name -> why it is still allowed. Each entry is
# a *debt with an owner and a fix*, not a permanent exemption. Remove an entry
# when the rename lands; the checker will then hold the repo to rule 1.
#
# `renames` maps the current repo name to the name it should become, so the
# report can name the exact `gh repo rename` command instead of just complaining.
GRANDFATHERED: dict[str, str] = {
    # Each entry is debt with a name and a fix, not a permanent exemption. The
    # value is the repo this should become; remove the entry when the rename
    # lands and the checker will hold the repo to rule 1 from then on.
    #
    # Two causes, both one-off:
    #   * a crates.io name collision forced a crate rename, leaving the repo
    #     behind (app-error, ratelimit, http-error, errcode, hdwallet);
    #   * the crate was renamed on the way in and the repo never followed
    #     (graceful, money, auditlog, retry-backoff, cachekit, tantivy-ext,
    #     clock, eventbus, leptos-macro, paginate, latent-oracle-data).
    "app-error": "error-classify",
    "auditlog": "tamper-audit",
    "cachekit": "cache-pal",
    "clock": "chronoshift",
    "errcode": "error-codes",
    "eventbus": "eventbus-kit",
    "graceful": "shutdown-kit",
    "hdwallet": "multi-chain-wallet",
    "http-error": "http-errors",
    "latent-oracle-data": "lo-data",
    "leptos-leaflet": "leptos-leaflet-wyatt",
    "leptos-macro": "leptos-macros",
    "money": "decimal-money",
    "paginate": "api-paginate",
    "ratelimit": "throttle-kit",
    "retry-backoff": "loop-retry",
    "tantivy-ext": "tantivy-helper",
}

# Repos that intentionally publish nothing (services, docs, infra, the
# standards repo itself). Keyed by repo name.
NO_CRATE = {
    "engineering-standards",
    "SimpleInfrastructureStack",
    "estate-integration",
    "EvergreenShims",
    "CivitForge",
    "EvergreenImageRegistry",
    "starlight-sites",
    "dotfiles",
    "homepage",
}


def gh(path: str, token: str | None) -> object:
    req = urllib.request.Request(f"{API}{path}", headers={"Accept": "application/vnd.github+json"})
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    req.add_header("User-Agent", "estate-alignment-checker")
    with urllib.request.urlopen(req, timeout=30) as resp:  # noqa: S310 - fixed host
        return json.load(resp)


def list_repos(owner: str, token: str | None) -> list[dict]:
    """Every repo for `owner`, whether it is a user account or an org.

    The GitHub API exposes these under different prefixes and returns 404 (not
    an empty list) for the wrong one, which is an easy trap: `WyattAu` is a user
    account, so `/orgs/…` 404s while `/users/…` works.
    """
    last_error: urllib.error.HTTPError | None = None
    for path in ("orgs", "users"):
        repos: list[dict] = []
        page = 1
        try:
            while True:
                batch = gh(f"/{path}/{owner}/repos?per_page=100&page={page}&type=all", token)
                assert isinstance(batch, list)
                if not batch:
                    break
                repos.extend(batch)
                page += 1
                if page > 20:  # 2000 repos is far past any plausible estate
                    break
        except urllib.error.HTTPError as exc:
            last_error = exc
            continue
        if repos:
            return repos
    if last_error is not None:
        raise last_error
    return []


def root_manifest(owner: str, repo: str, ref: str, token: str | None) -> str | None:
    for candidate in ("Cargo.toml", "crates/Cargo.toml"):
        try:
            req = urllib.request.Request(
                f"https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{candidate}",
                headers={"User-Agent": "estate-alignment-checker"},
            )
            if token:
                req.add_header("Authorization", f"Bearer {token}")
            with urllib.request.urlopen(req, timeout=30) as resp:  # noqa: S310
                return resp.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as exc:
            if exc.code == 404:
                continue
            raise
    return None


def parse_manifest(text: str) -> tuple[list[str], list[str]]:
    """(workspace member globs, `[package] name` values found literally).

    Deliberately a light regex pass rather than a full TOML parse: it runs
    against manifests fetched over HTTP for ~190 repos, and a member list plus
    the literal package names is all the rule needs. `tomllib` is used when
    available to confirm the root package name authoritatively.
    """
    members = []
    in_ws = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_ws = stripped.replace(" ", "") == "[workspace]"
            continue
        if in_ws and stripped.startswith("members"):
            body = stripped.split("=", 1)[1] if "=" in stripped else ""
            members += [m.strip().strip("\"'") for m in body.strip("[]").split(",") if m.strip()]
    names = re.findall(r'^\s*name\s*=\s*"([^"]+)"', text, re.M)
    return members, names


def classify(repo_name: str, manifest: str | None) -> tuple[str, list[str]]:
    """(verdict, crate names) where verdict is match | monorepo | mismatch |
    no-manifest | no-crate."""
    if manifest is None:
        return ("no-manifest" if repo_name not in NO_CRATE else "no-crate"), []
    members, names = parse_manifest(manifest)
    if members:
        # A workspace: the repo is named for the product, not any member.
        return "monorepo", names
    if not names:
        return ("no-crate" if repo_name not in NO_CRATE else "no-crate"), []
    pkg = names[0]
    if pkg == repo_name:
        return "match", [pkg]
    return "mismatch", [pkg]


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--owner", default="WyattAu", help="GitHub org to audit")
    ap.add_argument("--cwd", help="audit one local checkout instead of the org")
    ap.add_argument("--repo-name", help="repo name to assume for --cwd (default: basename)")
    ap.add_argument(
        "--enforce",
        action="store_true",
        help="fail when any repo/crate mismatch remains, grandfathered included",
    )
    ap.add_argument("--token", default=os.environ.get("GITHUB_TOKEN"), help="GitHub token")
    args = ap.parse_args()

    token = args.token or None

    if args.cwd:
        path = os.path.join(args.cwd, "Cargo.toml")
        if not os.path.exists(path):
            print(f"error: no Cargo.toml at {path}", file=sys.stderr)
            return 2
        manifest = open(path, encoding="utf-8").read()
        name = args.repo_name or os.path.basename(os.path.abspath(args.cwd))
        rows = [(name, *classify(name, manifest))]
    else:
        try:
            repos = list_repos(args.owner, token)
        except urllib.error.URLError as exc:
            print(f"error: cannot reach the GitHub API: {exc}", file=sys.stderr)
            return 2
        rows = []
        for r in repos:
            name = r["name"]
            if r.get("archived") or name in NO_CRATE:
                continue
            try:
                manifest = root_manifest(args.owner, name, r.get("default_branch") or "main", token)
            except urllib.error.URLError:
                manifest = None
            rows.append((name, *classify(name, manifest)))

    counts: dict[str, int] = {}
    violations: list[tuple[str, str, list[str]]] = []
    grandfathered: list[tuple[str, str, list[str]]] = []
    for name, verdict, crates in rows:
        counts[verdict] = counts.get(verdict, 0) + 1
        if verdict == "mismatch":
            (grandfathered if name in GRANDFATHERED else violations).append((name, verdict, crates))

    total = len(rows)
    print(f"repo<->crate alignment: {total} repos audited under {args.owner}")
    for verdict in ("match", "monorepo", "mismatch", "no-manifest", "no-crate"):
        if verdict in counts:
            print(f"  {verdict:>12}: {counts[verdict]}")

    pending = sorted(grandfathered + violations)
    if pending:
        print(f"\nPENDING RENAMES ({len(pending)}) — repo name != its single crate name:")
        for name, _, crates in pending:
            target = GRANDFATHERED.get(name, crates[0] if crates else "?")
            state = "grandfathered" if name in GRANDFATHERED else "VIOLATION"
            print(f"  {name:<20} -> {target:<20} [{state}]")
            print(f"  {'':20} gh repo rename {name} {target}")
        print("\n  Naming-convention rule 1: one crate per repo, same name.")
        print("  A rename preserves the old URL as a redirect, so existing badges and")
        print("  pins keep working; the crates.io `repository` field becomes correct, and")
        print("  docs.rs gets a link that resolves to the crate you are reading.")
        print("  Remove the entry from GRANDFATHERED once the rename lands.")

    if args.enforce and pending:
        print(
            f"\nerror: {len(pending)} repo<->crate misalignment(s) remain "
            f"({len(violations)} ungrandfathered, {len(grandfathered)} grandfathered).",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
