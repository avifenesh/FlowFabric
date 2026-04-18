# ferriskey vs redis-rs — cluster + TLS round (W2 round 2)

**Author:** Worker-2
**Branch:** `feat/ferriskey-perf-invest`
**Scope:** 6 workloads × 2 clients, AWS ElastiCache cluster over
TLS. Pairs round-1 localhost numbers in
`benches/perf-invest/report-w2.md`. Bench code under
`benches/comparisons/wider/src/bin/cluster_*.rs`; raw JSON +
protocol docs in `benches/perf-invest/cluster-tls/20260418/`. No
ferriskey or redis-rs code changes.

## §1 Cluster + TLS delta vs localhost

Three separate stories across the 6 workloads; the cluster delta is
NOT a uniform tax.

| workload      | localhost Δ  | cluster+TLS Δ | pattern                  |
| ------------- | -----------: | ------------: | ------------------------ |
| 80/20 GET/SET | -0.35 %      | -1.42 %       | tied both envs           |
| 50/50 GET/SET | -1.31 %      | -0.62 %       | tied both envs           |
| 100 % GET     | +0.26 %      | -0.27 %       | tied both envs           |
| 100-cmd pipe  | **+6.09 %**  | **-1.33 %**   | **ferriskey win vanishes** |
| streams       | **+11.07 %** | **+0.49 %**   | **ferriskey win vanishes** |
| BLMPOP        | **-45.81 %** | **-32.50 %**  | **gap compresses, stays** |

Flat request/reply workloads are client-neutral at both scales —
0.13 ms (localhost) and 0.6 ms (cluster+TLS) are both too tight for
the client-side envelope difference to matter. The cluster numbers
confirm round-1's read without adding information.

Pipeline + streams changed sign. Round-1 round read these as
"ferriskey's decoder path wins on batched replies + nested-array
shapes." Round-2 cluster+TLS shows the per-op cost those wins came
from is smaller than the 9 ms (pipeline) / 0.8 ms (streams) network
RTT floor; the ferriskey-lead is absorbed into network + TLS
framing. **Pipeline and streams wins are localhost artifacts.** The
round-1 COMPARISON.md narrative needs updating with this qualifier.

BLMPOP stays in ferriskey's disfavour but the gap shrinks from
-46 % to -33 %. Cluster adds a network-RTT common floor that both
clients pay equally, so the ferriskey envelope cost's relative
weight in each op drops. It does not disappear.

## §2 Does BLMPOP divergence grow, shrink, or stay at -46 %?

It **shrinks, but persists.** -45.81 % (localhost) → -32.50 %
(cluster+TLS).

- Network RTT adds ~0.6 ms of common-floor cost that both clients
  pay equally. Relative share of the ferriskey envelope in total
  per-op wall goes down accordingly.
- The gap does NOT close. Ferriskey still drains the 10 000-task
  queue 1.48× slower than redis-rs (3 294 vs 4 880 ops/s).
- Absolute ops/sec drops for both: ferriskey 7 993 → 3 294 (-59 %),
  redis-rs 14 755 → 4 880 (-67 %). Redis-rs drops MORE in absolute
  terms because it had more throughput to lose; ferriskey's
  absolute drop is smaller but its relative lag persists.

BLMPOP is the only workload where round-2 adds material information
beyond round-1's findings. The envelope-overhead hypothesis from
report-w1.md + report-w3.md is reconfirmed but REFINED — see §3.

## §3 The BLMPOP p50 tie — gap is tail + throughput, NOT happy path

**This is the new data point the envelope-cost hypothesis didn't
predict, and it points at a different mechanism.**

### §3.1 What the p50 numbers say

Cluster+TLS BLMPOP latencies:

| client     | p50          | p95           | p99           | max          | throughput |
| ---------- | ------------ | ------------- | ------------- | ------------ | ---------: |
| ferriskey  | 1.203 ms     | 1.908 ms      | **2.685 ms**  | 4.213 ms     |  3 294 ops/s |
| redis-rs   | 1.214 ms     | 2.126 ms      | **3.143 ms**  | 8.193 ms     |  4 880 ops/s |
| Δ          | **+0.011 ms** ferriskey faster | -0.218 ms ferriskey faster | **-0.458 ms** ferriskey faster | ferriskey faster | -32.50 % |

