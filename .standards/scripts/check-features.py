#!/usr/bin/env python3
"""Estate feature-compatibility checker — enforces the additive-feature rule.

Cargo unifies features **graph-wide**: if any crate in a consumer's dependency
graph enables `dep/feat`, every crate in that graph is compiled with it. A
crate that is correct under its own `--all-features` build can therefore still
fail to compile inside a host that enables a *different* feature set.

The unsound shape is a public API element whose existence depends on a
feature:

    #[cfg(feature = "timeout")]
    Timeout,                      // a new variant breaks every host `match`
                                  // that was exhaustive before unification

This checker finds those elements at the source, statically, before they reach
a consumer. It is the prevention gate; a downstream unification build is only
the detection.

Rules (see docs/feature-compatibility.md):

    R1  feature-gated variant of a public enum
    R2  feature-gated field of a public struct that is not #[non_exhaustive]
    R3  feature-gated match arm
    R4  feature-gated required method of a public trait (no default body)
    R5  suppression without a reason

Suppression: a line above or on the offending line may carry

    // feature-compat: allow — <non-empty reason>

A suppression with an empty reason is itself reported (R5): the estate does
not accept undocumented escape hatches.

Usage:

    python3 check-features.py --cwd .
    python3 check-features.py --cwd . --json
    python3 check-features.py --cwd . --rule R1 --rule R2

Pure stdlib. No third-party deps, nothing to install in kit repos.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path

RULE_TEXT = {
    "R1": "feature-gated variant of a public enum",
    "R2": "feature-gated field of a public struct",
    "R3": "feature-gated match arm",
    "R4": "feature-gated required method of a public trait",
    "R5": "suppression without a reason",
}

DEFAULT_RULES = ("R1", "R2", "R3", "R4", "R5")

DOC_REF = "docs/feature-compatibility.md"

# --- lexical helpers -------------------------------------------------------

ATTR_RE = re.compile(r'#!?\[', re.DOTALL)
CFG_FEATURE_RE = re.compile(r'\bcfg(?:_attr)?\s*\(\s*feature\s*=\s*"(?P<f>[^"]+)"')
NON_EXHAUSTIVE_RE = re.compile(r'\bnon_exhaustive\b')
SUPPRESS_RE = re.compile(r'feature-compat:\s*allow\s*(?:—|--|-)?\s*(?P<why>.*)$')

ITEM_RE = re.compile(
    r'^\s*(?P<vis>pub\s*(?:\([^)]*\)\s*)?)?'
    r'(?P<kind>enum|struct|trait|union|impl|mod)\b'
    r'(?P<rest>.*)$'
)
# `fn` is deliberately absent from ITEM_RE: a function line must be able to
# carry the R4 required-trait-method check, and a single combined alternation
# made `fn` win that branch before the trait logic ever ran.
FN_RE = re.compile(
    r'^\s*(?:pub\s*(?:\([^)]*\)\s*)?)?'
    r'(?:(?:async|const|unsafe|extern\s+"[^"]*")\s+)*fn\b'
)
MATCH_RE = re.compile(r'^\s*(?:let\s+[^=]*=\s*)?match\b')
NAME_RE = re.compile(r'\b([A-Za-z_][A-Za-z0-9_]*)')


def strip_comments(line: str) -> str:
    """Remove `//` line comments, preserving all string/char literal contents.

    Doc comments (`///`, `//!`) become empty so that `#[cfg(feature = ...)]`
    shown inside a doc example is never mistaken for real code.

    String contents are preserved verbatim on purpose: the feature *name*
    lives inside a string literal, so blanking literals would destroy exactly
    what this checker is looking for. Tracking literals only prevents a `//`
    inside a string from truncating the line.
    """
    if line.lstrip().startswith("///") or line.lstrip().startswith("//!"):
        return ""
    out = []
    i = 0
    n = len(line)
    while i < n:
        c = line[i]
        if c == "/" and i + 1 < n and line[i + 1] == "/":
            break
        if c == '"':
            out.append(c)
            i += 1
            while i < n:
                out.append(line[i])
                if line[i] == "\\" and i + 1 < n:
                    out.append(line[i + 1])
                    i += 2
                    continue
                if line[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        if c == "'":
            # A char literal is 'x' or '\x'; anything else is a lifetime.
            m = re.match(r"'(\\.|[^\\'])'", line[i:])
            if m:
                out.append(m.group(0))
                i += len(m.group(0))
                continue
            out.append(c)
            i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def take_attributes(code: str) -> tuple[list[str], str]:
    """Peel leading `#[...]` groups off `code`.

    Returns the attribute bodies and whatever remains of the line. Attributes
    frequently share a line with their item (`#[cfg(feature = "x")] pub fn
    f()`), so the remainder must be processed rather than skipped.
    """
    attrs: list[str] = []
    rest = code
    while True:
        m = ATTR_RE.match(rest.lstrip())
        if not m:
            break
        offset = len(rest) - len(rest.lstrip())
        start = m.start() + offset
        depth = 0
        j = start
        while j < len(rest):
            if rest[j] == "[":
                depth += 1
            elif rest[j] == "]":
                depth -= 1
                if depth == 0:
                    j += 1
                    break
            j += 1
        attrs.append(rest[start + 1 : j - 1])
        rest = rest[j:]
    return attrs, rest


def is_public_vis(vis: str | None) -> bool:
    """True only for unrestricted `pub`.

    `pub(crate)`, `pub(super)`, `pub(in path)` and `pub(self)` are all crate- or
    module-internal: no downstream crate can name the item, so gating it is
    invisible outside and cannot violate feature unification.
    """
    return (vis or "").strip() == "pub"


def features_in(bodies: list[str]) -> list[str]:
    found: list[str] = []
    for body in bodies:
        for m in CFG_FEATURE_RE.finditer(body):
            found.append(m.group("f"))
    return found


PATH_HEAD_RE = re.compile(r'\b([A-Za-z_][A-Za-z0-9_]*)\s*::')
# A gated *match arm* is a gated pattern followed by `=>`. Anything else inside
# a match body — a gated `if let`, a gated block, a gated macro call — is a
# statement, not an arm, and gating it is sound. Without this discriminator R3
# fires on every `#[cfg(feature)] tracing::warn!(..)` in the estate.
ARM_RE = re.compile(r'=>')


def collect_local_enums(sources: list[Path]) -> dict[str, bool]:
    """Every enum declared in this crate, mapped to whether it is public.

    Resolved crate-wide rather than per-file: a match arm over an enum declared
    in a sibling module is still an arm over an enum this crate owns.
    """
    local: dict[str, bool] = {}
    for path in sources:
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except OSError:
            continue
        for raw in lines:
            probe = strip_comments(raw)
            pm = ITEM_RE.match(probe)
            if pm and pm.group("kind") == "enum":
                pn = NAME_RE.search(pm.group("rest") or "")
                if pn:
                    local[pn.group(1)] = is_public_vis(pm.group("vis"))
    return local


NON_EXHAUSTIVE_DECL_RE = re.compile(
    r'#\[non_exhaustive\](?:\s*#\[[^\]]*\])*\s*'
    r'(?:#\[derive\((?P<derive>[^\]]*)\)\]\s*)?'
    r'(?:#\[(?P<between>[a-z_][a-z_0-9]*)(?:\([^\]]*\))?\])*\s*'
    r'(?:pub\s*(?:\([^)]*\)\s*)?)?'
    r'(?:enum|struct)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)'
)


def collect_non_exhaustive(sources: list[Path]) -> set[str]:
    """Names of types declared `#[non_exhaustive]`.

    Resolved across the whole crate and over a *window* of following lines,
    because the attribute is routinely separated from its item by a doc comment
    or a `derive` list:

        #[derive(Debug, Clone)]
        #[non_exhaustive]
        pub struct Config { .. }

    An earlier revision missed exactly this shape and reported R2 on a type
    that was opted out, so the scan reads ahead rather than pattern-matching one
    line.
    """
    out: set[str] = set()
    for path in sources:
        try:
            lines = [
                strip_comments(ln) for ln in path.read_text(encoding="utf-8").splitlines()
            ]
        except OSError:
            continue
        for i, line in enumerate(lines):
            if not NON_EXHAUSTIVE_RE.search(line):
                continue
            for j in range(i, min(len(lines), i + 6)):
                pm = ITEM_RE.match(lines[j])
                if pm and pm.group("kind") in ("enum", "struct", "union"):
                    pn = NAME_RE.search(pm.group("rest") or "")
                    if pn:
                        out.add(pn.group(1))
                    break
    return out


def owning_enum(
    arm_line: str, local_enums: dict[str, bool], self_type: str | None = None
) -> str | None:
    """The crate-local enum a match arm is matching over, if any.

    Returns the enum name when the arm provably matches an enum declared in
    this crate, else `None`. `None` means "not ours" — the unification
    hazard — and is deliberately the fail-safe default: an unresolvable arm is
    reported rather than assumed safe.

    `Self::Variant` is resolved through `self_type`, the self type of the
    nearest enclosing `impl` (matching a private enum inside its own `impl` is
    the common sound case).
    """
    for head in PATH_HEAD_RE.findall(arm_line):
        if head in local_enums:
            return head
        if head == "Self" and self_type and self_type in local_enums:
            return self_type
    return None


class Frame:
    __slots__ = ("kind", "name", "pub", "open_depth", "non_exhaustive", "in_test")

    def __init__(self, kind, name, pub, open_depth, in_test=False):
        self.kind = kind
        self.name = name
        self.pub = pub
        self.open_depth = open_depth
        self.non_exhaustive = False
        self.in_test = in_test


class Finding:
    __slots__ = ("rule", "path", "line", "feature", "message")

    def __init__(self, rule, path, line, feature, message):
        self.rule, self.path, self.line = rule, path, line
        self.feature, self.message = feature, message

    def as_dict(self, root: Path) -> dict:
        try:
            rel = str(Path(self.path).relative_to(root))
        except ValueError:
            rel = str(self.path)
        return {
            "rule": self.rule,
            "rule_text": RULE_TEXT.get(self.rule, ""),
            "file": rel,
            "line": self.line,
            "feature": self.feature,
            "message": self.message,
        }


def scan_file(
    path: Path,
    local_enums: dict[str, bool],
    non_exhaustive_names: set[str],
) -> list[Finding]:
    findings: list[Finding] = []
    try:
        raw_lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return findings

    stack: list[Frame] = []
    pending: list[tuple[int, str]] = []  # (line number, attribute body)
    depth = 0
    test_mod_depth: int | None = None

    def suppressed(lineno: int) -> tuple[bool, str]:
        # A suppression is conventionally written above the attribute, which
        # may itself sit above the item, so probe a short window upwards.
        for probe in range(lineno, max(0, lineno - 4), -1):
            if 1 <= probe <= len(raw_lines):
                m = SUPPRESS_RE.search(raw_lines[probe - 1])
                if m:
                    return True, (m.group("why") or "").strip()
        return False, ""

    def report(rule: str, lineno: int, feature: str, message: str) -> None:
        ok, why = suppressed(lineno)
        if not ok:
            findings.append(Finding(rule, path, lineno, feature, message))
        elif not why:
            findings.append(
                Finding(
                    "R5",
                    path,
                    lineno,
                    feature,
                    "suppression of %s carries no reason: expected "
                    "`// feature-compat: allow — <reason>`" % rule,
                )
            )

    def container(kinds: tuple[str, ...]) -> Frame | None:
        for f in reversed(stack):
            if f.kind in kinds:
                return f
        return None

    for lineno, raw in enumerate(raw_lines, 1):
        code = strip_comments(raw)
        if not code.strip():
            continue  # blank / doc-comment lines keep `pending` intact

        attr_bodies, rest = take_attributes(code)
        pending.extend((lineno, b) for b in attr_bodies)
        if not rest.strip():
            continue

        # Report at the line the cfg attribute was written on, which is where
        # a reader (and any suppression comment) will look.
        attr_line = pending[0][0] if pending else lineno
        feats = features_in([b for _, b in pending])
        pending = []

        in_test = test_mod_depth is not None
        m_item = ITEM_RE.match(rest)
        m_fn = FN_RE.match(rest)
        m_match = MATCH_RE.match(rest)

        if m_fn:
            # A required trait method is a `fn` with no body inside a public
            # trait. Must be tested before ITEM_RE so it is not classified as
            # a plain item.
            in_pub_trait = container(("trait",))
            if (
                feats
                and not in_test
                and in_pub_trait is not None
                and in_pub_trait.pub
                and "{" not in rest
            ):
                for f in feats:
                    report(
                        "R4",
                        attr_line,
                        f,
                        "required method of public trait `%s` is gated behind "
                        "feature `%s`; every host must provide an impl, but a "
                        "host that never enabled the feature cannot know the "
                        "method exists (%s R4)"
                        % (in_pub_trait.name, f, DOC_REF),
                    )

        elif m_item:
            kind = m_item.group("kind")
            vis = (m_item.group("vis") or "").strip()
            is_pub = is_public_vis(vis)
            nm = NAME_RE.search(m_item.group("rest") or "")
            name = nm.group(1) if nm else "?"

            if kind == "mod" and name in ("tests", "test") and "{" in rest:
                test_mod_depth = depth + 1

            if "{" in rest:
                frame = Frame(kind, name, is_pub, depth, in_test=in_test)
                frame.non_exhaustive = name in non_exhaustive_names
                stack.append(frame)

        elif m_match:
            if "{" in rest:
                stack.append(Frame("match", "?", False, depth, in_test=in_test))

        else:
            # A bare line inside a container: an enum variant, a struct
            # field, or a match arm.
            if feats and not in_test:
                c_enum = container(("enum",))
                c_struct = container(("struct", "union"))
                c_match = container(("match",))
                # Only a plain `pub` field is public API. A private or
                # `pub(crate)` field on a public struct already blocks
                # struct-literal construction downstream, so gating one is
                # invisible to every host. Restricted visibilities must NOT
                # match: `pub(crate) x: T` has no `pub` followed by space.
                is_pub_field = bool(re.match(r'\s*pub\s', rest))

                if c_enum is not None and c_enum.pub:
                    # `#[non_exhaustive]` is the sanctioned remedy: it converts
                    # "silently breaks every exhaustive match" into one compile
                    # error at the point of use. worker-kit's `Trigger` and
                    # `RegisterError` are fixed this way.
                    if c_enum.name in non_exhaustive_names:
                        continue
                    for f in feats:
                        report(
                            "R1",
                            attr_line,
                            f,
                            "variant of public enum `%s` is gated behind feature "
                            "`%s`; a host enabling it must add a match arm, and a "
                            "host built without it cannot name the variant. Mark "
                            "the enum #[non_exhaustive] to convert this into a "
                            "single actionable error (%s R1)"
                            % (c_enum.name, f, DOC_REF),
                        )
                elif (
                    c_struct is not None
                    and c_struct.pub
                    and not c_struct.non_exhaustive
                    and is_pub_field
                ):
                    for f in feats:
                        report(
                            "R2",
                            attr_line,
                            f,
                            "public field of public struct `%s` is gated behind "
                            "feature `%s` and the struct is not "
                            "#[non_exhaustive]; hosts that construct or destructure "
                            "it with a literal break under unification (%s R2)"
                            % (c_struct.name, f, DOC_REF),
                        )
                elif c_match is not None and ARM_RE.search(rest):
                    # Gating an arm that matches an enum *this crate declares*
                    # is self-consistent: the crate owns both the enum and the
                    # arm, so no host can observe the difference. R3 exists for
                    # arms over enums the crate does not own — the historical
                    # outbox-kit / breaker case.
                    impl_frame = container(("impl",))
                    owner = owning_enum(
                        rest,
                        local_enums,
                        self_type=impl_frame.name if impl_frame else None,
                    )
                    if owner is not None:
                        continue  # local enum: R1 already governs its variants
                    for f in feats:
                        report(
                            "R3",
                            attr_line,
                            f,
                            "match arm over a foreign enum is gated behind "
                            "feature `%s`; if that enum has feature-gated variants "
                            "this arm silently vanishes under unification, so an "
                            "exhaustive match stops being exhaustive (%s R3)"
                            % (f, DOC_REF),
                        )

        # ---- keep the frame stack consistent with brace depth --------------
        net = rest.count("{") - rest.count("}")
        depth += net
        while stack and depth <= stack[-1].open_depth:
            gone = stack.pop()
            if gone.kind == "mod" and gone.name in ("tests", "test"):
                if test_mod_depth is not None and depth < test_mod_depth:
                    test_mod_depth = None

    return findings


MAX_SRC_DEPTH = 3  # single crate (src/), crates/*/src, members/*/src

# Never scan vendored or generated third-party code: these are the estate's
# rules, and reporting them against a dependency's source is noise at best and
# a false accusation at worst. Dot-directories are excluded for the same
# reason (`.cargo-vendor`, `.git`, editor state).
EXCLUDED_DIRS = {"target", "vendor", ".cargo-vendor", "node_modules"}


def _excluded(path: Path, root: Path) -> bool:
    try:
        rel = path.relative_to(root)
    except ValueError:
        return True
    return any(part in EXCLUDED_DIRS or part.startswith(".") for part in rel.parts[:-1])


def iter_sources(cwd: Path) -> list[Path]:
    """Every `src/**/*.rs` under `cwd`, across library members of a workspace.

    The shared CI job is called per kit repo, and kit repos are sometimes a
    single crate and sometimes a workspace (`crates/*/src`). Globbing keeps the
    checker pure-stdlib and avoids depending on a resolvable Cargo graph.

    A member is scanned only when it has a `lib.rs`. Only a library target
    exposes a public API a host can compile against: in a binary-only member,
    `pub enum Commands` and friends are internal to the application, and clap
    subcommands gated per feature are the idiomatic pattern, not a hazard.
    """
    candidates: list[Path] = [cwd / "src"]
    for depth in range(2, MAX_SRC_DEPTH + 1):
        pattern = "/".join(["*"] * (depth - 1) + ["src"])
        candidates.extend(sorted(cwd.glob(pattern)))

    roots: list[Path] = []
    for cand in candidates:
        if not cand.is_dir() or not (cand / "lib.rs").is_file():
            continue
        # The candidate's own path must be free of vendored/generated
        # segments too: `clawdius/.cargo-vendor/half/src` is a valid glob match
        # but is a dependency's source, not ours.
        try:
            rel = cand.relative_to(cwd)
        except ValueError:
            continue
        if any(
            part in EXCLUDED_DIRS or part.startswith(".") for part in rel.parts
        ):
            continue
        if any(p.is_file() and not _excluded(p, cand) for p in cand.rglob("*.rs")):
            roots.append(cand)

    files: list[Path] = []
    for root in roots:
        files.extend(
            p for p in root.rglob("*.rs") if p.is_file() and not _excluded(p, root)
        )
    return sorted(set(files))


def main() -> int:
    ap = argparse.ArgumentParser(description="WyattAu feature-compatibility checker")
    ap.add_argument("--cwd", default=".", help="crate/workspace root to check")
    ap.add_argument(
        "--rule",
        action="append",
        choices=sorted(RULE_TEXT),
        help="restrict to specific rules (repeatable; default all)",
    )
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    args = ap.parse_args()

    rules = set(args.rule) if args.rule else set(DEFAULT_RULES)
    root = Path(args.cwd).resolve()
    sources = iter_sources(root)

    findings: list[Finding] = []
    # Both maps are crate-wide: a match arm or a struct declared in one module
    # must be judged against types declared in another.
    local_enums = collect_local_enums(sources)
    non_exhaustive_names = collect_non_exhaustive(sources)
    for path in sources:
        findings.extend(scan_file(path, local_enums, non_exhaustive_names))
    findings = [f for f in findings if f.rule in rules]
    findings.sort(key=lambda f: (str(f.path), f.line, f.rule))

    if args.json:
        print(
            json.dumps(
                {
                    "root": str(root),
                    "files_scanned": len(sources),
                    "findings": [f.as_dict(root) for f in findings],
                },
                indent=2,
            )
        )
        return 1 if findings else 0

    if not sources:
        if not args.json:
            print("no src/ directory at %s — nothing to check" % root)
        return 0

    gh = os.environ.get("GITHUB_ACTIONS") == "true"
    for f in findings:
        prefix = "::error::" if gh else "error: "
        d = f.as_dict(root)
        print(
            "%s%s:%d: [%s] %s (feature `%s`)"
            % (prefix, d["file"], f.line, f.rule, f.message, f.feature)
        )

    if findings:
        print(
            "\nfeature-compatibility check FAILED: %d finding(s).\n"
            "Additive features must not gate public API elements. Fix by:\n"
            "  - making the element unconditional, or\n"
            "  - introducing a whole new type behind the feature, or\n"
            "  - marking the struct #[non_exhaustive], or\n"
            "  - documenting a deliberate exception with\n"
            "    `// feature-compat: allow — <reason>` on the line.\n"
            "See %s." % (len(findings), DOC_REF)
        )
        return 1

    print(
        "feature-compatibility check passed (%d file(s) scanned, %d rule(s) active)"
        % (len(sources), len(rules))
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
