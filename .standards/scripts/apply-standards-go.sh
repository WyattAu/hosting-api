#!/usr/bin/env bash
# Bulk-apply engineering-standards templates to a Go repo.
# Usage: scripts/apply-standards-go.sh <repo-dir>
# Idempotent: never overwrites an existing file (diffs printed instead).
set -euo pipefail

repo="${1:?usage: apply-standards-go.sh <repo-dir>}"
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
    uses: WyattAu/engineering-standards/.github/workflows/go-kit.yml@main
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
  - package-ecosystem: gomod
    directory: "/"
    schedule:
      interval: weekly
      day: monday
    open-pull-requests-limit: 5
    groups:
      minor-and-patch:
        update-types: ["minor", "patch"]
  - package-ecosystem: github-actions
    directory: "/"
    schedule:
      interval: weekly
EOF
  echo "dependabot: written"
else
  echo "dependabot: exists"
fi

echo "go standards applied. Gate: quality uses go-kit.yml@main."
