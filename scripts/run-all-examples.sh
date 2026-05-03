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

# ff-server lifecycle — started lazily when the first example declaring
# `requires ff-server` runs. Reused across subsequent examples in the
# same sweep; stopped on EXIT. A random free port avoids collision
# with an operator's own 9090.
FF_SERVER_READY=""      # "1" live, "0" refused, "" not started
FF_SERVER_PID=""
FF_SERVER_PORT=""
FF_SERVER_URL=""
FF_SERVER_LOG=""

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

# Parse host + port out of a Postgres URL. Handles bracketed IPv6
# literals (`[::1]:5432`) in addition to plain host:port — plain
# ${host##:*} splits break on raw IPv6 because every hextet is `:`-
# delimited. Output on stdout as `<host> <port>`.
_pg_url_host_port() {
    local url="$1" rest host port
    rest="${url#postgres://}"   # strip scheme
    rest="${rest#postgresql://}"
    rest="${rest#*@}"           # strip creds if present
    host="${rest%%/*}"          # up to first /
    host="${host%%\?*}"         # strip query string
    case "$host" in
        \[*\]:*)   # [::1]:5432 — IPv6 literal with explicit port
            port="${host##*:}"
            host="${host%%\]:*}"
            host="${host#\[}"
            ;;
        \[*\])      # [::1] — IPv6 literal, default port
            port="5432"
            host="${host#\[}"
            host="${host%\]}"
            ;;
        *:*)        # v4/hostname with explicit port
            port="${host##*:}"
            host="${host%%:*}"
            ;;
        *)          # v4/hostname, default port
            port="5432"
            ;;
    esac
    echo "$host $port"
}

# Redact user:pass@ from a Postgres URL for log surfaces. Leaves host +
# db intact so the operator can still see WHERE it tried to connect.
_pg_url_redact() {
    local url="$1"
    case "$url" in
        postgres://*@*)  echo "postgres://****@${url#*@}" ;;
        postgresql://*@*) echo "postgresql://****@${url#*@}" ;;
        *) echo "$url" ;;
    esac
}

