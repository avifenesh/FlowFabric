# Cross-review round 2 — Worker-2 reviewing 3cee229 (W1) + c545664 (W1) + 0b86024 (W3)

**Reviewer:** Worker-2
**Branch:** `feat/cairn-sdk-gaps` @ `4a7a149`
**Scope:** per manager, W2 reviews W1's `3cee229` + `c545664` and
W3's `0b86024`. W2 does NOT review own commits (`863f2f0`, the
downstream `3f140dd` permit-acquire fix, or the downstream
`4a7a149` ClaimedTask::new revert).

## §1 Verdict table

| SHA     | Subject                                                   | Verdict |
| ------- | --------------------------------------------------------- | ------- |
| 3cee229 | ff-sdk: pub ClaimedTask::new                              | **RED → FIXED** in `4a7a149` (revert to `pub(crate)`) |
| c545664 | ff-sdk: pub claim_from_grant                              | **GREEN** (2 YELLOW) |
| 0b86024 | ff-core + ff-sdk: pub parse_report_usage_result + usage_dedup_key | **GREEN** (1 YELLOW) |

One blocker surfaced (3cee229 unnecessary API widening), reported
immediately via team-send, fix approved + landed as `4a7a149`. No
other RED items. Remaining YELLOWs are documentation / test-coverage
polish, none block merge.

## §2 3cee229 — `pub ClaimedTask::new`

### RED (now fixed in 4a7a149) — `pub` promotion is unnecessary + footgun

**File:line:** `crates/ff-sdk/src/task.rs:191` (pre-revert).

Commit message claims: "Two new entry points (worker-1:
claim_from_grant wrapping ff-scheduler's ClaimGrant; worker-2:
claim_from_reclaim_grant wrapping ReclaimGrant) both need to build
a ClaimedTask from outside the crate-local helpers."

The factual check:

```
$ grep -rn 'ClaimedTask::new' --include='*.rs'
crates/ff-sdk/src/worker.rs:884     (in claim_execution)
crates/ff-sdk/src/worker.rs:1181    (in claim_resumed_execution)
```

Both callers are in `ff-sdk/src/worker.rs` — **same crate** as
`task.rs`. `pub(crate)` was sufficient. The commit's stated
rationale (cross-crate access) does not apply.

Two concrete harms from the unnecessary promotion:

1. **SemVer lock-in on a 13-arg positional constructor** (`task.rs:191-205`).
   A future refactor that renames/combines/adds a mandatory field
   breaks every external caller — even though there are no external
   callers today. `pub(crate)` keeps the same refactors as non-
   breaking internal changes.

2. **Silent-footgun via asymmetric visibility.** `set_concurrency_permit`
   is `pub(crate)` (`task.rs:246`). If an external caller constructs
   `ClaimedTask::new`, they get a task without a concurrency permit
   AND cannot attach one. That task then bypasses the semaphore by
   construction — the exact invariant W1's R1 RED on 863f2f0
   (missing-permit-acquire in `claim_from_reclaim_grant`) was
   catching. Keeping `new` at `pub(crate)` eliminates the footgun.

**Status:** fixed at `4a7a149`. `pub(crate) fn new` restored; `dead_code`
allow dropped (call sites are live). Clippy + ff-sdk + ff-test green
across `--no-default-features`, default, and `--features direct-valkey-claim`.

## §3 c545664 — `pub claim_from_grant`

### GREEN — public API + saturation semantics

**File:line:** `crates/ff-sdk/src/worker.rs:900-968`.

The shape is correct:

- `pub async fn claim_from_grant(&self, lane: LaneId, grant: ff_core::contracts::ClaimGrant) -> Result<ClaimedTask, SdkError>`
- Explicit `lane` parameter matches the scheduler's `claim_for_worker` signature; no implicit lane-lookup that could drift.
- `ClaimGrant` consumed by value → compile-time prevents accidental reuse.

The concurrency ordering at `worker.rs:950-963` is the load-bearing
bit:

```rust
let permit = self.concurrency_semaphore
    .clone()
    .try_acquire_owned()
    .map_err(|_| SdkError::WorkerAtCapacity)?;
let now = TimestampMs::now();
let mut task = self.claim_execution(&grant.execution_id, &lane, &grant.partition, now).await?;
task.set_concurrency_permit(permit);
Ok(task)
```

Permit acquired BEFORE `claim_execution` fires the FCALL. If
`try_acquire_owned` fails, FCALL never runs → grant untouched. If
`claim_execution` errors after permit acquisition, the permit drops
(its owning stack variable goes out of scope), semaphore recycles.
If `set_concurrency_permit` runs, permit ownership transfers into
the task and releases on task drop/complete/fail/cancel. The
ordering is provably correct.

