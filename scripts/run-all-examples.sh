#!/usr/bin/env bash
# run-all-examples.sh — mechanical harness for the CLAUDE.md §5 item 3
# pre-tag release check.
#
# Phases:
#   3a: build-clean gate (cargo build --locked --bins per example)
#   3b: single-command live-runs for SQLite-only / FF_DEV_MODE examples
#   3c: HITL / multi-bin orchestration (deploy-approval, media-pipeline, ...)
#   3d: LLM-dependent examples (pre-release-local only — out of CI scope,
#       CI has no provider keys and mocking defeats real-fidelity)
#   3e: CI integration
#   3f: grafana dashboard JSON validation
#
# Usage:
#   scripts/run-all-examples.sh                    # build + run everything reachable
#   scripts/run-all-examples.sh --build-only       # phase 3a only
#   scripts/run-all-examples.sh --run-only         # phase 3b only (assumes a prior build)
#   FF_EXAMPLES_ONLY=ff-dev,token-budget scripts/run-all-examples.sh
#
# Exit codes:
#   0 — all selected examples passed (or SKIPped with rationale)
#   1 — one or more examples failed
#   2 — environment / preflight fault

set -u
set -o pipefail
shopt -s nullglob

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXAMPLES_DIR="$ROOT/examples"

if [ ! -d "$EXAMPLES_DIR" ]; then
    echo "[run-all-examples] FATAL: $EXAMPLES_DIR does not exist" >&2
    exit 2
fi

# Resolve `timeout` portably — macOS/BSD ships `gtimeout` from
# coreutils. If neither is available, emit FATAL rather than letting
# the run phase fail cryptically with exit 127.
if command -v timeout >/dev/null 2>&1; then
    TIMEOUT=timeout
elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT=gtimeout
else
    TIMEOUT=""
fi

# Per-session opt-in filter. Empty = run everything.
ONLY="${FF_EXAMPLES_ONLY:-}"
MODE="both"   # both | build | run

for arg in "$@"; do
    case "$arg" in
        --build-only) MODE="build" ;;
        --run-only) MODE="run" ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

# ── per-example metadata ───────────────────────────────────────────────
#
# `skip_reason` short-circuits the build+run pair with a stable
# rationale (grafana has no Cargo workspace).
skip_reason() {
    case "$1" in
        grafana) echo "dashboard JSON only — no Cargo workspace" ;;
        *) echo "" ;;
    esac
}

# `run_cmd` emits the command-line for the example's live-run, or an
# empty string for examples not yet covered in phase 3b (3c+ lands the
# HITL orchestration + ff-server-dependent ones).
run_cmd() {
    local t="${TIMEOUT:+$TIMEOUT 60 }"
    case "$1" in
        ff-dev)
            echo "FF_DEV_MODE=1 ${t}cargo run --locked --release --bin ff-dev" ;;
        external-callback)
            echo "FF_DEV_MODE=1 ${t}cargo run --locked --release -- --backend sqlite" ;;
        *)
            echo "" ;;
    esac
}

# `run_reason` explains the SKIP for examples that don't have a run_cmd
# yet. Operators should see why, not just "skipped".
run_reason() {
    case "$1" in
        coding-agent) echo "phase 3d — LLM-dependent, pre-release-local only" ;;
        llm-race) echo "phase 3d — LLM-dependent, pre-release-local only" ;;
        deploy-approval) echo "phase 3c — HITL multi-bin orchestration pending" ;;
        media-pipeline) echo "phase 3c — HITL multi-bin orchestration pending" ;;
        incident-remediation) echo "phase 3c — requires ff-server choreography" ;;
        retry-and-cancel) echo "phase 3c — requires ff-server" ;;
        token-budget) echo "phase 3c — requires ff-server" ;;
        v010-read-side-ergonomics) echo "phase 3c — requires ff-server" ;;
        v011-wave9-postgres) echo "phase 3c — requires ff-server + Postgres choreography" ;;
        v013-cairn-454-budget-ledger) echo "phase 3c — requires Valkey (trait-direct, no ff-server)" ;;
        *) echo "phase 3b does not cover this example yet" ;;
    esac
}

