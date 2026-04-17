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

# Wait up to 15s for the TCP listener.
for _ in $(seq 1 30); do
    if (echo > /dev/tcp/127.0.0.1/"$FAKTORY_PORT") >/dev/null 2>&1; then
        echo "[faktory] ready on tcp://127.0.0.1:$FAKTORY_PORT"
        echo "[faktory] web UI: http://127.0.0.1:$WEB_PORT"
        exit 0
    fi
    sleep 0.5
done

echo "[faktory] FATAL: daemon did not accept connections within 15s" >&2
docker logs --tail=40 "$CONTAINER_NAME" >&2 || true
exit 1
