# P3.6 — Bridge-event gap report (comparison pass)

**Author:** Worker-3.
**Branch:** `bench/p36-bridge-audit` (stacked on W2's `aab0c83`).
**Input:** `benches/perf-invest/bridge-event-audit.md` (W2's inventory,
415 lines) + cairn-rs at `/tmp/cairn-rs` (read-only).
**Scope:** cross-reference W2's 19 candidate gaps (§5.1–§5.4) + 1
non-FCALL defect (§5.5) against cairn-fabric's actual consumer /
emit sites.

Read-only. No code changes.

## §0 Load-bearing finding

**Cairn does not subscribe to any FF stream for lifecycle
observation.** It emits its own `BridgeEvent` variants synchronously
after calling FF — the bridge is cairn-internal, driven entirely by
cairn's service-method call graph, not by FF-side XADDs.

Evidence:
- `grep -rn "BridgeEvent::" /tmp/cairn-rs/crates/` — every emit is
  from cairn's own wrapper methods (`run_service.rs`,
  `task_service.rs`, `session_service.rs`, `worker_sdk.rs`). Pattern
  is always `FCALL-to-FF → emit(BridgeEvent::…)` in the same
  request function.
- `grep -rn "xread\|XREAD\|XRANGE\|xrange" /tmp/cairn-rs/crates/`
  returns only `ff_sdk::task::read_stream` call sites in
  `cairn-fabric/src/stream.rs`, all for attempt-output frame replay
  — not lifecycle observation.
- `grep -rn "tail\|subscribe\|poll.*execution" /tmp/cairn-rs/…` —
  only `is_lease_healthy()` SDK-side self-checks and many
  `hgetall(&ctx.core())` call sites inside cairn's own service
  methods (read-back-after-write, not polling).
- There is NO background task in cairn that periodically reads
  exec_core state.

Cairn's authoritative emit list (cairn-fabric/src/event_bridge.rs:17-87):

```
BridgeEvent::ExecutionCreated { run_id, session_id, project, correlation_id }
BridgeEvent::ExecutionCompleted { run_id, project, prev_state }
BridgeEvent::ExecutionFailed { run_id, project, failure_class, prev_state }
BridgeEvent::ExecutionCancelled { run_id, project, prev_state }
BridgeEvent::ExecutionSuspended { run_id, project, prev_state }
BridgeEvent::ExecutionResumed { run_id, project, prev_state }
BridgeEvent::ExecutionRetryScheduled { run_id, project, attempt }
BridgeEvent::TaskCreated { task_id, project, parent_run_id, parent_task_id }
BridgeEvent::TaskLeaseClaimed { task_id, project, lease_owner, lease_epoch, lease_expires_at_ms }
BridgeEvent::TaskStateChanged { task_id, project, to, failure_class }
BridgeEvent::SessionCreated { session_id, project }
BridgeEvent::SessionArchived { session_id, project }
```

**Implication for W2's candidate gaps:** the FF-side gap analysis
(XADD or no XADD?) is orthogonal to cairn's consumption. Most of
W2's candidates are not gaps FROM CAIRN'S PERSPECTIVE because cairn
doesn't observe FF streams at all. But some ARE gaps in a different
sense: FF-initiated state transitions (scanner-driven promotions,
lease expiries, timeout-triggered retries) that cairn's
call-then-emit pattern cannot reach by construction.

This report reclassifies W2's candidates under that lens.

## §1 Confirmed gaps (cairn expects, FF doesn't emit)

These are transitions where cairn's read-model will diverge from
FF's exec_core truth because cairn has no way to observe the
transition.

### §1.1 FF-initiated lease expiry / reclaim

**W2 inventory §4 row:** `ff_mark_lease_expired_if_due` — emits
`lease_history` `expired`, core state writes
`ownership_state=lease_expired_reclaimable`.

**Cairn expectation:** When a lease expires and FF transitions the
execution off the worker, cairn's `TaskReadModel` should see a
`TaskStateChanged` → `Failed` (failure_class = `LeaseExpired`) or
equivalent. The test at
`/tmp/cairn-rs/crates/cairn-fabric/tests/integration/test_event_emission.rs:1-20`
explicitly documents this historical bug: "tasks claimed via any
path that did not populate the registry … silently skipped emission
— and the cairn-store TaskReadModel projection drifted from FF's
exec_core truth."

**Current cairn handling:** cairn emits `TaskStateChanged` only
from its own `FabricTaskService::complete` / `fail` / `cancel` paths
(task_service.rs:562, 663, 741). If the worker dies mid-task and FF
scanner-reclaims the lease, none of those methods fires. Cairn's
read model stays stuck at `Running`.

