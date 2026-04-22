# RFC-011 operator runbook

Operational guidance for FlowFabric deployments running post-[RFC-011](../rfcs/RFC-011-exec-flow-colocation.md)
(phases 1-5). Covers environment variables, rolling upgrade, collision
observability, and migration artifacts.

Paired with the [consumer migration guide](rfc011-migration-for-consumers.md)
(which tells SDK consumers what to change in their code). This doc is for the
operator responsible for the Valkey backend + FlowFabric server deployment.

## Environment variables

| Variable | Default | Notes |
|---|---|---|
| `FF_HOST` | `localhost` | Valkey host |
| `FF_PORT` | `6379` | Valkey port |
| `FF_TLS` | unset | Set any value to enable TLS |
| `FF_CLUSTER` | unset | Set any value to enable cluster mode |
| `FF_FLOW_PARTITIONS` | `256` | **Was `FF_EXEC_PARTITIONS` pre-RFC-011**. Authoritative partition count for both flow and execution routing (hash-slot co-located). Any positive `u16` value (1–65535); powers of 2 are not required by the implementation but are recommended for even hash-slot distribution. |
| `FF_BUDGET_PARTITIONS` | `32` | Budget partition count |
| `FF_QUOTA_PARTITIONS` | `32` | Quota partition count |

### Retired

- `FF_EXEC_PARTITIONS` — retired under RFC-011 §3.2. Operators who previously
  set `FF_EXEC_PARTITIONS=N` must either rename to `FF_FLOW_PARTITIONS=N` or
  accept the new default of 256. The server does **not** read the old name;
  setting it has no effect.

## Valkey version requirement

FlowFabric requires **Valkey ≥ 7.2** (see RFC-011 §13 and the §13 Amendment F
floor-revert note). The server verifies this at boot by issuing `INFO server`
and parsing the authoritative version field:

- Prefers `valkey_version:` (present on Valkey 8.0+; this is the real
  server version).
- Falls back to `redis_version:` for Valkey 7.x, which doesn't emit
  `valkey_version:`.

**Note:** Valkey 8.x/9.x keeps `redis_version:7.2.4` pinned for Redis-client
compat and exposes the true version in `valkey_version:`. Operators
inspecting the INFO response manually should read `valkey_version:`, not
`redis_version:`, to see the real server version.

If the parsed `(major, minor)` is below `(7, 2)`, the server refuses to start
with a typed `ServerError::ValkeyVersionTooLow { detected, required }` and
exits.

### Why 7.2

Valkey 7.2 is where the Functions API and RESP3 stabilized — the primitives
the co-location design and typed FCALL wrappers actually depend on. Older
versions do not implement the required APIs. We previously shipped an 8.0
floor but reverted to 7.2 (RFC-011 §13 Amendment F) because nothing in the
design requires 8-only behavior, and the lower floor supports downstream
operators on slower-moving infra. CI exercises both 7.2 and 8 to guard
against accidental 8-specific adoption.

### Rolling upgrade tolerance

The version check includes a **60-second exponential-backoff retry budget**
(RFC-011 §9.17). During that window both error classes retry:

- **Transport transients** — connection refused, `BusyLoadingError`,
  `ClusterDown`, etc. (Valkey error kinds classified as retryable).
- **Low-version responses** — if the connected node happens to be a
  pre-upgrade replica during a rolling upgrade (e.g. a 7.0 node briefly
  reached while rolling to 7.2+), the response is treated as a
  rolling-state transient and retried. After 60s of consistent low
  responses, the server exits with `ServerError::ValkeyVersionTooLow` —
  that's the misconfiguration signal, not a transient.

Backoff starts at 200ms and doubles up to a 5-second cap per attempt.

**Exception — fast-fail classes:** auth failures, permission denied, and
invalid client config are **not** retried. These are operator
misconfiguration (wrong credentials, wrong ACL, bad TLS config) and
retrying them just hides the true cause under a 60s hang. Server boot fails
immediately with the structured error and the underlying Valkey error kind
preserved.

## Rolling upgrade procedure (below-floor → ≥ 7.2)

1. **Prepare:** confirm cairn (and any other consumer) is on a FF-SDK release
   that supports the target Valkey version. Consumer coordination is
   out-of-scope for this runbook; see the consumer migration guide.