Test coverage at `crates/ff-test/tests/e2e_lifecycle.rs:4218-4418`:

- `test_claim_from_grant_via_scheduler` — happy path, verifies
  scheduler→SDK shape agreement, attempt_index=0 on fresh claim,
  execution_kind roundtrip, drop-clean contract.
- `test_claim_from_grant_rejects_at_capacity` —
  `max_concurrent_tasks=1`, two same-partition executions to
  guarantee priority order, first claim holds permit, second grant
  → `WorkerAtCapacity`, grant_key EXISTS=1 after the refusal,
  `is_retryable() == true`.

The EXISTS=1 post-refusal check is the critical assertion — it
proves `ff_claim_execution` was not invoked (because the FCALL
atomically DELs the grant key on success). Without this check the
test could pass while silently consuming the grant.

### YELLOW (test) — no "second worker can consume the rejected grant" positive path

`test_claim_from_grant_rejects_at_capacity` asserts the grant key
is preserved in Valkey after `WorkerAtCapacity`, but does not
demonstrate that a non-saturated worker can actually consume it.
A future Lua change that DELs the grant on the capacity-check path
(hypothetically) would still pass the EXISTS=1 assertion if the
check fired AFTER the DEL. Adding a second-worker consumption step
— or asserting the grant's `HGETALL` payload is still populated —
would close the last gap.

Not a blocker; the current assertion catches the realistic
regression (forgetting to pre-check the semaphore, which fires the
FCALL and DELs the key). Flagging for a future tightening.

### YELLOW (error surface) — `WorkerAtCapacity.valkey_kind()` returning `None`

**File:line:** `crates/ff-sdk/src/lib.rs:102`.

Manager's review dimension 3 asked whether `kind=None` risks
silent classification in a caller's retry loop. Reading cairn's
retry shape is out of scope here (cairn lives in a separate repo)
but the contract is reasonable:

- `Self::Config(_) | Self::WorkerAtCapacity => None` matches the
  intent "this error has no transport kind." A retry loop that
  dispatches on `valkey_kind` for transport classification won't
  find WorkerAtCapacity classifiable — correct, because it's not a
  transport issue.
- `is_retryable()` returns `true` for WorkerAtCapacity — a caller
  that routes on the boolean flag sees the retry-eligibility
  directly.

The cairn-side concern would be a caller that does
`err.valkey_kind().map(classify_transport).unwrap_or(DropSilently)`.
That idiom would swallow WorkerAtCapacity. It's also swallowing
`Config(...)`, which would be worse (a misconfiguration drop would
never surface), so the pattern is already suspect. Not a problem
introduced by this commit.

Flagging YELLOW only because the docstring could spell out that
`WorkerAtCapacity` is classified via `is_retryable()` rather than
`valkey_kind()`. One-sentence fix; not a blocker.

## §4 0b86024 — `pub parse_report_usage_result + usage_dedup_key`

### GREEN — both promotions sound

**Files:line:**
- `crates/ff-sdk/src/task.rs:1170` — `parse_report_usage_result` now `pub`.
- `crates/ff-core/src/keys.rs:600` — `USAGE_DEDUP_KEY_PREFIX` const.
- `crates/ff-core/src/keys.rs:607` — `usage_dedup_key(hash_tag, dedup_id)`.

All producer sites are migrated (verified by greps on
`ff:usagededup` and `usagededup`):

- `crates/ff-sdk/src/task.rs:701` — uses helper.
- `crates/ff-server/src/server.rs:1168` — uses helper.
- No remaining raw `format!("ff:usagededup:...")` in the tree.

Signature match at both call sites: `hash_tag` comes from
`bctx.hash_tag()` (returns `&str` with braces already in place,
e.g. `"{bp:7}"`), and the helper concatenates without double-wrapping.
The `usage_dedup_key_format` test at `keys.rs:707-713` asserts the
exactly-one-hash-tag-region invariant, which guards future accidental
double-wrap.

Lua consumer (`lua/budget.lua:109-167`) takes the dedup_key as an
opaque ARGV string — no format assumption on the Lua side. Rust
owns the key format; Lua just passes it to `GET` / `SET`. Wire
contract is safe.

### YELLOW — negative tests don't catch numeric-parse drift

**File:line:** `crates/ff-sdk/src/task.rs:1200-1215`.

The parser has two `.parse().unwrap_or(0)` fallbacks on the
SOFT_BREACH and HARD_BREACH numeric fields:

```rust
let current: u64 = usage_field_str(arr, 3).parse().unwrap_or(0);
let limit: u64 = usage_field_str(arr, 4).parse().unwrap_or(0);
```