**Fix site recommendation — FF side.** Add emission in
`ff_mark_lease_expired_if_due` via an already-present
`lease_history` XADD. Cairn then needs a tail consumer on
`lease_history_key` for the lease-expiry kind. Alternative: FF emits
a dedicated `BridgeEvent`-shape message on a new stream. The
already-existing `lease_history` XADD is the obvious path; the
missing piece is cairn-side consumer that reads it.

Two fix-owner options, pick one:
- **Option A (FF-side):** keep the `lease_history` `expired` XADD,
  add a FF-server-owned tail daemon that forwards lease-expiry
  events to cairn over the existing event-bridge mpsc. Lower cairn
  churn.
- **Option B (cairn-side):** add a cairn-fabric background task
  that tails `lease_history_key` for each active execution and
  emits `BridgeEvent::ExecutionFailed { failure_class: LeaseExpired }`
  on an `expired` frame. Per-execution tail cost; may need
  cross-partition aggregation.

**Recommendation: Option B — cairn owns it.** The observation is
cairn-specific (cairn is the only current consumer), and FF-server
should not grow a bridge-specific forwarder. Cairn can subscribe to
`lease_history_key` per-execution at claim time (the ClaimedTask
shape already knows the exec_id) and unsubscribe on terminal.

### §1.2 FF-initiated retry schedule (scanner-triggered)

**W2 inventory §4 row:** `ff_fail_execution` retry path — emits
`lease_history` `retry_scheduled`, core writes
`attempt_state=pending_retry_attempt`, `public_state=delayed`.

**Cairn expectation:** When a task fails via cairn's `fail_with_retry`
(worker_sdk.rs:293-333), cairn emits
`BridgeEvent::ExecutionRetryScheduled`. BUT: when FF re-eligibles
a delayed retry via `ff_promote_delayed`
(scheduling.lua:308, W2 §5.3), cairn isn't told that the task is
now eligible to be re-claimed. The retry scheduler's transition
from `delayed` → `eligible` is scanner-driven, cairn never called
anything.

**Current cairn handling:** cairn's read model projects retry via
the initial `ExecutionRetryScheduled` event. It does NOT track the
delayed→eligible transition.

**Is this a gap?** Arguable. The external observer of a retry is
usually interested in "retry was scheduled" and "retry outcome"
(next claim + terminal), not the scheduler tick. Cairn's current
model seems fine for external SSE audit.

**Recommendation: NO FIX NEEDED.** Mark as intentional silence —
cairn tracks retry via the initial scheduling event and the
eventual next-attempt claim; the scanner tick between them is not
observable and doesn't need to be.

### §1.3 FF-initiated timeout expiry

**W2 inventory §4 row:** `ff_expire_execution` has three branches:
`active`, `suspended`, `runnable`. Only `active` and `suspended`
emit `lease_history`; the `runnable` branch (L1464→L1615, execution
expired on deadline WITHOUT ever being claimed) writes terminal
state with no XADD.

**Cairn expectation:** Cairn's emission for execution-level terminal
transitions comes from
`run_service.rs:628` (`ExecutionCancelled`) and `worker_sdk.rs`
complete/fail/cancel paths. None of those fires when FF's scanner
expires an unclaimed execution. Cairn's read model stays at
`Created` / `Waiting` indefinitely.

**Severity: HIGH.** An execution that never got claimed and hit
its deadline is a real operator-visible event — the user submitted
work, the worker never picked it up in time, and the execution
died silently. Cairn's projection showing the run as still
`Waiting` is pathological for UX.

**Fix site recommendation — FF side + cairn side.**
- **FF side:** add the missing `lease_history` `expired` XADD in
  the `runnable` branch of `ff_expire_execution` (L1464→L1615). The
  inventory flag from W2 §5.2 is correct. This is a
  lease-history-emit-gap in isolation from bridge events.
- **Cairn side:** subscribe to `lease_history_key` (per §1.1
  recommendation) and map `expired` frames to
  `BridgeEvent::ExecutionFailed { failure_class: Timeout }` or a
  new `ExecutionExpired` variant.

**Recommendation: BOTH, FF fix first.** The FF-side emit is a
straightforward symmetric fix — the other two branches already
emit. Cairn-side subscription can land once FF is consistent.

### §1.4 FF-initiated scheduler promotions (delayed → eligible, blocked → eligible)

**W2 inventory §5.3:** `ff_promote_delayed`,
`ff_promote_blocked_to_eligible`, `ff_unblock_execution` — all
transition `eligibility_state` → `eligible_now` with no stream emit.

