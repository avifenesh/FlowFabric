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
| 4 — linear flow DAG           | **Yes, approximation** — `apalis-scenario4` | apalis has no DAG primitive. 10-stage chain wired by hand; each stage's handler pushes the next stage. Emits system label `apalis-approx`. |
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
- **apalis-approx system label** — the JSON report for scenario 4
  uses `system = "apalis-approx"` (not plain `apalis`) so an
  aggregator doesn't accidentally graph these as a native DAG
  benchmark. Tracked explicitly per
  `ff_bench::comparison::SystemSupport::Approximation`.

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
