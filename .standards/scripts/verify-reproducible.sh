#!/usr/bin/env bash
# Verify same-machine build determinism for a crate: two from-scratch builds
# through the SAME absolute paths (wiped in between) with sanitized
# environments, then compare artifact hashes.
#
# Usage: scripts/verify-reproducible.sh <crate-dir>
#
# Why same absolute paths: rustc embeds absolute paths of the target dir and
# dependency sources (under CARGO_HOME) in rlib/rmeta, so distinct build dirs
# legitimately produce distinct hashes. Wiping and reusing one fixed path
# removes that variable while still catching real nondeterminism.
#
# What this catches: nondeterministic codegen, environment leaks (RUSTFLAGS,
# ccache/sccache wrappers, incremental state), registry drift (deps are
# re-downloaded from scratch each build, checksums re-verified).
#
# CAVEAT: Rust builds are reproducible only with the same toolchain, flags,
# and dependency set. This script verifies SAME-MACHINE determinism — it does
# NOT prove cross-machine reproducibility (different OS, CPU, rustc binary, or
# absolute paths can legitimately change output). A mismatch is a smell, not
# by itself a security event.
set -euo pipefail

dir="${1:?usage: verify-reproducible.sh <crate-dir>}"
cd "$dir"
[ -f Cargo.lock ] || { echo "FAIL: no Cargo.lock — run cargo generate-lockfile"; exit 1; }

crate=$(grep -m1 '^name = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
epoch=$(git log -1 --format=%ct 2>/dev/null || echo 0)
scratch="${REPRO_SCRATCH:-/tmp/opencode}/reproducible/$crate"
lock="$scratch.lock"
mkdir -p "$(dirname "$lock")"

exec 9>"$lock"
flock 9

build_and_manifest() {
  local n=$1 out=$2
  # From scratch: no incremental state, no shared registry, fresh download +
  # checksum verification of every dependency.
  rm -rf "$scratch/build" "$scratch/cargo-home"
  mkdir -p "$scratch/build" "$scratch/cargo-home"
  env -i \
    PATH="$PATH" HOME="$HOME" \
    CARGO_HOME="$scratch/cargo-home" \
    CARGO_INCREMENTAL=0 \
    CARGO_TERM_COLOR=never \
    SOURCE_DATE_EPOCH="$epoch" \
    TZ=UTC LC_ALL=C \
    sh -c "cd '$PWD' && cargo build --release --locked --target-dir '$scratch/build'" \
    > "$scratch/build$n.log" 2>&1
  # Normalize nothing — both manifests come from the identical path.
  {
    find "$scratch/build/release" -maxdepth 1 -type f ! -name '*.d' ! -name 'CACHEDIR.TAG' -print0
    find "$scratch/build/release/deps" -maxdepth 1 -type f \( -name '*.rlib' -o -name '*.rmeta' -o -name '*.so' \) -print0
  } | sort -z | xargs -0 -r sha256sum > "$out"
}

echo "== $crate: build 1/2 (from-scratch, sanitized env)"
t0=$(date +%s)
build_and_manifest 1 "$scratch/manifest1.txt"
echo "   done in $(( $(date +%s) - t0 ))s"

echo "== $crate: build 2/2 (from-scratch, sanitized env)"
t0=$(date +%s)
build_and_manifest 2 "$scratch/manifest2.txt"
echo "   done in $(( $(date +%s) - t0 ))s"

echo
echo "== artifacts ($crate)"
while read -r hash path; do
  match=DIFF
  while read -r h2 p2; do
    if [ "$p2" = "$path" ]; then [ "$h2" = "$hash" ] && match=MATCH; break; fi
  done < "$scratch/manifest2.txt"
  echo "   $match  ${path#*/release/}  $hash"
done < "$scratch/manifest1.txt"

if diff -q "$scratch/manifest1.txt" "$scratch/manifest2.txt" >/dev/null; then
  echo
  echo "REPRODUCIBLE: $crate — all artifact hashes identical across from-scratch builds"
  rm -rf "$scratch/build" "$scratch/cargo-home"
  exit 0
else
  echo
  echo "NONDETERMINISTIC: $crate — hash mismatch; manifests at $scratch/manifest{1,2}.txt"
  diff "$scratch/manifest1.txt" "$scratch/manifest2.txt" | head -20 || true
  exit 1
fi
