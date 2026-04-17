#!/usr/bin/env bash
# Wait for 6 valkey nodes on 127.0.0.1:7000-7005 to accept PING, then
# slot them into a 3-master 3-replica cluster. Idempotent on re-invoke:
# `valkey-cli --cluster create` returns non-zero if the cluster is
# already formed, so we probe `CLUSTER INFO` first and bail early if
# `cluster_state:ok` is already true.
set -euo pipefail

HOST="127.0.0.1"
NODES=(7000 7001 7002 7003 7004 7005)

pick_cli() {
    if command -v valkey-cli >/dev/null 2>&1; then
        echo "valkey-cli"
    elif command -v redis-cli >/dev/null 2>&1; then
        # Valkey 7.2+ is wire-compatible with redis-cli 7; cluster
        # subcommands work identically. Fall back so CI runners that
        # only have the redis package installed still pass.
        echo "redis-cli"
    else
        echo "no valkey-cli or redis-cli on PATH — install valkey or redis client" >&2
        exit 1
    fi
}

CLI="$(pick_cli)"

# 1. Wait up to 30s for every node to answer PING.
deadline=$(( $(date +%s) + 30 ))
for port in "${NODES[@]}"; do
    while :; do
        if "$CLI" -h "$HOST" -p "$port" ping 2>/dev/null | grep -q '^PONG$'; then
            break
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "timeout: valkey on $HOST:$port never answered PING" >&2
            exit 1
        fi
        sleep 0.5
    done
done

# 2. If the cluster is already formed (e.g. composer restart after a
# partial failure), short-circuit. Accept either cluster_state:ok
# or a non-empty cluster_enabled:1 reply as "bootstrap happened".
state="$("$CLI" -h "$HOST" -p "${NODES[0]}" cluster info 2>/dev/null | tr -d '\r' | grep '^cluster_state:' | cut -d: -f2 || true)"
if [ "$state" = "ok" ]; then
    echo "cluster already formed (cluster_state:ok)"
    exit 0
fi

# 3. Form the cluster: 3 masters (first three nodes) + 1 replica each.
addrs=()
for port in "${NODES[@]}"; do
    addrs+=("$HOST:$port")
done
echo "forming cluster across: ${addrs[*]}"
"$CLI" --cluster create "${addrs[@]}" --cluster-replicas 1 --cluster-yes

# 4. Poll up to 30s for cluster_state:ok so the test step doesn't race
# the gossip convergence.
deadline=$(( $(date +%s) + 30 ))
while :; do
    state="$("$CLI" -h "$HOST" -p "${NODES[0]}" cluster info 2>/dev/null | tr -d '\r' | grep '^cluster_state:' | cut -d: -f2 || true)"
    if [ "$state" = "ok" ]; then
        echo "cluster ready (cluster_state:ok)"
        exit 0
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "timeout: cluster_state never reached ok (last: ${state:-unknown})" >&2
        "$CLI" -h "$HOST" -p "${NODES[0]}" cluster info >&2 || true
        exit 1
    fi
    sleep 0.5
done
