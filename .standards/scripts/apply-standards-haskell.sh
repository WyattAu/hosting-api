#!/usr/bin/env bash
# Bulk-apply engineering-standards templates to a Haskell (cabal) repo.
# Usage: scripts/apply-standards-haskell.sh <repo-dir>
# Idempotent: never overwrites an existing file (diffs printed instead).
set -euo pipefail

repo="${1:?usage: apply-standards-haskell.sh <repo-dir>}"
std_dir="$(cd "$(dirname "$0")/.." && pwd)"

cd "$repo"
mkdir -p .github/workflows

if [ ! -f .github/workflows/ci.yml ] || [ "${FORCE:-0}" = "1" ]; then
  cat > .github/workflows/ci.yml <<'EOF'
name: CI
on:
  push:
    branches: [main, master]
  pull_request:

permissions:
  contents: read

jobs:
  quality:
    uses: WyattAu/engineering-standards/.github/workflows/haskell-kit.yml@main
    with:
      ghc-version: "9.8.4"
EOF
  echo "CI: written"
else
  echo "CI: exists — review manually"
fi

for f in SECURITY.md CHANGELOG.md; do
  if [ ! -f "$f" ]; then
    cp "$std_dir/templates/$f" "$f"
    echo "$f: copied"
  else
    echo "$f: exists"
  fi
done

if [ ! -f .github/dependabot.yml ]; then
  cat > .github/dependabot.yml <<'EOF'
version: 2
updates:
  # No dependabot ecosystem for cabal — index-state + bitrot cron carry it.
  - package-ecosystem: github-actions
    directory: "/"
    schedule:
      interval: weekly
EOF
  echo "dependabot: written"
else
  echo "dependabot: exists"
fi

echo "haskell standards applied. Gate: quality uses haskell-kit.yml@main."
