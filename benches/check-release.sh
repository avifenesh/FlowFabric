#!/usr/bin/env bash
# Release gate for FlowFabric benchmarks.
#
# Phase A behaviour (this commit):
#   1. Run scenario 1 (submit → claim → complete throughput) via criterion.
#   2. The criterion harness writes benches/results/<scenario>-<sha>.json.
#   3. If a prior-release snapshot exists under
#      benches/results/<prev_tag>/, compare the current run against it
#      with benches/scripts/check_release.py. FAIL on regressions >10%
#      throughput or >20% p99; WARN on >5% throughput or >10% p99.
#   4. If no prior snapshot exists, emit "no baseline — writing baseline"
#      and exit 0. The first-tagged release captures itself as the
#      baseline, no FAIL on bootstrap.
#
# Phase B (follow-up) will add scenarios 2-5; the loop at `for s in ...`
# is ready to absorb them once the binaries exist.
#
# Exit codes:
#   0  — green (or bootstrap)
#   1  — hard regression detected, release should be blocked
#   2  — harness / infrastructure failure (bench didn't run to completion)

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

: "${FF_BENCH_SERVER:=http://localhost:9090}"
: "${FF_BENCH_VALKEY_HOST:=localhost}"
: "${FF_BENCH_VALKEY_PORT:=6379}"

# Pre-flight: the version-bootstrap path parses `cargo metadata` via
# jq. jq is a hard dep for that one fallback — declare it up-front so
# the failure isn't a surprise three steps later.
for tool in curl jq python3; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "[release-gate] FAIL: required tool missing: ${tool}" >&2
        echo "[release-gate]   curl, jq, python3 must all be on PATH" >&2
        exit 2
    fi
done

# Pre-flight: server must be reachable. Failing here is infrastructure,
# not a regression — exit 2.
if ! curl -sS --max-time 2 "${FF_BENCH_SERVER}/healthz" > /dev/null; then
    echo "[release-gate] FAIL: ff-server not reachable at ${FF_BENCH_SERVER}" >&2
    echo "[release-gate] start it with:" >&2
    echo "  FF_WAITPOINT_HMAC_SECRET=\$(openssl rand -hex 32) \\" >&2
    echo "  FF_LANES=default,bench \\" >&2
    echo "    cargo run -p ff-server --release" >&2
    exit 2
fi

SHA="$(git rev-parse --short HEAD)"
PREV_TAG="$(git describe --tags --abbrev=0 --match 'v*.*.*' 2>/dev/null || true)"

echo "[release-gate] running benches at ${SHA} (previous release: ${PREV_TAG:-none})"

# Phase A scenarios. Each binary must write
#   benches/results/<scenario>-<sha>.json
# on success. Add Phase B scenarios to this list as they land.
SCENARIOS=(submit_claim_complete)

for s in "${SCENARIOS[@]}"; do
    echo "[release-gate] ==> ${s}"
    if ! (cd benches && cargo bench --bench "${s}" -- --test 2>&1 | tail -40); then
        echo "[release-gate] harness failure on ${s}" >&2
        exit 2
    fi
    # Full (non --test) run for the real numbers.
    if ! (cd benches && cargo bench --bench "${s}" 2>&1 | tail -5); then
        echo "[release-gate] harness failure on ${s} (full run)" >&2
        exit 2
    fi
done

# Compare against prior release.
if [[ -z "${PREV_TAG}" ]] || [[ ! -d "benches/results/${PREV_TAG}" ]]; then
    echo "[release-gate] no prior baseline at benches/results/${PREV_TAG:-<none>}"
    echo "[release-gate] first run — snapshotting current as baseline"
    # Prefer cargo metadata — it parses the manifest the same way the
    # release toolchain does, so we don't diverge if someone ever moves
    # `version` out of `[workspace.package]`, wraps it in a comment, or
    # reformats the TOML. ff-core is chosen as the canonical workspace
    # member (all product crates share the same workspace version).
    CURRENT_VERSION=""
    if command -v jq >/dev/null 2>&1; then
        CURRENT_VERSION="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
            | jq -r '.packages[] | select(.name == "ff-core") | .version')"
    fi
    if [[ -z "${CURRENT_VERSION:-}" ]]; then
        echo "[release-gate] WARN: could not resolve workspace version via cargo metadata + jq — using 'dev'"
        CURRENT_VERSION="dev"
    fi
    SNAPSHOT_DIR="benches/results/v${CURRENT_VERSION}"
    mkdir -p "${SNAPSHOT_DIR}"
    for f in benches/results/*-"${SHA}".json; do
        [[ -e "${f}" ]] || continue
        cp "${f}" "${SNAPSHOT_DIR}/"
    done
    echo "[release-gate] wrote baseline to ${SNAPSHOT_DIR}/"
    exit 0
fi

python3 "${HERE}/scripts/check_release.py" \
    --current-dir benches/results \
    --current-sha "${SHA}" \
    --baseline-dir "benches/results/${PREV_TAG}" \
    --scenarios "${SCENARIOS[@]}"
RC=$?

if [[ "${RC}" -ne 0 ]]; then
    echo "[release-gate] comparator exited ${RC} — regressions blocked this release" >&2
    exit 1
fi

# Verify COMPARISON.md has the current sha in the header. Missing-entry
# check emits WARN; the MD is filled incrementally as comparison systems
# implement each scenario.
if [[ -f benches/results/COMPARISON.md ]] && ! grep -q "${SHA}" benches/results/COMPARISON.md; then
    echo "[release-gate] WARN: COMPARISON.md does not reference ${SHA}"
    echo "                     — filling with at-least scenario 1 numbers is recommended"
fi

echo "[release-gate] GREEN"
exit 0
