# L — round 2 challenge on v0.7 migration master

Reviewer persona: cairn operations + migration + SDK deltas (same as round
1). Target: `rfcs/drafts/v0.7-migration-master.md` @ `5cc456a` on
`docs/v0.7-migration-master`. Round-1 K not relitigated; round-2 K runs in
parallel and was not read.

## Bottom line

**ACCEPT with two residual follow-ups.** The revisions between `efaf2b3`
and `5cc456a` materially close my round-1 dissent. Q12 (schema migration
mode), Q13 (version-pin policy), Q14/§8 (dual-backend = per-instance, no
shared state), Q15 (rollback contract), and §9 (operational runbook) each
address a specific round-1 gap with a concrete, non-placeholder answer.
The two residuals below are refinements, not blockers.

## Round-1 resolution status

| Round-1 finding | Status | Evidence |
|---|---|---|
| Op-gap 1 — schema migration ordering | **RESOLVED** | Q12 L978–L1013 + Stage A L1075–L1080. Out-of-band `sqlx migrate run` + boot-time `REQUIRED_SCHEMA_VERSION` check mirroring Q5 partition-count check. Rolling-upgrade matrix (additive / two-phase / destructive) spelled out L995–L1003. |
| Op-gap 2 — dual-backend running mode | **RESOLVED** | §8 L1312–L1383. Per-instance, one backend per process, no shadow/dual-write, cross-backend cascade returns `EngineError::Validation`. Ends the "side-by-side" ambiguity. |
| Op-gap 3 — metrics cardinality / labels | **RESOLVED** | §9.1 L1392–L1408 commits Stage E to a labels-compat matrix with stable/renamed/new/dropped buckets and a cairn-dashboard check before Stage E merges. |
| Op-gap 4 — backup / PITR shape | **RESOLVED** | §9.2 L1410–L1424 covers `pg_basebackup`, WAL archive, lease-expiry-after-PITR semantics (reclaimed via existing scanner), and outbox-is-the-guarantee note. |
| Op-gap 5 — reconciler-as-primary observability | **RESOLVED** | Stage D L1156–L1163 lands `ff_completion_recovered_by_reconciler` + `ff_listen_connection_healthy`; §9.3 L1426–L1435 wires the >5%/5min + 30s-down thresholds. |
| SDK-delta 1 — per-partition HMAC no-op | **RESOLVED** | Author-response L150–L155 + rustdoc-plus-structured-warning. Deprecation deferred to v0.8 RFC. Acceptable given admin tooling can observe the warning. |
| SDK-delta 2 — `ts_ms` monotonicity under PITR | **RESOLVED** | Q3 invariant — ordering authoritative by `(ts_ms, seq)`, `ts_ms` advisory. |
| SDK-delta 3 — tail reconnect storm | **RESOLVED** | Q2 reconnect spec (K-3 / round-1) includes jittered per-SDK reconnect budget. |
| SDK-delta 4 — `backend_version()` pin | **RESOLVED** | Q13 L1015–L1032. Warn-by-default, major-mismatch-refused-unconditionally, minor/patch knob-configurable. |
| SDK-delta 5 — new `BackendErrorKind` retry classes | **RESOLVED** | Stage B deliverable L1114–L1119 ships ff-sdk retry compat shim alongside Stage B impls so cairn never sees raw `SerializationFailure` / `DeadlockDetected`. |
| Exec-summary 8-week framing | **RESOLVED** | L82–L85 — aggressive = A–D, realistic = A–E w/ staging week. |

Round-1 questions 1–9: all answered in the adjudications above or in §8/§9.

## New findings (round 2)

### L-r2-1 — rolling-upgrade with 5+ replicas: `REQUIRED_SCHEMA_VERSION` interaction under an additive migration is ambiguous

Q12 L986–L994 says the binary "refuses to start if the ledger's
`latest_applied_version` is less than the binary's
`REQUIRED_SCHEMA_VERSION`, or (more subtly) logs a warning and serves
read-only if it is greater." That second clause is the one that matters
during a rolling deploy:

- Operator applies additive migration v3 → v4 out-of-band. Ledger is at v4.
- Rolling deploy starts. Some pods are v0.7.0 (expects v3), some are v0.7.1
  (expects v4). Under the clause above, the v0.7.0 pods would see
  `latest_applied > required` and **serve read-only** — which means the LB
  now has half its pool silently unable to accept writes.
- The Q12 zero-downtime matrix L995–L998 says "additive change: old
  binaries ignore the new column; deploy in any order" — i.e. old binaries
  should be fully operational. This contradicts the "read-only if ledger
  ahead" clause.

