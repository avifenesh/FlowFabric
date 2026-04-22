# RFC: FCALL ARGV Cleanup (Issue #58.5)

**Status:** Draft — awaiting owner approval before any Lua edits.
**Scope:** Audit + design for ARGV surface reduction across the `ff_*` FCALL
inventory (`lua/*.lua`). Doc-only PR. No implementation in this branch.
**Goal:** Reduce Valkey-cluster-specific baggage in the FCALL surface ahead of
the Postgres-backend work (#58), without regressing stale-lease fence
semantics.

---

## Executive summary

Today several FCALLs accept fields in ARGV that the server could resolve from
`exec_core` (the canonical per-execution hash): `lane_id`,
`current_attempt_id`, `current_attempt_index`, `current_waitpoint_id`,
`current_lease_epoch`. The audit in this doc shows the actual ARGV shapes are
less uniform than the brief suggested — some callers pass these fields as a
**fence** (to reject the op if the server's current value disagrees with the
caller's view), others pass them as **initialisers** (write-through on first
assignment, e.g. `lane` on claim), and others don't pass them at all.

Our recommendation is **Option B — override-with-default**: the caller passes
`execution_id` always, and passes `lease_id`/`lease_epoch`/`attempt_id` (the
fence triple) **optionally**. When present, Lua fences exactly as it does
today. When absent (empty string), Lua resolves from `exec_core` and **does
not fence**. The "no fence" mode is strictly reserved for callers that
provably don't need one — operator overrides, timer-driven janitors,
read-model paths. The Rust SDK will continue to pass the fence triple on
every worker-driven call.

This preserves correctness, keeps the fence as the default for workers, and
clears the ARGV of fields that are never a fence (`lane`, `attempt_index`,
`waitpoint_id`). Backward compatibility is free because FCALLs read ARGV
positionally and we are proposing **ADDITIONAL-OR-EMPTY**, not removal — old
callers supplying non-empty values keep working unchanged.

---

## Options considered

### Option A — always resolve server-side
Lua unconditionally reads `current_lease_id` / `current_lease_epoch` /
`current_attempt_id` from `exec_core` and ignores any ARGV. **Loses
stale-lease fence.** A zombie worker whose lease has since been reclaimed by
another worker would still "complete" the execution, overwriting the rightful
owner's work. **Rejected.**

### Option B — override-with-default (recommended)
Caller passes `execution_id` always. Caller optionally passes `lease_id`,
`lease_epoch`, `attempt_id`. Present (non-empty) → Lua fences (current
behavior). Absent (empty string) → Lua resolves from `exec_core` and skips
the fence. Fence is opt-in but the SDK's worker path opts in universally.

### Option C — always resolve + return authoritative
Lua always resolves and returns the server's lease/attempt view; caller
re-validates locally and retries if stale. Adds a round-trip on every fenced
op, doubles read load, and forces every caller to reimplement fencing.
**Rejected** — cost outweighs benefit and stale-lease windows widen.

### Why B
1. No correctness regression vs. today for the worker path — SDK always
   supplies the fence.
2. Janitors / operator tools that today have to fabricate fake fence values
   (or use operator-override paths) get a clean API.
3. Per-FCALL, we remove three **genuinely redundant** fields:
   `lane`/`lane_id`, `attempt_index`, `waitpoint_id` — they are server-owned
   after claim, never a fence, and used only as write-through or
   cross-reference.
4. Backward compatible: existing ARGV positions stay. `""` becomes a valid
   sentinel for the fence triple.

---

## Per-FCALL audit

Legend:
- **Stays required**: field must be non-empty; no change.
- **R→O(fence)**: stays in-slot; non-empty means "fence me", empty means
  "server-resolve, no fence".
- **R→O(resolve)**: stays in-slot; non-empty is still accepted for compat
  but ignored; empty triggers server resolve. Earmarked for removal in a
  future major Lua version.
- **Remove-in-v2**: proposed to be dropped entirely after a deprecation
  window.

### Group 1 — post-claim fenced writes (worker-driven)

These are the hot-path FCALLs that every worker calls. They share the
`validate_lease_and_mark_expired` helper. The fence triple is
`(lease_id, lease_epoch, attempt_id)`.

| FCALL | Current ARGV | Proposed change |
|---|---|---|
| `ff_complete_execution` | `execution_id, lease_id, lease_epoch, attempt_id, result_payload` | `lease_id`/`lease_epoch`/`attempt_id` → **R→O(fence)** |
| `ff_fail_execution` | `execution_id, lease_id, lease_epoch, attempt_id, failure_reason, failure_category, retry_policy_json` | `lease_id`/`lease_epoch`/`attempt_id` → **R→O(fence)** |
| `ff_renew_lease` | `execution_id, attempt_index, attempt_id, lease_id, lease_epoch, lease_ttl_ms, lease_history_grace_ms` | `attempt_index` → **R→O(resolve)** (never a fence; resolved from `core.current_attempt_index`); `attempt_id`/`lease_id`/`lease_epoch` → **R→O(fence)** |
| `ff_delay_execution` | `execution_id, lease_id, lease_epoch, attempt_id, delay_until` | fence triple → **R→O(fence)** |
| `ff_move_to_waiting_children` | `execution_id, lease_id, lease_epoch, attempt_id` | fence triple → **R→O(fence)** |
| `ff_suspend_execution` | `..., attempt_index, attempt_id, lease_id, lease_epoch, suspension_id, waitpoint_id, ...` | `attempt_index` → **R→O(resolve)**; `waitpoint_id` **stays required** (caller-minted new id, not a fence); fence triple → **R→O(fence)** |

### Group 2 — operator / janitor / timer-driven

These either already take only `execution_id` (no ARGV cleanup needed) or
take a lease_id for *targeted* revocation, which is different from the fence
use case.

| FCALL | Current ARGV | Proposed change |
|---|---|---|
| `ff_cancel_execution` | `execution_id, reason, source, lease_id, lease_epoch` | Already optional (`args[4]`/`args[5]` default `""`). **Unchanged.** Already exhibits Option-B behavior: present ⇒ fence, empty ⇒ operator override (gated on `source=="operator_override"`). Document this as the canonical example. |
| `ff_mark_lease_expired_if_due` | `execution_id` | **Unchanged.** |
| `ff_expire_suspension` | `execution_id` | **Unchanged.** |
| `ff_revoke_lease` | `execution_id, expected_lease_id, revoke_reason` | `expected_lease_id` is already opt-in (empty = skip check). **Unchanged.** |
| `ff_close_waitpoint` | TBD — owner to confirm scope | Flagged; will re-audit in impl PR. |

### Group 3 — pre-claim / claim / reclaim

These run **before** the execution is fully bound to a worker, so the fence
triple isn't in `exec_core` yet. `lane` here is a **write-through
initialiser** (`HSET core_key current_lane <lane>`), not a fence.

| FCALL | Current ARGV | Proposed change |
|---|---|---|
| `ff_claim_execution` | `execution_id, worker_id, worker_instance_id, lane, capability_hash, lease_id, lease_ttl_ms, renew_before_ms, attempt_id, attempt_policy_json, attempt_timeout_ms, execution_deadline_at` | **Unchanged.** `lane` still needed because the caller (scheduler) mints the lane assignment and Lua writes it. |
| `ff_reclaim_execution` | `execution_id, worker_id, worker_instance_id, lane, lease_id, lease_ttl_ms, attempt_id, attempt_policy_json` | `lane` → **Remove-in-v2**. Exec_core already has `current_lane` from the original claim; re-passing it lets the caller disagree with stored state (no current safeguard). Verified usage: only written back to `current_lane` (line 1302 `HSET core_key current_lane A.lane`). |
| `ff_claim_resumed_execution` | `execution_id, worker_id, worker_instance_id, lane, capability_snapshot_hash, lease_id, lease_ttl_ms, remaining_attempt_timeout_ms` | `lane` → **Remove-in-v2**. Same reasoning as reclaim. |
| `ff_issue_claim_grant` | (see scheduling.lua:79) `lane_id` is present | `lane_id` **stays required** for scheduler FCALLs — these run on per-lane zsets and compute assignments before the grant is bound to an execution. Document as "scheduling-plane, not data-plane". |
| `ff_issue_reclaim_grant` | similar | **Unchanged** (scheduling-plane). |

### Group 4 — waitpoints / signals

| FCALL | Current ARGV | Proposed change |
|---|---|---|
| `ff_create_pending_waitpoint` | `execution_id, attempt_index, waitpoint_id, waitpoint_key, expires_at` | `attempt_index` → **R→O(resolve)**. Currently used as a fence-like check against `core.current_attempt_index` — but the check is **not a lease fence**, it only guards against a late call from a stale attempt. Since caller must also hold the active lease (see check at suspension.lua:434), redundant. Recommend opt-in in v2, dropped in v3. `waitpoint_id`/`waitpoint_key` stay required (caller-minted). |
| `ff_deliver_signal` | (see signal.lua:27) external caller, not lease-bound | **Unchanged.** |
| `ff_buffer_signal_for_pending_waitpoint` | similar | **Unchanged.** |

### Group 5 — non-scope

`ff_version`, `ff_create_execution`, `ff_create_flow`, `ff_add_execution_to_flow`,
`ff_create_quota_policy`, `ff_check_admission_and_record`, `ff_release_admission`,
`ff_create_budget`, `ff_report_usage_and_check`, `ff_reset_budget`,
`ff_unblock_execution`, `ff_block_execution_for_admission`, `ff_append_frame`,
`ff_read_attempt_stream`, `ff_change_priority`, `ff_update_progress`,
`ff_promote_delayed`, `ff_stage_dependency_edge`, `ff_apply_dependency_to_child`,
`ff_resolve_dependency`, `ff_evaluate_flow_eligibility`,
`ff_promote_blocked_to_eligible`, `ff_replay_execution`,
`ff_set_execution_tags`, `ff_set_flow_tags`, `ff_expire_execution`,
`ff_cancel_flow`, `ff_ack_cancel_member`, `ff_rotate_waitpoint_hmac_secret`,
`ff_list_waitpoint_hmac_kids`, `ff_resume_execution`, `ff_close_waitpoint`.

No fenced fields in ARGV or already takes the minimal surface. Re-audit
during implementation to confirm.

### Note on brief-mentioned fields that were NOT found

The original brief called out five fields as candidates: `lane_id`,
`current_attempt_id`, `current_attempt_index`, `current_waitpoint_id`,
`current_lease_epoch`. Audit findings:

- **`current_waitpoint_id` in ARGV: not observed** in any FCALL. The
  waitpoint_id in `ff_suspend_execution` and `ff_create_pending_waitpoint`
  is a **new** id the caller mints, not a reference to `core.current_waitpoint_id`.
- **`lease_epoch` / `attempt_id`**: present in Group 1 FCALLs — these are
  the fence, and Option B keeps them opt-in.
- **`attempt_index`**: present in renew/suspend/create_pending_waitpoint —
  not a fence, just a redundant reference. Can become opt-in.
- **`lane` / `lane_id`**: present in claim/reclaim/claim_resumed
  (data-plane) and scheduler grant FCALLs (scheduling-plane). Only reclaim
  and claim_resumed are redundant; the rest stay.

---

## Fence semantics

This section exists because stale-lease fencing is the one correctness
property we must not regress.

### When does "caller passes lease_epoch" actually fence?

In the data-plane write FCALLs (complete, fail, delay,
move_to_waiting_children, suspend, renew), `validate_lease_and_mark_expired`
(helpers.lua:497) rejects the call if any of `current_lease_id`,
`current_lease_epoch`, `current_attempt_id` on `exec_core` disagrees with the
caller's ARGV. This prevents:

1. **Zombie workers.** Worker A's lease expires, scanner marks it
   `lease_expired_reclaimable`, Worker B reclaims with a new lease (epoch
   N+1, new attempt_id). Worker A wakes from a GC pause and calls
   `ff_complete_execution`. Without the fence, A overwrites B's in-flight
   attempt and marks the execution `terminal/success`, silently dropping
   B's work.
2. **Cross-attempt replay.** A retry is scheduled (new attempt_id pending),
   then the old attempt's buffered response arrives late. The attempt_id
   mismatch rejects it.
3. **Revoked-lease writes.** Operator revokes A's lease. A's subsequent
   complete/fail is rejected via `ownership_state == "lease_revoked"`
   (separate check, but same code path).

### Absent-means-resolve: what happens?

Under Option B, if the caller passes `lease_id=""`, `lease_epoch=""`,
`attempt_id=""`, Lua will:

1. Read `exec_core`.
2. Bind the internal `A.lease_id`/`A.lease_epoch`/`A.attempt_id` from
   `core.current_lease_id` / `core.current_lease_epoch` /
   `core.current_attempt_id`.
3. **Skip** the `validate_lease_and_mark_expired` call and proceed with
   whatever `exec_core` says is current.

The op still succeeds (assuming lifecycle/ownership preconditions pass) —
but it succeeds **unfenced**. A zombie caller that passes empty fence
fields would be treated as authoritative.

### Absent-means-unfenced is only safe when the caller is demonstrably not a zombie

Allowed callers (unfenced mode):

- **Operator CLI / control-plane** paths that already require auth and are
  not racing with workers.
- **Timer-driven janitors** (`ff_expire_suspension`,
  `ff_mark_lease_expired_if_due`) — these only fire after a scheduler
  re-validates the timer is still due.
- **Same-process SDK helpers** that have not been paused long enough for
  the lease to plausibly expire (e.g. internal bookkeeping within a single
  FCALL round-trip).

Callers that **must** fence (continue passing the triple):

- Every worker-driven write on the execution hot path.
- Any path where the caller might have been paused long enough that the
  lease could have turned over (GC pauses, container migrations, network
  partitions).

### Correctness-regression watchlist

The following FCALLs, if called unfenced, would introduce a silent data
corruption path. The implementation MUST refuse to accept empty fence
fields from these callers **unless** a separate `source` ARGV is set to an
allowlisted value (e.g. `operator_override`, matching how
`ff_cancel_execution` already gates it):

- `ff_complete_execution` (terminal success — cannot be undone)
- `ff_fail_execution` (may schedule retry, may go terminal — both
  irreversible)
- `ff_delay_execution` (releases lease; next claimer believes the
  execution is cleanly paused)
- `ff_move_to_waiting_children` (same)

Recommendation: reuse the `source` pattern from `ff_cancel_execution`.
Absence of fence + absence of allowlisted `source` → return `err("fence_required")`.
This gives the SDK an explicit failure mode instead of a silent drop.

`ff_renew_lease` is self-policing — the body reads the expiry and rejects
if already expired, so unfenced mode is materially pointless (the only way
it succeeds is if the caller is still the rightful lease holder, in which
case they had the fence anyway). Recommend keeping the fence as required
for renew and not accepting empty.

---

## Rollout plan

### Phase 1 — this PR (doc only, #58.5)
Publish this doc, land owner approval.

### Phase 2 — Lua bump to next minor version, Group 1 first
1. Bump `lua/version.lua` (RFC-010 §4.8g protocol).
2. Introduce helper `resolve_lease_fence(core, argv)` in `helpers.lua` that
   returns the fence triple from ARGV if non-empty, else from `exec_core`,
   and flags whether the fence check should run.
3. Migrate Group 1 (6 FCALLs) in one PR. Each FCALL gets the
   `fence_required` check for terminal ops per the watchlist above.
4. Exec_core-side audit: verify `current_lease_id` / `current_lease_epoch` /
   `current_attempt_id` / `current_attempt_index` / `current_lane` are
   written exactly where today's code expects them to be — no new read
   paths introduced that would race the write.
5. Update `ff-core` FCALL wrapper fns to pass empty strings through (no
   behavior change for SDK callers).
6. SDK integration tests: add coverage for the unfenced path via a new
   `janitor_client` helper.

### Phase 3 — Group 3 `lane` removal
`ff_reclaim_execution` and `ff_claim_resumed_execution` drop the `lane`
ARGV slot. This is a **breaking** ARGV positional change and requires a
major Lua version bump per RFC-010. Coordinate with Worker C's capability
routing work (#58 Batch B) — they touch the same files.

### Phase 4 — Group 4 `attempt_index` removal
Drop `attempt_index` from `ff_renew_lease` / `ff_suspend_execution` /
`ff_create_pending_waitpoint`. Same major-bump process.

### semver-checks note
`ff-core`'s FCALL wrapper functions expose typed Rust args. Making a
Rust-level arg `Option<LeaseId>` is a **breaking change** to the public
function signature per cargo-semver-checks. Options:
- Introduce a parallel `*_unfenced` fn and keep the fenced fn signature
  unchanged (additive, non-breaking). **Recommended.**
- Gate the signature change on the next major `ff-core` release (heavier).

The doc-only PR is semver-safe. The impl PR will need the parallel-fn
approach to stay additive.

### Backward compatibility
Phase 2 is backward-compatible at the Lua ARGV level — old callers keep
working. Phase 3+ are breaking; announce in RFC-010 migration doc with at
least one minor-version deprecation window.

---

## Open questions for owner

1. Is the **fence_required gate on terminal ops** (Option-B refinement)
   the right failure mode, or do you prefer empty-fence ⇒ silent resolve
   across the board? The former costs one ARGV slot (`source`) but rules
   out a whole class of silent corruption.
2. Is the **parallel `*_unfenced` fn** approach acceptable, or do you
   prefer a single fn with `Option<Fence>` and taking the
   cargo-semver-checks hit at the next major?
3. Scope of Phase 2: land Group 1 as one PR (6 FCALLs) or split by FCALL?
   One PR is simpler to review; split is easier to revert if a bug lands.

---

## Appendix — helper signature

```lua
-- helpers.lua (new)
-- Returns (fence_triple, must_check) or nil,err.
-- must_check=true when caller supplied non-empty fence fields.
-- must_check=false when fields were empty and we resolved from core.
local function resolve_lease_fence(core, argv)
  if is_set(argv.lease_id) or is_set(argv.lease_epoch) or is_set(argv.attempt_id) then
    -- Partial fence triples are a programming error — reject.
    if not (is_set(argv.lease_id) and is_set(argv.lease_epoch) and is_set(argv.attempt_id)) then
      return nil, err("partial_fence_triple")
    end
    return { lease_id = argv.lease_id, lease_epoch = argv.lease_epoch, attempt_id = argv.attempt_id }, true
  end
  return {
    lease_id = core.current_lease_id or "",
    lease_epoch = core.current_lease_epoch or "",
    attempt_id = core.current_attempt_id or "",
  }, false
end
```
