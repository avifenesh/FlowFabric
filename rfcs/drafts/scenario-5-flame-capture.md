# Scenario 5 (suspend_signal_resume) tail regression — flame capture

**Issue:** #173
**Baseline source:** `benches/results/baseline.md` §Scenario 5
**Investigated SHA:** d595b27 (v0.4.0 post PR #170)
**Prior baseline SHA:** v0.3.2 (da89fa9)
**Author:** Worker-SSSS (single-agent)
**Status:** Analysis + recommendation. No PR opened — fix is small but
empirical measurement was blocked by criterion harness behaviour in
this environment (see §Empirical blockers). Recommending a 20-LOC
attr-level fix on a follow-up PR once a clean rerun is possible.

---

## 1. Reported regression

| metric | v0.4.0 | v0.3.2 | delta |
|--------|--------|--------|-------|
| p50 ms |  9.36  |  9.23  | +1.4% |
| p95 ms | 10.83  |  9.93  | +9.1% |
| p99 ms | 11.96  | 10.76  | +11.2% |
| ops/s  | 106.87 | 108.34 | -1.4% |

Tail-only shift. p50 inside noise. Documented as non-release-blocking
against the v0.1.0 floor per `feedback_perf_honesty.md`.

## 2. Prime suspect — PR #170

PR #170 added `#[tracing::instrument]` to 19 `*_impl` + one `fcall`
function in `ff-backend-valkey` at the **default `info` level** with
3–4 `Display`-valued fields (`execution_id`, `attempt_id`,
`lease_epoch`, ...). Default server filter is
`ff_server=info,ff_engine=info,ff_script=info,tower_http=debug,audit=info`
(no `ff_backend_valkey` directive), so the global crate default
(`info`) applies and every span is live — subscribed to and allocated.

## 3. Call-graph of the measured window

Scenario 5 measures:

```
t0 = Instant::now()
task.suspend(...)                 # direct FCALL — NOT instrumented
GET /v1/executions/{}/pending-waitpoints
POST /v1/executions/{}/signal     # server-side — no *_impl hop
loop { worker.claim_next() }      # direct FCALL — NOT instrumented
task.complete(None)               # ↓ goes through EngineBackend::complete
t0.elapsed()
```

Only **`complete_impl`** fires inside the measured window (one span
per iteration). Cross-checked:

- `task.suspend` calls `self.client.fcall("ff_suspend_execution", …)`
  directly (`crates/ff-sdk/src/task.rs:898`). It does not dispatch
  through `EngineBackend::create_waitpoint` → `create_waitpoint_impl`
  is NOT exercised.
- `worker.claim_next` uses raw `ZRANGEBYSCORE` + FCALL
  (`crates/ff-sdk/src/worker.rs:678-780`). No `*_impl` hop.
- `task.complete` calls `self.backend.complete(…)` → `complete_impl`
  (instrumented).
- `task.resume_signals` is NOT called in this bench; `observe_signals_impl`
  is cold.
- `renew_impl` fires from the background renewal task every
  `lease_ttl_ms/3 = 10_000ms`. With iter ~9ms, ~1 tick per ~1000
  iters — not a hot contributor.
- `ff-server` does NOT hold a `dyn EngineBackend`
  (`grep "EngineBackend" crates/ff-server/src`: no impl, only error
  wrapping). HTTP POST /signal goes through server-local script
  paths, not through the instrumented `*_impl` surface.

**Hot instrumented fn: `complete_impl` (1 call/iter).**

## 4. Span-cost arithmetic

`tracing::instrument` at info level with `skip_all` + 3
`Display`-valued fields costs:

- Callsite interest check: ~20 ns (cached)
- Span alloc + value registry insert: ~200–500 ns
- `%f.execution_id` + `%f.attempt_id` + `%f.lease_epoch` formatted via
  `Display` into the span's field store: ~100–300 ns each (UUID
  itoa/u64 fmt + allocation for the string slot)
- `enter` / `exit` guard drops: ~20 ns each

Budget per call: **~1–2 µs** worst case under the default
`fmt::Subscriber` (which does NOT emit on span open/close by default;
`FmtSpan::NONE` is the default).

One `complete_impl` call per iter × 100 iters = ~200 µs total
overhead spread across 100 samples. **This cannot cause a ~0.9 ms
p95 shift on its own.**

## 5. Alternative hypotheses

