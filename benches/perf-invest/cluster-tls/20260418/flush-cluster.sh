#!/usr/bin/env bash
# Cluster FLUSHALL + verify for the round-2 bench protocol.
#
# ElastiCache doesn't expose a broadcast FLUSHALL. We loop over the
# three master nodes discovered from CLUSTER NODES on the config
# endpoint, FLUSHALL each one directly over TLS, then verify DBSIZE
# landed at 0 on every shard.
#
# Fails loud if any shard is non-zero after FLUSHALL — that's a
# leakage signal for the next bench run.

set -euo pipefail

CFG_HOST="${CFG_HOST:-clustercfg.glide-perf-test-cache-2026.nra7gl.use1.cache.amazonaws.com}"
CFG_PORT="${CFG_PORT:-6379}"

# Discover masters from CLUSTER NODES output: lines whose role field
# says 'master' or 'myself,master' (the config endpoint always
# routes to a live master, so one of them will be 'myself,master').
masters=$(valkey-cli -h "$CFG_HOST" -p "$CFG_PORT" --tls --insecure CLUSTER NODES 2>/dev/null \
    | awk '$3 ~ /master/ { split($2, a, "@"); split(a[1], b, ":"); print b[1] }')

if [[ -z "${masters}" ]]; then
    echo "flush-cluster: no masters discovered" >&2
    exit 1
fi

echo "flush-cluster: discovered masters:"
echo "$masters" | sed 's/^/  /'

for m in $masters; do
    printf "flush-cluster: FLUSHALL on %s ... " "$m"
    valkey-cli -h "$m" -p "$CFG_PORT" --tls --insecure FLUSHALL >/dev/null
    size=$(valkey-cli -h "$m" -p "$CFG_PORT" --tls --insecure DBSIZE)
    if [[ "$size" != "0" ]]; then
        echo "FAIL dbsize=$size" >&2
        exit 2
    fi
    echo "ok"
done

echo "flush-cluster: all shards 0 keys"
