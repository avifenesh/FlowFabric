# Scenario 4 long_running regression investigation (2026-04-22)

**Investigator:** Worker DD
**Worktree:** `/home/ubuntu/ff-worker-dd-bisect` (branch `worker-dd/scenario4-bisect`, from `origin/main` at `23d3ce3`)
**Host:** local AMD EPYC (same class as documented baseline); Valkey **7.2.12** on port 6379 (NOT 8.1.0 — see §Caveat)
**Bench invocation:** `FF_BENCH_LANE=bench FF_BENCH_SERVER=http://localhost:9090 ./target/release/long_running --duration 300`, preceded by `valkey-cli -p 6379 FLUSHALL`.

## TL;DR

Regression in `missed_deadline_pct` (3.88% → 7.11%) **does not reproduce as a regression**. `missed_deadline_pct` is ~7% at HEAD *and* at the claimed-good commit `01f6327`, *and* at every bisected commit between them. The "3.88% at v0.1.0" baseline in `benches/results/baseline.md` appears to be a single-run outlier that never represented stable behavior on this host class.

## Reproduction

Two fresh runs at HEAD (`23d3ce3`, `main` post-#133/#134):

- **Run 1:** `completed=2680  failed=102  missed_count=202  missed_pct=7.0090%`
- **Run 2:** `completed=2680  failed=104  missed_count=203  missed_pct=7.0413%`

Both > 5% → regression is "real" in the sense that HEAD is genuinely at ~7%. Proceeded to bisect `da89fa9` (bad) vs `01f6327` (good, per `baseline.md`).

## Bisect log

Range size: `git log --oneline 01f6327..da89fa9 | wc -l` = 49 commits (~6 steps). Each step: checkout → `sed` the `REQUIRED_VALKEY_MAJOR` constant from 8 to 7 (local Valkey is 7.2.12; older commits require major≥8 and refuse to start) → rebuild `ff-server` + `long_running` → `FLUSHALL` → 300s scenario.

| step | commit    | title                                                                 | missed_pct | verdict |
|------|-----------|-----------------------------------------------------------------------|------------|---------|
| 0    | `23d3ce3` | HEAD (main, Run 1)                                                    | 7.01%      | bad     |
| 0b   | `23d3ce3` | HEAD (main, Run 2)                                                    | 7.04%      | bad     |
| 1    | `183c10f` | `perf(ff-scheduler): bound partition scan + rotation cursor (#86)`    | 6.98%      | bad     |
| 2    | `077a3c8` | `feat(ff-sdk): describe_execution + ExecutionSnapshot (#62)`          | 7.04%      | bad     |
| 3    | `d3361a6` | `fix(ff-script): reclaim must SREM execution from old worker (#54)`   | 7.04%      | bad     |
| 4    | `00608ef` | `fix(ff-script): embed flowfabric.lua in crate for v0.1.1 (#48)`      | 7.04%      | bad     |
| 5    | `aeea6cc` | `chore(release): fix v0.1.0 publish pipeline + runbook (#47)`         | 7.07%      | bad     |
| 6    | `4192664` | `bench: v0.1.0 baseline refresh + RFC-011 harness compat fix (#46)`   | 7.04%      | bad     |
| 7    | composite | `01f6327` **server** + `4192664` **bench** (the doc-implied baseline) | 6.98%      | bad     |

First-bad per `git bisect` = `4192664` (PR #46).

## Why PR #46 is a bisect artifact, not the regression

`git diff 01f6327..4192664 --stat` shows **zero changes in `crates/`**. The diff is:

- `benches/harness/src/workload.rs` (+ `runners/flowfabric.rs`, `bin/cap_routed.rs`) — mint bench ExecutionIds via `ExecutionId::for_flow(fresh_fid)` instead of the broken bare-UUID that returned HTTP 422 at `01f6327`.
- `benches/results/baseline.md` — the v0.1.0 numbers table.
- `benches/Cargo.lock`.

At `01f6327` the old bench harness **could not submit executions at all** (422 rejected by the post-RFC-011 server). The only way the "3.88%" number could have been produced at `01f6327` is by running the **post-#46 harness** against the `01f6327` server — the same composite I reconstructed in step 7 above.

That composite produces 6.98%, not 3.88%.

### Hypothesis for the 3.88% number in baseline.md

Most likely a **single-run lucky timing** captured during the PR #46 authoring window. The scenario uses `REFILL_TOPUP = 20` tasks every `REFILL_EVERY_MS = 10_000`, and every task in a batch shares the same `submit_ms` — so whether a batch slips past the 60s deadline is highly bimodal with respect to the phase alignment of refill vs steady-state drain. A single 300s run samples ~30 refill windows, and the miss rate has a visible jaggedness at that cadence. The scenario's own comment block flags this:

> `missed_deadline_pct` is submit-to-complete; refill batches cause per-batch spikes because every task in a batch shares `submit_ms`, so the rate smooths only over windows >> refill_interval_ms.

3.88% vs 7.0% is almost exactly a factor of 2, consistent with "about half the refill batches miss vs. all of them". A single 5-minute run is an insufficient sample to distinguish these regimes — the doc itself acknowledges "single-run observation" and recommends a repeat.

The three "reference" points in `baseline.md` — v0.1.0 3.88%, v0.3.2 7.11%, pre-Batch-C 7.01% — are each single-run snapshots. My 8 runs across the full 01f6327..23d3ce3 range all land in 6.98%-7.07%. **The cluster of 7 reproductions across 6 distinct commits is strong evidence this is the stable regime and the 3.88% number was the outlier.**

## Caveat: Valkey version mismatch

`baseline.md` notes the reference host ran **Valkey 8.1.0**. My local Valkey is **7.2.12** (same `redis_version: 7.2.4` string but different server binary). Older commits in the bisect range enforce `REQUIRED_VALKEY_MAJOR = 8`, which I relaxed via `sed` during each bisect build. This is the one environmental delta that could plausibly matter:

- If Valkey 8 changes its scheduling of `BLMPOP` tail latency or `ZRANGEBYSCORE` tail latency in the eligible-queue scanner, deadline behavior could differ.
- No evidence either way from this investigation.

If the person who captured the 3.88% baseline still has access to a Valkey-8.1.0 host, a single reproduction run there would be dispositive. Until then, the regression claim is unsupported on a Valkey-7.2 host.

## Suspect PR + mechanism hypothesis

**None pinned.** The bisect mathematically fingers PR #46, but that PR's diff contains no product code — it only unblocked the bench from returning HTTP 422 at `01f6327`. There is no plausible mechanism by which #46 altered engine behavior.

Candidates previously flagged in the task description — RFC-012 Stage 1a trait forwarding (#114), bridge-event PUBLISH in v0.3.1 GAP-1 (#130), ScannerFilter per-candidate HGETs (#122) — were all bisected past (each commit downstream of `01f6327` already showed ~7% miss rate), so none of them *introduced* the regression. They may all individually add small overheads, but the floor was already ~7% before any of them landed.

## Recommendation

**Not reproducible as a regression.** `missed_deadline_pct` is stable at ~7% across the entire 01f6327..23d3ce3 range on this host class. Three concrete follow-ups:

1. **Treat the `baseline.md` v0.1.0 "3.88%" entry as an outlier.** Mark it clearly as single-run or replace it with the median of ≥5 runs. The current text already warns single-run; the warning should be load-bearing enforcement (N≥5 required before entering `baseline.md`) not advisory.
2. **Rebench on Valkey 8.1.0** to rule out the version mismatch caveat before closing. If 8.1.0 also shows ~7%, close as "not a regression, fix baseline doc".
3. **Lower variance of the scenario itself.** The 10s refill cadence + 60s deadline is right at the bimodal tipping point. Either (a) jitter per-task submit_ms within a refill batch, or (b) lengthen the run to ≥ 30 minutes so the sample is ≥ 180 refill cycles. Current 30-cycle sample is too short for a stable percent.

No product code change recommended. No further bisect required.

## Time accounting

- Reproduction (2 × 300s runs + build): ~14 min
- Bisect (6 × [~1 min build + 5 min run]): ~36 min
- Composite-verify (01f6327 server + current bench): ~6 min
- Diff/doc analysis: ~4 min
- **Total bench + analysis time: ~60 min**, well under the 2-hour HALT budget.
