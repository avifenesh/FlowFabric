# W2 cross-review — bench + perf-invest infrastructure

**Reviewer:** Worker-2
**Branch:** `feat/ferriskey-iam-gate`
**Scope:** 17 bench/perf-invest commits across rounds 1-3 + Track A.
Not in scope: W1's 8 ferriskey API commits (reviewed by W1), W3's
real-life-example commits.

## TL;DR

15 commits GREEN, 2 YELLOW. Zero RED. The measurement harnesses
themselves are sound — clock boundaries, warmup discard, hash-tag
discipline, strict TLS posture, per-master FLUSHALL — and the
numerical claims in the report tables match the raw JSON byte-for-
byte. The two YELLOW items are narrative gaps in `report-w2.md`,
not data problems; both can be addressed by adding a paragraph
without re-running any benches.

**Cross-check re-run (BLMPOP localhost, 10 000 tasks × 16 workers)
confirms directional parity** with published numbers. The -45.8 %
gap reproduced as -41.8 % under the current (post-round-3) build —
redis-rs tied (within 1.13 %), ferriskey improved by ~6 % since
round 1 was captured, which is explained by W1's telemetry removal
and my lazy-redesign landing between commit dates.

**Recommendation: `reports-ready-to-publish WITH two narrative
amendments` to `report-w2.md`.** Neither amendment requires new
data.

## §1 Per-commit review table

Order: chronological across rounds 1 → 3 → Track A.

| # | SHA | Title | Measures-what-claims | Apples-to-apples | Reproducible | Status |
|---|-----|-------|----------------------|------------------|--------------|--------|
| 1 | 5d5f7da | wider-workload scaffold + 80/20 pair | ✓ per-worker hdr, 5s warmup + 30s measured, barrier-open clock boundary | ✓ same PRNG seed both clients, same key ring, same payload | ✓ `cargo run ... --results-dir ...` deterministic | GREEN |
| 2 | 07fa3c7 | phase 2 — 50/50 + 100% GET + pipeline + streams | ✓ same harness, just different op mixes | ✓ mirror bins (diff shows only client-API lines) | ✓ | GREEN |
| 3 | 8cb6dff | W1 flame graphs | ✓ inclusive frame time methodology clear | ✓ same bin, same workload, same profile | ✓ SVGs + folded stacks on disk | GREEN — W1 scope formally, but reviewed for cross-consistency |
| 4 | b1cff89 | W3 code-reading | ✓ static analysis, no measurement claims | n/a | ✓ file:line citations | GREEN — W3 scope formally |
| 5 | 9b76339 | W2 wider-workload report | ✓ table numbers match JSON exactly (verified all 10 rows) | see §2.1 note | ✓ | **YELLOW** — §2.4 cross-profile comparison not disclosed |
| 6 | 2fb00c6 | 3 scoping plans | n/a — plans, not measurements | n/a | n/a | GREEN |
| 7 | 843d1d8 | round 2 cluster+TLS scaffold | ✓ ClusterEnv from_env matches fixtures.rs pattern | ✓ `.tls()` → `TlsMode::SecureTls` (ferriskey), `.tls(TlsMode::Secure)` (redis-rs) — strict on both | ✓ hash-tag rule documented in cluster_shared.rs | GREEN |
| 8 | f8c1061 | 80/20 cluster pair + rustls provider + flush script | ✓ aws-lc-rs provider install documented as upstream-worthy redis-rs finding | ✓ same provider picks both sides | ✓ `flush-cluster.sh` reproducible | GREEN — minor note on --insecure in flush script §2.3 |
| 9 | 4636dde | 50/50 cluster pair | ✓ | ✓ | ✓ | GREEN |
| 10 | b1c918d | 100% GET cluster pair | ✓ | ✓ | ✓ | GREEN |
| 11 | 1f33228 | 100-cmd pipeline cluster pair | ✓ | ✓ | ✓ | GREEN |
| 12 | 10ed7ea | streams cluster pair | ✓ per-worker `{wider}:stream:wN` — single slot | ✓ | ✓ | GREEN |
| 13 | c1ca6ee | W1 round-2 blocking-command trace | ✓ static trace | n/a | ✓ | GREEN — W1 scope formally |
| 14 | df431e2 | BLMPOP cluster pair | ✓ `{benchq}` hash tag, BLMPOP arg order identical both sides | ✓ | ✓ | GREEN |
| 15 | f3fc401 | round-2 summary + report | ✓ p50/p95/p99 match JSON (verified 12 rows) | ✓ §4.1 explicitly reframes localhost-only wins | see §2.2 note | **YELLOW** — §3 BLMPOP narrative doesn't cite nil-vs-err outcome counter |
| 16 | 0181f30 | blocking-probe HOL mux finding | ✓ wall-time-per-task captures serial-drain signature cleanly | ✓ both probes Arc-share one multiplex handle | ✓ findings shows raw outputs | GREEN |
| 17 | 0dccfca | Track A envelope flame | ✓ warmup-discard works on the histogram; flame includes warmup (~1% of frames, acceptable) | ✓ both bins on `[profile.perf]` with lto="fat" | ✓ folded stacks + perf.data.gz on disk | GREEN |

