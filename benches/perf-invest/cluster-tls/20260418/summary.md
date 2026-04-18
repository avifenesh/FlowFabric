# Round-2 cluster+TLS vs round-1 localhost — summary

Side-by-side numbers, same 6 workloads, same 16 workers × 30 s, same
HdrHistogram methodology. Localhost column is from
`benches/perf-invest/wider_*-8cb6dff.json`; cluster+TLS column is
from this folder.

All cluster runs: strict TLS verification (aws-lc-rs +
webpki-roots), single-slot hash tags, 30 s calm + per-master
FLUSHALL between pairs. Zero errors, zero CROSSSLOTs across all 12
runs.

## Throughput (ops/sec) + p50 / p99 latency (ms)

| workload      | client    | cluster+TLS               | localhost               | cluster Δ vs redis |
| ------------- | --------- | ------------------------- | ----------------------- | -----------------: |
| 80/20 GET/SET | ferriskey | 25 312 / p50 0.627 / p99 0.748 | 115 029 / 0.132 / 0.228 | **-1.42 %** |
| 80/20 GET/SET | redis-rs  | 25 678 / p50 0.619 / p99 0.724 | 115 434 / 0.133 / 0.219 | — |
| 50/50 GET/SET | ferriskey | 25 899 / p50 0.611 / p99 0.725 | 116 511 / 0.129 / 0.230 | **-0.62 %** |
| 50/50 GET/SET | redis-rs  | 26 059 / p50 0.608 / p99 0.716 | 118 063 / 0.129 / 0.221 | — |
| 100 % GET     | ferriskey | 25 108 / p50 0.636 / p99 0.726 | 113 129 / 0.135 / 0.219 | **-0.27 %** |
| 100 % GET     | redis-rs  | 25 176 / p50 0.633 / p99 0.735 | 112 834 / 0.136 / 0.214 | — |
| 100-cmd pipe  | ferriskey | 166 834 / p50 9.231 / p99 12.679 | 317 655 / 4.547 / 7.203 | **-1.33 %** ← was +6.09 % lhost |
| 100-cmd pipe  | redis-rs  | 169 091 / p50 9.127 / p99 12.551 | 299 423 / 5.267 / 7.107 | — |
| streams       | ferriskey |  18 758 / p50 0.834 / p99 1.247 |  55 670 / 0.271 / 0.473 | **+0.49 %** ← was +11.07 % lhost |
| streams       | redis-rs  |  18 666 / p50 0.837 / p99 1.279 |  50 121 / 0.309 / 0.633 | — |
| BLMPOP        | ferriskey |   3 294 / p50 1.203 / p99 2.685 |   7 993 / — / —         | **-32.50 %** ← was -45.81 % lhost |
| BLMPOP        | redis-rs  |   4 880 / p50 1.214 / p99 3.143 |  14 755 / — / —         | — |

## Quick read

- **Flat request/reply workloads**: Δ within ±1.5 % (noise) under
  cluster+TLS, same as localhost was within ±1.3 %. Two environments
  concur: these workloads are client-neutral.

- **Pipeline + streams**: ferriskey's localhost wins disappear.
  Pipeline +6 % → -1.3 %. Streams +11 % → +0.5 %. In both cases the
  per-op cost collapses onto network+TLS RTT.

- **BLMPOP**: the only workload with a persistent gap. Compresses
  from -45.81 % (localhost) to -32.50 % (cluster+TLS) but does not
  close. On cluster+TLS: **p50 is tied**, the gap lives entirely in
  the tail + overall throughput. See `report-w2-round2.md §3` for
  the mechanism hypothesis.

## Why the scaled-down numbers

Absolute ops/sec drops ~78 % across every non-blocking workload
(115 k → 25 k for flat RPC, 318 k → 167 k for pipeline). That's
network RTT at ~0.6 ms per round-trip absorbing everything the
client used to contribute. Pipeline drops less (-47 %) because
batch-RTT amortises the network cost across 100 commands.

Streams drops -66 % — in between flat and pipeline, consistent with
the heavier-reply shape: streams' XRANGE returns ~40 KiB per reply
vs GET's 4 KiB, so TCP+TLS framing overhead scales more slowly than
ops/s does.

## TLS + cluster operational findings

1. **rustls 0.23 requires an explicit `CryptoProvider` install before
   first TLS op.** redis-rs 1.2.0's `tls-rustls-*` features don't
   pick one, so the bin panics inside `ClientConfig::builder` if we
   don't call `rustls::crypto::aws_lc_rs::default_provider().
   install_default()` from the bench. ferriskey internally pins
   `aws-lc-rs` so this would have bitten it too on its own; our
   shared init matches both clients onto one provider for a fair A/B.
   Likely upstream-worthy for redis-rs.

2. **Strict cert verification holds on both clients.** No fallback
   to insecure was needed. webpki-roots as the trust store is
   deterministic across hosts.

3. **Per-master FLUSHALL discipline via `flush-cluster.sh`** — reads
   `CLUSTER NODES`, touches every master, verifies DBSIZE=0. All 6
   pairs start from a clean cluster. No leakage observed.
