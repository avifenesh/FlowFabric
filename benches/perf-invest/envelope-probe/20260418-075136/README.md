# envelope-probe — Track A flame capture (post-round-3)

Capture date (UTC): 2026-04-18T07:51Z

## Host

- CPU: AMD EPYC 9R14
- Cores: 16
- Kernel: Linux 6.8.0-1031-aws
- `perf` 6.8.12, `perf_event_paranoid = -1` (no sudo required)

## Target

- Valkey: redis_version 7.2.4 (reports valkey_version 7.2.12), standalone,
  localhost:6379
- Branch: `feat/ferriskey-iam-gate` (round 3 merged)
- Git SHA at capture: `0181f30`

## Binaries profiled

Both built with `[profile.perf] inherits = "release", debug = 1` on
top of `opt-level = 3, lto = "fat", codegen-units = 1`.

| bin                   | crate                           |
| --------------------- | ------------------------------- |
| `probe-ferriskey`     | `ferriskey` @ branch HEAD       |
| `probe-redis-rs`      | `redis` 1.2.0                   |

## Workload (identical on both sides)

1 client + 1 tokio task + a tight 100 000-iter loop of
`INCR bench:envelope-counter`. Non-blocking, scalar int reply, no
value-conversion array walks. 1 000-iter warmup first (discarded).

INCR is the minimum wire workload that exercises the full per-command
envelope without any workload-specific noise (HOL on blockers, nested
value parsing, concurrency dynamics).

## Measured throughput

Numbers below are WITH the perf sampler attached (some overhead from
DWARF call-graph collection). Without perf the same binaries measure
p50 = 31 µs on both clients (see §§probe-ferriskey.txt / probe-redis-rs.txt).

| system    | p50      | p95      | p99      | p99.9    |
| --------- | -------- | -------- | -------- | -------- |
| ferriskey | 0.058 ms | 0.077 ms | 0.089 ms | 0.140 ms |
| redis-rs  | 0.056 ms | 0.074 ms | 0.082 ms | 0.123 ms |

Delta at p50: ~2 µs (~3.6%). At p99.9: ~17 µs (~14%). ferriskey's
tail is slightly fatter; the p50 is near-parity.

## Files

- `probe-ferriskey.svg`          — flame graph
- `probe-redis-rs.svg`           — flame graph
- `probe-ferriskey.folded`       — folded stacks (greppable)
- `probe-redis-rs.folded`        — folded stacks
- `probe-ferriskey.perf.data.gz` — raw capture (gunzip + `perf script`)
- `probe-redis-rs.perf.data.gz`  — raw capture
- `probe-ferriskey.txt`          — per-iter latency sanity (perf NOT attached)
- `probe-redis-rs.txt`           — per-iter latency sanity

## Capture commands

```bash
cd benches/perf-invest/envelope-probe

# build once with the perf profile
cargo build --profile perf --bin probe-ferriskey --bin probe-redis-rs

# ferriskey
valkey-cli FLUSHALL
cargo flamegraph --profile perf --bin probe-ferriskey \
  -o 20260418-075136/probe-ferriskey.svg \
  -c 'record -F 997 --call-graph dwarf,16384 -g'
mv perf.data 20260418-075136/probe-ferriskey.perf.data

# redis-rs
valkey-cli FLUSHALL
cargo flamegraph --profile perf --bin probe-redis-rs \
  -o 20260418-075136/probe-redis-rs.svg \
  -c 'record -F 997 --call-graph dwarf,16384 -g'
mv perf.data 20260418-075136/probe-redis-rs.perf.data

# folded stacks
perf script -i 20260418-075136/probe-ferriskey.perf.data | inferno-collapse-perf > 20260418-075136/probe-ferriskey.folded
perf script -i 20260418-075136/probe-redis-rs.perf.data  | inferno-collapse-perf > 20260418-075136/probe-redis-rs.folded
```

## Next

See `benches/perf-invest/report-envelope.md` for the per-item
analysis (round-1 inclusive % vs current inclusive %).
