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

FlowFabric requires **Valkey ≥ 8.0** (see RFC-011 §13). The server verifies
this at boot by issuing `INFO server`, parsing `redis_version`, and comparing
the major component to the required floor. If the detected version is below
8.0, the server refuses to start with a typed `ServerError::ValkeyVersionTooLow
{ detected, required }` and exits.

### Why 8.0

The hash-slot co-location design (RFC-011 §2) and the Valkey Functions API
behavior the engine relies on stabilized in 8.0. Older versions will silently
behave differently on some cluster edge cases. 8.0 is cairn-fabric's reference
deployment, and it's what FlowFabric's integration tests pin.

### Rolling upgrade tolerance

The version check includes a **60-second exponential-backoff retry budget**
(RFC-011 §9.17). During that window both error classes retry:

- **Transport transients** — connection refused, `BusyLoadingError`,
  `ClusterDown`, etc. (Valkey error kinds classified as retryable).
- **Low-version responses** — if the connected node happens to be a
  pre-upgrade replica during a rolling upgrade, the 7.x response is treated
  as a rolling-state transient and retried. After 60s of consistent low
  responses, the server exits with `ServerError::ValkeyVersionTooLow` —
  that's the misconfiguration signal, not a transient.

Backoff starts at 200ms and doubles up to a 5-second cap per attempt.

**Exception — fast-fail classes:** auth failures, permission denied, and
invalid client config are **not** retried. These are operator
misconfiguration (wrong credentials, wrong ACL, bad TLS config) and
retrying them just hides the true cause under a 60s hang. Server boot fails
immediately with the structured error and the underlying Valkey error kind
preserved.

## Rolling upgrade procedure (Valkey 7.x → 8.0+)

1. **Prepare:** confirm cairn (and any other consumer) is on a FF-SDK release
   that supports the target Valkey version. Consumer coordination is
   out-of-scope for this runbook; see the consumer migration guide.

2. **Upgrade Valkey node-by-node:**
   - If single-node standalone: stop ff-server briefly, upgrade Valkey, start
     ff-server. The 60s retry budget tolerates restart windows under that.
   - If cluster: roll node-by-node. If ff-server restarts and connects to a
     pre-upgrade (7.x) node while the roll is in progress, the version check
     retries within the 60s budget; once the connected node completes its
     upgrade (or a reconnect lands on a post-upgrade node), the check
     succeeds. Keep the rolling window under 60s per node so the check
     doesn't exhaust before the node ahead finishes.

3. **Verify:** after the upgrade, ff-server boot log should emit
   `Valkey version accepted detected_major=8 required=8` at INFO level.

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

> The `ff-server admin partition-collisions` subcommand documented here lands
> in phase-5B. Until it ships, operators who want to check can compute
> `crc16(lane_id) % num_flow_partitions` manually or use any crc16 CLI.

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
| Server exits with `valkey version too low: detected 7, required >= 8.0` | Valkey 7.x backend | Upgrade Valkey to 8.0+; see [rolling upgrade](#rolling-upgrade-procedure-valkey-7x--80) |
| Boot hangs for ~60s then exits with `valkey ({context}): ...` | Valkey unreachable during rolling upgrade OR truly down | If the cluster is mid-restart, wait and retry; otherwise check network/DNS |
| `ServerError::PartitionMismatch` on `add_execution_to_flow` | Consumer minted exec with wrong flow/lane routing | Consumer bug — see [migration guide Step 1](rfc011-migration-for-consumers.md#step-1--executionid-construction) |
| `partition_config mismatch: num_flow_partitions expected N, got M` on boot | `FF_FLOW_PARTITIONS` changed after first boot | Cannot hot-change partition count — re-seed or accept existing config |

## Reference

- [RFC-011 exec/flow hash-slot co-location](../rfcs/RFC-011-exec-flow-colocation.md) — primary design (revisions 3 + 4)
- [Consumer migration guide](rfc011-migration-for-consumers.md) — what SDK consumers change
- [Releasing FlowFabric](RELEASING.md) — release cadence + publish workflow

Merged RFC-011 implementation PRs: #19, #20, #23, #25, #26, #27, #28, #29, #30.