**Cairn expectation:** None. These are intra-lifecycle transitions
(an execution becomes ready for claim). Cairn observes claim via
`TaskLeaseClaimed` (task_service.rs:420), so the "promoted to
eligible" step is redundant — the next claim makes the transition
cairn-visible. If no claim ever happens, the timeout path in §1.3
catches it.

**Recommendation: NO FIX NEEDED.** Mark as intentional silence —
these are scheduling tick transitions, not lifecycle events cairn
cares about.

### §1.5 FF-initiated admission block

**W2 inventory §4 row:** `ff_block_execution_for_admission` —
transitions `public_state=rate_limited` via scanner. No emit.

**Cairn expectation:** Unclear. If cairn wants to surface
"rate-limited" status to users (via SSE audit), the
`rate_limited` public_state transition must be visible. Cairn's
current `BridgeEvent` enum has no `ExecutionRateLimited` variant,
so the answer is probably "no, cairn does not surface this."

**Recommendation: NO FIX NEEDED.** Cairn's consumer shape doesn't
include rate-limited as a public-facing state. Document as
intentional silence. If cairn ever adds a rate-limited UI, this
moves to confirmed-gap and needs FF-side emission.

## §2 Intentional silences (cairn doesn't subscribe, documented rationale)

These are the W2 candidates that are NOT gaps from cairn's
perspective. Listed so future readers know the silence is
deliberate.

