#!/usr/bin/env bash
# Clone or update the estate into one directory, so path dependencies resolve.
#
# The estate is path-linked: no crate in it is published to crates.io, so a crate can only
# find its siblings if they are on disk beside it. That works fine for someone with the
# estate already checked out, and fails for anyone who clones a single repository, because
# cargo says only `no matching package named X` with no hint that the answer is to clone
# the neighbours.
#
# This script is that answer. It clones every crate named in the estate registry, checks
# them out beside each other, and then verifies that each one resolves its dependencies.
# It is idempotent: running it again updates what is already there.
#
# Usage:
#   scripts/bootstrap.sh [DESTINATION]
#
# DESTINATION defaults to a directory beside this script's parent, which is where the
# estate is normally kept. Pass somewhere else to put a second copy.

set -euo pipefail

ORG="${WYATTAU_ORG:-WyattAu}"
REGISTRY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/estate-crates.txt"

if [[ ! -f "$REGISTRY" ]]; then
    echo "bootstrap: cannot find the crate registry at $REGISTRY" >&2
    exit 1
fi

if [[ $# -ge 1 ]]; then
    DEST="$(cd "$1" 2>/dev/null && pwd || echo "$1")"
else
    DEST="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi

mkdir -p "$DEST"

if ! command -v git >/dev/null 2>&1; then
    echo "bootstrap: git is required" >&2
    exit 1
fi

# One crate per line, blanks and comments ignored.
mapfile -t CRATES < <(grep -vE '^\s*(#|$)' "$REGISTRY" | tr -d '\r')
echo "bootstrap: ${#CRATES[@]} crates into $DEST"

cloned=0
updated=0
failed=0

for crate in "${CRATES[@]}"; do
    target="$DEST/$crate"
    if [[ -d "$target/.git" ]]; then
        if git -C "$target" fetch --quiet origin 2>/dev/null; then
            git -C "$target" pull --ff-only --quiet 2>/dev/null && updated=$((updated + 1)) || true
        fi
        continue
    fi
    if git clone --quiet "https://github.com/$ORG/$crate.git" "$target" 2>/dev/null; then
        cloned=$((cloned + 1))
    else
        # A registry entry with no repository is not fatal. It is usually a crate that
        # lives inside another repository's workspace, and skipping it silently would make
        # the totals at the end a lie, so it is counted and named.
        echo "  skipped: $crate (no repository at $ORG/$crate)"
        failed=$((failed + 1))
    fi
done

echo "bootstrap: $cloned cloned, $updated updated, $failed unavailable"

# Verify the result, because a directory full of clones that cannot build is not a
# bootstrapped estate. Only the crates this one checked out are attempted, and a failure
# is reported rather than fatal: some crates in the registry are mid-rename and a broken
# one should not stop the other 160 from being usable.
echo "bootstrap: verifying that each crate resolves its dependencies"
buildable=0
unbuildable=0
for crate in "${CRATES[@]}"; do
    [[ -f "$DEST/$crate/Cargo.toml" ]] || continue
    if (cd "$DEST/$crate" && cargo metadata --format-version 1 --quiet >/dev/null 2>&1); then
        buildable=$((buildable + 1))
    else
        echo "  does not resolve: $crate"
        unbuildable=$((unbuildable + 1))
        # Name the unsatisfied requirement, not just the fact of one. "Does not resolve"
        # sends the user into cargo's output themselves; the version gap is usually one
        # line, and it is the difference between knowing the fix and knowing a problem
        # exists. Path dependencies resolve against the sibling checkout, so a gap here
        # means exactly: the sibling's main is older than what this crate's main wants,
        # and the fix is merging the branch that has it.
        reason=$(cd "$DEST/$crate" && cargo metadata --format-version 1 2>&1 >/dev/null | \
            grep -m1 -E 'failed to select a version|required by package' || true)
        [[ -n "$reason" ]] && echo "    because: $reason"
    fi
done

echo "bootstrap: $buildable crate(s) resolve, $unbuildable do not"
if [[ $unbuildable -gt 0 ]]; then
    echo "bootstrap: a crate that does not resolve usually wants a dependency branch" >&2
    echo "           checked out rather than main. See each crate's CHANGELOG." >&2
fi