#!/usr/bin/env bash
# Regenerate crates/ff-script/src/flowfabric.lua from lua/*.lua.
#
# Why this exists: ff-script used to concatenate these at build time via
# build.rs reading ../../lua. That path escapes the crate root and was not
# included in the published tarball, so ff-script v0.1.0 failed to build on
# crates.io. Fix: the concatenation now produces a file checked into the
# crate itself, shipped in the tarball. This script is the only supported
# way to regenerate it; a CI drift check (matrix.yml) fails the build if
# the checked-in copy diverges from what this script would produce.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LUA_DIR="$ROOT/lua"
OUT="$ROOT/crates/ff-script/src/flowfabric.lua"
VER_OUT="$ROOT/crates/ff-script/src/flowfabric_lua_version"

# Order matters: helpers must come first (all functions depend on it).
FILES=(
  helpers.lua
  version.lua
  lease.lua
  execution.lua
  scheduling.lua
  suspension.lua
  signal.lua
  stream.lua
  budget.lua
  quota.lua
  flow.lua
)

{
  printf '#!lua name=flowfabric\n'
  for f in "${FILES[@]}"; do
    printf '\n-- source: lua/%s\n' "$f"
    cat "$LUA_DIR/$f"
    printf '\n'
  done
} > "$OUT"

# Extract LIBRARY_VERSION from lua/version.lua — same contract as the
# old build.rs: exactly one non-commented `return 'X'` literal.
VERSION="$(awk '
  { line = $0; sub(/^[[:space:]]+/, "", line) }
  line ~ /^--/ { next }
  match(line, /return '\''[^'\'']*'\''/) {
    s = substr(line, RSTART + 8, RLENGTH - 9)
    print s
  }
' "$LUA_DIR/version.lua")"

# `grep -c .` exits 1 on zero matches; with pipefail that would abort before
# we reach the helpful error message below. `|| :` neutralizes the exit code.
count=$(printf '%s\n' "$VERSION" | grep -c . || :)
if [ "$count" -ne 1 ]; then
  echo "error: expected exactly one non-commented \`return 'X'\` in lua/version.lua, got $count" >&2
  exit 1
fi

# Sanity bounds mirror the old build.rs: non-empty, no embedded quotes or
# newlines (would imply a broken parse), short enough to be a version string.
if [ -z "$VERSION" ]; then
  echo "error: extracted LIBRARY_VERSION is empty" >&2
  exit 1
fi
if [ "${#VERSION}" -ge 64 ]; then
  echo "error: extracted LIBRARY_VERSION too long (${#VERSION} chars): $VERSION" >&2
  exit 1
fi
case "$VERSION" in
  *"'"*|*'"'*|*$'\n'*|*$'\r'*)
    echo "error: extracted LIBRARY_VERSION contains illegal char: $VERSION" >&2
    exit 1
    ;;
esac

printf '%s' "$VERSION" > "$VER_OUT"

echo "wrote $OUT"
echo "wrote $VER_OUT (version=$VERSION)"
