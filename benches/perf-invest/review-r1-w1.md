# Cross-review round 1 — Worker-1 reviewing 863f2f0 (W2) + 0b86024 (W3)

**Reviewer:** Worker-1
**Branch:** `feat/cairn-sdk-gaps` @ `c545664`
**Scope:** Per manager, W1 reviews W2's P1B (`863f2f0`) and W3's
P2+P3.4 (`0b86024`). W1 does NOT review own commits (`3cee229`,
`c545664`).

## Verdict table

| SHA     | Subject                                                          | Verdict |
| ------- | ---------------------------------------------------------------- | ------- |
| 863f2f0 | ff-sdk: ReclaimGrant + claim_from_reclaim_grant (cairn P1B)      | **RED** (1 RED + 3 YELLOW) |
| 0b86024 | ff-core + ff-sdk: pub parse_report_usage_result + dedup helper   | **GREEN** (3 YELLOW) |

Overall: round-1 has ONE RED blocker (the missing
semaphore check in `claim_from_reclaim_grant` already reported via
team-send). Everything else is YELLOW — docstring + polish.

## 863f2f0 — `ReclaimGrant + claim_from_reclaim_grant`

### Dim 1 — Public API surface
- GREEN — `ReclaimGrant` in `ff_core::contracts` alongside `ClaimGrant`,
  both re-exported from `ff-scheduler`. Clean dep-graph: neither
  ff-sdk nor ff-scheduler pulls the other.
- GREEN — `pub async fn claim_from_reclaim_grant(&self, grant)` has
  the right shape (grant carries partition + lane_id, so no extra
  parameter bloat).
