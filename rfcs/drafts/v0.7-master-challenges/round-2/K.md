# K — round 2 challenge on v0.7 migration master

Target: `rfcs/drafts/v0.7-migration-master.md` @ `5cc456a`.

## Bottom line

**ACCEPT with two remaining correctness risks.** The revision addressed
every round-1 K-finding substantively. The risks below are narrow enough
to land as Stage A/C test-gates or Stage E runbook entries; they do not
block the master doc from advancing to owner sign-off.

## Round-1 findings — resolution status

- **K-1 (isolation self-contradiction) — RESOLVED.** Q11 (L941–976)
  adjudicates default `READ COMMITTED + SKIP LOCKED` + three
  `SERIALIZABLE` escalations (`ff_resolve_dependency`, `ff_deliver_signal`,
  `ff_suspend_execution`), exec-summary L50–54 matches, §L0 L115–124
  matches. Retry contract stated (3 attempts, 50/100/200ms jitter,
  `EngineError::Backend(SerializationFailure)` on exhaustion).
- **K-2 (cascade recursion tx boundaries) — PARTIAL.** §3.1 L437–451
  picks per-hop commit and adds a crash-recovery contract for the
  reconciler. Substance correct. Gap: the contract says
  `dependency_reconciler`'s scan must span "the transitive-descendant
  reachable set from any terminal execution committed within the last
  reconciler-interval." On Valkey today this scanner sweeps a
  blocked-set, not a transitive-reachable set from terminals. That is a
  real semantic delta in the reconciler workload; the doc names it but
  doesn't flag it as "reconciler port is new work, not a transplant."
  Small but worth a one-line note in Stage D.
- **K-3 (shared LISTEN SPOF) — RESOLVED.** Q2 L692–715 nails reconnect
  protocol, PgBouncer transaction-pool unsupported, health gauge,
  watchdog. Clean.
- **K-4 (NOTIFY-in-tx atomicity) — RESOLVED.** Q1 L643–652 + §3.2
  L500–510 state the on-COMMIT semantic and narrow the reconciler's role
  to subscriber-side drops only. `last_seen_event_id` persisted
  subscriber-side — good, the "infer via MAX()" trap is explicitly
  rejected.
- **K-5 (per-row NOTIFY storm) — RESOLVED.** §3.2 L500–510 explicitly
  rejects per-row trigger, chooses application-level NOTIFY at
  `append_frame` tx end. Coalescing story is spelled out.
- **K-6 (Q6 retention tx contention) — RESOLVED.** Q6 L830–865 rewrote
  to async per-partition janitor with bounded overshoot. Rejects
  piggyback on INSERT tx with the right reasoning (non-index-only
  ROW_NUMBER, oldest-N row lock hot-band).
- **K-7 (RFC-015 MAXLEN mischaracterization) — RESOLVED.** Q6 now
  acknowledges approximate-N is RFC-015's deliberate p99 choice.
- **K-8 (Q9 scanner drop wrong) — RESOLVED.** Q9 L896–927 keeps
  `budget_reconciler` + `quota_reconciler` (cross-process drift
  survives per-tx atomicity); drops only `index_reconciler`. Net
  scanner count unchanged at 17 (−1 index, +1 stream_frame_janitor).
- **K-9 (Stage A/B boundary) — RESOLVED.** Stage A L1082–1086 promotes
  Q6+Q7+Q8 into Stage A's adjudication set. Merge blocker explicitly
  requires Q1–Q8 + Q11–Q13 decided before Stage B.
- **K-10 (dual-backend failure modes) — RESOLVED.** §8 added, explicit
  per-instance scope, cross-backend cascade out-of-scope.
- **K-11 (fence-triple preservation) — RESOLVED.** §6 L1235–1242 adds
  the cross-backend invariant. Also flags `handle_codec` v2 dropping
  `lease_ttl_ms` without touching the triple. Good.
- **K-12 (HMAC kid rollout) — RESOLVED.** Q4 L782–788 adds cutover
  union-seed from all Valkey partitions, additive rotation thereafter.

## New findings in the revision

### Q11 — SERIALIZABLE retry budget may be tight under fan-in storms

The 40001-retry budget is "3 attempts with jitter" applied per caller.
Under wide-flow dependency resolution, the canonical hotspot is K-of-N
completion fan-in: N parent executions terminate near-simultaneously,
each cascade hop calling `ff_resolve_dependency_sp` against the same
child row. That's the textbook SSI-serialization-failure scenario, and
three retries at 50/100/200 ms may not be enough when N is tens-of-
parents. The doc says retry-exhaust is `EngineError::Backend(
SerializationFailure)` and §9.3 alerts on `>0.1% exhaust` — that's the
*signal*, not the *mitigation*. Two things worth thinking about:

