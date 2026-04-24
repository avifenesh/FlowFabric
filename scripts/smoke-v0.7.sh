#!/usr/bin/env bash
# scripts/smoke-v0.7.sh — pre-release v0.7 smoke gate.
#
# Exit 0 iff every scenario passes (parity-clean) on every requested
# backend. Non-zero exit blocks release.
#
# Per feedback_smoke_before_release.md: this script MUST run green
# BEFORE `git tag v0.7.0`. The v0.6.0 release shipped a broken
# read_summary because smoke ran after publish; do not repeat.
#
# Required fixtures (operator responsibility — CI brings these up
# via service containers, see .github/workflows/release.yml):
#   * Valkey 8.x on 127.0.0.1:6379
#   * Postgres 16 on 127.0.0.1:5432 with a database reachable at
#     $FF_SMOKE_POSTGRES_URL (default
#     postgres://postgres:postgres@localhost:5432/ff_smoke)
#
# Env overrides (all optional):
#   FF_SMOKE_SERVER         — ff-server base URL (default :9090)
#   FF_SMOKE_VALKEY_HOST    — Valkey host (default localhost)
#   FF_SMOKE_VALKEY_PORT    — Valkey port (default 6379)
#   FF_SMOKE_POSTGRES_URL   — Postgres URL (default above)
#   FF_LANES                — ff-server lanes (default: default)
#   FF_SMOKE_BACKEND        — 'valkey' | 'postgres' | 'both' (default both)
#   FF_SMOKE_STRICT         — '1' to promote Skip to Fail (default: 1)
#
# Exit codes:
#   0 — all scenarios pass, parity clean
#   1 — one or more scenarios failed or parity violation
#   2 — fixture preflight failed (Valkey or Postgres unreachable)
#   3 — ff-server failed to come up / stay healthy

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

BACKEND="${FF_SMOKE_BACKEND:-both}"
STRICT_FLAG=""
if [[ "${FF_SMOKE_STRICT:-1}" == "1" ]]; then
    STRICT_FLAG="--strict"
fi

SERVER_URL="${FF_SMOKE_SERVER:-http://localhost:9090}"
VALKEY_HOST="${FF_SMOKE_VALKEY_HOST:-localhost}"
VALKEY_PORT="${FF_SMOKE_VALKEY_PORT:-6379}"
PG_URL="${FF_SMOKE_POSTGRES_URL:-postgres://postgres:postgres@localhost:5432/ff_smoke}"

# 1. Fixture preflight.
echo "==> smoke-v0.7: fixture preflight"

if [[ "${BACKEND}" == "valkey" || "${BACKEND}" == "both" ]]; then
    if command -v valkey-cli >/dev/null 2>&1; then
        if ! valkey-cli -h "${VALKEY_HOST}" -p "${VALKEY_PORT}" ping >/dev/null 2>&1; then
            echo "FAIL: Valkey not reachable at ${VALKEY_HOST}:${VALKEY_PORT}" >&2
            exit 2
        fi
    elif command -v redis-cli >/dev/null 2>&1; then
        # Operator fallback — Valkey is wire-compatible with redis-cli
        # for PING, so accept that if valkey-cli isn't installed.
        if ! redis-cli -h "${VALKEY_HOST}" -p "${VALKEY_PORT}" ping >/dev/null 2>&1; then
            echo "FAIL: Valkey not reachable at ${VALKEY_HOST}:${VALKEY_PORT} (via redis-cli)" >&2
            exit 2
        fi
    else
        echo "WARN: neither valkey-cli nor redis-cli found; skipping Valkey ping preflight"
    fi
fi

if [[ "${BACKEND}" == "postgres" || "${BACKEND}" == "both" ]]; then
    if ! command -v pg_isready >/dev/null 2>&1; then
        echo "FAIL: pg_isready not on PATH — install postgresql-client" >&2
        exit 2
    fi
    # Extract host + port from the URL for pg_isready.
    # Format: postgres://user:pass@host:port/db
    PG_HOSTPORT="$(printf '%s\n' "${PG_URL}" \
        | sed -E 's|^[a-z]+://[^@]*@([^/]+).*$|\1|')"
    PG_HOST="${PG_HOSTPORT%%:*}"
    PG_PORT="${PG_HOSTPORT#*:}"
    if [[ "${PG_PORT}" == "${PG_HOSTPORT}" ]]; then
        PG_PORT=5432
    fi
    if ! pg_isready -h "${PG_HOST}" -p "${PG_PORT}" >/dev/null 2>&1; then
        echo "FAIL: Postgres not reachable at ${PG_HOST}:${PG_PORT}" >&2
        exit 2
    fi
