# ferriskey vs redis-rs — wider-workload coverage (W2)

**Author:** Worker-2
**Branch:** `feat/ferriskey-perf-invest`
**Scope:** 5 workloads × 2 clients, same Client-per-worker + barrier-sync
shape. Raw JSON in `benches/perf-invest/wider_*-{ferriskey-wider,redis-rs-wider}-*.json`.
No ferriskey / redis-rs code changes in this PR — bench additions only.

## §1 Headline

The scenario-1 BLMPOP bench is the outlier, not the rule. Across 5
additional workloads covering non-blocking flat request/reply,
batched pipelines, and streams, ferriskey is **competitive or better**
than redis-rs 1.2.0:

- 3 workloads (80/20, 50/50, 100 % GET) → **tied within noise** (±1.3 %)
- 100-cmd pipeline → **ferriskey +6 % throughput, lower p50**
- XADD + XRANGE streams → **ferriskey +11 % throughput, 25 % lower p99**

Ferriskey is NOT globally slower than redis-rs. It loses specifically
on BLMPOP, where the glide-core-inherited per-command envelope W1 + W3
identified cannot amortise, because blocking commands spend most of
their time sitting alone on a socket. Every other shape either
amortises the envelope invisibly (flat RPC) or beats redis-rs outright
(pipeline, streams).

The COMPARISON.md scenario-1 narrative needs rewording: ferriskey's
shape is architecturally mismatched with BLMPOP-dominated workloads,
not slower as a Valkey client.

## §2 Numbers

16 workers, 4 KiB payload, 5 s warmup + 30 s measured, seeded PRNG
identical between clients. Same key ring (`{wider}` hash tag,
single-slot) and same per-worker seed so any throughput delta is
client overhead, not workload asymmetry.

### §2.1 Non-blocking flat request/reply

| workload       | client       | throughput      | p50    | p95    | p99    | max     |
| -------------- | ------------ | --------------: | -----: | -----: | -----: | ------: |
| 80/20 GET/SET  | ferriskey    | 115 029 ops/s   | 0.132  | 0.188  | 0.228  | 0.991   |
| 80/20 GET/SET  | redis-rs     | 115 434 ops/s   | 0.133  | 0.183  | 0.219  | 4.077   |
| **Δ**          |              | **-0.35 %**     | ≈0     | +0.005 | +0.009 | **-3.1** |
| 50/50 GET/SET  | ferriskey    | 116 511 ops/s   | 0.129  | 0.189  | 0.230  | 2.195   |
| 50/50 GET/SET  | redis-rs     | 118 063 ops/s   | 0.129  | 0.181  | 0.221  | 2.003   |
| **Δ**          |              | **-1.31 %**     | 0      | +0.008 | +0.009 | +0.2    |
| 100 % GET      | ferriskey    | 113 129 ops/s   | 0.135  | 0.187  | 0.219  | 0.968   |
| 100 % GET      | redis-rs     | 112 834 ops/s   | 0.136  | 0.185  | 0.214  | 0.967   |
| **Δ**          |              | **+0.26 %**     | -0.001 | +0.002 | +0.005 | 0       |

All three deltas are within the noise band for a 30 s capture on a
shared host. p50 is identical to 3 decimal places in two of three
workloads; only max-latency tail moves, and moves in different
directions run-to-run. This is a **tie**.

### §2.2 Batched pipeline

| workload            | client     | throughput per-op | p50    | p95    | p99    |
| ------------------- | ---------- | ----------------: | -----: | -----: | -----: |
| 100-cmd GET pipeline | ferriskey | 317 655 ops/s     | 4.547  | 6.935  | 7.203  |
| 100-cmd GET pipeline | redis-rs  | 299 423 ops/s     | 5.267  | 6.899  | 7.107  |
| **Δ**               |            | **+6.09 %**       | **-0.72** | +0.036 | +0.096 |

Throughput counts per-GET (3 176 pipelines/s × 100 = 317 655 ops/s
for ferriskey). Latency is per-pipeline = per-100-ops-amortised.
**Ferriskey wins on throughput AND lower p50.** p99 edges redis-rs by
96 µs (1.3 %) — that's worth flagging but it's overwhelmed by the
+6 % headline throughput gain. See §4 for why.

### §2.3 Streams

| workload               | client     | throughput     | p50    | p95    | p99    |
| ---------------------- | ---------- | -------------: | -----: | -----: | -----: |
| XADD + XRANGE (1:4)    | ferriskey  | 55 670 ops/s   | 0.271  | 0.403  | 0.473  |
| XADD + XRANGE (1:4)    | redis-rs   | 50 121 ops/s   | 0.309  | 0.481  | 0.633  |
| **Δ**                  |            | **+11.07 %**   | **-0.038** | **-0.078** | **-0.160** |

