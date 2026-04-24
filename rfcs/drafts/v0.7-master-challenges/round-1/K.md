# K — round 1 challenge on v0.7 migration master

Target: `rfcs/drafts/v0.7-migration-master.md` @ `efaf2b3` on branch
`docs/v0.7-migration-master`. Persona: correctness + schema-first
skeptic. Q1–Q5 are locked; this challenge goes after *consequences*
the spec has not yet thought through.

## Bottom line

**DISSENT.** The spec is structurally sound but has three class-A
correctness holes that must be closed before Stage A opens:
(1) a direct self-contradiction on isolation level between §Exec-summary
crit-path item 1 and §L0; (2) silent reliance on `NOTIFY` semantics
without spelling out the "NOTIFY-inside-tx + listener-reconnect gap"
contract that Q1's outbox hinges on; (3) Q2's "one shared LISTEN
connection" design as written is a single point of failure for every
tail consumer and has no stated recovery path. Secondary issues below
are each fixable with a paragraph.

## Per-section review

- §Executive summary (L34–L96): **YELLOW.** Crit-path item 1 (L50–L52)
  asserts every FCALL becomes a single `BEGIN…COMMIT` "under
  `SERIALIZABLE` (or explicit row-lock + `READ COMMITTED`)" — that
  is a decision the spec claims is *already made* and then contradicts
  in §L0. See finding K-1.
- §Part 1 Layers (L99–L218): **YELLOW.** L0 (L104–L117), L2 (L133–L147)
  and L3 (L148–L162) each leave material semantic gaps around
  transaction duration, trigger-per-row NOTIFY storms, and NOTIFY
  transactional semantics. See K-1, K-4, K-5.
- §Part 2 Inventory (L221–L394): **YELLOW.** The difficulty column
  under-rates several rows: TRAIT-cancel at line 269 calls the cascade
  "21-key" but doesn't flag the cascade recursion crossing transaction
  boundaries (same hole as Hotspot 3.1). `TRAIT-append_frame` M-rated
  where Q6's "piggy-back DELETE in the INSERT tx" would make it a
  latency hotspot under a hot attempt. See K-6.
- §Part 3 Hotspots (L397–L570): **YELLOW.** 3.1 (L401–L427) glosses
  the transaction-boundary question that is the *whole* point of
  porting a recursive FCALL cascade: Rust-side recursion with one tx
  per hop is NOT equivalent to the Valkey behavior. Must state
  explicitly which invariants break if hop 2 rolls back after hop 1
  committed. See K-2.
- §Part 4 Open questions Q6–Q10 (L573–L799): **YELLOW.** Q6 dismisses
  "approximate MAXLEN" as a legacy artifact (L737) — that's wrong:
  RFC-015 deliberately chose approximate trim for p99. See K-7.
  Q9's proposal to *drop* `budget_reconciler` + `quota_reconciler`
  (L783) is only safe if *every* admission path is inside one SQL
  tx — Q1's LISTEN/NOTIFY + outbox + reconciler model explicitly
  contemplates partial-failure paths, which would re-introduce drift.
  See K-8.
- §Part 5 Stage plan (L802–L901): **YELLOW.** Stage A (L811–L827)
  defers Q6/Q7/Q8/Q9/Q10 past the scaffolding gate but Stage B
  (L830–L848) ships 22 stored procs that bake Q6's retention choice
  and Q7's capability contract. Either those Qs belong to Stage A,
  or Stage B must be partitioned. See K-9.
- §Part 7 Risks (L948–L1010): **YELLOW.** Missing: scanner death /
  ff-server partial-failure during migration while cairn runs
  dual-backend. 7.5 names the area but only at the "operator doc"
  level. See K-10.

## Specific findings

### K-1 (CRITICAL) — isolation level is self-contradictory

