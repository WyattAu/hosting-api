#!/usr/bin/env python3
"""Unit test: absence of a repo is an error ONLY for a token that sees private repos.

Loop 25: the estate audit ran red for six consecutive nights in CI because
GITHUB_TOKEN (an installation token) cannot see the account's private repos,
so fourteen of them were reported as "manifest lists a repo GitHub does not
return". The same manifest audited with a PAT reported zero. This test pins
both halves of that contract so neither can regress silently.
"""
from __future__ import annotations

import importlib.util
import pathlib
import sys

HERE = pathlib.Path(__file__).parent
SPEC = importlib.util.spec_from_file_location("estate_audit", HERE / "estate-audit.py")
assert SPEC and SPEC.loader
audit_mod = importlib.util.module_from_spec(SPEC)
sys.modules["estate_audit"] = audit_mod
SPEC.loader.exec_module(audit_mod)


def finding_kinds(sees_private: bool) -> dict[str, list[str]]:
    """Run the manifest-vs-GitHub comparison with a controlled token scope."""
    manifest = audit_mod.Manifest.__new__(audit_mod.Manifest)
    entry = {"area": "app", "status": "support", "crates": []}
    manifest.repos = {"public-thing": dict(entry), "private-thing": dict(entry)}
    manifest.pins_held = []
    # GitHub returns only the public repo -- exactly what an installation
    # token sees (and what a PAT would also see for a deleted private repo).
    gh_listing = [{"name": "public-thing"}]
    original = audit_mod.token_sees_private_repos
    audit_mod.token_sees_private_repos = lambda: sees_private
    try:
        findings = audit_mod.audit(manifest, gh_listing, {}, {})
    finally:
        audit_mod.token_sees_private_repos = original
    out: dict[str, list[str]] = {}
    for f in findings:
        out.setdefault(f["kind"], []).append(f["subject"])
    return out


def main() -> int:
    as_user_token = finding_kinds(True)
    if "error" not in as_user_token or "private-thing" not in as_user_token["error"]:
        print("FAIL: a PAT-audit must treat an unseen repo as an error")
        return 1
    # The repo the listing DOES return must never be flagged missing.
    for kind, subjects in as_user_token.items():
        if "public-thing" in subjects:
            print(f"FAIL: a listed repo was flagged as {kind}")
            return 1

    as_app_token = finding_kinds(False)
    if "error" in as_app_token:
        print(f"FAIL: installation token produced phantom errors: {as_app_token['error']}")
        return 1
    if "private-thing" not in as_app_token.get("info", []):
        print("FAIL: the invisible repo must still be reported, as info")
        return 1

    print("ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