2. **Upgrade Valkey node-by-node:**
   - If single-node standalone: stop ff-server briefly, upgrade Valkey, start
     ff-server. The 60s retry budget tolerates restart windows under that.
   - If cluster: roll node-by-node. If ff-server restarts and connects to a
     pre-upgrade node while the roll is in progress, the version check
     retries within the 60s budget; once the connected node completes its
     upgrade (or a reconnect lands on a post-upgrade node), the check
     succeeds. Keep the rolling window under 60s per node so the check
     doesn't exhaust before the node ahead finishes.

3. **Verify:** after the upgrade, ff-server boot log should emit
   `Valkey version accepted detected_major=<M> detected_minor=<m> required_major=7 required_minor=2`
   at INFO level.

4. **Post-upgrade partition collision check** (optional, phase-5 probe):
   see the [Partition-collision observability](#partition-collision-observability)
   section below.

## Partition-collision observability

Under RFC-011 §5.6, solo-exec routing derives partition from
`crc16(lane_id) % num_flow_partitions`. With 256 partitions and ~15+ lanes,
birthday-paradox math gives a non-negligible chance of two popular lanes
colliding on the same partition. A collision is not a correctness bug —
routing still works — but it concentrates write traffic on one Valkey master,
which can hot-spot under load.

### Running the probe

```sh
# Reads FF_LANES + FF_FLOW_PARTITIONS from env. Validates lane names
# (length + ASCII-printable per LaneId::try_new) and rejects duplicate
# entries — note ff-server boot itself currently uses LaneId::new
# without duplicate detection, so the probe may catch misconfigurations
# that a prod boot silently accepts.
# Does NOT connect to Valkey or start any server; pure computation.
FF_LANES="default,high,low,bulk" \
  ff-server admin partition-collisions
```

Output columns:

- `partition` — the partition index the lane hashes to (crc16 % num_flow_partitions)
- `lane` — the configured lane id
- `collides_with` — other lanes sharing the same partition (empty `—` if none)

Severity field summarizes the whole deployment:

| Severity | Ratio of colliding lanes | Operator action |
|---|---|---|
| `Clean` | 0% | No action |
| `Watch` | <5% | Monitor; remediate if throughput asymmetry surfaces |
| `Elevated` | 5-15% | Worth remediating preventively |
| `Remediate` | >15% | Hot-spot risk under load; apply one of the three options below |

The tool exits `0` regardless of severity — it's an observability probe,
not a gate. Integrate it into deployment pipelines as a reporting step if
you want severity thresholds to fail CI.

### When to investigate

- Tail-latency on a specific lane spikes without a corresponding load spike
  on other lanes.
- Valkey `SLOWLOG` shows one master serving substantially more traffic than
  its siblings.
- `RedisCommandsProcessed` per-master metrics diverge by >2x.

### Remediation options

In increasing order of operator cost:

1. **Rename the lane** — cheapest. If the lane name is internal, pick a new
   name that hashes to a different partition. Use the probe subcommand to
   find a safe target partition.

2. **Bump `FF_FLOW_PARTITIONS`** — doubles the space, halves the collision
   probability. Requires re-seeding the partition config key (clean state) or
   a full FLUSHDB + bootstrap. Not valid for production with live state.

3. **Custom `SoloPartitioner`** (RFC-011 §5.6 pluggable trait, rev-3
   amendment) — for advanced operators who want to override the default
   crc16 routing with a custom scheme. Requires forking `ExecutionId::solo`
   minting logic into deployment-owned code; `solo_partition_with(lane,
   config, &custom_impl)` is the public escape hatch. Defer to a follow-up
   RFC if this becomes common demand.

## Data passing between flow nodes

When a flow edge is staged via `ff_stage_dependency_edge` with a
non-empty `data_passing_ref`, the engine copies the upstream's
`result` bytes into the downstream's `input_payload` atomically at
satisfaction time (inside `ff_resolve_dependency`). No separate
client-side chain is required — submit posts all nodes up-front,
the worker's subsequent `claim_next` reads the already-injected
payload.

Semantics:

- Any non-empty `data_passing_ref` triggers injection. The string
  content is currently opaque to the engine (reserved for a future
  field-path or schema-ref extension).
- Copy runs **before** the downstream's eligibility transition so
  a crash mid-FCALL can't leave the child eligible with a stale
  payload (RFC-010 §4.8b write-ordering rule).
- If the upstream called `complete(None)` (no result written), the
  copy is a no-op — the downstream's original `input_payload` (set
  at `create_execution` time) is preserved.
- Multi-upstream children with `data_passing_ref` on multiple edges
  experience last-writer-wins semantics. Configure the flow with at
  most one data-passing edge per child if this matters.
