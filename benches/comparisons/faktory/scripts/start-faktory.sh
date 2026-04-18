#!/usr/bin/env bash
# Spin up a local Faktory server for the comparison benchmarks.
#
# Faktory is NOT wired into the matrix CI because it needs its own
# long-lived daemon. Run this on your laptop before invoking either
# scenario:
#
#   ./scripts/start-faktory.sh        # start + health-check
#   cargo run --release --bin faktory-scenario1
#   cargo run --release --bin faktory-scenario3
#   docker stop ff-bench-faktory      # when done
#
# The server binds :7419 (protocol) and :7420 (web UI). No password,
# no TLS, no persistence — tuned for benchmarking, not production.
#
# OUT OF SCOPE: server-restart detection. The bench assumes the
# daemon is stable for the full duration of a run (including the
# 5-minute scenario 3). If Faktory crashes or is killed mid-run, the
# client will surface connect errors but the harness does not attempt
# reconnect or restart detection. Re-run after restarting the
# container.

set -euo pipefail

CONTAINER_NAME="${CONTAINER_NAME:-ff-bench-faktory}"
FAKTORY_IMAGE="${FAKTORY_IMAGE:-contribsys/faktory:latest}"
FAKTORY_PORT="${FAKTORY_PORT:-7419}"
WEB_PORT="${WEB_PORT:-7420}"

command -v docker >/dev/null 2>&1 || {
    echo "docker missing — install docker (or podman + alias) before running" >&2
    exit 1
}

# Stop any existing container with the same name so re-running is
# idempotent. Ignore errors when it doesn't exist.
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true

echo "[faktory] starting $FAKTORY_IMAGE on :$FAKTORY_PORT / :$WEB_PORT"
docker run -d \
    --name "$CONTAINER_NAME" \
    -p "$FAKTORY_PORT:7419" \
    -p "$WEB_PORT:7420" \
    -e FAKTORY_PASSWORD= \
    "$FAKTORY_IMAGE" \
    /faktory -b 0.0.0.0:7419 -w 0.0.0.0:7420 >/dev/null

# Wait up to 15s for the protocol — NOT just the TCP listener.
# TCP up != FAKTORY HI handshake ready; a half-initialized server
# yields "protocol error" from the client crate on connect. Poll the
# docker logs for the "Accepting connections" line Faktory emits
# once the protocol handler is live. Falls back to the TCP probe
# if we can't read logs for some reason.
for _ in $(seq 1 30); do
    if docker logs "$CONTAINER_NAME" 2>&1 | grep -q 'Accepting connections'; then
        echo "[faktory] ready on tcp://127.0.0.1:$FAKTORY_PORT"
        echo "[faktory] web UI: http://127.0.0.1:$WEB_PORT"
        exit 0
    fi
    # Belt + suspenders: some older faktory images don't log the
    # accept line verbatim, so fall back to a TCP probe as a
    # liveness proxy.
    if (echo > /dev/tcp/127.0.0.1/"$FAKTORY_PORT") >/dev/null 2>&1; then
        # Give it one more half-second for the HI handshake to arm
        # before declaring ready.
        sleep 0.5
        echo "[faktory] ready on tcp://127.0.0.1:$FAKTORY_PORT (TCP-probe fallback)"
        echo "[faktory] web UI: http://127.0.0.1:$WEB_PORT"
        exit 0
    fi
    sleep 0.5
done

echo "[faktory] FATAL: daemon did not accept connections within 15s" >&2
docker logs --tail=40 "$CONTAINER_NAME" >&2 || true
exit 1
