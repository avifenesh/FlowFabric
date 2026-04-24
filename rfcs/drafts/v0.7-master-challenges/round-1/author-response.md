# Author response — round-1 K + L challenges on v0.7 migration master

Target: `rfcs/drafts/v0.7-migration-master.md` on `docs/v0.7-migration-master`
(PR #229). This response adjudicates each challenge finding as CONCEDE,
REVISE (concede + land text), or ARGUE-BACK. Q1–Q5 adjudications are locked
and are not reopened by either challenger; new decisions needed are captured
as Q11–Q15 and adjudicated inline in the master doc.

## K's findings (correctness + distributed-systems)

### K-1 — isolation-level self-contradiction — **CONCEDE + REVISE**

K is right. Exec-summary L51 asserts a decision; §L0 L115–L117 defers to
owner. Two different semantic regimes (SSI vs row-locked READ COMMITTED)
with different caller-side retry contracts. Fix: lift to new **Q11** and
adjudicate immediately. Default: `READ COMMITTED + SELECT FOR UPDATE SKIP
LOCKED`; the ~3 composite FCALLs (`ff_resolve_dependency`, `ff_deliver_signal`
composite-cond, `ff_suspend_execution` multi-waitpoint) declare `SERIALIZABLE`
at proc entry with a documented `SQLSTATE 40001` retry contract. Exec-summary
text rewritten to match.

### K-2 — cascade recursion tx boundaries — **CONCEDE + REVISE**

K is right. §3.1 L416–L418 names the choice but doesn't pick or state
crash-recovery semantics. Decision: **per-hop commit** (latency-conservative;
avoids seconds-long row-lock holds across a depth-50 cascade). Added crash-
recovery contract: `dependency_reconciler`'s blocked-set scan spans the
**transitive-descendant reachable set**, not just 1-hop children, and runs
on a bounded interval. Captured as §3.1 invariant + amendment to §Q9
adjudication.

### K-3 — shared LISTEN SPOF — **CONCEDE + REVISE**

K is right. Q2 adjudication said "ONE long-lived shared LISTEN connection"
with no reconnect or pooler-compatibility contract. Fix: amended Q2 with
(a) reconnect protocol (re-LISTEN all channels + synthetic poll-now wake
to every parked tailer); (b) **PgBouncer transaction-pool mode unsupported
for ff-server ↔ pg connections**; session-pool required; (c) a
`ff_listen_connection_healthy` gauge + auto-restart watchdog.

### K-4 — NOTIFY-in-transaction semantics — **CONCEDE + REVISE**

K is right that the property is load-bearing and currently unstated.
Added one paragraph under Q1 adjudication: NOTIFY-on-COMMIT makes the
outbox-row + wake-up atomic by construction; single `BIGSERIAL event_id`
suffices for ordering (no vector clock); reconciler role narrowed to
subscriber-side drops only.

### K-5 — per-row INSERT trigger NOTIFY storm — **CONCEDE + REVISE**

K is right. "Row-level trigger on INSERT" is the worst option under a
10k-frame/sec burst. Fix: §3.2 L463–L467 rewritten to specify
**application-level NOTIFY at end of `append_frame` tx** (composable with
batched inserts; natural coalescing if a single tx writes multiple
frames). Per-row trigger explicitly rejected in text.

### K-6 — Q6 retention-in-tx lock contention — **CONCEDE + REVISE**

K is right; piggybacked `DELETE WHERE rank > N` on the INSERT tx is a
contention trap and `ROW_NUMBER() OVER (PARTITION BY …)` is not index-
only. Rewrote Q6 to: **async janitor, per-partition, approximate-N with
bounded overshoot**, matching RFC-015's contract.

### K-7 — RFC-015 MAXLEN mischaracterization — **CONCEDE + REVISE**

K is right. "Approximate-N is a legacy Valkey artifact" misread RFC-015,
which explicitly chose approximate trim for p99. Rewrote Q6 per K-7's
minimal fix.

### K-8 — Q9 scanner drop assumes stronger atomicity than Q1 provides — **CONCEDE + REVISE**

K is right. `report_usage` runs on a separate connection/process/tx from
the grant, so cross-process drift survives the SQL-tx-atomicity argument.
Fix: **keep `budget_reconciler` + `quota_reconciler`** as-is; drop only
`index_reconciler` (which is genuinely redundant when indexes are real
pg indexes). Q9 proposal rewritten.

### K-9 — Stage A/B boundary vs design-lock boundary — **CONCEDE + REVISE**

K is right that Stage B bakes Q6/Q7/Q8 decisions that Stage A didn't lock.
Per K's option (a), promoted Q6 + Q7 + Q8 into Stage A's design-lock set.
Stage A's merge-blocker now requires all of Q1–Q8 adjudicated.

### K-10 — dual-backend operator failure modes — **CONCEDE + REVISE (merges with L-2)**

K and L both raise this from different angles. Fix: new §8 "Dual-backend
running mode" with explicit statement: **v0.7 dual-backend = two
independent logical engines, cairn migration is per-flow (or per-tenant)
not per-execution, no cross-backend cascade resolution**. Cross-backend
cascade is explicitly out-of-scope for v0.7.

### K-11 — RFC-003 fence-triple preservation — **CONCEDE + REVISE**

Trivial. Added line in §6 "What STAYS": RFC-003 fence-triple
`(lease_id, lease_epoch, attempt_id)` preserved; every lease-fenced
stored proc takes the triple in ARGV and rejects on mismatch.

### K-12 — HMAC kid rollout — **CONCEDE + REVISE**

K is right. Added rollout contract under Q4: Postgres `ff_waitpoint_hmac`
seeded from the union of all Valkey partitions' kids at migration cutover;
rotation thereafter is additive.

### K scenarios 1–7 — **CONCEDE (answered inline)**

All seven scenarios land in the revisions above:
- (1) scanner death mid-cascade → §3.1 invariant (K-2)
- (2) PgBouncer → Q2 (K-3)
- (3) NOTIFY storm → §3.2 rewrite (K-5)
- (4) rollback-after-NOTIFY + last_seen_event_id → Q1 addendum (K-4)
- (5) cross-backend cascade → §8 (K-10/L-2)
- (6) MAXLEN overshoot → Q6 rewrite (K-6/K-7)
- (7) tail reconnect storm → Q2 reconnect spec includes jittered wake
  (K-3)

## L's findings (cairn ops + migration + consumer)

### L-1 — schema-migration vs FUNCTION LOAD ordering — **CONCEDE + REVISE**

L is right; Stage A deliverable list named `sqlx::migrate!` but didn't
specify mode. Decision captured as **Q12**: **operator-applied
out-of-band via `sqlx migrate run` + boot-time schema-version check that
refuses start on mismatch**, mirroring Q5's boot-time partition-count
check. Zero-downtime rollout story: forward-only migrations, add-column-
then-deploy-binary pattern documented. Stage A deliverable amended.

### L-2 — dual-backend running mode — **CONCEDE + REVISE**

See K-10. New §8 covers per-binary vs per-cluster (answer: per-binary,
each ff-server instance targets one backend), source-of-truth (the
backend the instance is configured against), fan-out (no cross-backend
`CompletionBackend` fan-out in v0.7), and rollback (covered in §8).

### L-3 — metrics/dashboard + backup/PITR — **CONCEDE + REVISE**

L is right that §6's "OTEL-via-sqlx swap is a config change" is
structurally true but operationally incomplete. Added §9 operational
runbook checklist covering: labels-compat matrix (Stage E), backup/PITR
one-page runbook (Stage E), reconciler-as-primary-path alerting counter
`ff_completion_recovered_by_reconciler` (Stage D), rollback procedure
from Postgres to Valkey under SLO regression (Stage E).

### L exec-summary critique (weeks estimate) — **CONCEDE (minor)**

L is right that the 8-week "aggressive: Stages A–C only" framing is
incoherent because Stage D is the transport layer. Rewrote the range
bullets: aggressive = Stages A–D (no Stage E ops), realistic = full
A–E including cairn staging week.

### SDK-delta 1 — `rotate_waitpoint_hmac_secret(partition, …)` no-op — **CONCEDE + REVISE**

L is right. Silent no-op is an ergonomic trap. Fix: Postgres impl logs a
structured warning (`backend.hmac.per_partition_call_on_postgres` span
event with partition arg echoed back) so admin tooling can detect the
mismatch. Deprecation-and-remove deferred to v0.8 (separate RFC).

### SDK-delta 2 — `StreamCursor::At(String)` ts_ms non-monotonicity under PITR/clock-skew — **CONCEDE + REVISE**

L is right. Added Q3 invariant: ordering authoritative is `(ts_ms, seq)`
tuple; `ts_ms` alone is advisory. Documented in §3.2 + Stage C spec.

### SDK-delta 3 — `tail_stream` reconnect shape — **CONCEDE + REVISE**

Same as K-3/scenario-7. Q2 amendment now includes jittered SDK-side
reconnect budget when the backend is Postgres.

### SDK-delta 4 — `backend_version()` pin policy — **CONCEDE + REVISE**

L is right; added to Stage A deliverable: version-pin policy is **warn,
not refuse** by default on the SDK side; operators can flip to refuse
via config. Prevents ff-sdk pinning from becoming a hard deploy gate.

### SDK-delta 5 — `BackendErrorKind` new retry classes — **CONCEDE + REVISE**

L is right. Added to Stage B deliverable: ff-sdk retry-policy compat
shim translates `SerializationFailure`/`DeadlockDetected` to the existing
retryable-error classification cairn already handles.

### L author-questions 1–9 — **answered inline in revisions above**

All nine land in revisions L-1 through SDK-delta-5 and the new §8 + §9.

## Escalations (owner decisions, flagged)

None of K's or L's findings require escalation — all are mechanical
tradeoffs resolvable by the feedback_decide_obvious_escalate_tradeoffs
rule. The one borderline case is **Q11 (isolation level)**: strictly this
is a correctness tradeoff with performance implications, but the choice
(RC + row locks as default, SERIALIZABLE at the ~3 composite sites) is
industry-standard for pg-backed job systems and matches the existing
Valkey semantics (which are effectively SERIALIZABLE via slot lock) at
the sites that need it.

## Remaining follow-ups (tracked for round-2)

- Concrete migration-tool spec for Valkey→Postgres state transfer (Stage E
  deliverable, L walkthrough step 3). Currently named, not detailed.
- Rollback runbook from Postgres back to Valkey (Stage E deliverable,
  L walkthrough step 4). Currently named, not detailed.
- Labels-compat matrix table (Stage E deliverable). Currently named.

These three are Stage E content, not v0.7-master-doc content. They are
referenced in §9 checklist so round-2 challengers can see them.
