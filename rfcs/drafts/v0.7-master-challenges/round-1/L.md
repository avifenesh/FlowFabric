# L — round 1 challenge on v0.7 migration master

Reviewer persona: cairn consumer / SRE operations pragmatist. Focus: adoption
story, dual-backend semantics, schema-vs-loader ordering, metrics parity,
SDK-visible behavior deltas under a "trait stays the same" claim. Written
against `rfcs/drafts/v0.7-migration-master.md` @ `efaf2b3` (post-Q1–Q5
adjudications). K's round-1 not read.

## Bottom line

**DISSENT — approve-in-principle, block-on-ops.**

The technical translation table (Parts 1–3) is the best artifact we've had
on this migration. Parts 4–5, however, are written as an engineering plan
and not as an adoption plan. There is no stage at which a cairn-deployed
ff-server can be **safely** moved from Valkey to Postgres with a rollback,
and the dual-backend running story (load-bearing for Q5's "rolling
migration" claim) is underspecified. Those are fixable in a round-2 revision
and I would flip to ACCEPT if they are.

## Per-section review

- §Executive summary (L34–L96): **YELLOW.** Claim at L77–L80 that 8 weeks
  is "Stages A–C only, scanner polling kept, no performance tuning" ignores
  that Stage D is the transport layer (LISTEN/NOTIFY) — without Stage D
  there is no working `CompletionBackend` and therefore no usable Postgres
  server. Stage A–C alone does not produce a shippable artifact. The
  aggressive-estimate scope is incoherent.
- §Part 1 (L99–L219): **GREEN.** Translation patterns are clean, L10
  (L211–L218) correctly flags the loader→migration-tooling gap, though
  see ops-gap #1 below.
- §Part 2 (L221–L395): **GREEN.** Inventory is the load-bearing artifact;
  the difficulty calibrations look right to an adopter.
- §Part 3 (L397–L570): **GREEN.** Hotspots identify the right five. §3.2
  (L429–L470) understates tail reconnect semantics, see SDK deltas below.
- §Part 4 (L573–L799): **YELLOW.** Q1–Q5 adjudications are sound on
  correctness but silent on operability. Specifically Q1 outbox (§L581–L604)
  does not specify retention/cleanup; Q4 (§L665–L694) creates an SDK
  divergence that is called out only in a rustdoc note (see SDK delta #1).
- §Part 5 (L802–L901): **RED.** This is the section that blocks my ACCEPT.
  Stages A–E describe an engineering schedule, not an adoption schedule.
  See ops gaps #1, #2, #3 below.
- §Part 6 (L904–L945): **GREEN.** Honest inventory of what survives.
- §Part 7 (L948–L1010): **YELLOW.** §7.5 (L996–L1001) correctly flags
  mixed-version rollouts as "operator-doc risk" and then… does not assign
  it to any stage. Stage E is where this has to land and it is not a
  deliverable there.

## Operational findings

Each gap names the stage where cairn's SRE will first notice it and what
they will ask before production adoption.

### Op-gap 1 — schema-migration vs function-load ordering is undefined

Surfaces at: **Stage A (L817–L827)**, in the `sqlx::migrate!`-based
schema-loader deliverable. Lua FCALLs land via `FUNCTION LOAD REPLACE` at
server boot (L212, L390) — any ff-server instance can bootstrap its own
primitives against a shared Valkey. Postgres DDL cannot be done that way:
schema migrations must land **before or in coordination with** server
binaries that expect them. The master doc does not specify:

1. Is the migration applied by the ff-server binary at boot (`sqlx::migrate!`
   auto-apply), or out-of-band by an operator step (`sqlx migrate run`
   pre-deploy)?
2. What happens when a new ff-server binary starts against an un-migrated
   database — does it refuse boot, auto-apply, or block waiting for
   migration?
3. What happens to an *old* ff-server binary against a *newer* schema
   (rolling deploy mid-rollout)?

Cairn SRE will ask: "If I'm running three ff-server replicas and I want
zero-downtime rollout of v0.7.1 that adds a column, what's my procedure?"
The answer is nontrivial and the Stage A deliverable list doesn't name it.

**Fix:** Add an explicit migration-tool contract to Stage A. Either (a)
migrations applied out-of-band with a compat check at boot (refuses start
on schema mismatch; matches Q5's boot-time partition-count check at L711–
L717), or (b) binary auto-applies forward-only migrations with a lock and
a version check. Pick one; document the deploy order.

### Op-gap 2 — no defined dual-backend running mode

Surfaces at: **Stages B–D (L830–L882).** The Q5 adjudication (L695–L726)
justifies keeping the 256-partition model because it lets cairn "run
Valkey + Postgres side-by-side during migration without consumer-code
changes" (L703–L705). That's an adoption claim. Nothing in Stages A–E
describes:

1. Whether a single ff-server binary can be configured with *two* backends
   simultaneously (one read-path, one write-path) or whether migration is
   per-replica flag-flip.
2. What the truth is during migration — is Valkey still source-of-truth
   and Postgres a shadow? Does Postgres become source-of-truth at some
   cutover? Are in-flight executions mirrored or drained?
3. Whether the `CompletionBackend` can fan out to two transports at once
   (Valkey PubSub + Postgres LISTEN) while subscribers migrate.

Without this, "side-by-side" is aspirational. Cairn's SRE cannot migrate
an active workflow DAG if they have to cut over atomically.

**Fix:** Either (a) admit Q5's "rolling" claim is wrong and spec a
flag-day cutover with a drain procedure in Stage E (honest), or (b)
add a Stage D.5 "mirrored-writes dual-running mode" with a feature flag
and define truth semantics. Today the doc handwaves between the two.

### Op-gap 3 — metrics cardinality delta is unlabeled

Surfaces at: **Stage B (L846–L848)** and §Part 6 L923–L925. §6 claims
OTEL metrics are backend-agnostic ("OTEL-via-sqlx swap is a config
change"). This is *structurally* true and *operationally* incomplete.
Dashboards today carry labels derived from Valkey call taxonomy
(command-name, slot, cluster-node). Postgres equivalents are different
(query-name, partition_id, pool-acquire latency). Cairn's existing
Grafana dashboards will break — the panels won't error, they'll go
blank, which is worse. Label stability under backend swap is not a
Stage deliverable.

**Fix:** Stage E should ship a labels-compat matrix. What metrics names
survive identically, what rename, what are new. If any dashboard panel
from the 0.3.2/0.4.0 set goes blank on Postgres, cairn needs to know
before cutover, not after.

### Op-gap 4 — backup / restore / point-in-time recovery shape is absent

Surfaces at: **Stage E (L884–L900).** Valkey backup model is RDB/AOF +
whole-keyspace replay. Postgres backup model is WAL + `pg_basebackup` +
PITR. The master doc does not cover:

- Can an ff-server come up against a `pg_basebackup` snapshot mid-flow?
  Specifically: lease expiry times are wall-clock; do running leases
  after a PITR get reclaimed cleanly or explode?
- What are the at-least-once vs exactly-once semantics of LISTEN/NOTIFY
  after a crash-recover from WAL replay (the outbox replay at L593–L595
  mostly handles this, but stream-tail NOTIFY per L617–L635 is *not* in
  the outbox)?
- Does the cairn operator need streaming replication, or is a nightly
  `pg_dump` enough? This is a T-shirt-sizing answer cairn can't make
  from the doc.

**Fix:** Stage E should ship a restore-runbook (even a one-page sketch).
Valkey RDB→Postgres basebackup is a real operator-training cost.

### Op-gap 5 — observability for the migration itself

Surfaces at: **Stage D (L869–L882).** Once LISTEN/NOTIFY is live, a
dropped NOTIFY (reconciler safety net at L595) is by design silent.
The reconciler's recovery latency is the SLA. We need per-scanner
counters for "completions recovered via reconciler vs pushed via
LISTEN" — otherwise we cannot tell at runtime whether LISTEN/NOTIFY
is healthy or silently degraded to polling. The master doc names the
reconciler as a backstop; it does not name the metric that tells
ops when the backstop is carrying load.

**Fix:** Stage D should ship a `ff_completion_recovered_by_reconciler`
counter (or similar). Threshold alert ships in Stage E.

## Cairn migration walkthrough

Today: cairn runs ff-server v0.6.1 backed by Valkey 0.6.1 with
`PartitionConfig{256,32,32}`. Migration to v0.7 Postgres-backed. Step
by step:

1. **Cairn picks up ff-server v0.7.0 (Valkey still).** Stage A lands;
   cairn sees `TRAIT-claim` + `TRAIT-claim_from_reclaim` filled in (from
   `Unavailable`) — this is an SDK-visible change even on Valkey (Q10,
   L788–L798). Cairn's ff-sdk code paths that used the SDK-pre-dating
   claim route now route through the trait. **Missing from spec:**
   does the old SDK path stay as a deprecated fallback or is it ripped
   out? If ripped, cairn upgrades ff-sdk and ff-server in lockstep or
   breaks. Name this in Stage A deliverables.

2. **Cairn deploys ff-server v0.7.1 (first Postgres-backed canary).**
   Stage D is done, Stage E partial. At this moment cairn has one
   Postgres-backed ff-server and N Valkey-backed. **Missing from
   spec:** what URL does cairn hit for what? If both are serving the
   same tenants, who arbitrates conflicting leases? If partitioned by
   tenant, how does cairn cut a tenant over — is there a "drain the
   Valkey version of tenant T" step? Q5 says wire-compat is preserved,
   but wire-compat alone doesn't make state-shared dual-running safe.

3. **Cairn moves tenant T to Postgres-only.** Operator step. Needs a
   state-transfer. Stage E (L892–L894) says "Migration tooling for
   Valkey→Postgres state transfer (one-shot dump + load). Spec only if
   cairn's migration doesn't need it." — the entire *point* of co-
   adoption (L950–L958) is that cairn migrates; the conditional is
   self-contradictory. Cairn will need this tool. Promote from "spec
   only if" to unconditional Stage E deliverable.

4. **Cairn runs Postgres-only.** `Stage E` merge blocker at L898–L900
   says "cairn running entirely on Postgres in staging for a full
   week." Good. But if that week exposes a latency regression on
   `append_frame` retention (§Q6 fallback) or on scheduler claim
   (§7.2 L968–L971), what's the rollback? **Missing from spec:**
   a defined rollback path from Postgres back to Valkey under
   pressure.

## SDK contract deltas

Places where "trait stays the same" is technically true but the
consumer sees different behavior.

### SDK-delta 1 — `rotate_waitpoint_hmac_secret(partition, …)` partition arg is a no-op on Postgres

Severity: **ergonomic-gap, not docs-only.** Q4 adjudication (L665–L693)
keeps the old per-partition method signature and silently ignores the
`partition` arg on Postgres. Callers who loop "rotate per partition for
auditability" (reasonable admin script pattern) will on Postgres
redundantly overwrite the same global row N times. The rustdoc note
(L685–L687) is not sufficient; admin tooling that measures rotation
progress per-partition will report meaningless progress. **Fix:** either
deprecate-and-remove the per-partition method on a release cadence, or
return a structured warning from the Postgres impl ("partition arg
ignored") that admin tools can surface. Cairn's admin tooling audit is
a real consumer cost here.

### SDK-delta 2 — `StreamCursor::At(String)` wire-identical, semantics-adjacent

Severity: **real-divergence, latent.** Per Q3 (L638–L664), the
`<ms>-<seq>` string shape is preserved and parsed at the boundary.
However, the **resolution of `ts_ms`** on Postgres is
`clock_timestamp()*1000` which on a clock-skewed primary or post-PITR
can regress. Valkey's `TIME` is process-monotonic-within-one-node.
Two frames appended in the same ms with the same seq are unique on
Valkey (one node mints); on Postgres under a failover with clock
skew, the invariant "strictly increasing per-stream" relies entirely
on the `seq_for_stream` sequence, not on `ts_ms`. Downstream
consumers filtering on `ts_ms` (timestamp queries at L655–L657) can
see mild non-monotonicity. **Fix:** document the invariant "ordering
authoritative by `(ts_ms, seq)` tuple, ts_ms alone is advisory" in
the Stage C spec.

### SDK-delta 3 — `tail_stream` reconnect latency characteristic shifts

Severity: **ergonomic-gap.** Q2 (L605–L636) promises SDK-visible
behavior unchanged. Operationally, Valkey XREAD BLOCK reconnect is
client-driven; the SDK holds its own reconnect budget. LISTEN-backed
tail under the shared-notifier model at L613–L628 has a *different*
reconnect shape — a dropped LISTEN connection on the server side
drops *all* tailers simultaneously (shared conn). SDK-side reconnect
logic sized for independent stream failures may storm-reconnect
against one restart. **Fix:** Stage D should include a jittered
reconnect contract on the SDK side when running against a Postgres
backend.

### SDK-delta 4 — `Backend::backend_version()` is new, pins are new

Severity: **docs-only, but visibility-relevant.** Q8 (L758–L769)
proposes a new trait method returning `(name, version)`. Consumers
that previously had no version negotiation now have one, and if
cairn's ff-sdk starts asserting version compatibility, it becomes a
hard gate on deploy ordering. **Fix:** spec the version-pin policy
in Stage A (does cairn refuse to talk to an older/newer backend, or
warn?).

### SDK-delta 5 — `BackendErrorKind` gains `SerializationFailure` / `DeadlockDetected`

Severity: **real-divergence.** Per §Part 6 L932–L934, the `#[non_exhaustive]`
additive path adds new error kinds on Postgres. These are retryable by
contract on Postgres (standard pg concurrency behavior) but were
*impossible* on Valkey (single-slot FCALL cannot deadlock). cairn's
existing error-handling code that classifies errors as "retry vs
fatal" has no prior case for these. **Fix:** Stage B should ship a
retry-policy compat shim in ff-sdk that translates the new error
kinds to the same retry classes cairn already has.

## Questions for the author

(Stage-plan specifics + operational unknowns — not Q1–Q5 territory.)

1. **Schema migration application mode** — is it binary-applied at
   boot (`sqlx::migrate!`) or operator-applied out-of-band? Pick in
   Stage A.
2. **Old ff-sdk → new ff-server compat** — does v0.7 require lockstep
   SDK upgrade, or does it accept v0.6.x SDKs? If lockstep, say so in
   Stage A.
3. **Dual-running mode** — is dual-backend per-binary or per-cluster?
   If per-binary, what's the feature flag? If per-cluster, what's
   the per-tenant cutover procedure?
4. **Cairn migration tooling** — promote §Stage E "spec only if
   cairn's migration doesn't need it" to an unconditional deliverable,
   or explain when cairn would plausibly not need it.
5. **Rollback contract** — what is the supported path from Postgres-
   backed v0.7 back to Valkey-backed v0.6.x if a latency SLO
   regresses in Stage E's cairn-staging week?
6. **Metrics-label stability matrix** — which metric names + labels
   are cross-backend stable? Cairn has Grafana panels depending on
   the Valkey taxonomy.
7. **Backup/PITR runbook** — one page sketch sufficient. Without it,
   cairn cannot size the operator-training cost of adoption.
8. **Reconciler-as-primary-path alerts** — where's the counter that
   says "LISTEN/NOTIFY is silently degraded to polling"? Stage D or E?
9. **`rotate_waitpoint_hmac_secret(partition, …)` Postgres behavior**
   — is the plan deprecate-and-remove on a release cadence, or forever-
   no-op? Names the migration window for admin tooling.

---

*L challenge written before reading K's round-1. Adoption/ops angle
deliberately distinct from correctness/semantics angle K was briefed on.*