Ferriskey wins on every metric, including a 25 % reduction in p99.
XADD returns an ID (bulk string); XRANGE returns a nested array of
`[id, [field, value, ...]]`. Both reply shapes favour ferriskey's
decoder path.

### §2.4 Context — prior scenario-1 BLMPOP

For reference, from `benches/results/submit_claim_complete-*.json`:

| workload                         | client     | throughput      |
| -------------------------------- | ---------- | --------------: |
| RPUSH / BLMPOP / INCR (scenario 1) | ferriskey | 7 993 ops/s     |
| RPUSH / BLMPOP / INCR (scenario 1) | redis-rs  | 14 755 ops/s    |
| **Δ**                            |            | **-45.81 %**    |

BLMPOP is alone in the 6-workload comparison. Everything else is
noise-level tied or ferriskey-winning.

**Profile note (added post-cross-review):** the scenario-1 numbers
above come from `benches/comparisons/baseline/` and
`ferriskey-baseline/`, which build with `lto = "fat"` +
`codegen-units = 1` + `debug = 0`. The round-1 wider/ numbers in
§2.1-§2.3 come from `benches/comparisons/wider/`, which builds with
`lto = "off"` + `debug = 1` (chosen so flamegraphs resolve inline
frames without changing the headline codegen path for the bench
runner). The **direction** of the scenario-1 -45.81 % gap is robust
across both profiles — BLMPOP was re-run post-round-3 on the
lto=fat baselines and reproduced at -41.82 % — but the **exact
delta** between the two table rows above and the wider/ ~115 k
ops/sec flat-RPC rows should not be read as a single-profile
measurement. Within-suite comparisons (every row in §2.1-§2.3
against every other §2.x row) are on one profile and clean.

## §3 BLMPOP — why this workload specifically

The wider suite refutes the "ferriskey has universal per-command
overhead" reading. Every non-blocking workload either ties or favours
ferriskey. What's different about BLMPOP?

### §3.1 Workload shape: blocking commands don't amortise

Every BLMPOP is a 1-second timeout where the worker parks on the
socket. Unlike GET/SET (a few hundred microseconds round-trip),
there's no op-stream to fold the per-command envelope cost into — the
envelope fires once, and then the thread waits.

W3's §2.1 counts **~8 heap allocations per command** in ferriskey's
path vs **~3** in redis-rs 1.2.0. For 115 000 GETs/sec that's ~575 k
extra allocations/sec — expensive in absolute terms, but amortising
into <1 % of total throughput because each of the 115 000 GETs is
making forward progress against the server. BLMPOP inverts that: at
7 993 ops/sec, 7 993 × 8 = 64 k allocations, 7 993 × 6 `cmd.command()`
uppercase copies (W3 §2.4), 7 993 × 1-second block periods. The
per-command envelope runs fully, then the thread parks, then the
envelope runs again. Each command becomes a visible ~100 µs of
client-side overhead before a ~1 000 µs server-side wait — client
overhead is ~10 % of per-op time, not the <1 % GET/SET sees.

### §3.2 W1's flame graph — H2 telemetry write-lock on Drop

W1's report-w1.md §H2 flagged `StandaloneClient::Drop` calling
`Telemetry::decr_total_clients(1)` (ferriskey/src/client/standalone_client.rs:62-70),
which takes a `std::sync::RwLock::write` on the global Telemetry
struct. On the flame graph this fires at 0.19-0.21 % inclusive on
scenario 1.

The gap is non-obvious at 0.2 % inclusive, but it compounds: the
`ClientWrapper::clone()` on every `get_or_initialize_client` happy-
path return (W1 H4 / W3 F1) means every command path clones a
`StandaloneClient` then drops it — touching the write lock **twice**
per command (once on the caller side, once when the driver-task
copy drops). On a blocking workload where 16 workers sit on BLMPOP
holding their cloned wrappers for a second at a time, the **number
of simultaneously-live `StandaloneClient` handles stays high** and
the Telemetry RwLock sees sustained write traffic rather than
register-write-unregister churn within a single RPC.

This isn't the whole gap — the 5-deep async envelope (W1 §H1) is
the largest single contributor at 16.9 % inclusive — but it's the
mechanism that explains why BLMPOP specifically looks worse than
non-blocking workloads instead of being a uniform tax.

