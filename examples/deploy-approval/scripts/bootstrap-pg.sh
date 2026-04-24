#!/usr/bin/env bash
# Bootstrap a Postgres database for the deploy-approval example.
#
# NOTE: ff-server 0.6.1 is Valkey-only; the Postgres backend crate
# ships with migrations but has no server wiring yet. This script is
# kept for when that lands. See README -> Known gaps.
set -euo pipefail

DB="${FF_PG_DB:-ff_deploy}"
USER="${FF_PG_USER:-postgres}"
PASS="${FF_PG_PASSWORD:-postgres}"
HOST="${FF_PG_HOST:-localhost}"
PORT="${FF_PG_PORT:-5432}"

export PGPASSWORD="$PASS"

psql -h "$HOST" -p "$PORT" -U "$USER" -c "CREATE DATABASE ${DB};" postgres 2>/dev/null || true

DATABASE_URL="postgres://${USER}:${PASS}@${HOST}:${PORT}/${DB}" \
    bash -c '(cd "$(git rev-parse --show-toplevel)/crates/ff-backend-postgres" && sqlx migrate run --source migrations)'

echo "bootstrap-pg: migrations applied to ${DB}"
