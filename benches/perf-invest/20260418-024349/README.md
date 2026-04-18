# ferriskey vs redis-rs — flame graph capture

Capture date (UTC): 2026-04-18T02:48Z

## Host

- CPU: AMD EPYC 9R14
- Cores: 16
- Kernel: Linux 6.8.0-1031-aws
- perf 6.8.12, `perf_event_paranoid = -1` (no sudo)

## Target

- Valkey server: valkey_version 7.2.12 (standalone, localhost:6379)
- Branch: `feat/ferriskey-perf-invest`
- Git SHA at capture: `b1cff89`

## Binaries profiled

Both built with a new `[profile.perf]` inheriting release + `debug = 1`
(line-table-only debug info, same codegen as release, so numbers match
what a production consumer would see).

| bin                                  | crate                                              |
| ------------------------------------ | -------------------------------------------------- |
| `baseline-scenario1`                 | `benches/comparisons/baseline` (redis-rs 1.2.0)    |
| `ferriskey-baseline-scenario1`       | `benches/comparisons/ferriskey-baseline` (fork)    |

## Workload

Identical for both binaries:

- 16 workers × 10 k payload seed of 4 KiB each (via `--tasks` override),
  then 100 k captured under perf (for ≥10 s sample window).
- Protocol: `RPUSH bench:q <payload>` seed, `BLMPOP 1 LEFT bench:q`
  claim, `INCR bench:completed` ack. No leases, no SDK glue.

One warmup round of `--tasks 2000` per binary before capture, discarded;
`FLUSHALL` between warmup and capture, and between the two capture runs.

## Measured throughput this capture

| system    | throughput    | p99      | notes                               |
| --------- | ------------- | -------- | ----------------------------------- |
| redis-rs  | 45 938 ops/s  | 0.39 ms  | capture at --tasks 100 000          |
| ferriskey | 37 284 ops/s  | 0.38 ms  | capture at --tasks 100 000          |

`delta(redis-rs, ferriskey) ≈ 19 %` on this host/workload. The 45 %
gap the top-level brief cited was measured on the 10 k shape where
both bases were cold; run-to-run variance on a warm Valkey on
localhost is wide and the 100 k capture tends to sit higher on the
throughput curve. Analysis below is based on the flame graphs, not
the absolute numbers.

## Files

- `redis-rs.svg`           — flame graph (cargo-flamegraph / inferno)
- `ferriskey.svg`          — flame graph
- `redis-rs.folded`        — folded stacks (plain text, greppable)
- `ferriskey.folded`       — folded stacks
- `redis-rs.perf.data.gz`  — raw perf capture (gzip -9; decompress with
                             `gunzip redis-rs.perf.data.gz` then run
                             `perf script -i redis-rs.perf.data`)
- `ferriskey.perf.data.gz` — raw perf capture

Capture command (both binaries):

```bash
cargo flamegraph --profile perf --bin <bin> \
  -o <this-dir>/<system>.svg \
  -c 'record -F 997 --call-graph dwarf,16384 -g' \
  -- --tasks 100000
```

## Next

See `benches/perf-invest/report-w1.md` for the hot-function analysis
and recommended optimisations.