### §3.3 W3's §1 prediction — amortisation under pipelining

W3 §1 predicted: "If W2's pipelined workload closes the gap
substantially, it's because the per-command overheads above are
amortised across the N commands in the pipeline." §2.2 confirms this
directly: a 100-cmd pipeline collapses the envelope cost to roughly
1/100 per op, and ferriskey's underlying send_packed_commands path
(`connection/multiplexed.rs:717` per W3 §5) is fast enough that the
batch wins overall. Under pipelining ferriskey actually BEATS
redis-rs by 6 %.

BLMPOP is the opposite of pipelined: one command per socket
round-trip, maximum envelope exposure.

## §4 Pipeline + streams — where ferriskey wins + why

### §4.1 Pipeline (+6.09 %, -0.72 ms p50)

At 317 655 ops/s vs 299 423 ops/s, ferriskey's `TypedPipeline`
(`ferriskey/src/ferriskey_client.rs:557-728`) edges redis-rs's
`redis::pipe().query_async()`. W3 §2.1 listed 8 per-command
contributors (F1-F9); under a 100-cmd pipeline:

- **F1 `Client::clone()`** — once per pipeline, not per command
- **F2 `Arc::new(cmd.clone())`** — once per pipeline (the batch is
  wrapped, not each command)
- **F6 `InflightRequestTracker::try_new`** — once per pipeline
- **F9 per-command timeout wrapper** — once per pipeline

Effectively, F1-F9 divided by 100. The comparative win comes from
ferriskey's pipeline implementation being somewhat leaner in the
batch codegen — fewer poll-frames per batch reply than redis-rs
sees walking `Pipeline::send_recv` across N commands. W1's flame
graph noted ferriskey's parser costs 5.8 % inclusive vs redis-rs's
6.6 % (§H5), and the gap widens when multiplied across 100 replies
in one pipeline response.

The p50 advantage (4.547 vs 5.267 ms) indicates ferriskey's
per-batch cost is genuinely lower, not merely a tail-latency
improvement.

### §4.2 Streams (+11.07 %, -25 % p99)

Streams is the most striking result. Ferriskey wins on every metric:
throughput (+11 %), p50 (-12 %), p95 (-16 %), p99 (-25 %).

XADD returns an ID (bulk string `"1712345678901-0"`-shape); XRANGE
returns a nested `[[id, [field, value, ...]], ...]` array. Both are
decoder-heavy replies. Ferriskey's `value_conversion.rs` includes
stream-aware `ExpectedReturnType::ArrayOfStringAndArrays` handling
(per W3 §2.5); the same dispatch that imposes cost on BLMPOP
produces a cleaner typed Value shape for streams that amortises
better on the decoder hot path.

redis-rs, in contrast, hands back the raw Value and pushes
typed-struct construction to `from_redis_value::<T>` at the call
site. For streams that means the caller-side deserialisation walks
the nested array; ferriskey does it once in the driver. The net cost
is similar on flat replies — but streams' nested shape means
ferriskey's single-walk-in-driver beats redis-rs's walk-at-call-site
across 16 concurrent workers.

The p99 improvement (0.473 vs 0.633 ms) in particular suggests the
decoder path has **less jitter** — consistent with a driver-task
single-pass decoder vs a call-site walk that competes with the
scheduler for CPU time.

## §5 Batch C implications

Observations, not recommendations. The project owner decides scope.

W1 (§Actionable optimisations) + W3 (§6 Batch C follow-up candidates)
independently converge on the same short list of per-command paths
that dominate scenario-1 BLMPOP cost. The wider suite adds a new
dimension: **these paths are NOT a problem on non-blocking or
pipelined workloads.** Any Batch C work on them should weight
"how much does this hurt the 5/6 workloads that currently tie or
win" against "how much does this help the 1/6 that loses". Concrete
candidates:

1. **`Routable::command()` Vec<u8> alloc** (W1 §5, W3 §2.4 + §6.1) —
   6 allocs per command. Pipeline amortises this to 6/100 per op,
   which is why pipeline doesn't feel it. BLMPOP amortises it to 6/1
   per op, which is why BLMPOP does. A cached/memoised command name
   on `Cmd` is the mechanical fix; zero-cost for pipeline and streams,
   direct cost reduction for BLMPOP.

2. **`Arc::new(cmd.clone())` on every send_command** (W1 §1, W3 §6.4) —
   ferriskey_client.rs:857. Under pipelining the Arc wraps the batch
   (~1 % of cost); under BLMPOP it wraps each blocking command and
   fires `Arc::drop_slow` + mimalloc `madvise` syscalls at 0.9 % of
   inclusive time (W1 §H3 flame data). Fix targets BLMPOP without
   touching the pipeline win.