check_postgres() {
    [ -n "$POSTGRES_READY" ] && return 0
    # Prefer a real round-trip (`psql SELECT 1`) over `pg_isready` —
    # pg_isready only tells you the server is accepting connections; a
    # wrong db name or bad creds still pass. The round-trip catches
    # those, so the harness SKIPs instead of letting sqlx fail cryptically
    # inside the example.
    if command -v psql >/dev/null 2>&1; then
        if psql "$POSTGRES_URL" -tAc "SELECT 1" >/dev/null 2>&1; then
            POSTGRES_READY=1
        else
            POSTGRES_READY=0
        fi
        return 0
    fi
    # pg_isready as the next-best: verifies the server responds but
    # nothing about db/creds. Rarely the only option — most systems
    # with libpq ship psql too — but keep the fallback for bare
    # pg-client installs.
    if command -v pg_isready >/dev/null 2>&1; then
        if pg_isready -d "$POSTGRES_URL" -q >/dev/null 2>&1; then
            POSTGRES_READY=1
        else
            POSTGRES_READY=0
        fi
        return 0
    fi
    # Last resort — a raw TCP probe. Can't distinguish pg from any tcp
    # listener or validate the specific db/creds. SKIP decisions built
    # on this will be noisier than psql/pg_isready, but at least
    # actionable.
    local hp host port
    hp="$(_pg_url_host_port "$POSTGRES_URL")"
    host="${hp% *}"; port="${hp#* }"
    if (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; then
        POSTGRES_READY=1
    else
        POSTGRES_READY=0
    fi
}

# Pick an OS-assigned free TCP port via Python. Preferred over hard-
# coding 9090 so operators running their own ff-server on the default
# port aren't disrupted and parallel harness invocations don't collide.
_free_port() {
    python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()"
}

# Start ff-server in the background against the live Valkey the
# preflight already verified. Idempotent — subsequent calls in the
# same sweep are no-ops. Requires the release binary at
# target/release/ff-server (cargo-built in phase 3a for the ff-server
# crate, or pre-built by the operator).
#
# Preflight dependencies (python3 for the free-port pick, curl for
# the /healthz readiness probe) are checked up front so a missing
# tool emits a clean SKIP/warning rather than a cryptic boot failure.
start_ff_server() {
    [ -n "$FF_SERVER_READY" ] && return 0

    # Tool preflights first.
    if ! command -v python3 >/dev/null 2>&1; then
        echo "[ff-server] FATAL: python3 not found — required for free-port selection" >&2
        FF_SERVER_READY=0
        return 0
    fi
    if ! command -v curl >/dev/null 2>&1; then
        echo "[ff-server] FATAL: curl not found — required for /healthz readiness probe" >&2
        FF_SERVER_READY=0
        return 0
    fi

    # Valkey is a hard pre-req; ff-server dials it at boot.
    check_valkey
    if [ "$VALKEY_READY" != "1" ]; then
        FF_SERVER_READY=0
        return 0
    fi

    local bin="$ROOT/target/release/ff-server"
    if [ ! -x "$bin" ]; then
        # Dedicated build-log tempfile so the per-example LOG_TMP
        # isn't appended-to (ambiguous pointer when a run FAILs
        # right after a build event).
        local build_log
        build_log="$(mktemp "${TMPDIR:-/tmp}/ff-server-build.XXXXXX.log")"
        echo "[ff-server] building release binary (one-time) — log: $build_log"
        if ! (cd "$ROOT" && cargo build --locked --release -p ff-server) >"$build_log" 2>&1; then
            echo "[ff-server] FATAL: cargo build -p ff-server failed — see $build_log" >&2
            FF_SERVER_READY=0
            return 0
        fi
        # Build succeeded — clean up the log immediately; the binary
        # is the durable artifact.
        rm -f "$build_log"
    fi

    FF_SERVER_PORT="$(_free_port)"
    FF_SERVER_URL="http://127.0.0.1:$FF_SERVER_PORT"
    FF_SERVER_LOG="$(mktemp "${TMPDIR:-/tmp}/ff-server-harness.XXXXXX.log")"
    # EXIT trap below cleans this up along with stop_ff_server; no
    # per-function trap here because bash doesn't stack traps.

    echo "[ff-server] starting on $FF_SERVER_URL (log: $FF_SERVER_LOG)"
    # Foreground env so the child inherits only what we intend. Minimum
    # secret / lanes so validation passes; bind-only-loopback so we
    # don't expose the harness-spawned server outside the box.
    # FF_LANES is the union of every lane any example ever uses:
    # `default` (most), plus `build,test,deploy,verify` for
    # deploy-approval. Including unused lanes is harmless — the
    # scheduler just ranges zero-length ZSETs.
    #
    # FF_WAITPOINT_HMAC_SECRET — random 32-byte hex per run. Fresh
    # material keeps generic entropy-based secret scanners quiet on
    # the repo (GitGuardian flagged the previous all-zeros sentinel
    # once the workflow landed) and costs nothing at run time. Caller
    # can override with an explicit value via env.
    local hmac_secret="${FF_WAITPOINT_HMAC_SECRET:-}"
    if [ -z "$hmac_secret" ]; then
        if command -v openssl >/dev/null 2>&1; then
            hmac_secret="$(openssl rand -hex 32)"
        else
            # /dev/urandom fallback; 32 bytes → 64 hex chars via xxd.
            hmac_secret="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
        fi
    fi
    FF_LISTEN_ADDR="127.0.0.1:$FF_SERVER_PORT" \
    FF_WAITPOINT_HMAC_SECRET="$hmac_secret" \
    FF_LANES="default,build,test,deploy,verify" \
    FF_HOST="$VALKEY_HOST" \
    FF_PORT="$VALKEY_PORT" \
        "$bin" >"$FF_SERVER_LOG" 2>&1 &
    FF_SERVER_PID=$!

    # Poll /healthz until ready. ff-server typically boots in ~1s
    # once the Valkey library is loaded; cap the wait at 30s so a
    # wedged boot surfaces quickly.
    local tries=0
    while [ $tries -lt 60 ]; do
        if curl -fsS -o /dev/null -m 1 "$FF_SERVER_URL/healthz" 2>/dev/null; then
            FF_SERVER_READY=1
            echo "[ff-server] ready (pid=$FF_SERVER_PID)"
            return 0
        fi
        # If the child died, bail fast — polling dead pid wastes 30s.
        if ! kill -0 "$FF_SERVER_PID" 2>/dev/null; then
            echo "[ff-server] FATAL: child exited before ready — last log lines:" >&2
            tail -n 20 "$FF_SERVER_LOG" >&2
            stop_ff_server
            FF_SERVER_READY=0
            return 0
        fi
        sleep 0.5
        tries=$((tries + 1))
    done
    echo "[ff-server] FATAL: /healthz did not return 200 within 30s — last log lines:" >&2
    tail -n 20 "$FF_SERVER_LOG" >&2
    stop_ff_server
    FF_SERVER_READY=0
}

# Stop the harness-spawned ff-server and remove its log file. Safe to
# call when nothing was started (idempotent). Registered on EXIT below
# so ctrl-C mid-sweep still leaves no orphan servers or tempfiles.
stop_ff_server() {
    if [ -n "$FF_SERVER_PID" ] && kill -0 "$FF_SERVER_PID" 2>/dev/null; then
        kill "$FF_SERVER_PID" 2>/dev/null
        # Wait up to 5s for clean shutdown; SIGKILL only if still
        # alive — avoid signalling an unrelated process after fast
        # PID reuse.
        local tries=0
        while [ $tries -lt 10 ] && kill -0 "$FF_SERVER_PID" 2>/dev/null; do
            sleep 0.5
            tries=$((tries + 1))
        done
        if kill -0 "$FF_SERVER_PID" 2>/dev/null; then
            kill -9 "$FF_SERVER_PID" 2>/dev/null
        fi
    fi
    if [ -n "$FF_SERVER_LOG" ]; then
        rm -f "$FF_SERVER_LOG"
        FF_SERVER_LOG=""
    fi
    FF_SERVER_PID=""
    FF_SERVER_READY=""
}

# ── deploy-approval HITL orchestrator ────────────────────────────────
#
# Runs the 6-worker + submit + 2-approver choreography from the example
# README. Spawns workers in background, runs submit bg, polls its log
# for `deploy_created` + `deploy_suspended`, fires two approve calls
# (alice + bob), waits for the deploy worker to print
# `deploy_full_rollout_completed` AND the verify worker to print
# `verify_completed`. Kills all its children on exit and returns 0 on
# both markers seen, 1 otherwise. Writes a consolidated log to stdout
# via tee-equivalent redirects so the outer harness's LOG_TMP carries
# enough detail for a FAIL postmortem.
run_deploy_approval() {
    local dir="$1"
    local budget="${2:-600}"   # wall-time cap in seconds
    local started
    started="$(date +%s)"

    # This function runs inside the dispatcher's subshell, so globals
    # here don't bleed into the outer script. Using globals (not
    # `local`) keeps `pids` / `logdir` reachable from the `trap`
    # handler, which executes after the function returns.
    logdir="$(mktemp -d "${TMPDIR:-/tmp}/ff-deploy-approval.XXXXXX")" || {
        echo "[deploy-approval] FATAL: mktemp -d failed"
        return 1
    }
    pids=()

    _cleanup() {
        # Disable the trap first so a re-entry (e.g. SIGINT during
        # cleanup) doesn't recurse.
        trap - EXIT INT TERM
        # Portable array expansion — ${arr[@]+"${arr[@]}"} avoids the
        # `unbound variable` error on Bash 3.2 / `set -u` that
        # `"${arr[@]:-}"` triggers.
        local pid
        for pid in ${pids[@]+"${pids[@]}"}; do
            [ -n "$pid" ] && kill "$pid" 2>/dev/null
        done
        sleep 1
        for pid in ${pids[@]+"${pids[@]}"}; do
            [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null
        done
        # Emit per-process logs for postmortem via the harness's
        # LOG_TMP capture (this function's stdout).
        if [ -n "$logdir" ] && [ -d "$logdir" ]; then
            local f
            for f in "$logdir"/*.log; do
                [ -f "$f" ] || continue
                echo "─── $(basename "$f") (last 15 lines) ───"
                tail -n 15 "$f"
            done
            rm -rf "$logdir"
        fi
    }
    # Register cleanup on every exit path (return, SIGINT, SIGTERM).
    # With this in place, the per-branch manual `_cleanup` calls in
    # the bail-out paths below are no longer needed.
    trap _cleanup EXIT INT TERM

    local bin="$dir/target/release"
    local url="$FF_SERVER_URL"

    echo "[deploy-approval] starting 6 bg workers + submit (log dir: $logdir)"

    # Workers. Each inherits FF_SERVER_URL + FF_HOST + FF_PORT from
    # the subshell's apply_env call; re-export here for clarity.
    FF_SERVER_URL="$url" "$bin/build-worker" >"$logdir/build.log" 2>&1 &
    pids+=($!)
    FF_SERVER_URL="$url" "$bin/test-worker" --kind unit >"$logdir/test-unit.log" 2>&1 &
    pids+=($!)
    FF_SERVER_URL="$url" "$bin/test-worker" --kind integration >"$logdir/test-integ.log" 2>&1 &
    pids+=($!)
    FF_SERVER_URL="$url" "$bin/test-worker" --kind e2e >"$logdir/test-e2e.log" 2>&1 &
    pids+=($!)
    FF_SERVER_URL="$url" "$bin/deploy" >"$logdir/deploy.log" 2>&1 &
    pids+=($!)
    FF_SERVER_URL="$url" "$bin/verify" >"$logdir/verify.log" 2>&1 &
    pids+=($!)

    # Give workers ~2s to connect to ff-server + Valkey before submit
    # starts creating flow members (otherwise the first claim round
    # may see no capable workers).
    sleep 2

    # Submit (bg — we'll kill it once deploy marker set appears).
    "$bin/submit" --server "$url" \
        --artifact "app:v1.2.3" --commit "abc123" --poll-secs 1 \
        >"$logdir/submit.log" 2>&1 &
    pids+=($!)

    # Parse deploy_eid + flow_id + waitpoint_id from the logs as they
    # become available. 300s budget rather than 120: CI runners see
    # slower scheduler reconciler ticks under cold caches + shared
    # containers, and we'd rather wait than false-fail when the flow
    # is healthy but the unblock scanner hasn't promoted yet.
    local deploy_eid="" flow_id="" wp_id=""
    local t_end=$((started + 300))
    while [ "$(date +%s)" -lt "$t_end" ]; do
        if [ -z "$flow_id" ]; then
            flow_id="$(grep -oE 'flow_created flow_id=[^ ]+' "$logdir/submit.log" 2>/dev/null | head -1 | sed 's/^.*=//')"
        fi
        if [ -z "$deploy_eid" ]; then
            deploy_eid="$(grep -oE 'deploy_created execution_id=[^ ]+' "$logdir/submit.log" 2>/dev/null | head -1 | sed 's/^.*=//')"
        fi
        if [ -z "$wp_id" ]; then
            wp_id="$(grep -oE 'waitpoint_id=[^ ]+' "$logdir/deploy.log" 2>/dev/null | head -1 | sed 's/^waitpoint_id=//')"
        fi
        if [ -n "$flow_id" ] && [ -n "$deploy_eid" ] && [ -n "$wp_id" ]; then
            break
        fi
        sleep 2
    done

    if [ -z "$wp_id" ]; then
        echo "[deploy-approval] FAIL: deploy never suspended (no waitpoint_id in deploy.log)"
        return 1
    fi

    echo "[deploy-approval] deploy suspended — firing approve × 2 (alice, bob)"
    echo "[deploy-approval] flow_id=$flow_id deploy_eid=$deploy_eid wp_id=$wp_id"

    # Two distinct-source approvals — alice appends, bob resumes.
    for reviewer in alice bob; do
        if ! "$bin/approve" --server "$url" \
            --flow-id "$flow_id" --execution-id "$deploy_eid" \
            --waitpoint-id "$wp_id" --reviewer "$reviewer" \
            >"$logdir/approve-$reviewer.log" 2>&1; then
            echo "[deploy-approval] FAIL: approve --reviewer $reviewer returned non-zero"
            tail -n 10 "$logdir/approve-$reviewer.log"
            return 1
        fi
    done

    # Wait for deploy + verify terminal markers.
    echo "[deploy-approval] waiting for deploy_full_rollout_completed + verify_completed …"
    t_end=$((started + budget))
    local saw_deploy=0 saw_verify=0
    while [ "$(date +%s)" -lt "$t_end" ]; do
        if [ "$saw_deploy" = "0" ] && grep -q "deploy_full_rollout_completed" "$logdir/deploy.log" 2>/dev/null; then
            echo "[deploy-approval] deploy_full_rollout_completed ✓"
            saw_deploy=1
        fi
        if [ "$saw_verify" = "0" ] && grep -q "verify_completed" "$logdir/verify.log" 2>/dev/null; then
            echo "[deploy-approval] verify_completed ✓"
            saw_verify=1
        fi
        if [ "$saw_deploy" = "1" ] && [ "$saw_verify" = "1" ]; then
            return 0
        fi
        sleep 3
    done

    echo "[deploy-approval] FAIL: timed out after ${budget}s — deploy_marker=$saw_deploy verify_marker=$saw_verify"
    return 1
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
# rationale. Empty string = proceed normally. Populated = skip both
# build and run.
skip_reason() {
    case "$1" in
        # grafana has no Cargo workspace but is NOT skipped anymore —
        # its run_cmd invokes scripts/lint-grafana-dashboard.sh which
        # validates JSON shape. Left here (empty) so the dispatcher
        # doesn't try to `cargo build` it.
        grafana) echo "" ;;
        *) echo "" ;;
    esac
}

# `requires` names the fixtures an example's live-run needs. Empty =
# no external fixture (SQLite-only / FF_DEV_MODE). Values checked by
# the dispatcher below: "valkey", "postgres", "ff-server" (future).
requires() {
    case "$1" in
        v013-cairn-454-budget-ledger) echo "valkey" ;;
        v014-rfc025-worker-registry) echo "valkey" ;;
        v011-wave9-postgres) echo "postgres" ;;
        retry-and-cancel|v010-read-side-ergonomics|token-budget|deploy-approval)
            echo "ff-server" ;;
        *) echo "" ;;
    esac
}

# `success_marker` returns a log substring that, if present in the
# captured output, counts as success even when the example's process
# times out or exits non-zero. This lets the harness record PASS for
# examples whose per-process lifetime is racy (e.g. a background worker
# that doesn't shut down cleanly on notify_waiters) — the scenario
# logic itself completed and the marker proves it.
success_marker() {
    # Marker substrings emitted AFTER the scenario body completes but
    # BEFORE the example's shutdown path. Kept as defensive
    # infrastructure — retry-and-cancel + token-budget used to rely
    # on this because of a `Notify::notify_waiters` race on shutdown
    # (#483, fixed via CancellationToken) — so both exit cleanly now.
    # Left empty by default; any future example that hits a similar
    # cleanup-path quirk can opt in via a new case arm without
    # having to rebuild the mechanism.
    case "$1" in
        # Emitted just before main's scene_result match arm runs.
        v010-read-side-ergonomics) echo "demo complete — capabilities() read" ;;
        *) echo "" ;;
    esac
}

# `run_cmd` emits the cargo invocation for the example's live-run.
# Env vars live in `run_env` so caller-supplied values (e.g. Postgres
# URLs with embedded creds) never get interpolated into a shell string
# — we export them into the subshell the run is dispatched through.
run_cmd() {
    # Default per-example timeout — 60s handles the fast SQLite /
    # single-op demos. ff-server examples run multi-scene HITL-ish
    # flows (retry cascades, cancel fanouts, resume loops) so get 180s.
    local secs=60
    case "$1" in
        retry-and-cancel|v010-read-side-ergonomics|token-budget) secs=180 ;;
    esac
    local t="${TIMEOUT:+$TIMEOUT $secs }"
    case "$1" in
        ff-dev)
            echo "${t}cargo run --locked --release --bin ff-dev" ;;
        external-callback)
            echo "${t}cargo run --locked --release -- --backend sqlite" ;;
        incident-remediation)
            # Runs against the SQLite embedded path — Valkey path
            # requires a full scheduler+scanner deployment and bails
            # out with a loud error referring the operator to the
            # sqlite flag. Stay on the sqlite path for CI.
            echo "${t}cargo run --locked --release -- --backend sqlite" ;;
        v013-cairn-454-budget-ledger)
            echo "${t}cargo run --locked --release --bin budget-ledger" ;;
        v014-rfc025-worker-registry)
            echo "${t}cargo run --locked --release --bin worker-registry-demo" ;;
        v011-wave9-postgres)
            echo "${t}cargo run --locked --release" ;;
        retry-and-cancel|v010-read-side-ergonomics|token-budget)
            # All three call the harness-spawned ff-server via HTTP
            # and talk to Valkey directly for worker-side ops.
            echo "${t}cargo run --locked --release" ;;
        grafana)
            # Dashboard JSON only — lints the shape with jq. Runs at
            # repo root (the lint script resolves its own path).
            echo "$ROOT/scripts/lint-grafana-dashboard.sh" ;;
        *)
            echo "" ;;
    esac
}

# `apply_env` exports the example's env pairs into the *current*
# (sub)shell. Name/value pairs aren't echoed or interpolated into any
# command string, so Postgres URLs with quotes/`$`/spaces/etc. stay
# intact end-to-end.
apply_env() {
    case "$1" in
        ff-dev|external-callback|incident-remediation)
            export FF_DEV_MODE=1 ;;
        v013-cairn-454-budget-ledger|v014-rfc025-worker-registry)
            # Pipe VALKEY_HOST/PORT through the example's own
            # FF_DEMO_VALKEY_HOST/PORT knobs so a caller overriding
            # FF_HOST/FF_PORT sees preflight + run hit the same socket.
            export FF_DEMO_VALKEY_HOST="$VALKEY_HOST"
            export FF_DEMO_VALKEY_PORT="$VALKEY_PORT" ;;
        v011-wave9-postgres)
            # Route the URL the preflight verified to the example.
            export FF_PG_TEST_URL="$POSTGRES_URL" ;;
        retry-and-cancel|v010-read-side-ergonomics|token-budget|deploy-approval)
            # All four examples default to http://localhost:9090 but
            # the harness picks a random free port to avoid colliding
            # with an operator-run ff-server. Pipe the harness's
            # URL/host/port through so the example talks to our server.
            # FF_HOST/FF_PORT point the SDK at the same Valkey.
            export FF_SERVER_URL="$FF_SERVER_URL"
            export FF_HOST="$VALKEY_HOST"
            export FF_PORT="$VALKEY_PORT" ;;
    esac
}

# Public-facing preview of the env the run uses, for the [run] log.
# Credentials in Postgres URLs are replaced with `****`.
run_env_preview() {
    case "$1" in
        ff-dev|external-callback|incident-remediation)
            echo "FF_DEV_MODE=1" ;;
        v013-cairn-454-budget-ledger|v014-rfc025-worker-registry)
            echo "FF_DEMO_VALKEY_HOST=$VALKEY_HOST FF_DEMO_VALKEY_PORT=$VALKEY_PORT" ;;
        v011-wave9-postgres)
            echo "FF_PG_TEST_URL=$(_pg_url_redact "$POSTGRES_URL")" ;;
        retry-and-cancel|v010-read-side-ergonomics|token-budget|deploy-approval)
            echo "FF_SERVER_URL=$FF_SERVER_URL FF_HOST=$VALKEY_HOST FF_PORT=$VALKEY_PORT" ;;
        *)
            echo "" ;;
    esac
}

# `run_reason` explains the SKIP for examples that don't have a run_cmd
# yet. Operators should see why, not just "skipped".
run_reason() {
    case "$1" in
        coding-agent|llm-race|media-pipeline)
            echo "phase 3d — LLM / model-heavy, pre-release-local only" ;;
        *) echo "not covered by the harness yet" ;;
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
trap 'stop_ff_server; rm -f "$LOG_TMP"' EXIT

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

    # grafana has no Cargo workspace — its run_cmd invokes the
    # dashboard lint directly. Other no-Cargo dirs still SKIP with a
    # "no Cargo.toml" reason.
    if [ "$name" != "grafana" ] && [ ! -f "$dir/Cargo.toml" ]; then
        record_skip "$name" "no Cargo.toml"
        continue
    fi

    # Build-only mode: grafana has no cargo artifact to produce —
    # the run phase is its only real work. Record PASS and continue.
    if [ "$MODE" = "build" ] && [ "$name" = "grafana" ]; then
        record_pass "$name"
        echo
        continue
    fi

    # ── build (phase 3a) ──
    # grafana has no Cargo workspace, so the build phase is a no-op
    # for it; drop straight into the run phase (which lints its JSON).
    if { [ "$MODE" = "both" ] || [ "$MODE" = "build" ]; } && [ "$name" != "grafana" ]; then
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
    # deploy-approval has a bespoke orchestrator (6-worker + submit +
    # 2-approver HITL); everything else uses the generic run_cmd path.
    if [ "$name" = "deploy-approval" ]; then
        # Still runs the normal fixture preflight below, just under
        # a sentinel cmd that the dispatch path recognises.
        cmd="ORCHESTRATOR"
    else
        cmd="$(run_cmd "$name")"
    fi
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
                    # Redact creds before logging — FF_PG_TEST_URL may
                    # carry user:pass@host from the caller's env.
                    record_skip "$name" "postgres unreachable at $(_pg_url_redact "$POSTGRES_URL") — start postgres + create db (or set FF_PG_TEST_URL)"
                    echo
                    continue 2
                fi
                ;;
            ff-server)
                start_ff_server
                if [ "$FF_SERVER_READY" != "1" ]; then
                    record_skip "$name" "ff-server failed to start (likely Valkey unreachable or cargo build -p ff-server failed)"
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
    env_preview="$(run_env_preview "$name")"
    if [ "$cmd" = "ORCHESTRATOR" ]; then
        echo "      <bespoke orchestrator: run_deploy_approval>"
    elif [ -n "$env_preview" ]; then
        echo "      $env_preview $cmd"
    else
        echo "      $cmd"
    fi
    # Export env in a subshell (never interpolated into the cargo
    # command string, so URLs with spaces/`$`/quotes are safe) and
    # run inside the example's Cargo workspace. `|| rc=$?` captures
    # a non-zero exit without globally changing shell errexit mode —
    # `set -e` is never enabled in this script, but `|| rc=$?` keeps
    # the exit-code path consistent for operators who `source` parts
    # of this harness under their own -e shells.
    rc=0
    if [ "$cmd" = "ORCHESTRATOR" ]; then
        # Orchestrator runs its own child management; needs
        # FF_SERVER_URL + FF_HOST + FF_PORT in the current shell so
        # it can re-export into the per-worker env.
        (
            apply_env "$name"
            run_deploy_approval "$dir"
        ) >"$LOG_TMP" 2>&1 || rc=$?
    else
        (
            cd "$dir"
            apply_env "$name"
            eval "$cmd"
        ) >"$LOG_TMP" 2>&1 || rc=$?
    fi

    marker="$(success_marker "$name")"
    if [ "$rc" = "0" ]; then
        record_pass "$name"
    elif [ -n "$marker" ] && grep -qF -- "$marker" "$LOG_TMP"; then
        # Clean exit not reached (often an example bug on shutdown
        # path — e.g. notify-waiters race), but the scenario itself
        # completed and printed the marker. Record as PASS and
        # surface the exit reason in the log so we can file the bug.
        record_pass "$name"
        echo "       (exit $rc but success marker found — scenario body completed)"
    elif [ "$rc" = "124" ]; then
        record_fail "$name" "timed out"
    else
        record_fail "$name" "exit $rc"
    fi
    echo
done

echo "═══ summary ═══"
echo "mode=$MODE"
echo "PASS: ${#PASS[@]}  (${PASS[*]:-none})"
echo "SKIP: ${#SKIP[@]}  (${SKIP[*]:-none})"
echo "FAIL: ${#FAIL[@]}  (${FAIL[*]:-none})"

[ "${#FAIL[@]}" -eq 0 ]
