# Bridge-event completeness audit (FF-side, 2026-04-22)

## Scope

Systematic review of all `exec_core`/`flow_core` state-mutating FCALL entry
points in `crates/ff-script/src/flowfabric.lua` against bridge-event
emissions on the `ff:dag:completions` channel, which is the sole wire feed
for `CompletionBackend::subscribe_completions` (post-#90). Audit covers
terminal transitions (success / failed / cancelled / expired / skipped),
non-terminal transitions that are consumer-observable (suspend / resume /
delay / block-unblock / priority / tags / progress), and the `flow_core`
lifecycle (`public_flow_state`). Not audited: lease-epoch/renewal detail,
attempt stream content, budget/quota bookkeeping — these are status fields
rather than DAG bridge events.

Repo HEAD: `47ef2b0` (origin/main, fully synced before audit). Lua file
7,085 lines, 51 `redis.register_function` entries. `CompletionPayload`
shape: `{ execution_id, flow_id, outcome, payload_bytes (None today),
produced_at_ms (0 today) }` — consumer reads `execution_id + flow_id +
outcome`.

## Method

`grep -n 'register_function'` → 51 FCALLs. `grep -n 'PUBLISH'` → 4 emit
sites (lines 1920, 2128, 2558, 3014). `grep -n 'public_state\s*=\|
lifecycle_phase\s*=\|terminal_outcome\s*=\|HSET.*core'` → state-mutation
map. For each `HSET K.core_key` that sets `lifecycle_phase="terminal"` or
`public_state=…`, confirmed whether a `PUBLISH` follows in the same
atomic FCALL. Every emit site is gated by `if is_set(core.flow_id)` —
standalone executions are intentionally silent (cairn-fabric requires a
`flow_id` field and the subscriber `parse_push` drops payloads whose
`flow_id` fails `FlowId::parse`, so a standalone emit would be discarded
anyway). Cross-checked consumer parse at
`crates/ff-backend-valkey/src/completion.rs:315` and struct at
`crates/ff-core/src/backend.rs:468`.

## FCALL → state-mutation → emission table

Scope-limited to FCALLs that write `lifecycle_phase` or `terminal_outcome`
in a consumer-observable way. Operational-only mutations (lease renewal,
HMAC rotation, tag writes, progress bumps) are listed under "Affirmative
no-findings".

| FCALL | Lua line | state mutation | emit? | payload | gap assessment |
|-------|----------|----------------|-------|---------|----------------|
| ff_complete_execution | 1866 | phase=terminal, outcome=success, public_state=completed | YES @ 1920 | eid, flow_id, outcome=success | OK |
| ff_cancel_execution | 2087 | phase=terminal, outcome=cancelled, public_state=cancelled | YES @ 2128 (skip if source=="flow_cascade") | eid, flow_id, outcome=cancelled | OK |
| ff_fail_execution (retry path) | 2463 | phase=runnable, public_state=delayed | NO | — | accept — non-terminal, not a DAG bridge event |
| ff_fail_execution (terminal path) | 2512 | phase=terminal, outcome=failed, public_state=failed | YES @ 2558 | eid, flow_id, outcome=failed | OK |
| ff_expire_execution | 2975 | phase=terminal, outcome=expired, public_state=expired | YES @ 3014 | eid, flow_id, outcome=expired | OK |
| ff_expire_suspension (fail/cancel/expire) | 4225 | phase=terminal, outcome∈{failed,cancelled,expired} | **NO** | — | **GAP-1** |
| ff_resolve_dependency (impossible → child skip) | 6722 | phase=terminal, outcome=skipped, public_state=skipped | **NO** | — | **GAP-2** (see note) |
| ff_suspend_execution | 3786 | phase=suspended, public_state=suspended | NO | — | accept — non-terminal |
| ff_resume_execution | 3923 | phase=runnable, public_state=waiting\|delayed | NO | — | accept — non-terminal |
| ff_expire_suspension (auto_resume) | 4143 | phase=runnable, public_state=waiting | NO | — | accept — non-terminal |
| ff_expire_suspension (escalate) | 4181 | preserve suspended, blocking_reason=paused_by_operator | NO | — | accept — observability only |
| ff_deliver_signal (resume path) | 4770 | phase=runnable | NO | — | accept — non-terminal |
| ff_claim_resumed_execution | 5107 | phase=active, ownership=leased | NO | — | accept — non-terminal |
| ff_delay_execution | 2195 | phase=runnable, public_state=delayed | NO | — | accept — non-terminal |
| ff_move_to_waiting_children | 2302 | phase=runnable, public_state=waiting_children | NO | — | accept — non-terminal |
| ff_reclaim_execution | 2643 | phase=active, new lease/attempt | NO | — | accept — non-terminal |
| ff_replay_execution | 6945 / 6979 | terminal→runnable (reverse terminal) | NO | — | see GAP-3 note |
| ff_promote_delayed | 3433 | phase=runnable, eligibility=eligible_now | NO | — | accept — non-terminal |
| ff_stage_dependency_edge | 6528 | public_state=waiting_children | NO | — | accept — non-terminal |
| ff_resolve_dependency (satisfied path) | 6652 | public_state=waiting, eligibility=eligible_now | NO | — | accept — non-terminal |
| ff_promote_blocked_to_eligible | 6837 | eligibility=eligible_now | NO | — | accept — non-terminal |
| ff_unblock_execution | 5675 | eligibility update | NO | — | accept — non-terminal |
| ff_block_execution_for_admission | 5748 | eligibility=blocked_by_* | NO | — | accept — non-terminal |
| ff_apply_dependency_to_child | — | edge write, no core | NO | — | accept |
| ff_cancel_flow | 6270 | flow_core public_flow_state=cancelled | NO | — | see GAP-4 note |
| ff_claim_execution | 1713 | phase=active, ownership=leased | NO | — | accept — non-terminal |
| ff_create_execution | 1411 | creates new core | NO | — | accept — creation is not a bridge event (historical SessionCreated gap, cairn PR #26, still correctly not emitted) |
| ff_revoke_lease | 1191 | ownership=lease_revoked | NO | — | accept — operational |
| ff_mark_lease_expired_if_due | 1014 | lease-only fields | NO | — | accept — operational |

## Findings

### GAP-1: ff_expire_suspension terminal paths do not PUBLISH

**Evidence:** `crates/ff-script/src/flowfabric.lua:4225-4280`
(`ff_expire_suspension`, `fail`/`cancel`/`expire` branches).

After writing `lifecycle_phase="terminal"`, `terminal_outcome ∈
{failed, cancelled, expired}`, `public_state=public_state_val`,
`ZADD terminal_key`, the function returns `ok(behavior, public_state_val)`
without any `redis.call("PUBLISH", "ff:dag:completions", …)`. The other
three terminal-writing FCALLs (`ff_complete_execution`,
`ff_fail_execution` terminal branch, `ff_expire_execution`) and
`ff_cancel_execution` all emit.

**Consumer impact:** A flow-bound execution whose suspension times out
with `timeout_behavior ∈ {fail, cancel, expire}` reaches a terminal
state that downstream DAG children are blocked on, but no push arrives
on `ff:dag:completions`. Children unblock only via the
`dependency_reconciler` safety-net scan (default 15s), producing a
latency spike identical in class to the one issue #44 closed for
`ff_expire_execution`. cairn-fabric's completion subscriber never sees
the terminal outcome, so any in-flight cairn await-completion / step
observer on a suspended-then-timed-out member will block until the
reconciler fallback.

The adjacent `auto_resume` branch (4143) and `escalate` branch (4181)
do not write terminal state and correctly do not emit.

**Recommendation:** FIX. Add an `if is_set(core.flow_id) then
PUBLISH ff:dag:completions { execution_id, flow_id, outcome =
terminal_outcome }` block after the `ZADD terminal_key` at line 4278,
mirroring the issue #44 pattern at 3008-3015. Scope: ~8 lines of Lua +
1 regression test in `ff-test/tests/` (parallel to
`expire_publish_dag.rs`). This is the same class of gap #44 fixed —
suspension timeout is the third scanner-driven terminal path.

### GAP-2: ff_resolve_dependency child-skip does not PUBLISH

**Evidence:** `crates/ff-script/src/flowfabric.lua:6722-6737`
(`ff_resolve_dependency`, impossible path → child skipped).

When a child's last required dependency resolves `impossible` and the
child is not yet terminal, the function sets `terminal_outcome=skipped`,
`public_state=skipped`, `ZADD terminal_zset` — all signs of a terminal
transition — but does not publish. Note that the FCALL is already
executing *because of* a PUBLISH (the engine's CompletionListener called
`dispatch_dependency_resolution`) — this is a cascaded terminal.

**Consumer impact:** Grandchildren (child-of-child) of the originally-
failed upstream do not receive a push event when their parent is
cascade-skipped. The `dependency_reconciler` still catches it. Under a
wide fan-out where layer N fails and layers N+1, N+2, … are all skipped,
the reconciler must walk each layer — the push-based fast path cuts off
at layer N+1. This is latency, not correctness, and is bounded by the
reconciler interval. Cairn-side impact: step observers on grandchildren
wait up to one reconciler cycle.

**Recommendation:** FIX, but lower priority than GAP-1. Add an emit
after `ZADD terminal_zset` at line 6735 gated on `is_set(core.flow_id)`
with `outcome="skipped"`. Consumer parser already tolerates arbitrary
outcome strings (it's a `String`, not an enum). Scope: ~8 lines Lua.
Mild caution: skipped cascades fan out exponentially on diamond graphs,
so the PUBLISH volume could multiply; acceptable because each push is
tiny and `ff:dag:completions` already carries cancel-cascade traffic
(modulo the `flow_cascade` suppression at 2122).

### GAP-3 (note, not a gap — accept): ff_replay_execution terminal→runnable reverse does not emit

**Evidence:** lines 6945 and 6979. Replay pulls a terminal execution
back to `lifecycle_phase=runnable`. No PUBLISH, no other bus emit.

**Assessment:** Accept. Replay is an operator action with its own RPC
audit trail (lease_history_key carries a `replay_initiated` event).
Consumers that need to know an execution resurrected from terminal rely
on either polling the flow summary or resubscribing; the completion
channel semantically is "forward transitions into terminal", not "any
transition". Flag: no consumer has asked for this yet (cairn
smoke-tests did not catch it).

### GAP-4 (note, not a gap — accept): ff_cancel_flow does not emit per-member PUBLISH

**Evidence:** line 6270. `ff_cancel_flow` sets flow_core
`public_flow_state=cancelled`, drains members into `pending_cancels`,
returns the member list. No PUBLISH; per-member emits happen later when
`cancel_member_execution` → `ff_cancel_execution` fires (and that path
suppresses PUBLISH for `source="flow_cascade"` at line 2122 by design).

**Assessment:** Accept. The `flow_cascade` suppression is explicit —
the engine already knows the fan-out and doesn't need the push. Cairn
observes the flow-level cancel via the scheduled member-cancels, not
the flow_core hash directly. If that changes (cairn wants flow-level
cancel events), it's a new requirement, not a gap.

## Affirmative no-findings

Paths where absence of PUBLISH is correct as-is:

- `ff_create_execution` — creation is not a bridge event (validates
  historical SessionCreated decision from cairn PR #26).
- `ff_claim_execution` / `ff_reclaim_execution` / `ff_claim_resumed_execution`
  — ownership changes, not terminal.
- `ff_renew_lease` / `ff_revoke_lease` / `ff_mark_lease_expired_if_due`
  — lease bookkeeping.
- `ff_suspend_execution` / `ff_resume_execution` /
  `ff_deliver_signal` / `ff_expire_suspension` (auto_resume + escalate)
  — non-terminal phase transitions; consumers that need these observe
  via stream_tail or direct HGET.
- `ff_delay_execution` / `ff_promote_delayed` /
  `ff_move_to_waiting_children` — eligibility bookkeeping.
- `ff_resolve_dependency` (satisfied) / `ff_stage_dependency_edge` /
  `ff_apply_dependency_to_child` / `ff_promote_blocked_to_eligible` —
  DAG mechanics, not terminal.
- `ff_block_execution_for_admission` / `ff_unblock_execution` —
  admission control.
- `ff_set_execution_tags` / `ff_set_flow_tags` / `ff_update_progress` /
  `ff_change_priority` / `ff_append_frame` — metadata / stream writes.
- `ff_create_budget` / `ff_report_usage_and_check` / `ff_reset_budget` /
  `ff_create_quota_policy` / `ff_check_admission_and_record` /
  `ff_release_admission` — budget/quota.
- `ff_issue_claim_grant` / `ff_issue_reclaim_grant` /
  `ff_rotate_waitpoint_hmac_secret` / `ff_list_waitpoint_hmac_kids` /
  `ff_create_pending_waitpoint` / `ff_close_waitpoint` /
  `ff_buffer_signal_for_pending_waitpoint` — grant/waitpoint plumbing.
- `ff_create_flow` / `ff_add_execution_to_flow` / `ff_ack_cancel_member`
  — flow membership / ack.
- `ff_version` / `ff_read_attempt_stream` — read-only.
- `ff_fail_execution` retry branch (non-terminal) — correctly silent.
- `ff_cancel_execution` with `source="flow_cascade"` — intentional
  suppression (comment at 2116-2121 documents the rationale).

Payload-shape check: all 4 existing emits use identical field set
`{execution_id, flow_id, outcome}`. Consumer `parse_push` tolerates this
fully; `payload_bytes` and `produced_at_ms` are documented as
not-yet-populated in `backend.rs:462-465`. No payload-field gaps.

## Recommendations

Ordered by consumer impact:

1. **GAP-1 (fix):** `ff_expire_suspension` terminal paths — add
   `is_set(core.flow_id)`-gated PUBLISH for fail/cancel/expire branches.
   Mirrors issue #44. Scope: ~8 Lua lines + 1 integration test. Closes
   the last scanner-driven terminal path that bypasses the push channel.
2. **GAP-2 (fix, lower priority):** `ff_resolve_dependency` child-skip —
   add PUBLISH with `outcome="skipped"`. Cuts reconciler latency on
   cascaded skips on deep DAGs. Scope: ~8 Lua lines + 1 test. Worth
   coordinating with cairn to confirm they want `skipped` as a distinct
   outcome (or a synthetic `failed` to match existing consumer code).
3. No other fix-worthy gaps.

Total: 2 emit sites to add. Both are the same pattern as the 4 existing
emits (identical gate, identical payload shape), so no design work
required.