- Uses Valkey `COPY src dst REPLACE` server-side, not round-trip
  `GET`/`SET` — large payloads don't inflate the FCALL memory budget.
- Upstream/downstream co-location is guaranteed by flow membership
  (RFC-011 §7.3 hash-tag routing), so the copy is a single-slot op
  even on cluster.

## DAG promotion: push listener + safety-net reconciler

Post-Batch-C item 6, FlowFabric uses two paths to promote downstream
executions when an upstream reaches a terminal state:

1. **Push (primary).** `ff_complete_execution` /
   `ff_fail_execution` / `ff_cancel_execution` `PUBLISH` a JSON
   completion payload to the `ff:dag:completions` channel. The engine
   runs a `CompletionListener` on a dedicated RESP3 client that
   `SUBSCRIBE`s to that channel and dispatches
   `ff_resolve_dependency` for each downstream edge.
2. **Reconciler (safety net).** The `dependency_reconciler` scanner
   still runs — default interval raised from 1s to 15s now that
   push is primary — and picks up any completion the listener
   missed (restart window, unstamped legacy `core.flow_id`,
   cross-node sharded-pubsub gaps).

Under normal operation DAG latency is `~RTT × levels`, not
`interval × levels`.

### When to tune `FF_DEPENDENCY_RECONCILER_INTERVAL_S`

- **Default (15s)**: listener is primary; the scanner is belt-and-
  suspenders. Correct for every deployment the server runs in by
  itself.
- **Listener disabled** (custom deployments only, via the
  `ff_engine::EngineConfig::completion_listener = None` escape hatch):
  drop this back to `1` so DAG latency doesn't regress to the
  pre-Batch-C floor.

### Cluster mode

Plain `PUBLISH` broadcasts across cluster nodes. Every ff-server
instance that runs an engine will receive every completion;
duplicate dispatches are harmless (idempotent) but redundant. If
contention surfaces under very wide fan-out, a follow-up can shard
the channel via `SPUBLISH` keyed by flow partition — not required
at current scale.

### Observability

- `tracing::info!` at subscribe time:
  `completion_listener: subscribed, awaiting push frames`.
- `tracing::debug!` per dispatched completion:
  `completion_listener: dispatching dependency resolution`.
- `tracing::warn!` on reconnect, malformed payload, or invalid
  `execution_id` — rare; a non-zero sustained rate means the Lua
  producers and engine parser have drifted.

## Superseded artifacts

### Issue #21 (crash-recovery scanner)

**Closed as superseded.** The scanner was designed to reconcile exec→flow
membership orphans from the pre-phase-3 two-phase contract. Under phase 3's
atomic FCALL (RFC-011 §7.3), orphans cannot be created — `ff_add_execution_to_flow`
commits all writes atomically or nothing. No scanner is needed; do not
resurrect.

### Pre-RFC-011 two-phase prose

Docstrings at `lua/flow.lua`, `crates/ff-server/src/server.rs`
(`describe_execution` + `replay_execution` reader invariants) previously
described the two-phase contract + orphan reader-safety catalog. Those were
rewritten in phase 3 to describe the atomic shape. If any operator
documentation still references "phase 2 HSET" or "orphan window" in FF
context, it's stale; replace with an RFC-011 §7.3 pointer.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Server exits with `valkey version too low: detected <M.m>, required >= 7.2` | Valkey below 7.2 backend | Upgrade Valkey to 7.2+; see [rolling upgrade](#rolling-upgrade-procedure-below-floor--72) |
| Boot hangs for ~60s then exits with `valkey ({context}): ...` | Valkey unreachable during rolling upgrade OR truly down | If the cluster is mid-restart, wait and retry; otherwise check network/DNS |
| `ServerError::PartitionMismatch` on `add_execution_to_flow` | Consumer minted exec with wrong flow/lane routing | Consumer bug — see [migration guide Step 1](rfc011-migration-for-consumers.md#step-1--executionid-construction) |
| `partition_config mismatch: num_flow_partitions expected N, got M` on boot | `FF_FLOW_PARTITIONS` changed after first boot | Cannot hot-change partition count — re-seed or accept existing config |

## Reference

- [RFC-011 exec/flow hash-slot co-location](../rfcs/RFC-011-exec-flow-colocation.md) — primary design (revisions 3 + 4)
- [Consumer migration guide](rfc011-migration-for-consumers.md) — what SDK consumers change
- [Releasing FlowFabric](RELEASING.md) — release cadence + publish workflow

Merged RFC-011 implementation PRs: #19, #20, #23, #25, #26, #27, #28, #29, #30.
