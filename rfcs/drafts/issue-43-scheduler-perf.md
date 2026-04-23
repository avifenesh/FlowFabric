# Issue #43 scheduler perf investigation (2026-04-23)

## Issue summary

Title: *ff-scheduler: claim_for_worker scans all partitions per request — consider bounded scan + observability*

Framing (verbatim abridged):

> `ff_scheduler::Scheduler::claim_for_worker` iterates all execution partitions
> (default 256) sequentially per call, checking budget + quota + capability
> admission per partition until it finds an eligible execution. Each partition
> check is a handful of Valkey round-trips.
>
> Pre-Batch-C this ran inline on the worker, so one slow scan blocked one
> worker. With the HTTP claim endpoint landed in PR #42, the same scan now runs
> inside an HTTP handler. A cold cluster (no eligible executions) + N
> concurrent workers = N scans × 256 partitions × ~fetch-time = server latency
> inflation proportional to N.

Options listed: (1) bounded/cursored scan, (2) parallel fan-out, (3) server-side
backpressure semaphore, (4) observability first.

Labels: `batch-c`, `performance`. State: **OPEN**. Filed: **2026-04-20**. No
comments.

## Age / still-relevant check

- Filed 2026-04-20, two days after the HTTP claim endpoint (PR #42) exposed the
  scan inside a shared handler.
- **PR #86 — `perf(ff-scheduler): bound partition scan + rotation cursor (#43)`
  — merged 2026-04-22** (commits `5ef052f` + review-round `869e51c`, squash
  `183c10f`). This PR explicitly addresses issue #43 and lands all four of the
  options the issue proposed *except* (2) parallel fan-out (intentionally
  rejected on the PR description — the bounded-scan approach removes the
  motivation).
- Concrete deltas in `crates/ff-scheduler/src/claim.rs` on today's main
  (a4057c7):
  - `SchedulerConfig { max_partitions_per_scan: u16 = 32, rotation_window_ms:
    u64 = 250 }` added (see claim.rs:207–252).
  - `iter_partitions(total, start, count)` bounds the scan to `count`
    partitions starting at a rotation cursor (claim.rs:258–266).
  - Rotation cursor is a process-wide `AtomicU64` (hi 48 = rotation window
    index, lo 16 = cursor index) persisted on the `Scheduler` instance held
    by `FlowFabricServer` (so cursor survives across HTTP claim calls — was
    per-call-constructed before).
  - Per-worker FNV jitter on `worker_instance_id` stacks on top of the cursor
    (the option-1 extension the issue requested).
  - `server_time_ms` hoisted out of per-candidate loop (one `TIME` per call).
  - `tracing::debug!` emits `partitions_visited / skipped / hit / elapsed_ms /
    start_p / worker_instance_id` once per claim call — the option-4
    observability the issue asked for.
- PR #86 body quantifies the RT-count win:
  | scenario                 | before | after |
  |--------------------------|-------:|------:|
  | no-hit (quiet cluster)   |    256 |    33 |
  | hit on first try         |     ~4 |    ~4 |

The issue is **stale open**: the code it flagged no longer exists in the form
the issue describes. Issue was not auto-closed because the squash-commit
message includes `(#43)` parenthetically but the merge happened via GitHub's
admin squash without a `Closes #43` trailer on the default branch — a process
miss, not a code miss.

## Reproduction

Reproducing the *original* complaint is not meaningful — the code path the
issue targeted (unconditional 256-partition sweep) is gone. A sanity check
against today's `crates/ff-scheduler/src/claim.rs`:

- `SchedulerConfig::DEFAULT_MAX_PARTITIONS_PER_SCAN = 32` (claim.rs:239).
- Server holds a single `Scheduler` instance (referenced by #86 description;
  persists rotation cursor across HTTP handler invocations).
- Tests `iter_partitions` regression + 2 fairness full-coverage tests exist
  in-crate (`cargo test -p ff-scheduler` advertised 10/10 on PR #86, incl. 5
  new).

No new benchmarks were run as part of this investigation (issue predicates on
idle-cluster RT inflation, which #86 reduces by ~8x on paper; verifying that
on hardware is a scenario-5-style bench, out of scope for a
close-as-obsolete decision).

## Stage 1c touchpoints

Stage 1c scope (`rfcs/drafts/stage-1c-scope-audit.md`) is the `FlowFabricWorker`
hot-path migration behind `EngineBackend`: ZRANGEBYSCORE eligible-scan,
`ff_issue_claim_grant`, `ff_claim_execution`, `ff_claim_resumed_execution`,
HGET attempt counters, and GET payload. All 17 sites live in
`crates/ff-sdk/src/worker.rs`. The scheduler partition-sweep loop (server-side,
`ff-scheduler::claim_for_worker`) is **not** in Stage 1c scope — it's the
server's own scan before it hands a candidate to the worker FCALL.

Stage 1c will reshape the worker-to-backend trait around claim ops, but it
does not touch the scheduler's cursor/bounded-scan logic. No collision.
Independent.

## Recommendation

**Close as obsolete (superseded by PR #86).**

Action items:

1. Post a closing comment on #43 linking to PR #86 + commit `183c10f`, noting
   that options 1 (bounded/cursored scan) and 4 (observability) landed and
   made option 2 (parallel fan-out) and option 3 (server-side semaphore)
   unnecessary. Keep `performance` + `batch-c` labels for audit.
2. Close the issue.
3. *No* code changes. No new RFC. No Stage 1c interaction.

If residual concern exists about option 3 (HTTP handler backpressure under
thundering-herd), that is a distinct concern about the `/v1/claim` HTTP layer,
not the scheduler scan loop, and should be filed as its own issue if a
real workload surfaces it.

## Effort if fixed (if recommendation is move-up)

N/A — already fixed. Close-comment + close is ~5 minutes of owner time.
