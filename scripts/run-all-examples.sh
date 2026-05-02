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

# ── fixture preflights ────────────────────────────────────────────────
#
# Each per-example case can flag `requires_<X>`. The preflight runs
# once per unique requirement, caches the result, and examples whose
# fixture is unreachable SKIP with an actionable "bring it up" message
# instead of failing cryptically inside the example.
VALKEY_READY=""         # "1" ready, "0" down, "" unchecked
VALKEY_HOST="${FF_HOST:-localhost}"
VALKEY_PORT="${FF_PORT:-6379}"

# Postgres. The harness default matches examples/v011-wave9-postgres's
# README snippet so `just connect via default` works out of the box
# when a local pg is up; callers point elsewhere via FF_PG_TEST_URL.
POSTGRES_READY=""       # "1" ready, "0" down, "" unchecked
POSTGRES_URL_DEFAULT="postgres://postgres:postgres@localhost:5432/ff_v011_demo"
POSTGRES_URL="${FF_PG_TEST_URL:-$POSTGRES_URL_DEFAULT}"

check_valkey() {
    [ -n "$VALKEY_READY" ] && return 0
    local probe
    if command -v valkey-cli >/dev/null 2>&1; then
        probe="valkey-cli"
    elif command -v redis-cli >/dev/null 2>&1; then
        probe="redis-cli"
    else
        # No CLI tool — use a RESP-level probe over bash's /dev/tcp.
        # Sends the inline `PING\r\n` command and checks for `+PONG`
        # in the reply, so an unrelated process listening on the port
        # is correctly reported as not-Valkey rather than a false
        # ready. All inside a subshell so the FD is released on exit.
        if (
            exec 3<>"/dev/tcp/$VALKEY_HOST/$VALKEY_PORT" 2>/dev/null || exit 1
            printf 'PING\r\n' >&3
            # Short read with a timeout so we don't hang on a silent
            # listener. `read -t` wants fractional seconds on Bash 4+;
            # pass an integer for 3.2 compat.
            IFS= read -r -t 2 reply <&3 || exit 2
            case "$reply" in *PONG*) exit 0 ;; *) exit 3 ;; esac
        ); then
            VALKEY_READY=1
        else
            VALKEY_READY=0
        fi
        return 0
    fi
    if "$probe" -h "$VALKEY_HOST" -p "$VALKEY_PORT" PING 2>/dev/null | grep -q PONG; then
        VALKEY_READY=1
    else
        VALKEY_READY=0
    fi
}

# Parse host + port out of `postgres://user:pass@host:port/db` — enough
# for the /dev/tcp fallback; sqlx itself handles the full parse when
# the example runs.
_pg_url_host_port() {
    local url="$1" rest host port
    rest="${url#postgres://}"   # strip scheme
    rest="${rest#postgresql://}"
    rest="${rest#*@}"           # strip creds if present
    host="${rest%%/*}"          # up to first /
    host="${host%%\?*}"         # strip query string
    case "$host" in
        *:*) port="${host##*:}"; host="${host%%:*}" ;;
        *)   port="5432" ;;
    esac
    echo "$host $port"
}

check_postgres() {
    [ -n "$POSTGRES_READY" ] && return 0
    # Prefer pg_isready (libpq-based, full connection verification).
    if command -v pg_isready >/dev/null 2>&1; then
        if pg_isready -d "$POSTGRES_URL" -q >/dev/null 2>&1; then
            POSTGRES_READY=1
        else
            POSTGRES_READY=0
        fi
        return 0
    fi
    # Fall back to psql -c "SELECT 1" if present.
    if command -v psql >/dev/null 2>&1; then
        if psql "$POSTGRES_URL" -tAc "SELECT 1" >/dev/null 2>&1; then
            POSTGRES_READY=1
        else
            POSTGRES_READY=0
        fi
        return 0
    fi
    # Neither available — fall back to a raw TCP probe. Imperfect
    # (can't distinguish pg from any tcp listener) but better than
    # nothing for a skip-vs-run decision.
    local hp host port
    hp="$(_pg_url_host_port "$POSTGRES_URL")"
    host="${hp% *}"; port="${hp#* }"
    if (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; then
        POSTGRES_READY=1
    else
        POSTGRES_READY=0
    fi
}

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

# `requires` names the fixtures an example's live-run needs. Empty =
# no external fixture (SQLite-only / FF_DEV_MODE). Values checked by
# the dispatcher below: "valkey", "postgres", "ff-server" (future).
requires() {
    case "$1" in
        v013-cairn-454-budget-ledger) echo "valkey" ;;
        v011-wave9-postgres) echo "postgres" ;;
        *) echo "" ;;
    esac
}

