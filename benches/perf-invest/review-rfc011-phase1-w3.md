# Cross-review — RFC-011 phase 1 (W3)

Reviewer: Worker-3. Branch: `feat/rfc011-phase1-exec-id`, 3 commits on
top of main @ `c2dd647`:

- `a87d5f2` — phase 1.D LaneId validation (§9.15)
- `0dd91aa` — phase 1.A+B ExecutionId API + retire `num_execution_partitions` (§2.3, §3.2, §4.1)
- `0f5d180` — phase 1.C ff-script Partial refactor + workspace partition-site fixes (§2.4, §3.1b)

Independent of W1. Did not read W1's review before writing.

## Overall: **1 RED, 2 YELLOW. Rest GREEN.**

RED flagged via team-send immediately and routed by manager to W2 for
fix (option A restore). This doc includes the full review for the
transcript regardless.

## Dimension 1 — RFC compliance

### RED 1 — `PartitionFamily::Execution` deletion violates §11 non-goal

**RFC §11 (Non-goals):**
> "Deleting `PartitionFamily::Execution` from the public API. The
> variant stays for API compatibility; only its routing behaviour
> changes."

**Phase-1 reality:** commit `0dd91aa` removes `PartitionFamily::Execution`
entirely from `crates/ff-core/src/partition.rs:12-20`. Only `Flow`,
`Budget`, `Quota` remain.

**Impact — confirmed breakage in cairn-fabric:**
- `/tmp/cairn-rs/crates/cairn-fabric/src/boot.rs:5` imports `PartitionFamily`.
- `/tmp/cairn-rs/crates/cairn-fabric/src/boot.rs:204` instantiates `family: PartitionFamily::Execution`.

On phase-4 cairn rebase, cairn fails to compile with a
"no variant named `Execution`" error. RFC explicitly promised zero
cairn-side churn on this axis.

**Also collateral:** 24 workspace sites flipped `Execution → Flow` per
W2's commit log. If the variant is restored (option A), most of those
reverts — the routing outcome is identical because prefix() returns
"fp" for both — but each site should be audited case-by-case. Some
(e.g. `flow_projector.rs`) semantically WERE already Flow; those stay.
Others (`lease_expiry.rs`, `unblock.rs`, `attempt_timeout.rs`, etc.)
semantically iterate executions — those revert.