1. The retry budget should probably grow with the expected fan-in
   (dep-resolution pre-read could tell the proc how many predecessors
   exist). Stage C test-gate candidate.
2. The cascade loop in Rust is where the retry should ultimately live —
   the stored-proc-call-site retry is fine for inner contention, but
   the cascade loop itself is already idempotent (re-running hop K
   converges), so the safe fallback is "on `SerializationFailure`
   exhaust, fall back to reconciler path" rather than surface to
   caller. Doc doesn't state this fallback.

### Q12 — rolling-upgrade story does not cover stored-proc versioning

L995–1003 covers additive columns, column-rename (two-phase), and
destructive drops. It doesn't cover the *stored procedures themselves*.
A Stage C migration that changes the signature of
`ff_resolve_dependency_sp` (e.g. adds a parameter) is equivalent to a
destructive schema change under a rolling deploy: old binaries call
old signature, new binaries call new signature, only one can coexist
with the migration ledger's current state. Either:

- Stored-proc signatures are additive-only (new name per version),
  mirroring the add-column-then-deploy pattern; old binary keeps
  calling the old-name proc until drained. Or
- Stored-proc changes force flag-day (destructive-change rules apply),
  which contradicts Q12's "zero-downtime" framing.

This is a Stage A deliverable gap: schema-migration policy for stored
procs needs a stated rule, same as columns. Recommend adding one line
to Stage A: "stored-proc signature changes follow the column-rename
two-phase rule; old-name proc retained for one release."

### Q14 / §8 — "Validation error on cross-backend parent" overclaims detection

§8.1 L1336–1341: "Any attempt to create child executions on a different
backend than their parent returns `EngineError::Validation(...)`." The
ff-server process only knows about executions in *its* backend. If a
Valkey-backed ff-server gets a `create_execution(parent=E_pg)` request,
it simply looks up `E_pg` locally, doesn't find it, and returns
`NotFound` — not `Validation("cross-backend")`. The cross-backend
detection is structurally impossible without a federation layer.

Fix is small: rephrase §8.1 to say "cairn's routing layer enforces that
parent + child land on the same backend; ff-server enforcement is
`NotFound` on the local-lookup miss, which is the same error path as a
genuine nonexistent parent." The semantic is the same; the claim of
explicit `Validation("cross-backend")` is just wrong about who enforces
what.

### §8.2 — drain has no bound for suspended executions

Step 3 ("waiting for in-flight executions to reach terminal state")
passes over the suspension case. A flow with a waitpoint on a
long-deadline external signal can sit for days in the "in-flight"
state. The cutover procedure needs either (a) an upper bound on drain
(e.g. drop suspensions after timeout; risky), (b) explicit allowance
for suspensions to be migrated mid-state (Stage E tool must handle
suspension rows), or (c) documentation that tenants with long-lived
suspensions can't be cutover this way.

Same issue affects Q15 rollback. Worth one-line acknowledgement.

### §9 — substantive, not hand-wavy

§9.1 labels-compat matrix, §9.2 PITR one-page, §9.3 alert thresholds
with specific rates, §9.4 rollback step-by-step. All four are concrete
Stage E deliverables, not aspirational. The `ff_completion_recovered_
by_reconciler` counter + `ff_listen_connection_healthy` gauge are
correctly threaded through Stage D deliverables. §9.3's threshold of
`>0.1%` for 40001-retry-exhaust is tight enough to catch a real
degradation but not so tight it fires on healthy serialization conflict.

## Remaining correctness risks

1. **Q11 retry budget under fan-in.** Not a ship blocker; is a Stage C
   test-gate. "N=32 simultaneous parents of one child, assert cascade
   converges without SerializationFailure reaching the caller."
2. **Stored-proc migration policy.** Ship blocker for Stage A: the
   policy needs to be stated before any Stage C proc lands.
3. **Cross-backend Validation error claim.** Documentation-only fix.

## Questions for the author

1. Is the cascade loop expected to catch `SerializationFailure` from
   `ff_resolve_dependency_sp` and fall back to the reconciler path, or
   surface it? (Q11 implies surface; the idempotent nature of the
   cascade loop suggests fallback is safer.)
2. Stage A deliverable list: add stored-proc signature migration
   policy? One line is enough.
3. §8.1 "cross-backend Validation error" — willing to rephrase as
   `NotFound` with a sentence about cairn-routing enforcement?
4. §8.2/Q15 drain — bound for long-running suspensions, or explicit
   "not supported for this tenant profile" note?
