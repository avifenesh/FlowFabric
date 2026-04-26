#!/usr/bin/env bash
# RFC-023 §4.1 + C-trivia-2 + issue #365 — parity-drift lint.
#
# Compares PG vs SQLite migration files. Every PG migration must
# have a SQLite sibling with the same filename OR be listed in
# `crates/ff-backend-sqlite/migrations/.sqlite-skip`. Every SQLite
# migration must have a PG sibling — no PG-less additions.
#
# Skip-list format (per RFC-023 §4.1 C-trivia-2):
#   * one relative path per line
#   * `#`-prefixed lines are comments
#   * inline `# issue:NNN` suffix on a path line cites a tracking issue

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PG_DIR="$ROOT/crates/ff-backend-postgres/migrations"
SQLITE_DIR="$ROOT/crates/ff-backend-sqlite/migrations"
SKIP_FILE="$SQLITE_DIR/.sqlite-skip"

if [ ! -d "$PG_DIR" ]; then
  echo "ERROR: PG migrations dir missing: $PG_DIR" >&2
  exit 2
fi
if [ ! -d "$SQLITE_DIR" ]; then
  echo "ERROR: SQLite migrations dir missing: $SQLITE_DIR" >&2
  exit 2
fi

# Build skip-set (strip comments + blank lines, take first whitespace field).
SKIP_SET=""
if [ -f "$SKIP_FILE" ]; then
  SKIP_SET=$(grep -vE '^\s*(#|$)' "$SKIP_FILE" | awk '{print $1}' | sort -u || true)
fi

pg_files=$(cd "$PG_DIR" && ls | grep -E '^[0-9]+_.*\.sql$' | sort || true)
sqlite_files=$(cd "$SQLITE_DIR" && ls | grep -E '^[0-9]+_.*\.sql$' | sort || true)

missing_siblings=()
for pg in $pg_files; do
  if printf '%s\n' $sqlite_files | grep -qx "$pg"; then
    continue
  fi
  if printf '%s\n' $SKIP_SET | grep -qx "$pg"; then
    continue
  fi
  missing_siblings+=("$pg")
done

if [ ${#missing_siblings[@]} -gt 0 ]; then
  echo "ERROR: PG migrations without SQLite sibling or .sqlite-skip entry:" >&2
  printf '  %s\n' "${missing_siblings[@]}" >&2
  exit 1
fi

orphan_sqlite=()
for sq in $sqlite_files; do
  if ! printf '%s\n' $pg_files | grep -qx "$sq"; then
    orphan_sqlite+=("$sq")
  fi
done

if [ ${#orphan_sqlite[@]} -gt 0 ]; then
  echo "ERROR: SQLite migrations without PG sibling:" >&2
  printf '  %s\n' "${orphan_sqlite[@]}" >&2
  exit 1
fi

echo "OK: migration parity verified ($(echo "$pg_files" | wc -w | tr -d ' ') pairs)"
