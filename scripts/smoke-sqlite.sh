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

echo "==> smoke-sqlite: verify production guard refuses without FF_DEV_MODE"
# Scrub FF_DEV_MODE from the environment for this leg even if the
# caller set it, so the guard regression test is deterministic.
if (
    cd examples/ff-dev
    unset FF_DEV_MODE
    cargo run --quiet --bin ff-dev 2>/dev/null
) ; then
    echo "FAIL: ff-dev exited 0 without FF_DEV_MODE=1 — production guard regressed" >&2
    exit 2
fi

echo "==> smoke-sqlite: run ff-dev end-to-end (FF_DEV_MODE=1)"
OUT="$(mktemp -t ff-dev-smoke.XXXXXX)"
trap 'rm -f "${OUT}"' EXIT

if ! (cd examples/ff-dev && FF_DEV_MODE=1 cargo run --quiet --bin ff-dev) \
        > "${OUT}" 2>&1 ; then
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

END_NS="$(date +%s%N)"
ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
echo "==> smoke-sqlite: PASS (${ELAPSED_MS} ms wall-clock)"