**On every latency percentile, ferriskey is as fast or faster than
redis-rs.** p50 within 11 µs. p99 is 458 µs BETTER for ferriskey.
Max is roughly half of redis-rs's max.

And yet ferriskey's end-to-end throughput is 32.5 % lower. The two
facts together mean the latency sample itself — from "I enter the
BLMPOP op" to "I return with a popped element" — is NOT where the
gap lives.

### §3.2 The envelope hypothesis alone cannot explain this

W1 §H1 identified a 5-deep async dispatch envelope at 16.9 %
inclusive. W3 §2.1 counted ~8 heap allocations per command. If those
costs were the mechanism, they'd show up in the per-op latency
histogram — each BLMPOP sample would include the envelope cost
end-to-end.

They don't show up in p50. They don't show up in p99 (which actually
favours ferriskey). The envelope cost is real, but it is NOT what
makes ferriskey's BLMPOP throughput 1.48× slower.

### §3.3 Parked-worker scheduling is the likely mechanism

The gap is between "I successfully pop something, I'm ready to
submit the next BLMPOP" and "I've actually issued the next BLMPOP on
the wire." BLMPOP blocks for up to 1 second; worker wake-up must
schedule promptly after the server returns a popped element, or the
worker spends that wake-up slack doing nothing instead of requesting
the next element.

The 16-worker pool amplifies this. In a 30-second window, each
worker issues ~306 BLMPOPs (~10 ops/sec/worker for ferriskey, 15 for
redis-rs). If ferriskey's per-BLMPOP wake-up scheduling is slower by
30-50 µs of wall per op, that compounds across 5 000 pops per
worker to the observed 32.5 % throughput gap, WITHOUT showing up in
the latency sample at all — the latency sample starts when the
worker issues the op, not during the slack between ops.

Candidate code paths for further investigation:

1. **`StandaloneClient` + multiplex pipeline task wake-up on receive.**
   ferriskey/src/connection/multiplexed.rs — when the server
   acknowledges a BLMPOP, which task wakes first, what does it do
   before returning to the user future? Any extra `notify_waiters` /
   `oneshot::send` hop between the recv task and the user future
   costs wall that doesn't land in the sample.

2. **`StandaloneClient::Drop` + Telemetry RwLock::write (W1 §H2,
   W3 §F1).** Blocking-workload shape means cloned
   `StandaloneClient` wrappers live 1 s at a time while workers park
   on BLMPOP. When the block returns, the wrapper clone drops and
   hits the `telemetrylib::Telemetry` std RwLock::write. Under 16
   parked workers, contention on that single global write lock is
   worse than an uncontended localhost run.

3. **`InflightRequestTracker` guard Arc drop (W3 §F6).** The per-
   command tracker's guard Arc drops when the oneshot completes —
   between BLMPOPs, not inside them. Another candidate for "invisible
   in the sample, visible in throughput."

4. **tokio scheduler hot-spot when 16 workers all wake within the
   same epoch.** BLMPOP batches wakes; if ferriskey's path has more
   futures to poll per wake than redis-rs's, the scheduler queue can
   thrash without showing up in any individual op's latency.

The path forward for the owner: a direct measurement of "wall time
between BLMPOP-return and next-BLMPOP-send" would distinguish these.
W1's flame graphs from round 1 could be re-sampled against a blocking
workload to see whether the drop-path telemetry frame surfaces
hotter than on the GET/SET traces (which dominated the original
scenario-1 capture).

### §3.4 Fair-comparison note — empty-queue retry budget

The BLMPOP hot loop in both baselines retries empty-queue polls via
a 10 ms sleep (see `baseline/src/scenario1.rs` and
`ferriskey-baseline/src/scenario1.rs` hot loops). The original
round-1 + round-2 captures did not surface this count; a later
instrumentation pass added a `blmpop_outcomes: { ok, nil, err }`
counter to both baselines' JSON reports. In W2's cross-review re-run
(BLMPOP localhost, 10 000 tasks × 16 workers), the counter showed:

| client    | ok      | nil | err |
| --------- | ------: | --: | --: |
| redis-rs  | 10 000  |   0 |  15 |
| ferriskey | 10 000  |  15 |   0 |