Pick one. Either (a) additive migrations are a soft-forward and old
binaries stay read-write (matching L995–L998), in which case the
`REQUIRED_SCHEMA_VERSION` check should only enforce `ledger >= required`
and treat `ledger > required` as silent-ok; or (b) schema bumps are
categorised explicitly in the migration file (`additive=true` / `breaking`)
and the boot check branches on the category. Today's text admits both
readings and a cairn SRE cannot predict rollout behaviour from it.

**Severity:** real. With 5+ replicas behind an LB, the "read-only on
ledger-ahead" clause would silently shift the canonical write path and
could cause cairn write latency to double during the deploy window.

**Fix:** One sentence in Q12 clarifying the additive case. Propose:
"Additive migrations are annotated `backward_compatible=true` in the
migration metadata; the boot check treats `ledger > required` as OK
**when every intervening migration is backward-compatible**, and
read-only otherwise." That preserves the zero-downtime matrix.

### L-r2-2 — Q15 rollback honest-but-costly: the "per-instance redeploy + reverse migration out of scope" line shifts burden that should be named in the executive summary

Q15 L1039–L1057 is honest: if cairn moves state-of-record to Postgres and
hits a latency regression, rollback is "redeploy v0.6.x at Valkey, abandon
or drain Postgres-side state, reverse migration is NOT automated."
Combined with §8.2 step 4 (one-shot Valkey→Postgres migration tool is
unidirectional), this means **there is no safe exit once a tenant's new
executions start landing on Postgres.** The operational mitigation
L1053–L1054 is "don't move state-of-record to Postgres until staging soak
is clean" — which is correct but is a strong prerequisite that isn't
surfaced in the executive summary's migration framing.

**Severity:** ergonomic-gap. Not technically wrong — it's a direct
consequence of choosing per-instance dual-backend (the right choice).
But a cairn operator reading the exec summary L34–L96 without reading
Q15 + §8.2 will come away with the impression that migration is
reversible per-tenant. It isn't, once writes land.

**Fix:** One-bullet addition to the exec summary under "migration model"
or similar: "Cutover is forward-only per tenant once new executions
land on Postgres; rollback requires abandoning or retaining Postgres
state and rerouting to the Valkey-backed ff-server." Makes the trade
visible up-front.

## Remaining operational risks (acknowledged, non-blocking)

- **Stage E labels-compat matrix is still just a deliverable, not
  content.** That's reasonable — Stage E is weeks 11–14 and the matrix
  can't be written before Stage B–D emit actual metrics. The commitment
  + the cairn-dashboard check as a Stage E merge condition is enough.
- **Reverse migration tooling is deferred to "separate RFC / v0.8."**
  Author-response L199 acknowledges. Acceptable given §8's per-instance
  isolation makes the rollback path non-catastrophic even without it.
- **Q12's two-phase column-rename pattern (L999–L1001) assumes cairn
  releases are paced slowly enough for two binaries to ship in sequence.**
  True today; worth an explicit note that operators running faster
  release cadences need a coordination step. Not a blocker.
- **NOTIFY-post-PITR clause in §9.2 L1420–L1423** is correct but
  depends entirely on the outbox being durable across PITR. That's
  fine for `ff_completion` (outbox table), but stream-tail NOTIFYs
  per §3.2 are application-level NOTIFYs emitted at end-of-tx — they
  are NOT in an outbox. A PITR that replays to before the tx will
  re-emit; a PITR that replays past the tx won't. Worth one sentence
  in §9.2 distinguishing outbox-backed NOTIFYs (durable) from
  stream-tail NOTIFYs (fire-once, reconciler-recoverable). Not a
  blocker because the reconciler backstop covers the drop case.

## Questions for the author (clarifications, not blockers)

1. **Q12 additive-vs-destructive boot-check branch (L-r2-1)** —
   please clarify the behaviour when ledger > required but the
   intervening migration is backward-compatible.
2. **Exec-summary forward-only-cutover callout (L-r2-2)** — okay
   to add a bullet so the rollback cost isn't hidden in Q15 + §8.2?
3. **Migration-tool scope for partial tenants** — §8.2 step 4 says
   "per-partition batches." What happens if cairn wants to abort a
   partial cutover mid-dump? Is the tool resumable, or does a mid-
   migration abort leave the Postgres side with a half-loaded
   tenant?
4. **§9.2 stream-tail NOTIFY post-PITR semantics** — worth one
   sentence distinguishing outbox-backed from application-NOTIFY
   durability? Asked above.

---

*L-r2 written against `5cc456a`. Round-2 K was not read. Adoption/ops
angle preserved from round-1.*
