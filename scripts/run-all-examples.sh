#!/usr/bin/env bash
# run-all-examples.sh — phase 3a: build-clean gate.
#
# Walks every subdirectory of `examples/` that has a Cargo.toml and runs
# `cargo build --bins` in its own workspace. Reports PASS/FAIL per
# example and exits non-zero if any example failed to build.
#
# Live-run coverage (end-to-end scenarios, multi-bin HITL orchestration,
# LLM-dependent flows) lands in phases 3b–3d. LLM-dependent examples
# (coding-agent, llm-race) are intentionally out of CI scope — they're
# pre-release-local runs, since CI has no API keys and the point is
# real-provider smoke, not mocked fidelity.
#
# Usage:
#   scripts/run-all-examples.sh            # build all
#   FF_EXAMPLES_ONLY=ff-dev,token-budget scripts/run-all-examples.sh
#
# Exit codes:
#   0 — all selected examples built clean (or SKIPped with rationale)
#   1 — one or more examples failed to build

set -u
set -o pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXAMPLES_DIR="$ROOT/examples"

# Per-session opt-in filter. Empty = run everything.
ONLY="${FF_EXAMPLES_ONLY:-}"

# Skip rationale — case statement keeps the script portable to Bash 3.2
# (macOS default). Prints empty string for non-matches.
skip_reason() {
    case "$1" in
        grafana) echo "dashboard JSON only — no Cargo workspace" ;;
        *) echo "" ;;
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

echo "[run-all-examples] phase 3a — build-clean gate"
echo "[run-all-examples] root=$ROOT"
[ -n "$ONLY" ] && echo "[run-all-examples] FF_EXAMPLES_ONLY=$ONLY"
echo

# Single tempfile reused per iteration. `trap` ensures cleanup even on
# SIGINT/SIGTERM so we don't leave orphans in /tmp.
LOG_TMP="$(mktemp)"
trap 'rm -f "$LOG_TMP"' EXIT

for dir in "$EXAMPLES_DIR"/*/; do
    name="$(basename "$dir")"
    in_only "$name" || continue

    reason="$(skip_reason "$name")"
    if [ -n "$reason" ]; then
        echo "[SKIP] $name — $reason"
        SKIP+=("$name")
        continue
    fi

    if [ ! -f "$dir/Cargo.toml" ]; then
        echo "[SKIP] $name — no Cargo.toml"
        SKIP+=("$name")
        continue
    fi

    echo "[build] $name …"
    if (cd "$dir" && cargo build --bins) >"$LOG_TMP" 2>&1; then
        echo "[PASS] $name"
        PASS+=("$name")
    else
        echo "[FAIL] $name"
        # Surface the last 30 lines of cargo output so CI logs carry the
        # actual error without dumping the whole compile.
        echo "─── last 30 lines of cargo output ───"
        tail -n 30 "$LOG_TMP"
        echo "─── end ───"
        FAIL+=("$name")
    fi
    echo
done

echo "═══ summary ═══"
echo "PASS: ${#PASS[@]}  (${PASS[*]:-none})"
echo "SKIP: ${#SKIP[@]}  (${SKIP[*]:-none})"
echo "FAIL: ${#FAIL[@]}  (${FAIL[*]:-none})"

[ "${#FAIL[@]}" -eq 0 ]
