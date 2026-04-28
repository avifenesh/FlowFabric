#!/usr/bin/env bash
# scripts/smoke-sqlite.sh — RFC-023 Phase 4 smoke gate for the SQLite
# dev-only backend.
#
# Exit 0 iff the `ff-dev` example binary runs end-to-end against an
# in-memory SQLite backend. Non-zero exit blocks the v0.12.0 release
# per RFC-023 §9.
#
# Surface exercised (must match RFC-023 §4.3 parity commitment):
#   * SqliteBackend::new — FF_DEV_MODE=1 production guard + migration
#     apply on pool init
#   * create_flow + create_execution + add_execution_to_flow
#   * claim (through the scheduler-less dev-seed path) → complete
#   * Wave-9 admin: change_priority, cancel_execution
#   * Read model: read_execution_info
#   * RFC-019 subscribe_completion cursor-resume surface
#   * shutdown_prepare
#
# No external fixtures — the whole run is in-process against `:memory:`.
#
# Exit codes:
#   0 — smoke passed
#   1 — example failed (non-zero exit, unexpected output, etc.)
#   2 — guard regression: the example ran without FF_DEV_MODE=1
#
# Per feedback_smoke_before_release.md: this script MUST run green
# BEFORE `git tag v0.12.0`.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

START_NS="$(date +%s%N)"

# Pre-build the example so the guard-regression leg below can attribute
# a non-zero exit to the `FF_DEV_MODE` refusal path specifically — a
# compile break would otherwise also non-zero-exit `cargo run` and
# falsely green the guard check.
echo "==> smoke-sqlite: pre-build ff-dev"
(cd examples/ff-dev && cargo build --quiet --bin ff-dev)

FF_DEV_BIN="${REPO_ROOT}/examples/ff-dev/target/debug/ff-dev"
if [[ ! -x "${FF_DEV_BIN}" ]]; then
    echo "FAIL: ff-dev binary not found at ${FF_DEV_BIN} after build" >&2
    exit 1
fi

OUT="$(mktemp -t ff-dev-smoke.XXXXXX)"
GUARD_OUT="$(mktemp -t ff-dev-guard.XXXXXX)"
trap 'rm -f "${OUT}" "${GUARD_OUT}"' EXIT

echo "==> smoke-sqlite: verify production guard refuses without FF_DEV_MODE"
# Invoke the pre-built binary directly. Scrub `FF_DEV_MODE` from the
# child env even if the caller set it so the leg is deterministic.
#
# The guard leg asserts three independent conditions so a compile-fail
# / panic / link error cannot false-pass by producing empty output:
#   (1) binary exists + is executable (checked above)
#   (2) binary exits non-zero without FF_DEV_MODE=1
#   (3) stderr/stdout contains the exact refusal message
#
# Condition (3) is checked against a captured exit code (not chained
# inside an `if` with `set -e` semantics) so the "no output" case
# produces a DISTINCT failure mode from the "exit-zero" case.
set +e
env -u FF_DEV_MODE "${FF_DEV_BIN}" > "${GUARD_OUT}" 2>&1
GUARD_EXIT=$?
set -e
if [[ "${GUARD_EXIT}" -eq 0 ]]; then
    echo "FAIL: ff-dev exited 0 without FF_DEV_MODE=1 — production guard regressed" >&2
    tail -n 40 "${GUARD_OUT}" >&2 || true
    exit 2
fi
if [[ ! -s "${GUARD_OUT}" ]]; then
    echo "FAIL: ff-dev exited ${GUARD_EXIT} but produced no output — likely crash/link-error, not a clean refusal" >&2
    exit 2
fi
if ! grep -Fq "FF_DEV_MODE=1 is required" "${GUARD_OUT}"; then
    echo "FAIL: ff-dev exited ${GUARD_EXIT} with output but refusal message missing — guard regressed or refusal text changed" >&2
    tail -n 40 "${GUARD_OUT}" >&2 || true
    exit 2
fi

echo "==> smoke-sqlite: run ff-dev end-to-end (FF_DEV_MODE=1)"
if ! FF_DEV_MODE=1 "${FF_DEV_BIN}" > "${OUT}" 2>&1 ; then
    echo "FAIL: ff-dev example exited non-zero — tail of output:" >&2
    tail -n 40 "${OUT}" >&2 || true
    exit 1
fi

# Surface assertions — each line corresponds to a numbered RFC-023 §4.3
# parity-commitment surface. If a future refactor removes any of these
# log lines without moving the assertion, smoke fails loud so the
# coverage regression is visible at release time, not post-publish.
EXPECTED=(
    "FlowFabric SQLite backend active (FF_DEV_MODE=1)"
    "dialed SQLite dev backend"
    "sqlite_version"
    "create_flow"
    "claim → minted handle"
    "complete"
    "change_priority"
    "cancel_execution"
    "read_execution_info"
    "subscribe_completion attached"
    "ff-dev demo complete"
)

for needle in "${EXPECTED[@]}"; do
    if ! grep -Fq "${needle}" "${OUT}"; then
        echo "FAIL: expected substring not in ff-dev output: ${needle}" >&2
        echo "---- ff-dev output (tail) ----" >&2
        tail -n 40 "${OUT}" >&2 || true
        exit 1
    fi
done

# Surface the bundled SQLite version from the ff-dev log (RFC-023 §7.1
# floor: >= 3.35, enforced by ff-dev itself). Grep the first
# `sqlite_version` log line so CI logs show which SQLite the bundled
# build pulled in.
if SQLITE_VERSION_LINE="$(grep -F 'sqlite_version' "${OUT}" | head -n 1)"; then
    echo "==> smoke-sqlite: bundled ${SQLITE_VERSION_LINE}"
fi

END_NS="$(date +%s%N)"
ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
echo "==> smoke-sqlite: PASS (${ELAPSED_MS} ms wall-clock)"