fi

# 2. Apply Postgres migrations (skip if the migrate CLI isn't
# installed — operators running the smoke locally without sqlx-cli
# must ensure the schema is already up; the release-workflow job
# always has it.)
if [[ "${BACKEND}" == "postgres" || "${BACKEND}" == "both" ]]; then
    if command -v sqlx >/dev/null 2>&1; then
        echo "==> smoke-v0.7: applying Postgres migrations"
        (cd crates/ff-backend-postgres && DATABASE_URL="${PG_URL}" sqlx migrate run) \
            || { echo "FAIL: sqlx migrate run" >&2; exit 2; }
    else
        echo "WARN: sqlx CLI not found — assuming ff_smoke schema is already applied"
    fi
fi

# 3. Boot ff-server. Only needed when the Valkey leg is in scope;
# the Postgres leg drives the EngineBackend trait directly.
SERVER_PID=""
cleanup() {
    if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

if [[ "${BACKEND}" == "valkey" || "${BACKEND}" == "both" ]]; then
    # Pre-build ff-server with a long-lived cargo invocation so the
    # subsequent background launch only has to start the binary, not
    # compile it. v0.8.0 tag run (run 24909817974) failed because the
    # 30s /healthz window was consumed by a cold --release compile on
    # an uncached smoke-job runner. Compile here (no time limit),
    # then launch the pre-built binary below.
    echo "==> smoke-v0.7: pre-building ff-server (release)"
    cargo build -p ff-server --release --quiet
    # Resolve target dir without requiring python: cargo locate-project
    # gives workspace root; CARGO_TARGET_DIR env overrides if set.
    CARGO_TARGET_DIR_RESOLVED="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
    FF_SERVER_BIN="${CARGO_TARGET_DIR_RESOLVED}/release/ff-server"
    if [[ ! -x "${FF_SERVER_BIN}" ]]; then
        echo "FAIL: ff-server binary not found at ${FF_SERVER_BIN} after build" >&2
        exit 3
    fi

    echo "==> smoke-v0.7: launching ff-server (backgrounded)"
    FF_HMAC_SECRET_FALLBACK="$(openssl rand -hex 32 2>/dev/null || date +%s%N)"
    FF_WAITPOINT_HMAC_SECRET="${FF_WAITPOINT_HMAC_SECRET:-${FF_HMAC_SECRET_FALLBACK}}" \
    FF_HOST="${VALKEY_HOST}" \
    FF_PORT="${VALKEY_PORT}" \
    FF_LANES="${FF_LANES:-default}" \
    FF_PORT_HTTP="9090" \
        "${FF_SERVER_BIN}" \
        >/tmp/ff-smoke-server.log 2>&1 &
    SERVER_PID=$!

    # Wait up to 30s for /healthz to respond.
    for i in {1..60}; do
        if curl -sf "${SERVER_URL}/healthz" >/dev/null 2>&1; then
            break
        fi
        if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
            echo "FAIL: ff-server exited during startup — tail of log:" >&2
            tail -n 40 /tmp/ff-smoke-server.log >&2 || true
            exit 3
        fi
        sleep 0.5
    done
    if ! curl -sf "${SERVER_URL}/healthz" >/dev/null 2>&1; then
        echo "FAIL: ff-server /healthz never became healthy within 30s" >&2
        tail -n 40 /tmp/ff-smoke-server.log >&2 || true
        exit 3
    fi
    echo "    ff-server healthy (pid=${SERVER_PID})"
fi

# 4. Run the scenarios. The binary exits non-zero on any Fail /
# parity violation / --strict Skip.
echo "==> smoke-v0.7: running scenarios (backend=${BACKEND}, strict=${STRICT_FLAG:-off})"
FF_SMOKE_SERVER="${SERVER_URL}" \
FF_SMOKE_VALKEY_HOST="${VALKEY_HOST}" \
FF_SMOKE_VALKEY_PORT="${VALKEY_PORT}" \
FF_SMOKE_POSTGRES_URL="${PG_URL}" \
    cargo run --release --quiet \
        --manifest-path benches/Cargo.toml \
        -p ff-smoke-v07 -- \
        --backend "${BACKEND}" ${STRICT_FLAG}

echo "==> smoke-v0.7: PASS"