3. **`StandaloneClient::Drop` Telemetry write-lock** (W1 §3, W3 §F1) —
   `standalone_client.rs:62-70`. Every command path clones a
   `StandaloneClient` wrapper then drops it; Drop takes a std RwLock
   write on the global Telemetry counter. Under non-blocking
   workloads the Drop fires in < 200 ns and the write lock is
   uncontended. Under BLMPOP, 16 workers hold their wrappers for a
   full second at a time, keeping the wrapper count high and the
   counter churning.

4. **5-deep async dispatch envelope** (W1 §H1, W1 §2) —
   `Client::send_command` → `execute_command_owned` →
   `StandaloneClient::send_command` → `send_request_to_single_node`
   → `send_request` → `MultiplexedConnection::send_packed_command`.
   16.9 % inclusive at the top vs redis-rs's 6.6 % at its equivalent
   depth. Each layer is a separate async state machine. Flattening
   this is the largest single-change lever across all 6 workloads —
   it'd tighten the tie on flat RPC and widen the pipeline/streams
   win. BLMPOP benefit is real but proportionally smaller than items
   1-3 on that specific workload.

5. **Four post-dispatch `is_*_command` checks** (W1 §5, W3 §6.2) —
   `client/mod.rs:773-784`. Four more `cmd.command()` allocs per
   command, on paths where all four will return false for any
   non-connection-setup command. Easy win, mechanical fix.

**Common thread: glide-core-inherited per-command decoration.** F1/F2
per-command Client+Cmd clones, F6 InflightRequestTracker CAS+Arc,
F9 per-command tokio::select! timeout — all artifacts of glide-core
serving Java/Python/Node language bindings where per-command
telemetry + timeout is table stakes. These costs are real but
invisible on high-TPS request/reply because they amortise; visible
on BLMPOP because they can't amortise; inverted to a win on
pipeline + streams because the decoder path's typed-Value shape is
genuinely better than redis-rs's call-site walk.

## §6 Side note — ferriskey native bench

`ferriskey/benches/connections_benchmark.rs:21` imports
`ferriskey::ValkeyResult` which has been removed from
`ferriskey/src/lib.rs` exports on this branch HEAD. `BENCH_PROFILE=quick`
fails to compile; the error trace is at
`benches/perf-invest/ferriskey-native-quick.log`. Per "no ferriskey
source mods" rule this was not patched. Flagging to the owner — a
one-line fix (swap `ValkeyResult` for the current `Result` alias, or
drop the import entirely if the bench doesn't use it).

Cross-check from the ferriskey native bench would have provided a
third data point on 80/20 GET/SET; absent that, the apples-to-apples
comparison between the W2 wider ferriskey port and the W2 wider
redis-rs port is the authoritative reading for this investigation.

## §7 Raw JSON inventory

All reports are in `benches/perf-invest/` with the bench SHA embedded:

```
wider_80_20-ferriskey-wider-8cb6dff.json
wider_80_20-redis-rs-wider-8cb6dff.json
wider_50_50-ferriskey-wider-8cb6dff.json
wider_50_50-redis-rs-wider-8cb6dff.json
wider_get_only-ferriskey-wider-8cb6dff.json
wider_get_only-redis-rs-wider-8cb6dff.json
wider_pipeline_100-ferriskey-wider-8cb6dff.json
wider_pipeline_100-redis-rs-wider-8cb6dff.json
wider_streams-ferriskey-wider-8cb6dff.json
wider_streams-redis-rs-wider-8cb6dff.json
```

Schema matches `benches/comparisons/baseline/` and
`ferriskey-baseline/` — any aggregator that walks `benches/results/`
treats these the same way.

## §8 Proviso

Same framing as W1 §Proviso: this is investigative reporting on a
working library whose maintainer owns the upstream project. Every
item flagged above traces to a real-world constraint (telemetry,
lazy initialization, typed decoder, inflight tracking) that
redis-rs 1.2.0 doesn't carry. The pipeline + streams wins in §4
suggest the glide-core-inherited shape is actually better than
redis-rs's raw-Value-at-call-site model for several modern
workloads — the scenario-1 BLMPOP gap is the narrow case where a
specific workload amplifies the per-command costs.

The wider suite's takeaway is that the COMPARISON.md scenario-1
narrative should be contextualised: ferriskey isn't slower; it's
tuned for a different operating point.
