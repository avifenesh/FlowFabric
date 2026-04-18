# Round-2 cluster + TLS bench run — 2026-04-18

Full 6-workload × 2-client sweep against the AWS ElastiCache test
cluster over TLS. Paired with round 1's localhost numbers — same
workloads, same shape, same HdrHistogram methodology — so the
cluster+TLS delta vs localhost is directly comparable.

## Endpoint

```
host:     clustercfg.glide-perf-test-cache-2026.nra7gl.use1.cache.amazonaws.com
port:     6379
TLS:      strict (rustls 0.23 with aws-lc-rs provider + webpki-roots trust store)
cluster:  3 masters, 6 replicas
```

Cluster topology (from `CLUSTER NODES` at start of run):

```
glide-perf-test-cache-2026-0001-001  master  slots 0-5461
glide-perf-test-cache-2026-0002-001  master  slots 5462-10922
glide-perf-test-cache-2026-0003-001  master  slots 10923-16383
+ 6 replicas (0001-002, 0001-003, 0002-002, 0002-003, 0003-002, 0003-003)
```

## TLS posture

Strict verification. Both clients bundle the Mozilla trust store via
`rustls-webpki-roots` (redis-rs dep feature) or the ferriskey-
internal rustls config; the process-wide `CryptoProvider` is
installed once at bin startup via
`rustls::crypto::aws_lc_rs::default_provider().install_default()`
(see `benches/comparisons/wider/src/cluster_shared.rs`).

Both clients PONG'd cleanly against the config endpoint with
`TlsMode::Secure` / `.tls()` — no cert-verify errors. If either had
failed, that was the agreed-on posture: **surface it as a finding,
do not silently fall back to insecure**.

Finding recorded in the scaffold commit: rustls 0.23 panics if no
CryptoProvider is installed. The redis crate's `tls-rustls-*`
features don't pick one; ferriskey pins `aws-lc-rs` internally. Our
bench init picks the same, matching both clients onto the same TLS
primitive.

## Bench protocol

Per the manager's round-2 spec:

1. `flush-cluster.sh` — iterates `CLUSTER NODES`, issues FLUSHALL to
   every master over TLS, verifies `DBSIZE == 0` on each. Fails loud
   on any non-zero.
2. 30 s calm between workload pairs to let the cluster connection
   pool settle + AWS networking converge.
3. 16 concurrent workers, 5 s warmup + 30 s measured, 4 KiB payload,
   seeded `StdRng` with per-worker offset — identical to round 1 so
   any cluster-vs-localhost delta is transport, not workload shape.
4. Workload-paired A/B (ferriskey → redis-rs for the SAME workload,
   THEN flush+calm+next pair). Minimises cluster-warmup drift between
   the comparison halves.

## Run order

```
80/20    ferriskey → flush → redis-rs
 (calm 30s)
50/50    ferriskey → flush → redis-rs
 (calm 30s)
100% GET ferriskey → flush → redis-rs
 (calm 30s)
pipeline ferriskey → flush → redis-rs   (single-slot, {wider} tag)
 (calm 30s)
streams  ferriskey → flush → redis-rs   (per-worker {wider}:stream:wN)
 (calm 30s)
BLMPOP   ferriskey → flush → redis-rs   (single-slot, {benchq} tag)
```

Total wall clock: ~15 min active (including seeding + drain phases).

## Key-hygiene

Every workload pins its keys to a single cluster slot via the
`{wider}` (or `{benchq}` for BLMPOP) hash tag. **This measures
client-in-cluster-mode + TLS overhead, NOT cross-slot routing.**
Cross-slot performance is a potential bonus bin (see
`report-w2-round2.md §4`).

Streams uses per-worker stream keys `{wider}:stream:w{N}` — the
`{wider}` tag means all 16 streams live on the same shard; workers
still contend on XADD ordering within their own stream but not
cross-worker.

Zero errors across all 12 runs. Zero CROSSSLOT errors — tag
discipline verified in the wild.

## Files

```
flush-cluster.sh                                          — FLUSHALL helper
cluster_wider_80_20-{ferriskey,redis-rs}-cluster-*.json   — 80/20 GET/SET
cluster_wider_50_50-{ferriskey,redis-rs}-cluster-*.json   — 50/50 GET/SET
cluster_wider_get_only-{ferriskey,redis-rs}-cluster-*.json — 100% GET
cluster_wider_pipeline_100-{ferriskey,redis-rs}-cluster-*.json — 100-cmd pipeline
cluster_wider_streams-{ferriskey,redis-rs}-cluster-*.json — XADD + XRANGE
cluster_submit_claim_complete-{ferriskey,redis-rs}-cluster-*.json — BLMPOP
summary.md                                                — localhost vs cluster table
```