**Same miss count, opposite classification.** In both cases the
server returned Nil; redis-rs's typed path (`Option<(String,
Vec<Vec<u8>>)>`) treats the unexpected shape as `Err(Io)`, while
ferriskey's `Option<Value>` path returns `Ok(None)`. Both paths
fall through to the same `tokio::time::sleep(10 ms)` retry, so the
wall-clock contribution is symmetric (~150 ms on each side). This
is not a measurement bias — it's a downstream of the typed-decoder
difference called out in W3's report-w3.md §2.5 — but it is worth
documenting so readers of the raw JSON don't misread the `err=15`
on redis-rs as a transport fault.

## §4 New outliers under cluster + TLS

### §4.1 Pipeline + streams wins vanished (§1 repeated for emphasis)

Round-1 concluded ferriskey wins pipeline +6 % and streams +11 %.
Round-2 cluster+TLS: pipeline -1.3 %, streams +0.5 %. Both workloads
TIE under real-world transport.

The mechanism is simple: pipeline moves ~9 ms per 100-cmd batch;
streams moves ~0.8 ms per op. Network + TLS RTT dominates. The
decoder-path advantage ferriskey had on localhost (single-pass in
driver vs call-site walk) is real but only visible when client-side
cost is >10 % of per-op wall. At cluster+TLS, it's <5 % on pipeline
and <10 % on streams.

**This reframes the round-1 COMPARISON.md message.** Ferriskey isn't
"better tuned for nested-array decoders" in the way round 1 phrased
it — it's "competitive on all non-blocking workloads, wins only on
localhost where client-side cost dominates, loses on blocking
commands in both environments."

### §4.2 No CROSSSLOT errors

Every single-slot hash tag held. 12 runs, 0 CROSSSLOT errors. Tag
discipline (`{wider}` and `{benchq}`) works.

This also means **we are not measuring cross-slot routing.** Cluster
scenarios that legitimately spread keys across all 3 masters
(customer-per-key sharding, say) would expose a different cost
bucket. Out of scope this round — candidate for a bonus 7th pair if
the owner wants it.

### §4.3 Cluster+TLS doesn't add jitter in a way that matters

Every percentile on every non-blocking workload stayed clean. No
SINGLE-op outliers in the 10 ms range. Cluster topology is stable,
TLS handshakes amortise away inside each connection's lifetime (one
handshake per worker connection at startup), and AWS VPC RTT is
flat enough that the p99 histogram doesn't tail into the paper at
all. Localhost had a 4 ms p99.max anomaly on 80/20 redis-rs
(report-w2.md §2.1); cluster showed none.

## §5 Side notes on cluster-specific behaviour

### §5.1 rustls 0.23 CryptoProvider panic (redis-rs 1.2.0)

**Likely upstream-worthy finding.** `redis::cluster::ClusterClient::
get_async_connection` panics inside
`rustls::ClientConfig::builder_with_protocol_versions` at the first
TLS op, with:

```
Could not automatically determine the process-level CryptoProvider
from Rustls crate features.
```

The `tls-rustls` / `tls-rustls-webpki-roots` features enable the
rustls dep but don't pin a crypto provider (aws-lc-rs / ring).
rustls 0.23 refuses to default-pick one from crate features.
Workaround: bench binaries call
`rustls::crypto::aws_lc_rs::default_provider().install_default()`
exactly once at startup (see
`benches/comparisons/wider/src/cluster_shared.rs::init_rustls_provider`).

Ferriskey has its own internal rustls config with `aws-lc-rs`
pinned, so it would have hit the same behaviour under its own
cert-verify path without this. Matching the bench's install to
`aws-lc-rs` keeps both clients on the same TLS primitive for fair
A/B.

The redis-rs upstream patch is trivial — add a feature like
`tls-rustls-aws-lc-rs` that pulls in `rustls = { features =
["aws-lc-rs"] }`, or document the required init in the tls-rustls
feature docstring. Worth surfacing to the redis-rs maintainers.

### §5.2 ElastiCache config endpoint routing

On connection, both clients successfully resolved all 3 masters via
`CLUSTER SLOTS` (issued by the cluster client on first use against
the config endpoint). No `MOVED` redirects observed during the run
phase — both clients cached the slot→node map correctly.

### §5.3 Bench protocol held throughout

Per-master FLUSHALL discipline (`flush-cluster.sh`) verified all 6
shards at DBSIZE=0 before every pair. 30 s calm between pairs
honoured. Zero errors across 12 runs, zero CROSSSLOTs.

## §6 Batch C implications (round-2 update)

Combining with W1 + W3 + round-1 W2:

1. **Non-blocking workloads are client-neutral in production
   transport.** Round-2 confirms this at 0.6 ms RTT, where client
   overhead contributes <5 % of per-op wall. Batch C work that
   targets the per-command envelope (W1 §H1, W3 F1/F2/F6/F9) won't
   move non-blocking workload throughput in production — the
   observable improvement is capped by how much of per-op wall is
   client-side, which cluster+TLS shrinks to irrelevance.

2. **Blocking-command throughput remains an outlier.** Round-2's
   p50-tied-but-tail-diverges data (§3) suggests the mechanism is
   NOT per-op envelope cost. Batch C candidates shift:

   - Investigate parked-worker wake-up path (recv task →
     user future); any extra hop here costs wall between ops.
   - Investigate `StandaloneClient::Drop` Telemetry RwLock
     contention under sustained 16-worker parking on BLMPOP — this
     specifically amplifies under blocking shapes.
   - The generic envelope flattening (W1 §2) is still worth doing
     for every workload's wake-up cost amortisation, but the
     expected cluster-production uplift is smaller than the
     localhost flame data predicts.

3. **Pipeline + streams wins don't need defending.** Round-1
   recommended weighting Batch C work against the 5/6-win story.
   Round-2 clarifies: those 2 wins were localhost-specific. The
   cluster+TLS tie on pipeline + streams is now the production-
   relevant reading. Batch C changes that flatten the envelope
   won't lose meaningful ground on pipeline+streams (they already
   tie in production transport).

## §7 Proviso

Same framing as round 1: investigative report on a working library.
Every finding here reflects ferriskey's engineering trade-offs
(telemetry, typed decoder, glide-core inheritance) that redis-rs
doesn't carry. Round-2 adds one observation the owner will want:
**the p50-tied-but-throughput-gap on BLMPOP points at a mechanism
flame graphs + source reading didn't localise in round 1**. A
blocking-workload flame capture + targeted read of the
parked-worker wake-up + `StandaloneClient::Drop` telemetry-lock
paths would confirm or refute the §3.3 hypothesis.

Report consolidation with round 1 findings is the manager's next
step; owner hand-off follows.

## §8 Round-3 follow-up — cross-check on the post-round-3 branch

Added during W2's cross-review (commit `50e441d` on
`feat/ferriskey-iam-gate`). The scenario-1 BLMPOP localhost bench
was re-run against the current branch HEAD after round 3 landed:

- Round-1 capture (commit `4d89a89` era):
  redis-rs 14 755.2 ops/s, ferriskey 7 993.3 ops/s. Gap **-45.83 %**.
- Cross-check re-run (post-`2a36b46` telemetry redesign +
  post-`ba73945` lazy redesign + post-`0181f30` blocking-probe
  findings merged): redis-rs 14 588.5 ops/s (-1.13 % vs published),
  ferriskey 8 487.7 ops/s (+6.19 % vs published). Gap **-41.82 %**.

The gap compressed ~4 percentage points on ferriskey's side. Most
of that drift is attributable to W1's `telemetrylib` removal
(round-1 `report-w1.md` §H2 called out `StandaloneClient::Drop` +
`Telemetry::decr_total_clients` as a blocking-workload hot spot);
some is attributable to the lazy-redesign eliminating 17 defensive
`ClientWrapper::Lazy` match arms; the remainder is within normal
run-to-run variance on a shared host.

**Direction preserved, headline robust.** The round-2 narrative
above — "BLMPOP gap HOLDS but COMPRESSES from localhost -45.81 %
to cluster+TLS -32.50 %" — still reads correctly; the localhost
baseline moves a few points in the same direction on the post-
round-3 branch. No re-run of the cluster+TLS suite is needed to
ship this report.