| Hypothesis | Verdict |
|------------|---------|
| `complete_impl` span accumulation | Rejected — too cheap. |
| `backend_context(err, label)` wrap | Rejected — happy path is zero-cost closure (`map_err` only enters on `Err`). |
| `ff-observability::Metrics` handle deref | Rejected — `observability` feature is OFF by default; shim is a no-op. `inc_lease_renewal` only fires in `renew()`, not `complete()`. |
| `connect_inner` refactor | Rejected — called once at worker setup, outside the measured window. |
| Rare `complete_impl` span × tokio scheduler interaction at p99 | Plausible but small. |
| **N=1 methodology variance** | **Most likely.** baseline.md §methodology: "p95/p99 are computed from 100 per-iteration samples inside one criterion run." No between-run averaging. The +9.1%/+11.2% is inside the single-run tail-noise band documented at §176: "Recommend: follow-up flame capture on one suspend_signal_resume run to attribute the tail drift". |

The v0.1.0 floor for p95 is 10.59 ms (baseline.md:151). v0.4.0 at
10.83 is +2.3% above the two-release floor — well inside the
documented `≥ 5% regression ⇒ investigate; ≥ 10% ⇒ block` policy
band against the FLOOR, even though the delta against v0.3.2
crosses the p95 threshold.

## 6. Empirical blockers

Planned: reproduce N=5 on d595b27, then ablate by stripping the
`complete_impl` instrument attr and re-running.

Actual: the isolated worktree ran `ff-server` cleanly against the
existing 4-partition Valkey; the criterion harness started warmup
but no sample rows landed within 8-minute windows across three
attempts. Server logs confirm exactly one POST /v1/executions
during the run (12:12:15Z) followed by silence — the bench's
`claim_next` tight-loop (`claim_poll_interval_ms = 1`) appears to
never observe the newly-created execution on the eligible ZSET.
Hypothesis: partition layout mismatch between the reused Valkey
instance and the bench worker's `num_flow_partitions=4` view.
Not pursued further — the bench was last known-good on the EPYC
bench host, and local criterion variance is not a safe substitute
for the release-gate host anyway.

## 7. Recommended fix

Downgrade hot-path `#[tracing::instrument]` attrs from the default
`info` to `debug` on the four `*_impl` functions the SDK hot path
touches, so span creation compiles out under the default
`ff_*=info` filter:

```rust
// crates/ff-backend-valkey/src/lib.rs
#[tracing::instrument(
    name = "ff.complete",
    level = "debug",          // ← add
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn complete_impl(…) { … }
```

Apply to: `complete_impl`, `renew_impl`, `progress_impl`,
`observe_signals_impl`, `append_frame_impl`. (~25 LOC across 5
attrs.) Leave `describe_execution_impl`, `describe_flow_impl`,
`list_edges_impl`, `describe_edge_impl`, `cancel_flow_fcall`,
`cancel_impl`, `fail_impl`, `report_usage_impl` at `info` — these
are operator-observed control-plane ops, not worker hot path, and
the span is load-bearing for incident response.

Rationale:
1. Closes a cost gap regardless of whether it is the true cause of
   the Scenario 5 shift — span cost ≈ 0 under default production
   filter is strictly better than ~1–2 µs.
2. Preserves audit coverage: operators who want the spans set
   `RUST_LOG=ff_backend_valkey=debug` to surface the worker-hot
   ops; the attrs are still present.
3. Parallel to how `renew_lease` in `ff-sdk/src/task.rs:1029` is
   already scoped via `#[tracing::instrument(name = "renew_lease",
   skip_all, ...)]` — the Round-7 work kept renewal at a dedicated
   span for bench harness `on_enter / on_exit` layers.

## 8. Deferred: metric-handle caching

If a future bench on the canonical EPYC host still shows the Scenario
5 tail drift after the `level = "debug"` downgrade, the next suspect
is the `metrics: Option<Arc<Metrics>>` field on `ValkeyBackend` —
every `renew()` trait call does an `Option` match + `Arc` deref. Not
a per-iter hit in Scenario 5, but on the capability-routed Scenario
5 variant (`cap_routed.rs`) where renewal is continuous, this
matters. Cache the handle at `connect_with_metrics` time into a
non-Option `Arc<Metrics>` where the shim provides the no-op default.
~40 LOC, separate PR, gated on re-measurement.

## 9. Next step

Open a follow-up PR (label `perf`, not release-blocking) with the
§7 attr-level downgrade only. Reviewer verification: on the EPYC
bench host, re-run `cargo bench --bench suspend_signal_resume` at
N=5 and confirm p95 ≤ v0.3.2's 9.93 ms + 5% = 10.43 ms. If the
shift persists after the downgrade, escalate to the §8 metric-cache
work and file a fresh investigation ticket against the new SHA.