# `run_cmd` emits the command-line for the example's live-run, or an
# empty string for examples not yet covered.
run_cmd() {
    local t="${TIMEOUT:+$TIMEOUT 60 }"
    case "$1" in
        ff-dev)
            echo "FF_DEV_MODE=1 ${t}cargo run --locked --release --bin ff-dev" ;;
        external-callback)
            echo "FF_DEV_MODE=1 ${t}cargo run --locked --release -- --backend sqlite" ;;
        incident-remediation)
            # Runs against the SQLite embedded path — Valkey path
            # requires a full scheduler+scanner deployment and bails
            # out with a loud error referring the operator to the
            # sqlite flag. Stay on the sqlite path for CI.
            echo "FF_DEV_MODE=1 ${t}cargo run --locked --release -- --backend sqlite" ;;
        v013-cairn-454-budget-ledger)
            # Pipe VALKEY_HOST/PORT through the example's own
            # `FF_DEMO_VALKEY_HOST/PORT` knobs so a caller overriding
            # FF_HOST/FF_PORT sees preflight and run hit the *same*
            # socket. Without this, `FF_HOST=remote` would preflight
            # against remote but run against localhost.
            echo "FF_DEMO_VALKEY_HOST='$VALKEY_HOST' FF_DEMO_VALKEY_PORT='$VALKEY_PORT' ${t}cargo run --locked --release --bin budget-ledger" ;;
        v011-wave9-postgres)
            # Same preflight/run URL so a caller-overridden
            # FF_PG_TEST_URL targets the same endpoint the preflight
            # checked. The example runs `apply_migrations` at boot so
            # any Postgres 16+ db works out of the box.
            echo "FF_PG_TEST_URL='$POSTGRES_URL' ${t}cargo run --locked --release" ;;
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
        retry-and-cancel) echo "phase 3c — requires ff-server" ;;
        token-budget) echo "phase 3c — requires ff-server" ;;
        v010-read-side-ergonomics) echo "phase 3c — requires ff-server" ;;
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

    # ── run (phase 3b/3c) ──
    cmd="$(run_cmd "$name")"
    if [ -z "$cmd" ]; then
        record_skip "$name" "$(run_reason "$name")"
        echo
        continue
    fi

    # Fixture preflight — skip-not-fail when the required service
    # isn't reachable so operators running without (e.g.) Valkey get
    # a clear "bring it up" signal instead of a cryptic example
    # failure deep inside the ferriskey handshake.
    req="$(requires "$name")"
    for dep in $req; do
        case "$dep" in
            valkey)
                check_valkey
                if [ "$VALKEY_READY" != "1" ]; then
                    record_skip "$name" "valkey unreachable at $VALKEY_HOST:$VALKEY_PORT — start valkey-server and re-run"
                    echo
                    continue 2
                fi
                ;;
            postgres)
                check_postgres
                if [ "$POSTGRES_READY" != "1" ]; then
                    record_skip "$name" "postgres unreachable at $POSTGRES_URL — start postgres + create db (or set FF_PG_TEST_URL)"
                    echo
                    continue 2
                fi
                ;;
            *)
                record_skip "$name" "unknown requires: $dep"
                echo
                continue 2
                ;;
        esac
    done

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