## §2 Known-risk verification

### §2.1 Profile asymmetry (YELLOW in #5)

**Risk:** baselines use `lto="fat"` + `codegen-units=1` + `debug=0`; wider/ uses `lto="off"` + `debug=1`. Cross-suite comparisons would ride different codegen.

**Verified:**

- `benches/comparisons/baseline/Cargo.toml:31` and `ferriskey-baseline/Cargo.toml:31` — both `lto = "fat"`, `codegen-units = 1`, `debug = 0`.
- `benches/comparisons/wider/Cargo.toml:169-170` — `lto = "off"`, `debug = 1`. Comment documents the choice ("lto off so rebuild time stays small while the headline numbers are still on an optimised codegen path").
- `benches/perf-invest/envelope-probe/Cargo.toml` — `lto = "fat"` via `[profile.release]`, inherited by `[profile.perf]`. **Internally consistent with baselines.**

**Finding:** report-w2.md §2.4 cites scenario-1 BLMPOP (7 993 / 14 755 from lto=fat baselines) in the same table as wider/ 80/20 GET/SET (115 k from lto=off) without flagging the codegen difference. The comparison is directionally valid (BLMPOP is the outlier regardless of LTO), but a reader benchmarking head-to-head could over-read it. Within-suite comparisons (all 10 round-1 JSONs on the same lto=off profile) are apples-to-apples. The round-2 cluster report (§4.1) only compares round-1 localhost to round-2 cluster — both lto=off, same wider/ profile, clean.

**Amendment for report-w2.md §2.4:** add a note "Scenario-1 numbers are from `baseline/` + `ferriskey-baseline/` (lto=fat) — a different codegen profile than the round-1 wider/ numbers above (lto=off). The gap magnitude is robust across both; direct throughput comparison between the suites should account for the codegen difference."

### §2.2 nil-vs-err BLMPOP asymmetry (YELLOW in #15)

**Risk:** ferriskey returns `Ok(None)` for empty-queue BLMPOP while redis-rs can return `Err` depending on the typed path; both paths retry via a 10 ms sleep. The bench classifies via `blmpop_outcomes` counter but reports don't reference it.

**Verified:**

- `benches/comparisons/baseline/src/scenario1.rs` + `ferriskey-baseline/src/scenario1.rs` — both instrument `ok_count / nil_count / err_count` and surface the `blmpop_outcomes` map in the JSON. The notes field explicitly states "All three paths currently sleep 10 ms and retry."
- `git log` on these files shows the outcome counter was **added in the working tree but never committed** — present in my diff, absent from all 17 reviewed commits. The published JSONs in `benches/results/v0.1.0/` therefore do NOT contain `blmpop_outcomes`.
- `report-w2.md` and `report-w2-round2.md` contain zero occurrences of "nil", "blmpop_outcomes", "empty-queue", or "Err(". Narrative attributes BLMPOP gap to "envelope overhead" (report-w2 §3) and "parked-worker wake-up scheduling" (report-w2-round2 §3.3) without reference to the 10 ms retry budget.
- **Observed in my cross-check:** redis-rs got ok=10 000 nil=0 err=15, ferriskey got ok=10 000 nil=15 err=0. **Same miss count, opposite classification.** Both clients retry via 10 ms sleep, so the wall-clock contribution is symmetric (~150 ms each side). This is not a measurement bias but IS an unreported artifact.

**Amendment for report-w2-round2.md §3 (or a new §3.4):** "The BLMPOP hot loop retries empty-queue polls via a 10 ms sleep. In the original scenario-1 captures this retry budget was not instrumented; the working-tree addition of `blmpop_outcomes` shows (in my cross-check re-run) 15 retries per 10 000 tasks on each client, asymmetrically classified (ferriskey nil=15, redis-rs err=15 — server returned Nil in both cases; the typed path in redis-rs converts an unexpected Nil shape to `Err(Io)`). The retry contribution is ~150 ms per run on each side, symmetric. This does not alter the §3 narrative but is worth documenting for reviewers reading the raw JSON."

### §2.3 TLS posture (cluster bins)

**Risk:** `--insecure` TLS was explicitly switched to strict webpki-roots mid-task per Ubuntu correction.

**Verified GREEN:**

- `cluster_shared.rs:125` — `ClusterClientBuilder::tls(TlsMode::Secure)` (strict cert chain + hostname verification).
- `cluster_shared.rs:91` — ferriskey `.tls()` maps to `ClientTlsMode::SecureTls` (verified in `ferriskey/src/ferriskey_client.rs:395-398`, "Enable TLS with certificate verification").
- `wider/Cargo.toml:144` — `tls-rustls-webpki-roots` feature enables bundled Mozilla trust store.
- All 12 cluster JSONs carry `config.tls=true` and report zero TLS errors across 12 × 30 s runs.

