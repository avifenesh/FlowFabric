# P3.6 — Bridge-event completeness audit

**Author:** Worker-2
**Branch:** `bench/p36-bridge-audit` (off `main @ cbe35f1`)
**Scope:** Inventory every FCALL and every `ff-server` pub method
that writes terminal/lifecycle/public_state fields, cross-referenced
against every emit site (XADD). Flag candidate gaps for W3's
comparison pass against cairn's consumer list.

Read-only. No code changes.

## §1 Emit-site inventory — "what the engine fires"

**Load-bearing finding: there is no dedicated 'bridge event' stream
in-repo.** A grep across `crates/` and `lua/` for `PUBLISH`,
`ff_event`, `ff:events`, `bridge`, `observer`, `consumer_group` finds
zero hits. All observable emits are `XADD`s to one of three
purpose-specific Valkey streams, each with its own semantics:

| Stream key (Lua binding)           | Scope              | Event shape                           | Intended consumer                                    |
| ---------------------------------- | ------------------ | ------------------------------------- | ---------------------------------------------------- |
| `K.lease_history_key`              | per-execution      | `kind=<transition>` + snapshot fields | audit / replay / cairn lifecycle observer            |
| `K.wp_signals_stream`              | per-waitpoint      | `signal_id, name, payload, token`     | cairn waitpoint-signal observer + suspension resumer |
| `K.attempt_stream` (`ff_append_frame`) | per-attempt    | caller-supplied frame bytes           | cairn attempt-output tail / task progress            |

Cairn presumably ingests bridge-events by:
  (a) tailing `lease_history_key` per exec for lifecycle transitions;
  (b) tailing `wp_signals_stream` per waitpoint for signal arrivals;
  (c) tailing `attempt_stream` per attempt for task output;
  (d) polling `exec_core` / `flow_core` via `get_execution_state` for
      fields that never stream.