declare -a PASS=()
declare -a FAIL=()
declare -a SKIP=()

in_only() {
    local name="$1"
    [ -z "$ONLY" ] && return 0
    local IFS=","
    for want in $ONLY; do
        [ "$want" = "$name" ] && return 0
    done
    return 1
}

echo "[run-all-examples] mode=$MODE"
echo "[run-all-examples] root=$ROOT"
[ -n "$ONLY" ] && echo "[run-all-examples] FF_EXAMPLES_ONLY=$ONLY"
if [ -z "$TIMEOUT" ] && { [ "$MODE" = "both" ] || [ "$MODE" = "run" ]; }; then
    echo "[run-all-examples] WARN: neither 'timeout' nor 'gtimeout' found; \
live-runs will not be time-bounded. On macOS install coreutils \
(brew install coreutils) to get gtimeout." >&2
fi
echo

# Single tempfile reused per iteration. `trap` ensures cleanup even on
# SIGINT/SIGTERM. The template argument is required on macOS/BSD mktemp.
LOG_TMP="$(mktemp "${TMPDIR:-/tmp}/ff-run-all-examples.XXXXXX")"
trap 'rm -f "$LOG_TMP"' EXIT

record_pass() { echo "[PASS] $1"; PASS+=("$1"); }
record_fail() {
    echo "[FAIL] $1 — $2"
    echo "─── last 30 lines of output ───"
    tail -n 30 "$LOG_TMP"
    echo "─── end ───"
    FAIL+=("$1")
}
record_skip() { echo "[SKIP] $1 — $2"; SKIP+=("$1"); }

for dir in "$EXAMPLES_DIR"/*/; do
    name="$(basename "$dir")"
    in_only "$name" || continue

    reason="$(skip_reason "$name")"
    if [ -n "$reason" ]; then
        record_skip "$name" "$reason"
        continue
    fi

    if [ ! -f "$dir/Cargo.toml" ]; then
        record_skip "$name" "no Cargo.toml"
        continue
    fi

    # ── build (phase 3a) ──
    if [ "$MODE" = "both" ] || [ "$MODE" = "build" ]; then
        echo "[build] $name …"
        # --release matches what the phase-3b run commands use below, so
        # the default `both` mode compiles each example exactly once
        # instead of debug-then-release. Callers who want a faster debug-
        # only build-check can add `--build-only` + set the profile
        # themselves (future; not exposed today since 3b + 3c all use
        # release for realistic runtime behaviour).
        if ! (cd "$dir" && cargo build --locked --release --bins) >"$LOG_TMP" 2>&1; then
            record_fail "$name" "cargo build failed"
            echo
            continue
        fi
        # If build-only, succeed here.
        if [ "$MODE" = "build" ]; then
            record_pass "$name"
            echo
            continue
        fi
    fi

    # ── run (phase 3b) ──
    cmd="$(run_cmd "$name")"
    if [ -z "$cmd" ]; then
        record_skip "$name" "$(run_reason "$name")"
        echo
        continue
    fi

    echo "[run] $name …"
    echo "      $cmd"
    # Run inside the example's Cargo workspace.
    if (cd "$dir" && bash -c "$cmd") >"$LOG_TMP" 2>&1; then
        record_pass "$name"
    else
        rc=$?
        if [ "$rc" = "124" ]; then
            record_fail "$name" "timed out"
        else
            record_fail "$name" "exit $rc"
        fi
    fi
    echo
done

echo "═══ summary ═══"
echo "mode=$MODE"
echo "PASS: ${#PASS[@]}  (${PASS[*]:-none})"
echo "SKIP: ${#SKIP[@]}  (${SKIP[*]:-none})"
echo "FAIL: ${#FAIL[@]}  (${FAIL[*]:-none})"

[ "${#FAIL[@]}" -eq 0 ]