- **YELLOW** (`crates/ff-core/src/contracts.rs:98` vs `:144`) —
  **asymmetric derives**. `ClaimGrant` has `#[derive(Clone, Debug)]`;
  `ReclaimGrant` has `#[derive(Clone, Debug, PartialEq, Eq)]`. The
  pair is framed as symmetric ("shared wire-level type between
  ff-scheduler issuer and ff-sdk consumer" in both docstrings), so
  the derives should match. Also neither has
  `Serialize`/`Deserialize` while the surrounding contracts types
  do — consumers like cairn that bus these over JSON/serde will
  need to re-implement.
- **YELLOW** (`crates/ff-core/src/contracts.rs:111-143`) —
  **`ReclaimGrant` docstring claims scheduler-issued but no
  producer in-tree.** The doc says "Produced when a suspended or
  signalled execution is ready to resume on a named worker", but
  `grep -rn 'ReclaimGrant {'` only finds test fixtures and the
  struct def. No `ff-scheduler` or `ff-server` code constructs a
  `ReclaimGrant`. In practice callers (cairn) build one manually
  from the existing `ff_issue_claim_grant` Lua + an `HGET`
  round-trip (see test helper
  `suspend_resume_setup_with_grant` at
  `crates/ff-test/tests/e2e_lifecycle.rs:8176`). Docstring should
  either drop the "scheduler issues" framing until a producer
  exists, or the producer should land in the same PR.

### Dim 2 — Error variant coverage
- GREEN for the documented surface — the
  `claim_from_reclaim_grant` docstring lists 5 specific
  `ScriptError` variants plus transport. All reachable error codes
  from `ff_claim_resumed_execution` are handled via
  `ScriptError::from_code_with_detail` inside the underlying
  `claim_resumed_execution` (`worker.rs:1092+`).
- **GREEN — no swallowed errors.** The 13-arg path returns
  `SdkError::Script(Parse(...))` on any shape mismatch, never
  `unwrap_or_default`'s through a malformed FCALL reply.

### Dim 3 — Test coverage
- GREEN — 6 tests cover happy / control / expired / wrong_worker /
  not_resumed / double_consume at
  `crates/ff-test/tests/e2e_lifecycle.rs:8068+`. Each test has
  meaningful assertions (attempt_index preservation, state vector
  transitions, grant_key consumption) rather than path-matching. The
  `test_claim_from_reclaim_grant_expired` pattern (short TTL +
  control companion with no sleep) specifically guards against the
  expiry test passing due to a stuck-clock bug.
- **YELLOW — no test for concurrency semantics.** This is the same
  gap that underlies the RED below. There's no test that saturates
  the worker and then calls `claim_from_reclaim_grant` — which
  means the missing semaphore acquire is not only not fixed, it's
  not detectable via the existing test suite.

### Dim 4 — Wire-format / Lua contract alignment
- GREEN — `ff_claim_resumed_execution` KEYS[11] + ARGV[8] match
  `lua/signal.lua` per the code comments at
  `crates/ff-sdk/src/worker.rs:1042`. Structure of the success reply
  (`{1, "OK", lease_id, lease_epoch, expires_at, attempt_id,
  attempt_index, attempt_type}`) is the same as
  `ff_claim_execution`, so the existing parser is correct.
- GREEN — `ReclaimGrant.partition` + `lane_id` line up with Lua's
  `KEYS[3]` (eligible_zset) and `KEYS[9]` (active_index), as the
  struct docstring states.

### Dim 5 — Concurrency semantics
- **🔴 RED — NO semaphore acquire before the FCALL.**
  `crates/ff-sdk/src/worker.rs:995-1007`:
  ```rust
  pub async fn claim_from_reclaim_grant(
      &self, grant: ff_core::contracts::ReclaimGrant,
  ) -> Result<ClaimedTask, SdkError> {
      self.claim_resumed_execution(
          &grant.execution_id, &grant.lane_id, &grant.partition,
      ).await
  }
  ```
  No `self.concurrency_semaphore.clone().try_acquire_owned()` call.
  Compare with:
  - `claim_next` (`worker.rs:465`): acquires permit, `Ok(None)` on
    saturation.
  - `claim_from_grant` (`worker.rs:959`, P1A per manager's explicit
    ADDITION): acquires permit, `Err(WorkerAtCapacity)` on
    saturation.
  `ff_claim_resumed_execution` atomically consumes the reclaim
  grant — same failure mode as `ff_claim_execution`. If the worker
  is at `max_concurrent_tasks` capacity when
  `claim_from_reclaim_grant` runs, the FCALL consumes the grant
  and returns a `ClaimedTask` with NO concurrency permit
  transferred — so the worker silently exceeds the configured cap
  every time this path succeeds past saturation, and the "permit
  bank" is never debited for the in-flight resume.
  Docstring does not document this. No rationale for skipping the
  check (the reclaim is often on a FRESH worker instance — after
  a crash or restart — so "already has capacity" can't be
  assumed).

  **Flagged to manager via team-send at round-1 start; W2 is
  fixing.** Cited here so the review doc captures the finding
  durably. Fix pattern must mirror P1A:
  ```rust
  let permit = self.concurrency_semaphore.clone()
      .try_acquire_owned()
      .map_err(|_| SdkError::WorkerAtCapacity)?;
  let mut task = self.claim_resumed_execution(...).await?;
  task.set_concurrency_permit(permit);
  Ok(task)
  ```
  Plus an e2e saturation test mirroring
  `test_claim_from_grant_rejects_at_capacity`.

### Minor observation (not scored, captured for W3's round)
- `crates/ff-scheduler/src/claim.rs:525` —
  `expires_at_ms: now_ms + grant_ttl_ms` is computed Rust-side but
  the Lua FCALL uses `redis.call("TIME")` for the real grant
  expiry. Drift of a few ms between Rust host clock and Valkey
  server clock is invisible to the Lua-side enforcement but shows
  up if consumers use this field for client-side retry deadlines.
  Not new in 863f2f0 (the `ClaimGrant` producer code is a
  pre-existing `return Ok(Some(ClaimGrant { ... }))`), but
  863f2f0 documents the field as "When the grant expires if not
  consumed" without the "advisory only" caveat.

## 0b86024 — `pub parse_report_usage_result + usage_dedup_key helper`

### Dim 1 — Public API surface
- GREEN — `parse_report_usage_result` promoted `fn` → `pub fn` with
  unchanged signature. `usage_dedup_key` + `USAGE_DEDUP_KEY_PREFIX`
  added to `ff_core::keys`. Both are narrowly scoped — no internal
  helper leaks.
- **YELLOW** (`crates/ff-sdk/src/task.rs:1168` and
  `crates/ff-core/src/keys.rs:598`) — **Lua filename in doc
  comments is wrong.** Both docstrings reference
  `lua/ff_report_usage_and_check.lua`; the function actually lives
  in `lua/budget.lua` at line 99
  (`redis.register_function('ff_report_usage_and_check', ...)`).
  Fix: update the doc comments to `lua/budget.lua` (optionally
  `lua/budget.lua:99`) so grep-seekers land on a real file.

### Dim 2 — Error variant coverage
- GREEN for happy paths and the main failure modes — non-Array,
  non-Int first element, unknown sub-status all emit
  `SdkError::Script(ScriptError::Parse)`.
- **YELLOW** (`crates/ff-sdk/src/task.rs:1202-1203, 1208-1209`) —
  **`.unwrap_or(0)` silently coerces malformed numerics to zero.**
  Lines 1202/1203 (SoftBreach) and 1208/1209 (HardBreach) parse
  `current` and `limit` via `.parse().unwrap_or(0)`. If Lua ever
  emits a non-numeric string (`"inf"`, `"NaN"`, blank on an HGET
  miss), consumers receive `SoftBreach{current_usage: 0,
  soft_limit: 0}` — which reads as "success with zero usage" and
  silently defeats the breach-detection contract the public API
  is meant to enforce. Should return
  `SdkError::Script(ScriptError::Parse)` like the rest of the
  function does for shape violations. Not user-triggerable
  under current Lua, but the whole point of moving the parser to
  a shared location is to catch producer-side drift — this one
  can drift without surfacing.

### Dim 3 — Test coverage
- GREEN — 6 new tests:
  - `ff-sdk::parse_report_usage_result_tests`: `ok_status`,
    `already_applied_status`, `soft_breach_status`,
    `hard_breach_status`, `non_array_input_is_parse_error`,
    `first_element_non_int_is_parse_error`
    (`crates/ff-sdk/src/task.rs:1835-1925`).
  - `ff-core::tests::usage_dedup_key_format`
    (`crates/ff-core/src/keys.rs:707`) asserts the exact string
    shape AND count of `{`/`}` to guard against future accidental
    double-wrapping.
- Meaningful assertions throughout — no path-matching.
- **YELLOW** — no test for the
  `.parse().unwrap_or(0)` silent-coercion path noted in Dim 2. If
  that arm is fixed to return `Parse`, add a test with
  `s("not_a_number")` for the current field to cover it.

### Dim 4 — Wire-format / Lua contract alignment
- GREEN — verified `lua/budget.lua:99-177`:
  - Line 115: `return {1, "ALREADY_APPLIED"}`
  - Line 137: `return {1, "HARD_BREACH", dim, tostring(current),
    tostring(hard_limit)}`
  - Line 173: `return {1, "SOFT_BREACH", breached_soft,
    tostring(cur_val), tostring(soft_val)}`
  - Line 176: `return {1, "OK"}`
  Matches the docstring wire-format claim at `task.rs:1159`.
  `tostring(...)` on the numeric fields means they reach Rust as
  BulkString or SimpleString — `usage_field_str` handles both.
- GREEN — `usage_dedup_key(hash_tag, dedup_id)` format
  (`ff:usagededup:{tag}:{dedup_id}`) matches the Lua consumer's
  expectations in `lua/budget.lua` (the Lua side reads the full
  key via `KEYS[...]` so there's no implicit prefix assumption on
  the script side; the matching side is the two Rust callers now
  using the helper).

### Dim 5 — Concurrency semantics
- N/A — the parser is pure, the key-builder is pure. No
  concurrency-relevant code introduced.

## Summary

- **863f2f0: RED.** The saturation check gap on
  `claim_from_reclaim_grant` violates the configured
  `max_concurrent_tasks` contract the SDK advertises and matches
  the exact scenario manager called out as mandatory for
  `claim_from_grant`. W2 is fixing per manager's routing.
- **0b86024: GREEN** with three docstring / defensive-parse
  polish items. Publishable as-is; YELLOWs should be swept before
  merge.

Round 2 should validate the W2 fix lands with a saturation test
and the YELLOWs clear.