**Manager already routed this to W2 for option A.** Fix verification
criteria listed at the bottom of this doc (§ "Pending fix —
PartitionFamily::Execution restore verification").

### GREEN — §2.1 id shape

`crates/ff-core/src/types.rs:104`:
```rust
pub struct ExecutionId(String);
```
String-backed, shape `{fp:N}:<uuid>` enforced at constructor time. Matches §2.1.

### GREEN — §2.3 API shape

`ExecutionId::for_flow(&FlowId, &PartitionConfig) → Self` @ `types.rs:123`.
`ExecutionId::solo(&LaneId, &PartitionConfig) → Self` @ `types.rs:133`.
`ExecutionId::parse(&str) → Result<Self, ExecutionIdParseError>` @ `types.rs:149`.
`ExecutionId::partition() → u16` @ `types.rs:169`.
`ExecutionId::as_str() → &str` @ `types.rs:180`.
No `new()`, no `from_uuid()`, no `Default` impl. `ExecutionIdParseError` has the three variants (`MissingTag`, `InvalidPartitionIndex`, `InvalidUuid`) spec'd in §12.2. All match the RFC.

`parse()` enforces `{fp:`, `}:`, u16, and UUID-v4 syntax. Does NOT bound-check `N < num_flow_partitions` — config is not passed. **Minor open question**: is this deliberate (accept any u16, deploy-level orphan partition tolerated) or an oversight? I read it as deliberate: the migration model is new-execs-only so post-shipping partitions can't shrink below a minted id's index; if operators bump down after shipping, orphan keys are an operational fact, not a correctness issue. No fix needed — flagging for awareness only.

### GREEN — §2.4 Partial-type refactor

8 refactored `FromFcallResult` impls enumerated in §3.1b, all present:
- execution.rs: 6 Partial types (Claim, Complete, Cancel, Delay, MoveToWaitingChildren, Expire)
- signal.rs: ClaimResumedExecutionResultPartial
- scheduling.rs: ChangePriorityResultPartial

`.complete()` on each is a total match over Partial variants.
Multi-variant totality explicitly tested on `ExpireExecutionResultPartial`
via `expire_partial_expired_variant_attaches_execution_id` +
`expire_partial_already_terminal_variant_ignores_execution_id`
(`execution.rs:699-719`). Future-variant breakage is compile-time.

CreateExecutionResult + flow.rs production parsers correctly identified
as forward-compat (no placeholder) and NOT refactored. Matches §3.1b.

### GREEN — §3.2 ff_add_execution_to_flow atomicity

NOT a phase-1 change — phase 3. Correctly deferred; phase 1 touches
only the exec_id shape (prerequisite for phase 3's co-location).

### GREEN — §4.1 solo partitioning

`crates/ff-core/src/partition.rs:126` — `solo_partition(lane, config)`
uses `crc16_ccitt(lane.as_str().as_bytes()) % config.num_flow_partitions`.
Matches RFC §4.1 exactly.

`ExecutionId::solo` at `types.rs:133` delegates to `solo_partition()`.
Consistent.

`solo_partition_determinism` + `solo_partition_different_lanes_usually_differ`
tests (`partition.rs:320-340`) assert behavior, not path.

### GREEN — §5.5 default bump

`PartitionConfig::default()` at `partition.rs:46-53` — `num_flow_partitions: 256`, `num_budget_partitions: 32`, `num_quota_partitions: 32`. Matches §5.5. No residual 64 default anywhere:

```
$ grep -rn "num_flow_partitions.*256" crates/
crates/ff-sdk/src/worker.rs:1403
crates/ff-server/src/config.rs:166
crates/ff-core/src/partition.rs:49
crates/ff-core/src/partition.rs:208  // assertion test
```

Consistent across ff-core/ff-sdk/ff-server.

### GREEN — §9.15 LaneId validation (D commit)

`crates/ff-core/src/types.rs:362-398`:
- `LaneId::new()` — infallible, for hardcoded names (123 pre-existing callers untouched per W2 commit log).
- `LaneId::try_new()` — validates: non-empty + ≤ 64 bytes + ASCII-printable (0x20-0x7e).
- `LaneIdError` enum with `Empty`, `TooLong`, `NonPrintable` variants.
- Custom `Deserialize` routes through `try_new` so ingress JSON is validated.

Only one ingress caller migrated to `try_new` (`ff-server/src/api.rs:3` list_executions handler). The commit correctly notes body-deserialized `CreateExecutionArgs.lane_id` auto-routes through `LaneId::Deserialize` and gets validated. Clean surgical fix.

Seven test names matching the validation boundary:
- `lane_id_try_new_accepts_valid`
- `lane_id_try_new_rejects_empty`
- `lane_id_try_new_rejects_too_long`
- `lane_id_try_new_rejects_non_ascii_printable`
- `lane_id_new_is_infallible_for_hardcoded_names`
- `lane_id_deserialize_accepts_valid`
- `lane_id_deserialize_rejects_malformed`

W2's commit claims "9 new tests" but I count 7 with `cargo test -p ff-core --lib lane_id`. Minor discrepancy — could be inside the same test functions asserting multiple classes (control char, multi-byte UTF-8, NUL, tab, DEL listed in commit but all asserted via `lane_id_try_new_rejects_non_ascii_printable`). Not a quality issue; just count drift.

### YELLOW 1 — `FF_EXEC_PARTITIONS` env-var removal undocumented

`ff-server/src/config.rs:166` replaces `env_u16_positive("FF_EXEC_PARTITIONS", 64)?` with `env_u16_positive("FF_FLOW_PARTITIONS", 256)?`. A deployment with `FF_EXEC_PARTITIONS=N` set in environment:
- Phase 1: silently ignored. No warning, no error.
- Behaviour: deployment boots with the default 256, NOT the operator-chosen N.

RFC §6.2 says "A single release commit bumps `ff-*` crate versions and flags the `ExecutionId` wire shape change in `CHANGELOG.md`." No explicit call-out that `FF_EXEC_PARTITIONS` silently stops working.

**Fix:** phase 5 runbook (`docs/runbooks/rfc-011-cutover.md`) must
document: "`FF_EXEC_PARTITIONS` is retired. Operators that set it
should rename to `FF_FLOW_PARTITIONS`." Alternatively, `ff-server`
could emit a loud `tracing::warn!` at boot if `FF_EXEC_PARTITIONS` is
set but not `FF_FLOW_PARTITIONS` — helps ops catch the drift at
first boot. Minor enough to leave to phase 5 as doc work; flagging
so it doesn't slip.

### YELLOW 2 — some PartitionFamily sites shouldn't revert on RED-1 fix

After W2's option-A fix restores `PartitionFamily::Execution` with
`prefix() = "fp"`, the 24 workspace sites that were changed
`Execution → Flow` in commit `0f5d180` each need a decision:

- **Revert** (most scanners, most server ops): site iterates over
  "executions" semantically — `lease_expiry.rs`, `attempt_timeout.rs`,
  `execution_deadline.rs`, `suspension_timeout.rs`, `retention_trimmer.rs`,
  `unblock.rs`, `delayed_promoter.rs`, `dependency_reconciler.rs`
  (when working per-exec), `pending_wp_expiry.rs`, `index_reconciler.rs`,
  `partition_router.rs`, ff-server's `execution_partition(eid)`
  wrappers.
- **Keep as Flow** (flow-level entry points): `flow_projector.rs` was
  already Flow, untouched.
- **Judgement call**: cairn-fabric's `boot.rs:204` is currently
  `PartitionFamily::Execution` — stays Execution. Any `ClaimGrant.partition`
  or inter-crate contract expecting "an execution's partition" should
  stay Execution.

The revert is ~20 of 24 sites (best estimate without line-by-line
audit). **W2's fix must audit each site**, not blanket-revert — some
may legitimately have needed Flow for iterator semantics (e.g. if a
scanner iterates flows not executions). Documented as a check-item
in the "pending fix" section below.

## Dimension 2 — Acceptance gates re-run

All per RFC §7.1:

```
$ cargo check -p ff-core       ✓ clean
$ cargo check -p ff-script     ✓ clean  (the RED-gate from round-1 review)
$ cargo check -p ff-scheduler  ✓ clean
$ cargo check -p ff-sdk        ✓ clean
$ cargo check -p ff-server     ✓ clean
$ cargo check --workspace      ✓ clean
$ cargo clippy --workspace -- -D warnings  ✓ clean (ferriskey excluded)
$ cargo test -p ff-core --lib   66/66 passed
$ cargo test -p ff-script --lib 30/30 passed
$ cargo test -p ff-sdk --lib    23/23 passed
```

All green. Clippy errors in ferriskey crate are pre-existing and
unrelated to this branch.

Phase-1 W2's claim: "5h actual" is plausible — scope was ~450 LOC net
(mostly test regen + Partial types) + 24 workspace call-site flips.

## Dimension 3 — Should-be-zero greps

W2's 5 sweep checks, independently verified:

| Grep target                                  | Expected | Actual |
|----------------------------------------------|:--------:|:------:|
| `num_execution_partitions` (non-RFC, non-comment) | 0        | **6** in ff-test/tests/e2e_lifecycle.rs |
| `PartitionFamily::Execution`                | 0        | **0**  |
| `impl Default for ExecutionId`              | 0        | **0**  |
| `ExecutionId::new` non-test-migration       | 0        | **0** (only 1 comment at fixtures.rs:113) |
| `ExecutionId::as_bytes`                     | 0        | **0**  |
| `ExecutionId::from_uuid`                    | 0        | **0**  |

The 6 `num_execution_partitions` hits in `ff-test/tests/e2e_lifecycle.rs` (L3729, 3730, 4908, 5100-5101, 5194-5195, 8185-8186) are two things:
1. `.arg("num_execution_partitions")` — string literal passed to `HSET ff:config:partitions`. Benign: ff-server reads only `num_flow_partitions` now; an extra unused config field in Valkey is harmless.
2. `.arg(config.num_execution_partitions.to_string())` — reads a field that no longer exists on `PartitionConfig`. These are **phase-2 compile errors** that cargo check surfaces (6 of them). Matches expected phase-2 scope.

No stowaways. GREEN.

## Dimension 4 — Partial-refactor correctness

Spot-checked each Partial type:

- `ClaimExecutionResultPartial` single-variant (Claimed) — correct.
- `CompleteExecutionResultPartial` single-variant (Completed) — correct.
- `CancelExecutionResultPartial` single-variant (Cancelled) — correct.
- `DelayExecutionResultPartial` single-variant (Delayed) — correct.
- `MoveToWaitingChildrenResultPartial` single-variant (Moved) — correct.
- `ExpireExecutionResultPartial` **multi-variant** (Expired + AlreadyTerminal) — the canonical multi-variant test case from §2.4 addendum. `.complete()` attaches exec_id ONLY on `Expired`; `AlreadyTerminal` passes through unchanged. Total match over variants ensures compile-time exhaustiveness on future variant additions.
- `ClaimResumedExecutionResultPartial` single-variant (Claimed) — correct.
- `ChangePriorityResultPartial` single-variant (Updated) — correct.

No Partial carries an `execution_id` field. `.complete(exec_id)` on each is the only path to construct the non-Partial form. Invariant "exec_id always populated" is now type-enforced. GREEN per §2.4 intent.

## Dimension 5 — Test meaningfulness

**GREEN across the board.** Signature-check pattern from past reviews
finds zero "tests that just match paths without asserting anything":

- `execution_partition_reads_hash_tag_not_uuid` (`partition.rs:248`):
  constructs an ExecutionId with KNOWN partition 0 but a UUID whose
  crc16 lands elsewhere. Asserts `p.index == 0`. This would FAIL if
  the old "rehash UUID" behaviour leaked through. Strong test.
- `execution_partition_ignores_config_value` (`partition.rs:261`):
  mints with `num_flow_partitions=4` then decodes with `1024`. Asserts
  partitions match. Catches any config-dependent decoding regression.
- `execution_id_parse_rejects_bare_uuid` (`partition.rs:281`):
  Valkey wire-shape migration guard. Returns `MissingTag`.
- 9 Partial tests at `execution.rs:652-719`: each asserts the
  **value** attached by `.complete()` via match-arm field
  extraction. Not `.is_ok()` checks — actual equality assertions.
- LaneId validation tests: each asserts the specific error variant
  (not just `.is_err()`), e.g. `rejects_non_ascii_printable` checks
  `LaneIdError::NonPrintable` kind.

No rubber-stamp tests. GREEN.

## Dimension 6 — Expected phase-2 errors sanity check

Independent count of compile errors on `cargo check --workspace --all-targets --exclude ferriskey`:

- 166 `no function or associated item named ``new`` found for struct ``ExecutionId``` errors — matches W2's claimed 166 ExecutionId::new migrations in ff-test.
- 6 `no field ``num_execution_partitions`` on type ``PartitionConfig``` errors — the 6 sites in e2e_lifecycle.rs identified above.
- 1 E0308 (mismatched types) — downstream type mismatch after ExecutionId change; expected.

Total: 166 + 6 + 1 = 173. W2 claimed ~172. Within 1 of the estimate.

The 166 ff-test migrations are all `TestCluster::new_execution_id` test-code calls AND direct `ExecutionId::new()` calls that haven't been updated because they're in the test target graph (not the lib graph that phase 1 rebuilds). All expected per §7.2 phase-2 scope. No stowaways (e.g. unrelated errors indicating unexpected consequences).

**Compile-error pattern is clean.** W2's "172 errors expected in phase 2" claim verified within rounding.

## Pending fix — PartitionFamily::Execution restore verification

When W2 pushes the option-A fix, verify:

1. **Variant restored:** `pub enum PartitionFamily { Flow, Execution, Budget, Quota }` at `crates/ff-core/src/partition.rs:12`.

2. **Routing equivalence:** `prefix()` returns `"fp"` for BOTH `Flow` and `Execution`. Document explicitly in the variant doc or adjacent.

3. **Call-site audit:** of the 24 sites changed in `0f5d180`, each revisited:
   - **Scanners iterating executions:** revert to `::Execution`. Sites: `lease_expiry`, `attempt_timeout`, `execution_deadline`, `suspension_timeout`, `retention_trimmer`, `unblock`, `delayed_promoter`, `pending_wp_expiry`, `index_reconciler`, `dependency_reconciler` (per-exec path).
   - **Partition router (`partition_router.rs`):** revert to `::Execution` — it's the exec-partition path.
   - **ff-server exec-id wrappers:** revert at each `execution_partition(eid)` result construction site.
   - **ff-scheduler `claim.rs`:** if iterating exec partitions, revert; if iterating flow partitions, stay Flow.
   - **ff-sdk `worker.rs`:** same logic per site.
   - **ff-test `fixtures.rs`, `admin_rotate_api.rs`, `cap_routed.rs`:** likely revert, as they construct exec-id partitions.
   - **Legitimately Flow:** `flow_projector.rs` (was already Flow, unchanged) — no action.

4. **New test:** `partition_family_execution_and_flow_route_to_same_prefix` asserting `PartitionFamily::Execution.prefix() == PartitionFamily::Flow.prefix()`. Prevents accidental divergence.

5. **cairn-fabric compile compat:** after the fix, `cargo check` against cairn's boot.rs:204 should pass (can't verify without pulling cairn into the workspace, but the variant restoration is the load-bearing piece).

Will re-verify these 5 points once W2 pushes; then shift this review to ACCEPT.

## Summary

1 RED (flagged + routed), 2 YELLOW (minor documentation and
post-fix-audit items), else GREEN. Design + implementation match RFC
§2.3/§2.4/§3.1b/§4.1/§5.5/§9.15 precisely.

The RED is a missed non-goal clause, not a design-level flaw —
straightforward to fix with option A (restore variant + prefix routing
equivalence). All acceptance gates green; 5 should-be-zero greps pass;
9 Partial tests assert actual behaviour not path-matches; 172 expected
phase-2 compile errors on-count.

Independent of W1. Transcript will note cross-review mechanics worked:
the §11 non-goal catch is a clean win for "one independent reviewer
with fresh eyes" — W2's plan flagged option (b) and neither W2 nor
manager cross-checked the existing §11 language before approval;
independent grep of cairn-fabric caught it at review time, before
phase-4 merge would have surfaced it the expensive way.
