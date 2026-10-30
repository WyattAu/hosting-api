#!/usr/bin/env bash
# Release a WyattAu kit crate: gate → changelog → semver → tag → publish → verify.
# Usage: scripts/release.sh <crate-dir> <version> [--dry-run]
# Policies: engineering-standards README §1 (no publish without green gates,
# changelog entry, and matching tag).
set -euo pipefail

dir="${1:?usage: release.sh <crate-dir> <version> [--dry-run]}"
version="${2:?usage: release.sh <crate-dir> <version> [--dry-run]}"
dry="${3:-}"

cd "$dir"
name=$(grep -m1 '^name = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
current=$(grep -m1 '^version = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
echo "== releasing $name $current -> $version ${dry:0:0}"

# 1. Working tree must be clean.
[ -z "$(git status --porcelain)" ] || { echo "FAIL: dirty working tree"; exit 1; }

# 2. Changelog must mention the target version.
grep -q "## \[$version\]" CHANGELOG.md || {
  echo "FAIL: CHANGELOG.md has no '## [$version]' entry"; exit 1; }

# 3. Bump version.
sed -i "0,/^version = \".*\"/s//version = \"$version\"/" Cargo.toml
cargo update --workspace --quiet 2>/dev/null || cargo update -q || true
git add -A

# 4. Full gate matrix (fast subset; deep gates run in CI).
cargo check --locked --all-features
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all-features --quiet

# 5. Semver gate: breaking change requires matching semver bump.
if command -v cargo-semver-checks >/dev/null && git describe --tags --abbrev=0 >/dev/null 2>&1; then
  cargo semver-checks --baseline-rev "$(git describe --tags --abbrev=0)"
fi

# 6. Publish dry-run, then publish.
cargo publish --dry-run --allow-dirty
if [ "$dry" = "--dry-run" ]; then
  echo "DRY RUN OK — would publish $name $version"
  git reset -q
  exit 0
fi

git commit -m "chore(release): $name $version"
cargo publish
tag="v$version"
git tag -a "$tag" -m "$name $version"
git push origin HEAD --follow-tags

# 7. Registry verification (eventual consistency: poll briefly).
sleep 10
if cargo search "$name" --limit 1 | grep -q "\"$version\""; then
  echo "== OK: $name $version published and visible"
else
  echo "WARN: $name $version not yet visible on crates.io (index lag) — verify manually"
fi