**Consequence for gap analysis:** an FCALL that mutates state
WITHOUT appending to any of those three streams requires cairn to
either poll `exec_core` (via the server's public getters) or miss
the change entirely. PR#26's `SessionCreated` gap is exactly that
pattern — an entity-creation write with no XADD.

### §1.1 `lease_history_key` XADD sites (audit/lifecycle)

Extracted from `grep -n 'XADD.*lease_history' lua/*.lua`:

| File                     | Line   | FCALL (enclosing)                 | Frame kind (ARGV) |
| ------------------------ | -----: | --------------------------------- | ----------------- |
| `lua/execution.lua`      |    533 | `ff_claim_execution`              | `lease_acquired`  |
| `lua/execution.lua`      |    643 | `ff_complete_execution`           | `attempt_success` |
| `lua/execution.lua`      |    763 | `ff_cancel_execution`             | `attempt_cancelled` |
| `lua/execution.lua`      |    924 | `ff_delay_execution`              | `delayed` |
| `lua/execution.lua`      |   1013 | `ff_move_to_waiting_children`     | `waiting_children` |
| `lua/execution.lua`      |   1165 | `ff_fail_execution` (retry path)  | `retry_scheduled` |
| `lua/execution.lua`      |   1205 | `ff_fail_execution` (terminal)    | `attempt_failed` |
| `lua/execution.lua`      |   1437 | `ff_reclaim_execution`            | `reclaimed` |
| `lua/execution.lua`      |   1534 | `ff_expire_execution`             | `expired` |
| `lua/suspension.lua`     |    238 | `ff_suspend_execution`            | `suspended` |
| `lua/signal.lua`         |    627 | `ff_claim_resumed_execution`      | `resumed` |
| `lua/lease.lua`          |    290 | `ff_revoke_lease`                 | `revoked` |
| `lua/flow.lua`           |    828 | `ff_replay_execution` (replaying) | `replay_*` |
| `lua/flow.lua`           |    861 | `ff_replay_execution` (rebuilding)| `replay_*` |
| `lua/helpers.lua`        |    443 | `append_lease_event` helper       | (caller-specified kind) |
| `lua/helpers.lua`        |    553 | `release_lease` helper            | `released` |

### §1.2 `wp_signals_stream` XADD sites (waitpoint signals)

| File                | Line | FCALL                                      | Purpose |
| ------------------- | ---: | ------------------------------------------ | ------- |
| `lua/signal.lua`    |  181 | `ff_deliver_signal`                        | active waitpoint receives a signal |
| `lua/signal.lua`    |  456 | `ff_buffer_signal_for_pending_waitpoint`   | signal buffered pre-suspend |

### §1.3 Attempt-output stream (`ff_append_frame` → attempt_stream)

| File             | Line | FCALL              | Purpose |
| ---------------- | ---: | ------------------ | ------- |
| `lua/stream.lua` |  122 | `ff_append_frame`  | worker writes a frame to the current attempt's output stream |

### §1.4 What is NOT emitted (documented absences)

No stream covers (confirmed by greps over all Lua):

- Execution creation (`ff_create_execution` writes exec_core but
  does not XADD anywhere).
- Waitpoint creation (`ff_create_pending_waitpoint` writes
  `waitpoint_key` but does not XADD — no `wp_signals_stream` entry
  on creation).
- Flow creation (`ff_create_flow` writes flow_core; no XADD).
- Budget/quota policy creation (`ff_create_budget`,
  `ff_create_quota_policy` — policy documents only, no XADD).
- Attempt start (the `attempt_state=started` write inside
  `ff_claim_execution`). Lease-history XADD is called `lease_acquired`
  — it carries the attempt transition implicitly but the event is
  framed around the lease, not the attempt.
- All scheduling-side mutations that do NOT change lifecycle (e.g.
  `ff_issue_claim_grant` writes the grant hash + stamps
  `last_capability_mismatch_at` on reject; no stream).
- Dependency resolution / flow-graph mutations
  (`ff_stage_dependency_edge`, `ff_apply_dependency_to_child`,
  `ff_resolve_dependency`, `ff_evaluate_flow_eligibility`,
  `ff_promote_blocked_to_eligible`).

This is the pool §5 draws candidate gaps from.

## §2 Lua FCALLs writing terminal/lifecycle/public_state

One row per `register_function`. "State writes" is the fields from
the set `{public_state, lifecycle_phase, ownership_state,
eligibility_state, attempt_state, blocking_reason, closed_at,
closed_reason, terminal_reason, public_flow_state}` actually
HSET'd inside that function body. Not every dimension is relevant
to every FCALL — the 7-dim state vector is in
`ff_core::state::StateVector` (see RFC-001 §2).

### `lua/execution.lua`

| FCALL                             | State writes (HSET K.core_key)                                                                                         | Lifecycle emit                  | Conditions |
| --------------------------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------- | ---------- |
| `ff_create_execution` (L22)       | lifecycle_phase, ownership_state, eligibility_state, attempt_state, public_state (new row)                             | **none** (no XADD)              | always on Ok |
| `ff_claim_execution` (L326)       | lifecycle_phase, ownership_state, eligibility_state, attempt_state, public_state                                       | `lease_history` `lease_acquired` | on Ok path |
| `ff_complete_execution` (L557)    | lifecycle_phase=terminal, attempt_state=attempt_terminal, public_state=completed, closed_at, closed_reason=attempt_success | `lease_history` `attempt_success` | on Ok path |
| `ff_cancel_execution` (L673)      | lifecycle_phase=terminal, attempt_state=ended_cancelled / attempt_terminal, public_state=cancelled, closed_at, closed_reason | `lease_history` (active branch) | branches on source lifecycle_phase |
| `ff_delay_execution` (L853)       | lifecycle_phase=runnable, eligibility_state=not_eligible_until_time, attempt_state=attempt_interrupted, public_state=delayed | `lease_history` delay entry | on Ok path |
| `ff_move_to_waiting_children` (L947) | lifecycle_phase=runnable, eligibility_state=blocked_by_dependencies, attempt_state=attempt_interrupted, public_state=waiting_children | `lease_history` waiting entry | on Ok path |
| `ff_fail_execution` (L1043)       | (retry) lifecycle_phase=runnable, attempt_state=pending_retry_attempt, public_state=delayed // (terminal) lifecycle_phase=terminal, attempt_state=attempt_terminal, public_state=failed, closed_at, closed_reason=attempt_failure | `lease_history` either branch | branches on retry policy |
| `ff_reclaim_execution` (L1231)    | (cap-exceeded) lifecycle_phase=terminal, public_state=failed, closed_reason=reclaim_limit // (success) lifecycle_phase=active, ownership_state=leased, attempt_state=started, reclaim_reason | `lease_history` `reclaimed`/`attempt_failed` | branches on reclaim_count vs max |
| `ff_expire_execution` (L1464)     | lifecycle_phase=terminal, attempt_state=ended_failure/attempt_terminal, public_state=expired, closed_at, closed_reason=expired | `lease_history` (active/suspended branches) | branches on source lifecycle_phase |

### `lua/suspension.lua`

| FCALL                               | State writes                                                                                         | Lifecycle emit          | Conditions |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------- | ----------------------- | ---------- |
| `ff_suspend_execution` (L31)        | lifecycle_phase=suspended, ownership_state=unowned, eligibility_state=not_applicable, attempt_state=attempt_interrupted/suspended, public_state=suspended | `lease_history` `suspended` | on Ok path |
| `ff_resume_execution` (L292)        | lifecycle_phase=runnable, eligibility_state=eligible_now/not_eligible_until_time, attempt_state=attempt_interrupted, public_state=waiting/delayed | **none** (no XADD)      | on Ok path |
| `ff_create_pending_waitpoint` (L403)| waitpoint_key.closed_at="" (new waitpoint doc)                                                       | **none** (no XADD)      | on Ok path — **entity creation, no emit** |
| `ff_expire_suspension` (L498)       | branches: auto_resume → runnable/waiting // terminal → lifecycle_phase=terminal, attempt_state=attempt_terminal, public_state=cancelled/expired/failed, closed_at/closed_reason | **lease_history (terminal branch only)** | behavior arg controls branch |
| `ff_close_waitpoint` (L710)         | waitpoint_key.closed_at, close_reason                                                                | **none** (no XADD)      | on Ok path |

### `lua/signal.lua`

| FCALL                                         | State writes                                                                                                                                       | Lifecycle emit                           | Conditions |
| --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------- | ---------- |
| `ff_deliver_signal` (L27)                     | waitpoint closed_at/closed_reason=satisfied; execution (on resume) lifecycle_phase=runnable, attempt_state=attempt_interrupted, public_state=waiting | `wp_signals_stream` (always); no `lease_history` emit on the runnable-transition branch | multi-branch on waitpoint match |
| `ff_buffer_signal_for_pending_waitpoint` (L342) | waitpoint signal buffer; no core state change                                                                                                      | `wp_signals_stream`                      | on Ok path |
| `ff_claim_resumed_execution` (L486)           | lifecycle_phase=active, ownership_state=leased, attempt_state=started/running_attempt, public_state=active                                         | `lease_history` `resumed`                | on Ok path |

### `lua/scheduling.lua`

| FCALL                              | State writes                                                             | Lifecycle emit | Conditions |
| ---------------------------------- | ------------------------------------------------------------------------ | -------------- | ---------- |
| `ff_issue_claim_grant` (L79)       | claim_grant hash (ephemeral, TTL-auto); on mismatch: `last_capability_mismatch_at` scalar on core | **none**       | advisory only |
| `ff_change_priority` (L185)        | ZADD on eligible ZSET only (score change)                                | **none**       | scheduling-op |
| `ff_update_progress` (L244)        | last_progress_ts + progress_tag + progress_percent on core               | **none**       | heartbeat-ish |
| `ff_promote_delayed` (L308)        | lifecycle_phase=runnable (preserve), eligibility_state=eligible_now, blocking_reason=waiting_for_worker, public_state=waiting | **none**       | timer-scanner callback |
| `ff_issue_reclaim_grant` (L419)    | claim_grant hash (ephemeral); on mismatch: `last_capability_mismatch_at` | **none**       | advisory only |

### `lua/lease.lua`

| FCALL                               | State writes                                                                                              | Lifecycle emit                   | Conditions |
| ----------------------------------- | --------------------------------------------------------------------------------------------------------- | -------------------------------- | ---------- |
| `ff_renew_lease` (L16)              | lease_expires_at, renewed_count                                                                           | **none**                         | liveness-op |
| `ff_mark_lease_expired_if_due` (L125) | via `append_lease_event` helper: ownership_state=lease_expired_reclaimable, blocking_reason, lease_expired_at | `lease_history` `expired` (via helper at 443) | on Ok path |
| `ff_revoke_lease` (L206)            | lifecycle_phase=active (preserve), ownership_state=lease_revoked, blocking_reason=waiting_for_worker, attempt_state=attempt_interrupted, public_state=active | `lease_history` (at 290) `revoked` | on Ok path |

### `lua/flow.lua`

| FCALL                                  | State writes                                                                                         | Lifecycle emit                          | Conditions |
| -------------------------------------- | ---------------------------------------------------------------------------------------------------- | --------------------------------------- | ---------- |
| `ff_create_flow` (L69)                 | flow_core.public_flow_state=open (new row)                                                           | **none** (no XADD)                      | on Ok path — **entity creation, no emit** |
| `ff_add_execution_to_flow` (L122)      | flow_core member count; exec_core.flow_id is NOT written here — see §3 ff-server non-FCALL write     | **none** (no XADD)                      | on Ok path |
| `ff_cancel_flow` (L190)                | flow_core.public_flow_state=cancelled                                                                | **none** (no XADD)                      | on Ok path |
| `ff_stage_dependency_edge` (L271)      | edges hash; edge attrs (status=waiting), no core_key HSET                                            | **none**                                | on Ok path |
| `ff_apply_dependency_to_child` (L369)  | lifecycle_phase=runnable (preserve), eligibility_state=blocked_by_dependencies, attempt_state=pending_first_attempt (preserve), public_state=waiting_children | **none**                                | on Ok path |
| `ff_resolve_dependency` (L460)         | lifecycle_phase=runnable (preserve), attempt_state=various (preserve or set to ended_cancelled on skip), public_state=waiting; closed_at/reason on skip-cancel path | **none**                                | branches on satisfied/skip/cancel |
| `ff_evaluate_flow_eligibility` (L612)  | no core HSET (read-only flow evaluation)                                                             | **none**                                | read path |
| `ff_promote_blocked_to_eligible` (L655)| lifecycle_phase=runnable (preserve), eligibility_state=eligible_now, attempt_state (preserve), public_state=waiting | **none**                                | scanner callback |
| `ff_replay_execution` (L731)           | lifecycle_phase=runnable, attempt_state=pending_replay_attempt, public_state=waiting/waiting_children | `lease_history` (at 828 + 861) `replay_*` | on Ok path |

### `lua/budget.lua` + `lua/quota.lua` + `lua/stream.lua`

| FCALL                              | State writes                                                                                                                                | Lifecycle emit | Conditions |
| ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- | -------------- | ---------- |
| `ff_create_budget` (L18)           | budget policy hash + definition (no exec_core touch)                                                                                        | **none**       | **entity creation** |
| `ff_report_usage_and_check` (L99)  | budget usage counters (no exec_core touch)                                                                                                  | **none**       | usage-op |
| `ff_reset_budget` (L188)           | budget usage (no exec_core touch)                                                                                                           | **none**       | admin-op |
| `ff_unblock_execution` (L243)      | lifecycle_phase=runnable, eligibility_state=eligible_now, blocking_reason=waiting_for_worker, attempt_state (preserve), public_state=waiting | **none**       | scanner callback |
| `ff_block_execution_for_admission` (L313) | lifecycle_phase=runnable, eligibility_state=(mapped from blocking_reason), blocking_reason, public_state=rate_limited                       | **none**       | admission-reject |
| `ff_create_quota_policy` (L17)     | quota policy hash (no exec_core touch)                                                                                                      | **none**       | **entity creation** |
| `ff_check_admission_and_record` (L70)| admission record + usage sliding window (no exec_core touch)                                                                                | **none**       | admission-op |
| `ff_release_admission` (L163)      | usage window rollback (no exec_core touch)                                                                                                  | **none**       | admission-op |
| `ff_append_frame` (L18)            | no core_key HSET; XADD'd                                                                                                                    | `attempt_stream` (L122) | on Ok path — this IS a bridge-event emit path for task output |
| `ff_read_attempt_stream` (L180)    | read-only                                                                                                                                   | —              | read path |
| `ff_version`                       | read-only                                                                                                                                   | —              | read path |

## §3 ff-server pub methods writing same

Each server method maps to one or more FCALLs. Inventory shape:
`pub fn` → delegated FCALL(s) → any extra direct Valkey writes
outside the FCALL.

Read `crates/ff-server/src/server.rs`. Methods that wrap FCALLs
without any extra direct write are GREEN on the non-FCALL-write
dimension; those are omitted from the table to cut noise. Only
methods with notable facts listed:

| Method (line)                      | Delegates to FCALL                     | Direct HSET(s) outside FCALL? | Note |
| ---------------------------------- | -------------------------------------- | ----------------------------- | ---- |
| `create_execution` (L444)          | `ff_create_execution`                  | no                            | Ok |
| `cancel_execution` (L528)          | `ff_cancel_execution`                  | no                            | Ok |
| `get_execution_state` / `get_execution_result` / `get_execution` (L560, 614, 1858) | read-only                              | no                            | getters |
| `list_pending_waitpoints` (L688)   | read-only (SCAN + HGET)                | no                            | getter |
| `create_budget` (L974)             | `ff_create_budget`                     | no                            | Ok |
| `create_quota_policy` (L1028)      | `ff_create_quota_policy`               | no                            | Ok |
| `get_budget_status` (L1066)        | read-only                              | no                            | getter |
| `report_usage` (L1142)             | `ff_report_usage_and_check`            | no                            | Ok |
| `reset_budget` (L1183)             | `ff_reset_budget`                      | no                            | Ok |
| `create_flow` (L1211)              | `ff_create_flow`                       | no                            | Ok |
| `add_execution_to_flow` (L1245)    | `ff_add_execution_to_flow`             | **YES — HSET flow_id on exec_core at L1279** | see §5 `non-FCALL` bucket |
| `cancel_flow` / `cancel_flow_wait` (L1327, 1337) | `ff_cancel_flow` + cross-partition dispatch | no (dispatched cancels go via `cancel_execution` on each member) | Ok |
| `stage_dependency_edge` (L1567)    | `ff_stage_dependency_edge`             | no                            | Ok |
| `apply_dependency_to_child` (L1611)| `ff_apply_dependency_to_child`         | no                            | Ok |
| `deliver_signal` (L1670)           | `ff_deliver_signal`                    | no                            | Ok |
| `change_priority` (L1771)          | `ff_change_priority`                   | no                            | Ok |
| `revoke_lease` (L1808)             | `ff_revoke_lease`                      | no                            | Ok |
| `list_executions` (L1948)          | read-only (SCAN + HGET)                | no                            | getter |
| `replay_execution` (L2078)         | `ff_replay_execution`                  | no                            | Ok |
| `read_attempt_stream` (L2179) / `tail_attempt_stream` (L2259) | read-only (`ff_read_attempt_stream` + XREAD) | stream_meta HSET is mentioned in comment L2282 but NOT called here | the HSET mentioned is a deferred v2 plan, not current |
| `rotate_waitpoint_secret` (L2673)  | no FCALL                               | **YES — multiple HSETs on HMAC secrets hash** | secret rotation, NOT exec state; out of scope for bridge events but flagged |
| `start` (L212)                     | library load + partition-config HSETs  | **YES — HSET ff:config:partitions at L2413-2427, HSET HMAC secrets at L2525-2555** | boot-time config, not exec state; out of scope |

All non-read-only methods are accounted for. The 3 non-FCALL HSET
sites are:

1. **`add_execution_to_flow` → HSET `flow_id` on `exec_core`** (L1279).
   A relational link added to exec_core AFTER the FCALL returns. Not
   a state-vector field; the 7-dim state is untouched. Flagged in §5
   because it violates the "all state writes go through Lua" rule,
   even though the field itself isn't bridge-event-relevant.
2. **`start` → HSET `ff:config:partitions`** (L2413-2427). Boot-time
   partition config, never mutated at runtime.
3. **HMAC secrets init + rotation** (L2525-2555, L2673). Crypto
   material, not exec state.

## §4 Cross-reference: writer → state fields → bridge-event emitted?

Columns:

- **FCALL** — Lua function name.
- **core state mutation** — does it HSET any of {public_state,
  lifecycle_phase, ownership_state, eligibility_state,
  attempt_state, blocking_reason, closed_at/closed_reason,
  terminal_reason, public_flow_state}? Y/N.
- **emits to lease_history?** — Y/N.
- **emits to wp_signals?** — Y/N.
- **emits to attempt_stream?** — Y/N.
- **gap?** — Y/candidate/N (see §5 for rationale on each candidate).

| FCALL                                        | core state | lease_history | wp_signals | attempt_stream | gap?         |
| -------------------------------------------- | :--------: | :-----------: | :--------: | :------------: | ------------ |
| `ff_create_execution`                        |     Y      |       N       |     N      |       N        | **candidate (entity creation)** |
| `ff_claim_execution`                         |     Y      |       Y       |     N      |       N        | N |
| `ff_complete_execution`                      |     Y      |       Y       |     N      |       N        | N |
| `ff_cancel_execution`                        |     Y      |       Y (active branch) / conditional on suspended branch | N | N | **candidate (suspended-branch closed_at on waitpoint without lease_history emit in some sub-paths)** |
| `ff_delay_execution`                         |     Y      |       Y       |     N      |       N        | N |
| `ff_move_to_waiting_children`                |     Y      |       Y       |     N      |       N        | N |
| `ff_fail_execution`                          |     Y      |       Y       |     N      |       N        | N |
| `ff_reclaim_execution`                       |     Y      |       Y       |     N      |       N        | N |
| `ff_expire_execution`                        |     Y      |   Y (active / suspended branches) / **N on runnable branch (L1464 → L1615)** | N | N | **candidate (runnable-expire branch emits no lease_history)** |
| `ff_suspend_execution`                       |     Y      |       Y       |     N      |       N        | N |
| `ff_resume_execution`                        |     Y      |       N       |     N      |       N        | **candidate (lifecycle_phase suspended→runnable without emit)** |
| `ff_create_pending_waitpoint`                |     N (waitpoint doc only) | N | N | N | **candidate (entity creation — waitpoint born silent)** |
| `ff_expire_suspension`                       |     Y      |   Y (terminal branch) / N (auto_resume branch) | N | N | **candidate (auto_resume branch mutates state, no emit)** |
| `ff_close_waitpoint`                         |     Y (waitpoint closed fields) | N | N | N | **candidate (waitpoint close without emit)** |
| `ff_deliver_signal`                          |     Y (on resume branch) | N (on resume branch) | Y | N | **candidate (resume-on-signal transitions exec state but lifecycle emit is wp_signals only, not lease_history)** |
| `ff_buffer_signal_for_pending_waitpoint`     |     N      |       N       |     Y      |       N        | N |
| `ff_claim_resumed_execution`                 |     Y      |       Y       |     N      |       N        | N |
| `ff_issue_claim_grant`                       |     N (grant hash only; advisory `last_capability_mismatch_at`) | N | N | N | N (advisory — grant is transient) |
| `ff_change_priority`                         |     N      |       N       |     N      |       N        | N (ZADD only) |
| `ff_update_progress`                         |     N (progress fields, not core state) | N | N | N | N (heartbeat-ish) |
| `ff_promote_delayed`                         |     Y      |       N       |     N      |       N        | **candidate (eligibility_state transition not_eligible_until_time→eligible_now silent)** |
| `ff_issue_reclaim_grant`                     |     N (grant hash only) | N | N | N | N |
| `ff_renew_lease`                             |     N (lease_expires_at only) | N | N | N | N (liveness-op) |
| `ff_mark_lease_expired_if_due`               |     Y (ownership_state transition) | Y (via helper) | N | N | N |
| `ff_revoke_lease`                            |     Y      |       Y       |     N      |       N        | N |
| `ff_create_flow`                             |     Y (flow_core only) | N | N | N | **candidate (entity creation — flow born silent)** |
| `ff_add_execution_to_flow`                   |     Y (flow member count + exec_core.flow_id via ff-server non-FCALL HSET) | N | N | N | **candidate (flow-membership event silent)** |
| `ff_cancel_flow`                             |     Y (flow_core.public_flow_state) | N | N | N | **candidate (flow terminal transition silent)** |
| `ff_stage_dependency_edge`                   |     N (edge attrs only) | N | N | N | N |
| `ff_apply_dependency_to_child`               |     Y (public_state→waiting_children, eligibility_state→blocked_by_dependencies) | N | N | N | **candidate (dependency-block transition silent)** |
| `ff_resolve_dependency`                      |     Y (on satisfied + skip-cancel branches) | N | N | N | **candidate (dependency-resolve transition silent; cancel-on-skip path especially)** |
| `ff_evaluate_flow_eligibility`               |     N (read-only) | N | N | N | N |
| `ff_promote_blocked_to_eligible`             |     Y (eligibility_state→eligible_now) | N | N | N | **candidate (block→eligible silent)** |
| `ff_replay_execution`                        |     Y      |       Y       |     N      |       N        | N |
| `ff_create_budget`                           |     N (policy only) | N | N | N | **candidate (entity creation — budget born silent)** |
| `ff_report_usage_and_check`                  |     N (counters only) | N | N | N | N (usage-op; breach reporting flows via return value) |
| `ff_reset_budget`                            |     N (counter clear) | N | N | N | **candidate (admin-mutation silent — is budget-reset observable to cairn?)** |
| `ff_unblock_execution`                       |     Y (eligibility_state→eligible_now, blocking_reason→waiting_for_worker) | N | N | N | **candidate (blocked→eligible silent)** |
| `ff_block_execution_for_admission`           |     Y (eligibility_state transition, public_state→rate_limited) | N | N | N | **candidate (admission-block → rate_limited silent)** |
| `ff_create_quota_policy`                     |     N (policy only) | N | N | N | **candidate (entity creation — quota policy born silent)** |
| `ff_check_admission_and_record`              |     N (counters only) | N | N | N | N (usage-op) |
| `ff_release_admission`                       |     N (counter rollback) | N | N | N | N (usage-op) |
| `ff_append_frame`                            |     N (stream_meta frame_count only) | N | N | Y | N |
| `ff_read_attempt_stream`                     |     N (read-only) | N | N | N | N |
| `ff_version`                                 |     N (read-only) | N | N | N | N |

## §5 Candidate gaps

Grouped by flag-class. Each row cites the §4 line and a short
rationale. W3 cross-references these against cairn's actual
consumer list to confirm/dismiss.

### §5.1 Entity-creation writes with no XADD (class flagged by manager)

New entities are born in Valkey but no stream announces them.
Cairn must discover them by polling — pathological per the manager's
note.

| FCALL                         | Entity created                  | Rationale                               |
| ----------------------------- | ------------------------------- | --------------------------------------- |
| `ff_create_execution`         | exec_core (new execution)       | **strong gap candidate** — direct analog to PR#26 `SessionCreated`. An execution is born runnable + eligible with zero stream emit; cairn must poll `list_executions` or tail the lane's `eligible` ZSET indirectly to discover new work. |
| `ff_create_flow`              | flow_core (new flow container)  | **strong gap candidate** — flow membership events downstream depend on cairn knowing the flow exists. Same polling-discovery problem. |
| `ff_create_pending_waitpoint` | waitpoint_key (suspension point)| **strong gap candidate** — a waitpoint comes into being able to receive signals, but `wp_signals_stream` starts empty. Cairn can only know the waitpoint exists via `list_pending_waitpoints`. |
| `ff_create_budget`            | budget policy document          | Medium-strength candidate. Budgets are admin-managed and typically created out-of-band; cairn may not need to observe creations. W3 confirm. |
| `ff_create_quota_policy`      | quota policy document           | Same rationale as budget — admin-managed. W3 confirm. |

### §5.2 Lifecycle transitions without lease_history emit

The state vector mutates but no entry appears in the
per-execution `lease_history` stream. Cairn's lifecycle-tracking
tail may miss these.

| FCALL                                | Transition (partial / branch)                                                                    | Rationale |
| ------------------------------------ | ------------------------------------------------------------------------------------------------ | --------- |
| `ff_expire_execution` (runnable branch, L1464→L1615) | lifecycle_phase=runnable → terminal + public_state=expired                                         | **strong gap candidate** — the `active` and `suspended` branches DO emit, but the `runnable` branch (never-claimed execution expired on deadline) writes the terminal row without XADD. Non-trivial: an execution can silently transition `runnable→terminal/expired` and the lease_history tail shows nothing because no lease was ever taken. |
| `ff_resume_execution`                | lifecycle_phase=suspended → runnable                                                             | **strong gap candidate** — a resume is a first-class lifecycle event (the execution becomes claimable again) but there is no XADD here. The paired `ff_claim_resumed_execution` emits `resumed`, but only IF and when the worker actually claims. Between resume and claim, lease_history shows nothing. |
| `ff_deliver_signal` (resume branch)  | exec_core transitions on resume-on-signal (suspended → runnable)                                 | **candidate** — `wp_signals_stream` records the SIGNAL, but the exec-state transition that resumes the execution does not emit to `lease_history`. Cairn's lifecycle view sees signal-in but must correlate with a later `ff_claim_resumed_execution` to know the exec resumed. |
| `ff_expire_suspension` (auto_resume) | suspended → runnable (auto-resume timeout behaviour)                                             | **candidate** — terminal branch emits; auto_resume branch mutates state silently. Same shape as the `ff_resume_execution` gap. |
| `ff_cancel_execution` (suspended source, sub-paths L712-L773) | suspension closed_at writes without an accompanying lease_history on some paths                  | YELLOW candidate — the `active`-lifecycle branch always emits; the suspended-lifecycle branch emits on most paths but a reader tracing `closed_at` transitions on the waitpoint may see updates that have no execution-level event. W3 verify. |

### §5.3 Scheduling / admission transitions with no emit at all

These transitions are functionally visible to cairn (an execution
becomes eligible, or becomes blocked) but the only way to observe
them is by polling `exec_core`.

| FCALL                                | Transition                                                                | Rationale |
| ------------------------------------ | ------------------------------------------------------------------------- | --------- |
| `ff_promote_delayed`                 | eligibility_state=not_eligible_until_time → eligible_now                  | Scanner-driven; no emit. Cairn only sees an execution "become claimable" by its next poll. |
| `ff_promote_blocked_to_eligible`     | eligibility_state=blocked_by_dependencies → eligible_now                  | Same shape. Critical for DAG-observer use cases: a dependency-satisfied promotion is invisible to streams. |
| `ff_unblock_execution`               | budget/quota unblock → eligible_now                                       | Same shape. Admission-scanner unblocks silently. |
| `ff_block_execution_for_admission`   | runnable/eligible → blocked (public_state=rate_limited)                   | Same shape. Admission-reject is a public_state transition that's only visible via polling. |
| `ff_apply_dependency_to_child`       | eligibility_state=blocked_by_dependencies + public_state=waiting_children | Same shape. Dependency-graph edge applied silently. |
| `ff_resolve_dependency`              | dependency-satisfied path: preserves phase; cancel-on-skip path: writes ended_cancelled + closed_at/reason | Two sub-paths, both silent. The cancel-on-skip path is higher-severity — it writes terminal-ish attempt state without a lease_history emit. |

### §5.4 Flow-level transitions with no emit

| FCALL                        | Transition                                     | Rationale |
| ---------------------------- | ---------------------------------------------- | --------- |
| `ff_add_execution_to_flow`   | flow membership added                          | A new member joins a flow; cairn must poll flow_core to discover. |
| `ff_cancel_flow`             | public_flow_state → cancelled                  | **strong gap candidate** — flow-terminal transition parallel to exec-terminal, but exec-terminal emits lease_history while flow-terminal emits nothing. Member cancellations propagate per-execution via `cancel_execution` (which does emit), so individual members are observable — but the FLOW-level transition is not. |
| `ff_reset_budget`            | budget usage counters cleared                  | Admin-mutation. Cairn-side may not care. W3 confirm. |

### §5.5 Non-FCALL direct state writes (manager's separate bucket)

Per manager: "atomic state should go through Lua; bypassing that is
a separate class of defect."

| File:line                                | Field(s)                                                     | Rationale |
| ---------------------------------------- | ------------------------------------------------------------ | --------- |
| `crates/ff-server/src/server.rs:1279`    | `exec_core.flow_id` (non-FCALL HSET in `add_execution_to_flow`) | **YELLOW** — the field is a relational link, not a state-vector member; the comment at L1274 acknowledges the two-phase write is "idempotent, safe to retry if phase 1 succeeded but phase 2 failed". But it sits outside the atomic FCALL boundary, meaning a crash between phases leaves the flow_core thinking the exec is a member while exec_core has no flow_id. Not a bridge-event gap per se; flagged for the non-FCALL-state-writes bucket. |
| `crates/ff-server/src/server.rs:2413-2427` | `ff:config:partitions` HSETs at server boot                   | Config, not exec state. GREEN — intentionally outside Lua because it's boot-time config a server writes once. No bridge-event concern. |
| `crates/ff-server/src/server.rs:2525-2555`, 2673, 2955, 3021-3046 | HMAC-secret init + rotation HSETs                     | Crypto material, not exec state. GREEN — intentionally outside Lua for crypto-rotation atomicity. No bridge-event concern. |

Only one non-FCALL exec-state write in the entire codebase: the
`flow_id` field in `add_execution_to_flow`. Worth noting because
it's the ONLY violation of the "all exec_core writes go through
Lua" invariant.

## §6 Summary

**Total FCALLs audited:** 45 (across 9 Lua files).
**Total ff-server pub methods audited:** ~30 (server.rs).
**Emit streams identified:** 3 (lease_history_key, wp_signals_stream,
attempt_stream via `ff_append_frame`). **No dedicated bridge-event
stream exists.**

**Candidate gap count by class:**

| Class                                        | Count |
| -------------------------------------------- | ----: |
| §5.1 Entity-creation writes with no XADD     |     5 |
| §5.2 Lifecycle transitions without emit      |     5 |
| §5.3 Scheduling/admission transitions silent |     6 |
| §5.4 Flow-level transitions silent           |     3 |
| §5.5 Non-FCALL direct state writes (not an emit-gap, but a separate defect bucket) | 1 |

**Strongest candidates (W3 should prioritise):**

1. `ff_create_execution` — PR#26 analog; executions born without
   emit.
2. `ff_create_flow` + `ff_create_pending_waitpoint` — entities born
   without emit.
3. `ff_expire_execution` runnable branch — terminal transition with
   no lease_history.
4. `ff_resume_execution` — suspended→runnable silent.
5. `ff_cancel_flow` — flow terminal transition silent.
6. `ff_promote_blocked_to_eligible` + `ff_unblock_execution` —
   dependency/admission unblocks that only surface via polling.

W3's comparison pass against cairn's known bridge-event consumer
list will confirm which of these are actual gaps vs intentional
polling points.

## §7 Deliverable boundary

This report ends at "candidate gaps." No fixes, no cairn touches,
no code changes. The three-way distinction (bridge-event / stream
tail / poll) from the manager's approval is the lens throughout.
W3 produces the confirmed-gap-plus-fix-recommendation report next.