| FCALL (W2 §5 class)                       | Cairn-side rationale                                                                                                     |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `ff_create_execution` (§5.1)              | Cairn emits `ExecutionCreated` itself at run_service.rs:399 after the call. No subscription needed.                      |
| `ff_create_flow` (§5.1)                   | Cairn's `BridgeEvent` has no `FlowCreated` variant. Flows are FF-internal coordination; cairn uses them as opaque IDs.   |
| `ff_create_pending_waitpoint` (§5.1)      | Cairn observes suspensions via `ExecutionSuspended` (worker_sdk.rs:376, 409, 441). Waitpoint lifecycle is a FF implementation detail it calls transparently. |
| `ff_create_budget` / `ff_create_quota_policy` (§5.1) | Admin-managed; cairn `BridgeEvent` has no budget/quota variants. No subscription intended.                          |
| `ff_resume_execution` (§5.2)              | Cairn calls `ff_resume_execution` itself (run_service.rs:888) and emits `ExecutionResumed` at :894 / :1134.              |
| `ff_deliver_signal` resume-branch (§5.2)  | Cairn calls `deliver_signal` via its SignalBridge (signal_bridge.rs) and emits via its own wrappers. Bridge covers it.    |
| `ff_expire_suspension` auto_resume (§5.2) | Out of cairn's current consumption model — cairn invokes `ExecutionSuspended` but does not track auto-resume transitions. If this is a defect, cairn-side fix (not in PR#19 scope). |
| `ff_close_waitpoint` (§5.2)               | Waitpoint close is FF-internal; cairn observes the paired `ExecutionCancelled` / `ExecutionCompleted` at the exec level. |
| `ff_promote_delayed` / `ff_promote_blocked_to_eligible` / `ff_unblock_execution` (§5.3) | Covered in §1.4 above — scheduling ticks, cairn doesn't need them.                                               |
| `ff_block_execution_for_admission` (§5.3) | Covered in §1.5 above — cairn has no rate-limited variant.                                                              |
| `ff_apply_dependency_to_child` / `ff_resolve_dependency` (§5.3) | Cairn does not currently track FF flow-edge dependencies (task_service.rs:350-356 explicitly defers to FF's native coordination). No subscription intended. |
| `ff_add_execution_to_flow` (§5.4)         | Membership is opaque to cairn — cairn uses flows as grouping handles, not as observed entities.                           |
| `ff_cancel_flow` (§5.4)                   | Cairn calls `cancel_flow` itself via `run_service.cancel_execution` for each member — per-execution cancels emit `ExecutionCancelled`. The flow-level transition is redundant. |
| `ff_reset_budget` (§5.4)                  | Admin-managed, no cairn consumer.                                                                                        |
| `ff_cancel_execution` suspended-branch sub-paths (§5.2) | Cairn emits `ExecutionCancelled` synchronously (run_service.rs:628 + worker_sdk.rs:344). The FF-side lease_history branch is covered by the FCALL — no cairn subscription needed. |

## §3 Poll-dependent consumers

**Finding: cairn does not poll FF state on any schedule.**

Every `hgetall(&ctx.core())` in cairn-fabric/src is a
read-back-after-write or a read-once-per-request (inside a service
method). There is no background task / interval / tick that reads
exec_core periodically.

Consequence: if FF transitions state server-side (scanner, lease
expiry, timeout) without cairn initiating it, cairn will NEVER
observe the transition until the next cairn-initiated call on that
execution. For most entity-creation events this is fine because
cairn initiated the creation. But for §1.1 (lease expiry) and
§1.3 (timeout expiry), this is the source of the drift.

**No pathological creation-event polling exists** — cairn emits
creation events itself from its own wrappers. This is the
healthiest pattern for creation; the gaps are on
FF-initiated-terminal transitions.

## §4 Non-FCALL atomicity defect (§5.5 only)

**Per-manager dispatch:** separate defect class from bridge events.

**Site:** `crates/ff-server/src/server.rs:1279` — `flow_id` HSET on
exec_core inside `add_execution_to_flow`, outside the FCALL
boundary.

**Risk:** phase-1 (ff_add_execution_to_flow FCALL) succeeds +
phase-2 (HSET flow_id) fails → flow_core thinks exec is a member
but exec_core has no flow_id back-pointer. The W2 inventory notes
the comment at L1274 calling it "idempotent, safe to retry if phase
1 succeeded but phase 2 failed" — but a retry requires the caller
to notice the failure and retry. If the process crashes between
phases, no retry fires and the inconsistency sticks.

**AMENDMENT (2026-04-18, W2):** the original fix recommendation
below — "move the HSET into the FCALL" — is **not implementable**.
`ff_add_execution_to_flow` operates on `{fp:N}` flow-partition
keys (`flow_core`, `members_set`, `flow_index`), routed via
`flow_partition(flow_id)` at `crates/ff-server/src/server.rs:1249`.
`exec_core` lives on `{p:N}` execution-partition keys, routed via
`execution_partition(execution_id)` at
`crates/ff-server/src/server.rs:1275-1276`. Valkey functions are
single-slot: on a clustered deploy, touching `exec_core` from
within `ff_add_execution_to_flow` would fail with `CROSSSLOT`. The
existing two-phase shape was chosen **deliberately** — see
`lua/flow.lua:117` ("Does NOT set flow_id on exec_core (that's on
`{p:N}`, caller must do it separately)"). Options A and B below are
both cross-slot-violating and should be disregarded.

**Revised fix plan.** Document the two-phase contract explicitly,
name the orphan direction, catalogue reader-side safety, and file a
reconciliation-scanner ticket to close the crash-recovery window.
Implemented in this commit (docs + scanner-ticket issue #21). No Lua or
Rust control-flow changes. See `lua/flow.lua:114-175` and
`crates/ff-server/src/server.rs:1240-1320` for the amended
contract; see `crates/ff-server/src/server.rs:1895-1910` +
`crates/ff-server/src/server.rs:2080-2115` for the reader-side
invariant notes.

~~**Fix site — FF side.** Move the `flow_id` HSET into the FCALL.~~
~~Two options:~~

~~- **Option A:** extend `ff_add_execution_to_flow` to accept an~~
~~  optional `back_pointer_field` arg and HSET inside the FCALL. The~~
~~  Lua already has KEYS[1]=exec_core; pass the exec_core key +~~
~~  field name, and do the HSET under the same Lua atomicity.~~
~~- **Option B:** add a new FCALL `ff_link_execution_to_flow` that~~
~~  does both writes atomically. Called in place of the~~
~~  two-phase ff-server code.~~

~~**Recommendation: Option A.** Lower churn; extends an existing~~
~~FCALL rather than introducing a new one. The ff-server caller~~
~~becomes a single FCALL invocation with no follow-up HSET. Any~~
~~cairn-fabric / ff-sdk callers of the FF API are unaffected because~~
~~`add_execution_to_flow` is a ff-server REST method, not an FCALL~~
~~cairn calls directly.~~

Defect class unchanged: still a crash-recovery atomicity gap. But
the fix shape is "documented two-phase contract + reconciliation
scanner ticket," not a single-FCALL transformation.

**UPDATE (2026-04-18, W2):** RFC-011 (`rfcs/RFC-011-exec-flow-colocation.md`,
**Accepted**) lands the actual atomic-by-construction fix via
`execution_id = "{fp:N}:<uuid>"` hash-tag co-location. When RFC-011's
phase 3 merges, `ff_add_execution_to_flow` becomes a single atomic
FCALL that takes both `flow_core` and `exec_core` as KEYS (same slot
under co-location), the two-phase contract documented here ceases to
apply, and issue #21 is closed as superseded. This commit's
documentation (commit 8df40fc) is the interim safety net that carries
the repo from today through RFC-011 phase 3. Readers auditing the
atomicity story should treat RFC-011 as the load-bearing artifact and
this §4 amendment as its transitional prelude.

**Not bridge-event related. No cairn-side work.**

## §5 Recommendations

Per-gap summary, owner assignment, and rationale.

| Gap                                                        | Confirmed? | Fix owner | Recommended emit/consumer site                                                                                                   |
| ---------------------------------------------------------- | :--------: | :-------: | -------------------------------------------------------------------------------------------------------------------------------- |
| §1.1 FF-initiated lease expiry (`ff_mark_lease_expired_if_due`) | YES   | **cairn** | Cairn-fabric subscribes to `lease_history_key` per-execution at claim time, maps `expired` → `BridgeEvent::ExecutionFailed { failure_class: LeaseExpired }`. FF already emits the XADD. |
| §1.2 FF-initiated retry schedule (`ff_promote_delayed`)    | NO          | —         | Intentional silence. Cairn tracks retry via the initial `ExecutionRetryScheduled` event.                                         |
| §1.3 FF-initiated timeout expiry runnable-branch (`ff_expire_execution` L1464) | YES   | **FF + cairn** | FF: add symmetric `lease_history` `expired` XADD in the runnable branch (matches active/suspended branches already doing it). Cairn: same subscription as §1.1, additional mapping for `expired` frames without a prior `lease_acquired`. |
| §1.4 Scanner promotions (delayed/blocked → eligible)       | NO          | —         | Intentional silence. Scheduling-tick transitions; cairn sees the eventual claim.                                                 |
| §1.5 Admission block (`ff_block_execution_for_admission`)  | NO          | —         | Intentional silence (at time of writing). Cairn has no `ExecutionRateLimited` variant.                                           |
| §5.5 Non-FCALL `flow_id` HSET (server.rs:1279)             | YES (atomicity, not emit) | **FF** | Cross-slot constraint rules out single-FCALL fix. Document two-phase contract in `lua/flow.lua` + `server.rs:1240-1320` + reader-side invariants at `server.rs:1895-1910` and `server.rs:2080-2115`; file reconciliation-scanner ticket. **See §4 amendment.** |

### Summary

- **2 confirmed bridge-event gaps** (§1.1 lease-expiry, §1.3
  timeout-expiry runnable-branch). Both centred on FF-initiated
  terminal transitions that cairn's call-then-emit pattern cannot
  reach.
- **1 confirmed atomicity defect** (§5.5) — orthogonal to
  bridge-events; FF-side fix.
- **15 of W2's 19 candidates are intentional silences** from
  cairn's perspective. Cairn emits its own events for every
  transition it initiates; the "no XADD" observation at the FF
  layer is noise for cairn's consumption model.

### Priority ordering for follow-up PRs

1. **§1.3 FF runnable-branch `ff_expire_execution`** — FF fix
   first, small + symmetric with existing emits. Closes a real
   read-model drift where unclaimed-expired executions hang in
   "Waiting" forever in cairn's projection.
2. **§5.5 non-FCALL `flow_id` HSET** — FF fix, atomicity-class
   defect. Not a bridge-event issue but manager explicitly scoped
   it. Single-FCALL fix ruled out (cross-slot); landed as
   documented two-phase contract + reconciliation-scanner ticket.
   See §4 amendment for the revised fix plan.
3. **§1.1 lease-expiry subscription** — cairn-side work. Larger
   change (new per-execution tail subscription pattern) but
   captures the biggest class of drift. Should come after FF-side
   fixes so the subscriber has consistent producer behaviour.

## §6 Deliverable boundary

This report ends at confirmed-gaps + fix-site-recommendations. No
code changes. Numbers to track in follow-up PRs:

- `lua/execution.lua:1464-1615` — `ff_expire_execution` runnable
  branch missing XADD.
- `crates/ff-server/src/server.rs:1279` — two-phase flow_id HSET.
  Fix shape revised (see §4 amendment): docs + reconciliation-
  scanner ticket, not a single-FCALL transform.
- `crates/cairn-fabric/src/event_bridge.rs:17-87` — canonical
  BridgeEvent enum (add `LeaseExpired` failure class if §1.1 fix
  lands).
- `lua/execution.lua:1437` — `ff_reclaim_execution` already emits
  `lease_history` `reclaimed`; cairn's §1.1 subscriber should also
  consume this frame shape so reclaim drives the same
  `ExecutionFailed { failure_class: LeaseExpired }` emission.
