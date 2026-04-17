# Faktory comparison benches

Comparison target for the FlowFabric benchmark suite. Faktory
([contribsys/faktory](https://github.com/contribsys/faktory)) is a
language-agnostic work server; this package exercises it with the
same workload envelope as the FlowFabric scenarios 1 and 3 so the
headline numbers can be placed side-by-side.

## Why not wired into matrix CI?

Faktory needs its own long-lived daemon. Matrix CI would have to
provision + tear down a container per job, which is a nontrivial
cost for a comparison bench that only runs on release prep. Local
runs only.

## Running

```bash
# One-time: start the daemon (runs in docker, port 7419).
./scripts/start-faktory.sh

# Scenario 1 — throughput (10 k jobs, single-bench drain).
cargo run --release --bin faktory-scenario1

# Scenario 3 — long-running steady-state (100 internal fetchers × 5 min).
cargo run --release --bin faktory-scenario3

# Teardown.
docker stop ff-bench-faktory
```

Reports land in `benches/results/<scenario>-faktory-<sha>.json` with
the Phase-A schema (same as FlowFabric + baseline + apalis). The
`notes` field explains where Faktory's operational model deviates
(heartbeat vs lease renewal, integrated worker pool vs multi-process
claim loop).

## Model differences worth knowing before reading numbers

- **No per-job leases.** Faktory's worker process holds a TCP
  connection to the daemon and sends `BEAT` heartbeats; in-flight
  jobs are implicit and tied to that connection rather than to an
  explicit lease. Scenario 3's `lease_renewal_count` and
  `lease_renewal_overhead_pct` are reported as `0` / `0.0` with a
  note.
- **Fetcher pool, not workers-per-task.** `Worker::workers(100)`
  spawns 100 internal fetchers inside a single process — the
  Faktory-canonical shape. FlowFabric instead runs 100 separate
  `FlowFabricWorker` instances to exercise the scheduler's
  lease-owner sharding. Both run "the system in its idiomatic
  deployment"; a 1:1 thread-for-thread comparison would distort
  each system's real behavior.
- **No cluster mode.** Faktory is single-node; all scenarios run
  standalone.
