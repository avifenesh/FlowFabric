# apalis — comparison notes

apalis 1.0 (currently `rc.7`) + `apalis-redis` is FlowFabric's closest
Rust-native peer: tokio-first, redis-backed, single-binary.  This
directory implements scenarios 1 and 4 against apalis so the Phase B
comparison report has apples-to-apples numbers next to
`ff-bench`'s own reports.

## Versions

| Crate          | Version      |
| -------------- | ------------ |
| `apalis`       | `=1.0.0-rc.7`|
| `apalis-redis` | `=1.0.0-rc.7`|
| `redis`        | `1`          |

Pinned EXACTLY (`=`) because apalis is pre-release and rc→rc bumps
have introduced API drift. A bench-workspace churn is acceptable when
we choose it explicitly; silent breakage during review is not.

## Scenario coverage

| ff-bench scenario | apalis here?            | Why |
| ----------------- | ----------------------- | --- |
| 1 — submit → claim → complete | **Yes** — `apalis-scenario1` | Direct analog: `TaskSink::push` + `WorkerBuilder` with concurrency. |
| 2 — suspend → signal → resume | **Skipped** | apalis has no first-class suspend/signal. Emulating it with external pub/sub would measure the emulator, not apalis. |
| 3 — long-running steady-state | (owned by W2) | n/a here |
| 4 — linear flow DAG           | **Yes, native** — `apalis-scenario4` | Uses `apalis-workflow::Workflow` (sequential `and_then` combinator) over a single `RedisStorage`. Emits system label `apalis`. Prior to 2026-04-22 this harness was hand-rolled with 10 typed queues and labelled `apalis-approx`; see the update note further down. |
| 5 — capability-routed claim   | **Skipped** | apalis queues are typed, not capability-routed. The closest mapping is "one queue per cap-set", which defeats the benchmark's point (routing arbitrates across a shared queue). |

## What's directly comparable

- **Throughput** — `throughput_ops_per_sec` for scenario 1 is
  comparable directly. Same Valkey, same payload size (4 KiB), same
  worker concurrency (16).
- **Scenario 4 throughput** is comparable as "flows per second for a
  10-stage linear chain". FlowFabric will be slower per flow on this
  specific shape (more atomicity-class-A work per stage); that delta
  IS the feature cost.

## What's NOT comparable (and why)

- **apalis scenario 1 at-least-once ack vs FlowFabric lease-backed
  at-most-once**. A crashed apalis worker re-delivers on heartbeat
  timeout without fencing — the replacement worker may concurrently
  process the same payload. FlowFabric uses a lease+epoch pair so the
  Lua side fences out the stale worker. This is not quantified in the
  numbers; see `rfcs/RFC-003-lease.md` for the protocol difference.
- **JSON codec overhead** — apalis-redis defaults to JSON-encoded
  payloads. FlowFabric's input_payload is raw bytes. For a 4 KiB
  random payload the JSON adds ~33% (base64-ish) bloat. That cost is
  on apalis's side of the measurement, consistent with what a consumer
  observes; we do NOT normalise it out.
- **Scenario 4 data flow** — apalis chain stages carry a dummy
  `flow_id` token; real users wiring data flow pay an extra
  serialization cost per hop. FlowFabric scenario 4 (W3's
  `flow_dag.rs`) measures the engine's data_passing_ref story (once
  that lands, see Batch C item 3).
- **Scenario 4 failure semantics** — an apalis Stage_i failure
  strands the flow at stage i. FlowFabric engine retries/cancels per
  policy. Numbers assume no failures; with failures, apalis's numbers
  don't extrapolate.
- **apalis system label (scenario 4)** — the JSON report for
  scenario 4 now uses `system = "apalis"` because the harness uses
  `apalis-workflow::Workflow` (apalis's first-class sequential
  workflow primitive). Prior to 2026-04-22 this scenario emitted
  `system = "apalis-approx"` because the harness was hand-rolled
  across 10 typed queues — see the update note below.

## Scenario 4 harness update (2026-04-22, issue #51)

**Change**: Scenario 4's apalis harness switched from a hand-rolled
10-stage chain (10 `WorkerBuilder` instances, each stage's handler
pushing the next via a typed `RedisStorage<StageN>`) to a single
`apalis-workflow::Workflow::new().and_then(…).and_then(…)` pipeline
over ONE `RedisStorage` with one `WorkerBuilder`.

**Why**: Issue #51. The apalis maintainer (geofmureithi) flagged the
prior harness as under-representing apalis:

> Ps in scenario 4, apalis offers apalis-workflow which has sequential
> and dag primitives and would be better than the current approach (I
> hope).

See `rfcs/drafts/apalis-comparison.md` for Worker VV's full
investigation. Adopting `apalis-workflow` is the hygienic fix — this
bench should represent apalis's idiomatic shape.

**Numerical effect (on the current bench host, Valkey 8.1.0)**:
- At `flows=100` the scenario sits at the 50 ms driver-poll-jitter
  floor (see the Polling-jitter-floor note in `src/scenario4.rs`):
  both harnesses measure 9.3 flows/s (wall ≈ 10.70 s). No
  measurable delta.
- At `flows=500` the two diverge: prior harness 34.0 flows/s
  (wall 14.69 s, N=3); `apalis-workflow` 9.8 flows/s (wall 50.93 s,
  N=5). The linear-chain workload favours the prior hand-rolled
  chain's cheap per-stage enqueue over `apalis-workflow`'s per-step
  state persistence.

The new harness is still correct to adopt even though this specific
shape measures slower: COMPARISON is about apalis's idiomatic
primitive, not "cheapest possible apalis harness". The FF-vs-apalis
engine gap on this shape widens modestly as a result; that is the
honest number.

**Implementation note (Workflow bounds)**: apalis-workflow's
`IntoWorkerService` impl requires `Backend::Args == BackendExt::Compact`.
For `RedisStorage` that means the storage must be typed over the
codec's compact form (`Vec<u8>`), not the user payload type.
`src/scenario4.rs` does `RedisStorage::<Vec<u8>>::new(conn)`; the
stage handlers still take typed `u64` (codec roundtrips at step
boundaries). Payload is `u64` rather than a custom struct because
`apalis-workflow::DagCodec` provides passthrough impls for primitives
(String, integer types, bool, …) but not user types; the prior
harness also only threaded a `flow_id` token (no data_passing) so
this preserves the measurement contract.

**Sample-teardown note**: `apalis-workflow` persists step-state keys
in the backing Redis across runs. The harness runs `FLUSHALL` between
N=5 samples to prevent sample-N state from leaking into sample-N+1
(without this, sample 2+ deadline-fails with 0/100 completions).

## Running

```bash
# From benches/comparisons/apalis/
cargo run --release --bin apalis-scenario1 -- --tasks 10000
cargo run --release --bin apalis-scenario4 -- --flows 100
```

Both write their JSON reports to `benches/results/` at the repo root.

## Invariants

- `cargo check` must pass; this workspace is isolated from the main
  FlowFabric workspace (see `benches/Cargo.toml` — `exclude = ["comparisons"]`).
- No code here imports `ff-core`/`ff-sdk`; only `ff-bench` for shared
  metadata helpers (`git_sha`, `host_info`, `valkey_version`,
  `iso8601_utc`). That matches the `baseline/` precedent.
- Throughput and latency fields use the same JSON schema as
  `ff-bench` scenarios so `benches/scripts/check_release.py` can
  ingest both without per-system glue.