If the Lua function ever shifts those fields (e.g. swapping in a
non-numeric string, or reordering so the numeric is in a different
slot), the parser silently returns `SoftBreach { current: 0, limit: 0 }`
instead of failing loudly.

The committed negative tests (`non_array_input_is_parse_error`,
`first_element_non_int_is_parse_error`) guard the outer shape
(must be Array, first element Int). They don't guard the inner
numeric slots.

A test like:
```rust
let raw = arr(vec![int(1), s("SOFT_BREACH"), s("tokens"), s("not_a_number"), s("100")]);
// expect either Parse error OR current_usage == 0 with a warning
```
— would make the `.unwrap_or(0)` fallback decision explicit
(surface it or tighten it). Not a regression introduced by
`0b86024` (the `unwrap_or(0)` predates this commit) but worth
noting since the commit adds the negative-tests infrastructure.

Not a blocker; flagging for parser hardening follow-up.

### YELLOW — `valkey_kind()` / `is_retryable()` path through `parse_report_usage_result`

The parser returns `SdkError::Script(ScriptError::Parse(...))` on
shape errors. `ScriptError::Parse` is classified as
`ErrorClass::NonRetryable` (verified earlier reading
`ff-script/src/error.rs`). So the negative tests' errors flow
through `is_retryable() == false` — correct, because a wire-format
break cannot be fixed by retry. Consistent with existing behavior.

The public promotion does not change the error-routing behavior.
Noting it GREEN explicitly so the cairn-side caller doesn't have
to re-derive: `parse_report_usage_result` errors are
NonRetryable-on-Parse, retryable on transport (since
`SdkError::is_retryable` delegates to the underlying transport
kind for `Valkey` variants).

## §5 Cross-cutting concerns

### Manager review dimension 1 (Public API surface)

3cee229 was the only one with a surface issue, now reverted. c545664
adds `SdkError::WorkerAtCapacity` (new pub variant) + a pub
`claim_from_grant` method — both minimal and well-scoped. 0b86024
adds 1 const + 1 fn + 1 fn-promotion, all trivially-scoped.

No accidental cross-crate re-exports. `ClaimGrant`/`ReclaimGrant`
live in `ff-core::contracts` and are re-exported from
`ff-scheduler`; the `pub use` re-export pattern is correct for a
shared wire type.

### Manager review dimension 2 (permit-before-FCALL)

c545664's pattern is sound. My own 863f2f0 had the bug (no permit
acquire) that W1 caught in R1 — fixed in `3f140dd` mirroring
c545664. R2 verified both pub entry points now acquire permits
before firing FCALLs.

### Manager review dimension 3 (WorkerAtCapacity classification)

Done above in §3 YELLOW. Short answer: `is_retryable=true,
valkey_kind=None` is the right combination. Docstring could note
the classification path explicitly.

### Manager review dimension 4 (negative tests)

Done above in §4. The committed negative tests guard the outer
shape but not the inner numeric slots (`unwrap_or(0)` silently
masks numeric-field drift). Not a regression; flagged for follow-up.

### Manager review dimension 5 (shared-keys helper signature parity)

Done above in §4. All producer sites migrated; no stale
`format!("ff:usagededup:...")` remaining. Signature matches both
call sites' `hash_tag` vs `dedup_id` pairing.

## §6 Final state

| commit   | original verdict     | status post-fix |
| -------- | -------------------- | --------------- |
| 3cee229  | RED (pub surface)    | FIXED in 4a7a149 |
| c545664  | GREEN (2 YELLOW)     | ships as-is |
| 0b86024  | GREEN (1 YELLOW)     | ships as-is |
| 863f2f0  | (W2's own, reviewed by W1 → RED → fixed in 3f140dd) | fixed |

Branch state at review close:
```
4a7a149 fix: revert ClaimedTask::new to pub(crate) (R2 YELLOW-leaning-RED from W2)
3f140dd fix: claim_from_reclaim_grant must acquire permit (R1 RED from W1)
e079d8a bench/perf-invest: W1 cross-review R1 of 863f2f0 + 0b86024
c545664 ff-sdk: pub claim_from_grant
863f2f0 ff-sdk: ReclaimGrant + claim_from_reclaim_grant
0b86024 ff-core + ff-sdk: pub parse_report_usage_result + usage_dedup_key helper
3cee229 ff-sdk: pub ClaimedTask::new  ← effect reverted by 4a7a149
```

Recommendation: **cleared for merge** from R2 cross-review. All
RED items have been caught and fixed. Remaining YELLOWs are
non-blocking (documentation + test-coverage polish items flagged
for post-merge follow-up).