**Where:** L50–L52 (Exec summary crit-path #1) says "`SERIALIZABLE`
(or explicit row-lock + `READ COMMITTED`)." L114–L117 (L0) then
proposes "`READ COMMITTED` + row locks for the hot path, `SERIALIZABLE`
for the ~3 composite FCALLs."

**Why it matters:** The two are not the same decision. Under pg,
`SERIALIZABLE` uses SSI predicate locks and retries on
`serialization_failure` — call sites must handle
`SQLSTATE 40001` retry. `READ COMMITTED + SELECT FOR UPDATE` never
retries but accepts phantom-read risk across statements. Every
stored proc's error model, the backend error taxonomy mapping
(`BackendErrorKind::SerializationFailure` at L934), and the
scanner retry policies differ between the two. "Owner decision"
is fine at §L0 (L115–L117) — but the Exec summary claims it's
already decided. Pick one framing; if it's still open, it belongs
under Part 4 as Q11 with a resolution before Stage A.

**Minimal fix:** Either (a) promote to an owner-adjudicated
question Q11 before Stage A, OR (b) delete the "`SERIALIZABLE`"
wording from L51 and state authoritatively: "default is
`READ COMMITTED + SELECT FOR UPDATE SKIP LOCKED`; stored procs
that need multi-row predicate safety declare themselves via a
BEGIN-wrapper at proc entry and document the 40001-retry contract."

### K-2 (CRITICAL) — cascade recursion crosses tx boundaries without stated semantics

**Where:** §3.1 L416–L418 says "On Postgres each recursion hop is a
fresh transaction or the whole cascade is one fat transaction." The
proposal sketch at L419–L423 is "stored proc per hop + Rust cascade
loop iterates" — i.e. one tx per hop.

**Why it matters:** On Valkey, `ff_resolve_dependency` is one FCALL
per hop but the wake-up that drove the next hop was the PUBLISH
*after* commit, so a crashed process between hops left the child in
a consistent-but-unresolved state that `dependency_reconciler`
picked up. On Postgres with per-hop transactions, if hop 1 commits
(child A advanced to eligible, NOTIFY fired) and hop 2 crashes
mid-commit (child B never advanced), the cascade is in the same
"reconciler will fix it" state — *only* if the reconciler actually
inspects every edge and not just blocked-dep-set scans. Worker-C's
survey (engine L104–L112) describes the current Valkey cascade cap
of 50. Under Pg, depth-50 cascade = 50 txs = 50 commit-points =
50 crash-recovery windows. The spec must state: (a) the per-hop
commit makes each hop independently durable; (b) the reconciler's
miss-recovery scan must span the full reachable descendant set, not
just 1-hop children. L421–L423 do not say this.

**Alternative the spec rejected without discussion:** one fat tx
per root cascade, which would hold row locks on every descendant
for the cascade duration (potentially seconds under a wide flow).
That's the correctness-conservative option; per-hop is the
latency-conservative option. The tradeoff must be stated.

**Minimal fix:** Amend §3.1 with an explicit "crash-recovery
contract": per-hop commit + reconciler transitive-descendant sweep,
with a stated bound on the sweep interval relative to cascade
depth.

### K-3 (CRITICAL) — Q2 shared LISTEN connection has no failure-mode spec

**Where:** §Q2 L614–L636. "ff-backend-postgres runs ONE long-lived
shared LISTEN connection subscribed to all `ff_stream_<eid>_<aidx>`
channels."

**Why it matters:** If that connection drops (pg restart, TCP reset,
failover, idle-kill by a connection-pooler like PgBouncer in
transaction-pooling mode — which is the common cairn deployment
shape) every parked tailer misses NOTIFYs silently. pg's
`LISTEN/NOTIFY` gives no replay on reconnect: NOTIFYs sent while
your session was down are simply lost. Q1 addresses this for
*completions* via the outbox table (L593–L596). Q2 does not
address it for tail. The parked tokio tasks will wake on their
block_ms timeout and catch up via `SELECT WHERE id > cursor`, so
data is not lost — but p99 tail latency goes from "NOTIFY-latency"
to "block_ms" across every reconnect event.

Additionally: PgBouncer's transaction-pool mode is incompatible
with `LISTEN`. The spec must state that ff-backend-postgres
requires session-pooled connections for LISTEN and document how
cairn operators configure this.

**Minimal fix:** Amend Q2 with (a) explicit LISTEN-reconnect
behavior (on reconnect: issue `LISTEN` for every channel any
parked tailer cares about; all tailers receive a synthetic
"poll-now" wake); (b) pooler-compatibility constraint
("session-mode pool only; transaction-mode pool is unsupported
for ff-server connections"); (c) LISTEN-connection health metric
+ auto-restart.

### K-4 (MAJOR) — NOTIFY-in-transaction semantics not called out

**Where:** §L3 L148–L162, §Q1 L581–L603.

**Why it matters:** pg's `NOTIFY` is queued on `COMMIT`, not
executed immediately — so the outbox + NOTIFY pair is *atomic* by
construction: the outbox row is visible exactly when the wake-up
is delivered. That is actually a *nicer* property than Valkey's
PUBLISH (which fires before the slot lock releases for readers in
a different slot). The spec should state this explicitly because:
(a) it means `dependency_reconciler`'s miss-recovery role is
narrower than it is on Valkey (only subscriber-side drops, not
commit-ordering races); (b) it means the completion ordering is
monotonic per-transaction, which constrains the outbox schema
(a single SERIAL row id is fine; no need for a vector clock).
Without stating this, a later author will re-add a vector clock
or miss a race they think exists.

**Minimal fix:** One paragraph under §Q1 adjudication stating the
transactional-NOTIFY invariant and what it buys.

### K-5 (MAJOR) — per-row INSERT trigger NOTIFY fan-out under hot append_frame

**Where:** §3.2 L463–L467 proposes "row-level trigger on INSERT
into `ff_stream_frame` → LISTEN `ff_stream_<exec_id>_<attempt>`."

**Why it matters:** At 10 k frames/sec across the fleet (plausible
under cairn's agentic-LLM workload where every tool call is a
frame), that's 10 k NOTIFYs/sec through the single shared LISTEN
connection of K-3 + the in-memory channel router. NOTIFY has no
built-in coalescing. A hot attempt producing 1 k frames in a burst
will fire 1 k NOTIFYs to the one tailer on that channel. The
obvious fix is NOTIFY-coalescing in the trigger (only NOTIFY if
no other NOTIFY since last checkpoint), or NOTIFY from the
application rather than the trigger. Either way, the spec must
state the strategy — and "row-level trigger" as written is the
worst of the options.

**Minimal fix:** Amend §3.2 L463–L467 to pick a strategy:
application-level NOTIFY at end of `append_frame` tx (preferred,
composable with batched inserts) rather than per-row trigger.

### K-6 (MAJOR) — Q6 retention-in-append_frame tx is a lock-contention trap

**Where:** §Q6 L728–L740: "retention job that runs on `append_frame`
write (piggy-back, not scheduled) — `DELETE WHERE rank > N`."

**Why it matters:** On a hot attempt, every INSERT must also take
row locks on the N-oldest frames to DELETE them, plus the WAL cost
of both operations in the same tx. With MAXLEN ~ 10_000 and a
10 k-frame/sec attempt, every INSERT competes for the same oldest-N
window under a `DELETE WHERE rank > N`. This is a textbook HOT-table
lock contention pattern. The fallback ("make retention async via
trigger or janitor") is not a fallback — it's the primary design.

Additionally, under the composite stream-id Q3, `rank` must be
computed by `ROW_NUMBER() OVER (PARTITION BY stream ORDER BY ts_ms,
seq DESC)` which is not index-only — it's a scan of the attempt's
frames every INSERT. Not viable.

**Minimal fix:** Adjudicate Q6 *before* Stage B. Proposal: async
janitor scanner (reuses the polling pattern) running per-partition;
MAXLEN is documented as approximate-N with a bounded overshoot
(RFC-015 already set this contract — see K-7).

### K-7 (MODERATE) — Q6 mischaracterises RFC-015's MAXLEN contract

**Where:** §Q6 L736–L737: "the 'approximate N' is a legacy Valkey
artifact, not a contract we want to preserve."

**Why it matters:** RFC-015 (dynamic MAXLEN + DurableSummary)
deliberately chose approximate-N because strict retention on
the hot path was measured as a p99 regressor. Calling that a
"legacy artifact" misreads the accepted RFC. The Q6 proposal to
go *strict* on Postgres is actively worse than the Valkey status
quo and would need its own RFC amendment.

**Minimal fix:** Reframe Q6: "approximate-N is the contract;
Postgres implements it via async janitor with bounded overshoot;
the threshold + overshoot are configurable per the RFC-015 shape."

### K-8 (MODERATE) — Q9 scanner drop assumes stronger atomicity than Q1 provides

**Where:** §Q9 L782–L786: "Drop scanners 10 + 11 (budget_reconciler,
quota_reconciler) entirely since SQL tx atomicity removes the drift
they correct."

**Why it matters:** On Valkey the drift comes from multi-hash-tag
writes not being atomic. On Postgres, if the admission path is
entirely inside one tx, Q9's reasoning holds. But the scheduler
claim pipeline (§3.4) emits a NOTIFY on grant + writes a grant
row; if the grant is consumed by a worker and the worker's
downstream `report_usage` runs in a *separate* tx (it does, on a
different connection, from a different process), then budget
counters drift under partial failure exactly as they do on Valkey.
The reconcilers exist to clean up that second-process drift, not
the in-tx race.

**Minimal fix:** Keep `budget_reconciler` + `quota_reconciler`
as-is; drop only `index_reconciler` (which *is* redundant under
SQL atomicity because the "indexes" are actual pg indexes).
Amend §Q9 with the cross-process-drift argument.

### K-9 (MODERATE) — Stage A/B boundary doesn't match design-lock boundary

**Where:** §Stage A L814–L826 locks Q1–Q5. §Stage B L830–L848
ships 22 stored procs that bake Q6 (retention), Q7 (capability),
Q8 (version negotiation).

**Why it matters:** `append_frame` is "moderate" in §2.2 and will
land in Stage B. Its stored proc cannot be written without a Q6
answer. `report_usage` and the cap-matching paths for read-only
list methods can't be written without a Q7 answer. Stage B will
either block on mid-stage adjudication or ship stored procs that
need rewriting.

**Minimal fix:** Either (a) promote Q6 + Q7 + Q8 into Stage A's
design-lock set, OR (b) split Stage B into B1 (pure-T methods,
~15 of 22) and B2 (M methods that need Q6/Q7/Q8), with Q6/Q7/Q8
adjudication as the B1 → B2 merge-blocker.

### K-10 (MODERATE) — dual-backend operator failure modes under-specified

**Where:** §7.5 L995–L1002, §Q5 L702–L721.

**Why it matters:** Q5 adjudication says cairn can run Valkey +
Postgres side-by-side during migration. §7.5 names "shared
completion channels" as a risk but punts to operator docs. The
specific failure mode that needs a design answer: if a flow's
executions are split across the two backends (flow created on
Valkey, new executions routed to Postgres), does dependency
resolution cross the boundary? The `partition_router` (§3.1)
assumes single-backend reachability. The spec is silent on
whether dual-backend is "two separate flowfabric instances with
distinct flow IDs" (safe but limited) or "one logical flowfabric
spanning both backends" (materially harder).

**Minimal fix:** §Q5 or §7.5 must state: "dual-backend = two
independent logical engines; cairn's migration is per-flow, not
per-execution; no cross-backend cascade resolution in v0.7."
If that's *not* the intent, the design hole is much larger.

### K-11 (MINOR) — RFC-003 fence-triple preservation not called out

**Where:** Not mentioned anywhere in the master doc.

**Why it matters:** RFC-003 established `(lease_id, lease_epoch,
attempt_id)` as the fence-triple checked on every lease-fenced
write. The handle_codec §3.3 L499 proposes "drop `lease_ttl_ms`"
but keeps the triple. Good — but the spec must *say* the triple
is preserved as a cross-backend invariant so a future stored-proc
author doesn't simplify it away.

**Minimal fix:** One line in §Part 6 "What stays": "RFC-003
fence-triple invariant preserved; every lease-fenced stored proc
takes `(lease_id, lease_epoch, attempt_id)` in ARGV and rejects
on mismatch."

### K-12 (MINOR) — HMAC rotation rollout gap

**Where:** §Q4 L665–L694.

**Why it matters:** Q4 collapses to a single global HMAC row on
Postgres. During a mixed-backend rollout, a token minted on Valkey
(per-partition kid) must validate on Postgres (global kid) and
vice versa. The new `rotate_waitpoint_hmac_secret_all` is fine
for rotation; the *validation* path during rollout is not
specified. If kid K exists in partition 3 on Valkey but not in
the Postgres global row, a token minted on Valkey pre-migration
will fail to validate on the Postgres read path.

**Minimal fix:** §Q4 must state the rollout contract: "Postgres
backend's waitpoint_hmac table is seeded from the union of all
partitions' kids at migration time; rotation thereafter is
additive."

## Scenarios the spec doesn't answer

1. **Scanner death mid-cascade.** `partition_router` starts a
   depth-8 cascade. After hop 3 commits, the ff-server pod crashes.
   Hop 4 never runs. Which reconciler picks this up, and how fast?
   Today on Valkey the `dependency_reconciler` scans blocked-dep
   sets every 15 s (Worker-C §1.1 row 13). On Postgres per-hop-tx,
   does the reconciler's blocked-set scan find child 4 given child
   3 already advanced to eligible? Spec says nothing.

2. **PgBouncer transaction-pool deployment.** Cairn operator wires
   ff-server to its standard PgBouncer transaction-pool proxy
   (recommended for most pg apps). All `LISTEN` commands silently
   break. Spec does not document the session-mode requirement.

3. **Hot attempt NOTIFY storm.** One LLM tool-call attempt emits
   1 k frames in 200 ms. With per-row NOTIFY trigger (K-5), that's
   5 k NOTIFYs/sec through the single LISTEN connection (K-3)
   serving all tailers across all attempts. What is the coalescing
   or throttling story?

4. **Rollback after NOTIFY queued.** Stored proc for
   `ff_complete_execution` issues `NOTIFY ff_completion, '123'`
   then hits a constraint violation and rolls back. pg correctly
   drops the NOTIFY (transactional). Now the dependency_reconciler
   must be the fallback — but what if the stored proc *did*
   commit and the *listener* is down? Spec hints at outbox +
   replay (§Q1 L593–L596) but does not say the replay cursor is
   persisted on the listener side. If the listener restarts and
   starts from `MAX(event_id)`, it misses events between last
   flush and restart. Needs explicit `last_seen_event_id`
   persistence contract.

5. **Cross-backend cascade under dual-run.** Flow F has executions
   E1 (on Valkey) and E2 (on Postgres). E1 completes; its NOTIFY
   fires on pg; partition_router tries to resolve E2's deps, which
   need upstream outcome from E1 — which is on Valkey, not pg.
   K-10 flags this; scenario needs an answer.

6. **MAXLEN overshoot under retention-janitor.** 10 attempts each
   producing 100 frames/sec share one partition. The janitor runs
   every 5 s. During the gap, the stream table grows by 5000
   frames × 10 = 50 k above MAXLEN. What's the documented bound?
   RFC-015 has one; master doc doesn't reference it.

7. **Tail reconnect storm.** Single shared LISTEN connection
   restarts. 10 k parked tailers all wake on their block_ms
   timeout simultaneously and all issue `SELECT WHERE id > :last`
   against the same partition. Thundering herd; no jitter spec.

## Questions for the author (not Q1–Q5)

1. Is the default isolation level `READ COMMITTED + SELECT FOR
   UPDATE` or `SERIALIZABLE`? (K-1) Needs Q11 adjudication before
   Stage A.
2. Cascade recursion: per-hop commit vs single fat tx? (K-2, §3.1)
3. LISTEN reconnect + pooler-compatibility contract. (K-3, §Q2)
4. NOTIFY strategy on `ff_stream_frame` INSERT: trigger vs
   application-level. (K-5, §3.2)
5. MAXLEN retention strategy: sync-in-tx vs async-janitor
   (approximate-N)? (K-6 + K-7, §Q6)
6. Does `budget_reconciler` + `quota_reconciler` drop, given
   cross-process drift is real? (K-8, §Q9)
7. Dual-backend semantics: per-flow routing only, or
   cross-backend cascade? (K-10, §7.5 + §Q5 rollout contract)
8. HMAC kid union at migration cutover. (K-12, §Q4)
