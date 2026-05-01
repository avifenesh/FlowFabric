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

# Examples that do NOT run in CI's build-clean pass — either because
# they have no Cargo.toml (grafana = dashboard JSON only) or their
# build is LLM-dependent in ways the offline build can still satisfy
# (coding-agent / llm-race build clean without any key, the SKIP is
# reserved for future live-run phases).
declare -A SKIP_REASON=(
    [grafana]="dashboard JSON only — no Cargo workspace"
)

# Per-session opt-in filter. Empty = run everything.
ONLY="${FF_EXAMPLES_ONLY:-}"

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

for dir in "$EXAMPLES_DIR"/*/; do
    name="$(basename "$dir")"
    in_only "$name" || continue

    if [ -n "${SKIP_REASON[$name]:-}" ]; then
        echo "[SKIP] $name — ${SKIP_REASON[$name]}"
        SKIP+=("$name")
        continue
    fi

    if [ ! -f "$dir/Cargo.toml" ]; then
        echo "[SKIP] $name — no Cargo.toml"
        SKIP+=("$name")
        continue
    fi

    echo "[build] $name …"
    log_tmp="$(mktemp)"
    if (cd "$dir" && cargo build --bins) >"$log_tmp" 2>&1; then
        echo "[PASS] $name"
        PASS+=("$name")
    else
        echo "[FAIL] $name"
        # Surface the last 30 lines of cargo output so CI logs carry the
        # actual error without dumping the whole compile.
        echo "─── last 30 lines of cargo output ───"
        tail -n 30 "$log_tmp"
        echo "─── end ───"
        FAIL+=("$name")
    fi
    rm -f "$log_tmp"
    echo
done

echo "═══ summary ═══"
echo "PASS: ${#PASS[@]}  (${PASS[*]:-none})"
echo "SKIP: ${#SKIP[@]}  (${SKIP[*]:-none})"
echo "FAIL: ${#FAIL[@]}  (${FAIL[*]:-none})"

[ "${#FAIL[@]}" -eq 0 ]