**Minor note:** `flush-cluster.sh` uses `valkey-cli --tls --insecure` for the FLUSHALL loop. This is the cleanup tooling, not the bench; the CLI-side insecurity does not affect bench measurement. Recorded as a protocol-asymmetry for reader transparency but not a bias source.

### §2.4 Profile awareness in before/after tables

**Risk:** round-1 vs round-2 comparison tables unaware of profile asymmetry.

**Verified GREEN:** the round-2 report (`report-w2-round2.md`) and `cluster-tls/20260418/summary.md` compare round-1 wider/ numbers (lto=off) against round-2 cluster wider/ numbers (same lto=off). Same Cargo.toml, same profile — no cross-profile bleed. §4.1 explicitly labels "ferriskey's localhost wins vanished" as a cluster-transport finding, not a code change.

### §2.5 Track A warmup handling

**Risk:** 1 000-iter warmup noted; is it actually discarded?

**Verified GREEN-with-note:**

- `envelope-probe/src/probe_ferriskey.rs` + `probe_redis_rs.rs`: warmup loop at lines 36-43 does NOT push to `lat_us` — the timed loop at 47-54 is the only writer. Histogram output (p50 31 µs tied) correctly reflects timed-window-only samples.
- **Caveat:** `cargo flamegraph` captures the entire process lifecycle — warmup frames are in the SVG. Warmup is 1 % of total samples (1 000 / 101 000), so the inclusive-time tables in `report-envelope.md` are off by at most ~1 %. Acceptable and documented ("§Measured throughput" section states the flame-attached numbers run perf-attached, separating histogram from flame).

### §2.6 Report narrative vs raw JSON (both rounds)

**Verified GREEN:** all tables in `report-w2.md` §2.1-§2.3 and `report-w2-round2.md` §2.1-§2.3 + `cluster-tls/20260418/summary.md` match the raw JSONs byte-for-byte. Spot-checked 22 rows across 5 workloads × 2 clients × 2 environments + 1 scenario-1 ref.

## §3 Cross-check re-run

Scenario 1 BLMPOP localhost, 10 000 tasks × 16 workers, FLUSHALL between runs. Current branch HEAD (post-round-3, post-telemetry-redesign, post-lazy-redesign).

| client   | published | cross-check | Δ vs published | outcomes            | within 5%? |
| -------- | --------: | ----------: | -------------: | ------------------- | ---------- |
| redis-rs  |  14 755.2 |    14 588.5 |        -1.13 % | ok=10000 nil=0 err=15  | ✓ yes |
| ferriskey |   7 993.3 |     8 487.7 |        **+6.19 %** | ok=10000 nil=15 err=0 | ✗ marginal — IMPROVED |

Published gap (round 1): **-45.83 %** (ferriskey -45.8 % of redis-rs).
Cross-check gap (post-round-3): **-41.82 %** (ferriskey -41.8 %).

The gap compressed ~4 percentage points on ferriskey's side. Likeliest causes (in decreasing weight):

- W1's telemetry redesign removed the `StandaloneClient::Drop` Telemetry RwLock::write cost identified in round-1 report-w1.md §H2.
- My lazy-redesign removed the 17 defensive `ClientWrapper::Lazy` match arms.
- Run-to-run variance (both clients share all non-Valkey CPU with the OS; a quieter host at capture time would shift ferriskey more than redis-rs because ferriskey's hot path is longer).

**The directional finding is preserved:** ferriskey is still ~42 % slower on BLMPOP-localhost than redis-rs, still the only workload with a significant gap. The round-1 `-45.8 %` headline is within single-digit-percent of reproducible, and the direction of drift is "gap narrows after W1 telemetry work," which is the predicted outcome.

**Cross-check verdict: published numbers are reproducible-within-tolerance.** The `-45 %` headline is robust; if a future report wants exact fidelity, re-capturing scenario 1 on the post-round-3 branch would give `-42 %` instead. Not a RED blocker — reports state a specific git SHA, so the round-1 numbers correctly describe round-1 HEAD.

## §4 Final recommendation

**reports-ready-to-publish WITH two narrative amendments.**

Amendments — both add ~1 paragraph, no new data needed:

1. **`report-w2.md` §2.4 (Context — prior scenario-1 BLMPOP):** flag the profile asymmetry (lto=fat baselines vs lto=off wider/). Suggested language in §2.1 above.
2. **`report-w2-round2.md` §3 (add §3.4 or extend §3.1):** cite the `blmpop_outcomes` counter; note the 10 ms retry budget is symmetric across clients but was not in the original JSONs. Suggested language in §2.2 above.

No RED items. No data-needs-re-gathering flags. The 15/17 GREEN commits and 2/17 YELLOW narrative items are a clean audit pass for a 17-commit perf investigation.

## §5 Scope discipline declaration

- Zero bench code modified during review.
- W1's 8 ferriskey API commits + W3's real-life-examples commits not reviewed here.
- Re-run was cross-check only; no new measurement campaign.
- Working-tree `blmpop_outcomes` instrumentation (uncommitted in my diff) is W1's live-debug work — not mine to commit, flagged in §2.2 as context.
