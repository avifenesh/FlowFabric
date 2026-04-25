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

# 4. Poll up to 60s for cluster convergence across ALL 6 nodes. The
# previous implementation polled only node[0]; `cluster_state:ok` flips
# true on a single node as soon as its local slot table is allocated,
# which can happen before nodes[3..5] have completed the REPLICAOF
# handshake with their masters. A client that dials into the partial
# cluster at that moment and issues a write-routed command (e.g.
# `FUNCTION LOAD`, which routes `AllPrimaries`) can hit a node whose
# topology view still has it as a primary even though it's already a
# replica, getting a `READONLY` error.
#
# Convergence criteria (ALL must hold from EVERY node's perspective):
#   * `cluster_state:ok`
#   * exactly 3 masters + 3 slaves in `CLUSTER NODES`
#   * no `fail?` or `handshake` flags in `CLUSTER NODES`
#
# See github.com/avifenesh/FlowFabric/issues/275 for the race this
# guards against.
deadline=$(( $(date +%s) + 60 ))
while :; do
    converged=1
    for port in "${NODES[@]}"; do
        info="$("$CLI" -h "$HOST" -p "$port" cluster info 2>/dev/null | tr -d '\r' || true)"
        state="$(printf '%s\n' "$info" | grep '^cluster_state:' | cut -d: -f2 || true)"
        if [ "$state" != "ok" ]; then
            converged=0
            last_fail="node $port: cluster_state=${state:-unknown}"
            break
        fi
        nodes_out="$("$CLI" -h "$HOST" -p "$port" cluster nodes 2>/dev/null || true)"
        # Count masters + slaves; reject if either count != 3 or any
        # handshake/fail flag is present.
        masters=$(printf '%s\n' "$nodes_out" | grep -c ' master ' || true)
        slaves=$(printf '%s\n' "$nodes_out" | grep -c ' slave ' || true)
        # `myself,master` and `myself,slave` are hyphenated in nodes flags,
        # but `grep -c ' master '` misses `myself,master` because of the comma.
        # Re-count using a broader match (master anywhere in the flag field,
        # same for slave):
        masters=$(printf '%s\n' "$nodes_out" | awk '{print $3}' | grep -c 'master' || true)
        slaves=$(printf '%s\n' "$nodes_out" | awk '{print $3}' | grep -c 'slave' || true)
        if [ "$masters" -ne 3 ] || [ "$slaves" -ne 3 ]; then
            converged=0
            last_fail="node $port: masters=$masters slaves=$slaves (expected 3/3)"
            break
        fi
        if printf '%s\n' "$nodes_out" | grep -qE 'handshake|fail\?|fail,' ; then
            converged=0
            last_fail="node $port: handshake/fail flags still present"
            break
        fi
    done
    if [ "$converged" = "1" ]; then
        echo "cluster fully converged across all 6 nodes (3 masters + 3 slaves, no handshake flags)"
        exit 0
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "timeout: cluster never fully converged (last: $last_fail)" >&2
        for port in "${NODES[@]}"; do
            echo "=== cluster nodes @ $port ===" >&2
            "$CLI" -h "$HOST" -p "$port" cluster nodes >&2 || true
        done
        exit 1
    fi
    sleep 0.5
done
# rerun marker 1
# rerun marker 2
# rerun marker 3
# rerun marker 4
