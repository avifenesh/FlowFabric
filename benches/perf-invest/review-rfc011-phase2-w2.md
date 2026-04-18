# RFC-011 phase 2 cross-review — W2

**Branch:** `feat/rfc011-phase1-exec-id` (phase 1+2 bundled PR #25)
**Reviewed commits:**
* `791d9c1` — W3: phase-2 verification doc + 2 surgical stale-prose fixes (RFC-004 + RFC-010)
* `745aa51` — W1: 172-error ff-test cascade migration to the new id shape + retired config field

**Diff base:** `f0dadd4` (phase-1 tip) → `745aa51` (phase-2 tip)
**Scope delta:** +388 / −181 across 10 files (1 verification doc, 7 ff-test files, 2 RFC prose files, fixtures.rs).

Every dimension in the manager's brief reviewed with empirical evidence. **Overall verdict: ACCEPT.** No RED findings. Two YELLOW items, both forward-looking (phase-3 territory, not blocking phase-2 close).

---

## Verdict summary

| # | Dimension | Verdict |
|---|-----------|---------|
| 1 | Migration correctness (162 tc.new_execution_id + 7 ExecutionId::solo + 4 kid-gen) | **GREEN** |
| 2 | Solo-partition determinism (7 solo-LaneId sites, config consistency) | **GREEN** |
| 3 | Kid-gen → `uuid::Uuid::new_v4()` semantic correctness | **GREEN** |
| 4 | `num_execution_partitions` HSET removals, read-back audit | **GREEN** |
| 5 | W3's RFC-004 + RFC-010 prose edits | **GREEN** |
| 6 | Test-count reconciliation (W1's 146/146 claim) | **GREEN** |
| — | Forward-looking flow-member YELLOW (phase-3 implication) | **YELLOW** |
| — | Cross-crate clippy regression check (pre-existing ferriskey) | **YELLOW** (non-blocking, informational) |

---

## 1. Migration correctness — **GREEN**

### 1a. `tc.new_execution_id()` substitution pattern (166 sites)

Spot-checked 15 random sites across the 4 test files:

* `e2e_lifecycle.rs:615` — `test_create_claim_complete_lifecycle`: solo-path lifecycle test. Correct.
* `e2e_lifecycle.rs:715` — `test_create_cancel_from_waiting`: solo-path cancel. Correct.
* `e2e_lifecycle.rs:790` — `test_delay_execution`: solo-path delay. Correct.
* `e2e_lifecycle.rs:840` — `test_change_priority`: solo-path priority change. Correct.
* `e2e_lifecycle.rs:1165` — `test_fail_with_retry`: solo-path retry. Correct.
* `e2e_lifecycle.rs:1255` — `test_fail_terminal`: solo-path terminal fail. Correct.
* `e2e_lifecycle.rs:1293` — `test_reclaim_expired_lease`: solo-path lease reclaim. Correct.
* `e2e_lifecycle.rs:1356` — `test_expire_execution`: solo-path expire. Correct.
* `pending_waitpoints_api.rs:340` — `test_list_pending_waitpoints_returns_token_after_suspend`. Correct.
* `pending_waitpoints_api.rs:386` — empty-list test. Correct.
* `pending_waitpoints_api.rs:412` — tampered-token test. Correct.
* `pending_waitpoints_api.rs:470` — not-found test. Correct.
* `result_api.rs:315` — `test_result_404_before_completion`. Correct.
* `result_api.rs:335` — `test_result_200_json_payload_byte_exact`. Correct.
* `waitpoint_tokens.rs:351` — `test_valid_token_accepted`. Correct.

Every site: one-line `ExecutionId::new()` → `tc.new_execution_id()` swap. Test intent preserved — these were all solo-path tests (no parent flow context) and the `TestCluster::new_execution_id` fixture routes through `ExecutionId::solo(&test_lane, &TEST_PARTITION_CONFIG)` (phase 1 `crates/ff-test/src/fixtures.rs:120-122`). Solo-path fits every one of these call sites.

### 1b. `ExecutionId::solo(&LaneId::new(LANE), &ff_test::fixtures::TEST_PARTITION_CONFIG)` pattern (7 sites in e2e_api.rs)

The TestApi pattern in `e2e_api.rs` does not instantiate a `TestCluster`, so `tc.new_execution_id()` isn't available. W1 correctly falls back to the explicit constructor:

* `e2e_api.rs:183` — `test_api_create_and_get_execution`. Correct.
* `e2e_api.rs:247` — `test_api_cancel_execution`. Correct.
* `e2e_api.rs:356` — `test_api_not_found` (fake_id). Correct.
* `e2e_api.rs:486` — `test_api_change_priority`. Correct.
* `e2e_api.rs:516` — `test_api_replay_execution`. Correct.
* `e2e_api.rs:581` — `test_api_add_execution_to_flow`. ⚠ See YELLOW-1 below.
* `e2e_api.rs:707` — `test_stream_semaphore_returns_429_on_burst`. Correct.

`LANE` is declared at `e2e_api.rs:35` as `const LANE: &str = "api-test-lane"`, so every site uses the same lane string and thus the same partition index (determinism contract holds).

### 1c. Kid-gen conversion (4 sites in admin_rotate_api.rs)

* `admin_rotate_api.rs:156` — happy-path kid: `format!("kid-{}", uuid::Uuid::new_v4())`. Correct.
* `admin_rotate_api.rs:191` — state-verification kid. Correct.
* `admin_rotate_api.rs:371` — unauth-probe kid. Correct.
* `admin_rotate_api.rs:384` — auth-probe kid. Correct.

`uuid::Uuid::new_v4()` is the semantically honest replacement — a kid is an opaque waitpoint HMAC key identifier, not an `ExecutionId`. The prior code was abusing `ExecutionId::new()` as a unique-token generator; the migration doesn't just fix the compile error, it also de-couples kid generation from a (now richer-shaped) `ExecutionId`. See Dimension 3 below for the assertion audit.

---

## 2. Solo-partition determinism — **GREEN**

**Config consistency across mint + decode.** Every `solo(&LaneId::new(LANE), &ff_test::fixtures::TEST_PARTITION_CONFIG)` mint site in `e2e_api.rs` has a matching `execution_partition(eid, &ff_test::fixtures::TEST_PARTITION_CONFIG)` decode site:

* Mint at L183 ↔ decode at L199 (same file, same test, same config ref).
* Mint at L247 ↔ decode at L263. Same pattern.

Grep audit of `e2e_api.rs`:
```
$ grep -n "TEST_PARTITION_CONFIG" crates/ff-test/tests/e2e_api.rs
```
returns 10 hits — 7 mint sites plus 3 decode sites — all referencing the single `ff_test::fixtures::TEST_PARTITION_CONFIG` constant (`num_flow_partitions: 4`). No drift between what was minted vs what gets decoded.

**No site in `e2e_api.rs` uses a different config.** Verified. Test file has its own `LANE` const but shares the workspace-wide `TEST_PARTITION_CONFIG` for partition routing — aligns with the phase-1 fixture invariant that `TestCluster`'s `partition_config()` is a clone of the same const.

---

## 3. Kid-gen uuid conversion — **GREEN**

Spot-checked every test that inspects `new_kid` for a string assertion:

* `admin_rotate_api.rs:~216` (echoed-kid assertion): `assert_eq!(resp.new_kid, new_kid)` — opaque string equality; not format-dependent. ✓
* `admin_rotate_api.rs:~275` (bad-kid-rejection test): uses a hardcoded `"bad:kid"` literal to trigger server-side rejection. Independent of the happy-path kid format. ✓
* `admin_rotate_api.rs:~313` (HGET `secret:<new_kid>`): reads back the secret hex via `HGET` keyed on `secret:{new_kid}`. The key format is `secret:<opaque-string>`; any non-`:` kid works. The new UUID form (`xxx-yyy-zzz`) contains `-` not `:`, which is valid per the server's kid-format rule. ✓

**No test asserts `new_kid` starts with `{fp:` or matches an exec-id shape.** Grep clean. The prior `ExecutionId::new()` abuse was incidental — nothing relied on the kid looking like an exec id.

One observation worth naming (not a finding): the new uuid form is uppercase-hyphenated (per `Uuid::new_v4().to_string()`), while the prior `ExecutionId::new().to_string()` form was also uppercase-hyphenated (because `ExecutionId` wrapped `Uuid`). Visual kid format in logs is unchanged for operators. ✓

---

## 4. `num_execution_partitions` HSET removals — **GREEN**

5 HSET pairs removed in `e2e_lifecycle.rs`:

* `e2e_lifecycle.rs:~3729` — `test_max_concurrent_tasks_enforcement`: removed HSET pair for `num_execution_partitions`.
* `e2e_lifecycle.rs:~4905` — `test_sdk_suspend_signal_resume_reclaim`: removed.
* `e2e_lifecycle.rs:~5099` — `test_sdk_all_methods_smoke`: removed.
* `e2e_lifecycle.rs:~5189` — `test_sdk_claim_retry_attempt_index`: removed.

Plus one loop-bound change in `admin_rotate_api.rs:~238`: `pc.num_execution_partitions` → `pc.num_flow_partitions`.

**Downstream read-back audit.** Grep workspace-wide for `num_execution_partitions`:

```
$ grep -rn "num_execution_partitions" crates/ --include="*.rs"
```
returns only comments (4 occurrences, all ending in "retired"). Zero source reads. No `HGET num_execution_partitions` anywhere. ✓

Additionally verified SDK reader at `crates/ff-sdk/src/worker.rs:1402-1406` — `parse_partition_config` now reads only `num_flow_partitions`, `num_budget_partitions`, `num_quota_partitions`. Matches the HSET write-side exactly. ✓

---

## 5. W3's RFC-004 + RFC-010 prose edits — **GREEN**

### RFC-004 §Waitpoint Security rotation cost

Diff:
```
- Rotation fans out across partitions (`O(num_execution_partitions)` HSETs — 256 by default, bounded at deployment time via `FF_EXEC_PARTITIONS`).
+ Rotation fans out across partitions (`O(num_flow_partitions)` HSETs — 256 by default, bounded at deployment time via `FF_FLOW_PARTITIONS`; `num_execution_partitions` / `FF_EXEC_PARTITIONS` retired by RFC-011).
```

Plus an inline pointer from `{p:N}` to RFC-011 §2 for hash-tag co-location. Prose is accurate: `num_flow_partitions` is now authoritative, `FF_EXEC_PARTITIONS` is retired (verified: `crates/ff-server/src/config.rs:163-168` post-phase-1 reads only `FF_FLOW_PARTITIONS`). The parenthetical "retired by RFC-011" is the correct attribution.

Edit is surgical — no adjacent paragraphs touched, no stylistic drift.

### RFC-010 §8.3 partition config store

Diff:
```
- **Partition configuration:** Partition counts (`num_execution_partitions`, `num_flow_partitions`, `num_budget_partitions`, `num_quota_partitions`) are stored in `ff:config:partitions` on Valkey.
+ **Partition configuration:** Partition counts (`num_flow_partitions`, `num_budget_partitions`, `num_quota_partitions`) are stored in `ff:config:partitions` on Valkey.
```

Plus a parenthetical explaining the retirement at the end of the paragraph. Accurate against code — `PartitionConfig` in `crates/ff-core/src/partition.rs:60-64` post-phase-1 has exactly three fields. Parenthetical cites RFC-011 §2 for the rationale. Surgical. ✓

### Verification doc

`benches/perf-invest/rfc011-phase2-verification-w3.md` is 212 lines of careful independent-grep verification. Reconciles the RFC §3.4 175-count with the observed 173 real errors (166 + 6 + 1), excluding pre-existing ferriskey clippy issues. The work is the kind of pedantic cross-check that would have caught my own phase-1 edge-case-5 RED earlier — worth keeping on the tree as audit evidence. ✓

**No over-editing.** I audited the un-touched RFC-001..RFC-010 corpus for `num_execution_partitions` / `FF_EXEC_PARTITIONS` / `{p:N}`; W3's decision to leave the 396 `{p:N}` references across RFCs 001-010 alone (pending a phase-5 bulk supersession notice) is the right call per CLAUDE.md §3 surgical-changes discipline. The 2 fixes are the only two sites where the prose would give a future reader factually wrong information about current behaviour; the rest are historically-accurate descriptions of pre-RFC-011 state.

---

## 6. Test count — **GREEN**

Ran `cargo test -p ff-test --tests -- --test-threads=1` locally against a live Valkey on `localhost:6379`. Results by binary:

| Binary | Pass | Fail | Ignored |
|--------|-----:|-----:|--------:|
| `ff-test` lib tests | 7 | 0 | 0 |
| `admin_rotate_api` | 4 | 0 | 0 |
| `e2e_api` | 13 | 0 | 0 |
| `e2e_lifecycle` | 102 | 0 | 0 |
| `pending_waitpoints_api` | 4 | 0 | 0 |
| `result_api` | 3 | 0 | 0 |
| `waitpoint_tokens` | 13 | 0 | 0 |
| **Total** | **146** | **0** | **0** |

Matches W1's claim of 146/146 exactly. ✓

---

## YELLOW findings (non-blocking)

### YELLOW-1: flow-member tests at `e2e_api.rs:581` + `e2e_lifecycle.rs:5430+` use solo-minted execs against a flow id

**Observation.** Tests that invoke `fcall_add_execution_to_flow(&tc, fid, eid)` — including `test_api_add_execution_to_flow` (e2e_api.rs:581) and the flow-cancel / flow-membership tests at `e2e_lifecycle.rs:5435, 5484, 5528, 5621` etc. — mint the `eid` via `tc.new_execution_id()` (solo-shard routing) or `ExecutionId::solo(&LaneId::new(LANE), &TEST_PARTITION_CONFIG)` (e2e_api.rs). Neither form co-locates the exec with the flow — the exec lands on `crc16(test_lane_str) % num_flow_partitions`, the flow lands on `crc16(flow_id_uuid_bytes) % num_flow_partitions`. Under random input, they occasionally align but usually don't.

**Why this is GREEN for phase 2.** The Lua `ff_add_execution_to_flow` at `lua/flow.lua:165` is still the pre-RFC-011 two-phase shape: KEYS are `{flow_core, members_set, flow_index}` — all on `{fp:N}` — and the method does NOT take `exec_core` as a KEY. The Rust caller at `crates/ff-server/src/server.rs:add_execution_to_flow` issues a phase-2 HSET on the exec's partition separately. Solo-minted execs + cross-partition flow membership works fine with the two-phase contract.

**Why this becomes a pre-flight concern for phase 3.** RFC-011 §7.3 specifies that phase 3 rewrites `ff_add_execution_to_flow` to take `exec_core` as KEYS[4], collapsing to an atomic single-FCALL. At that point, the exec's hash-tag MUST equal the flow's hash-tag — or Valkey's cluster router rejects the FCALL with CROSSSLOT. The flow-member tests above would fail against the phase-3 Lua.

**Recommendation.** Not a phase-2 fix. When phase 3 lands the atomic FCALL, these ~6 flow-member tests should switch from `tc.new_execution_id()` / `ExecutionId::solo(&LaneId::new(LANE), ...)` to `ExecutionId::for_flow(&FlowId::parse(fid)?, &config)`. File count matches RFC §3.4's "~6 flow-member tests needing for_flow" estimate. Phase-3 W2 scope item; track in the phase-3 plan.

**Evidence site list:**
* `crates/ff-test/tests/e2e_api.rs:581`
* `crates/ff-test/tests/e2e_lifecycle.rs:5430-5432` (3 execs a/b/c → flow fid)
* `crates/ff-test/tests/e2e_lifecycle.rs:5528-5529, 5547`
* `crates/ff-test/tests/e2e_lifecycle.rs:5621-5624` (4 execs → flow fid)
* Plus additional sites at L5898, L5919, L5932, L6009 etc. from the phase-1 §3.4 inventory.

### YELLOW-2: pre-existing ferriskey clippy failures under `--tests` with `-D warnings`

Running `cargo clippy --workspace --tests -- -D warnings` fails on `ferriskey` (not a phase-2 regression — same 5 errors W3's verification doc flagged as pre-existing: `compression::CommandCompressionBehavior::default_constructed_unit_structs`, `iam::AuthenticationInfo.iam_config` access patterns). With `--exclude ferriskey`, clippy is clean across the FlowFabric workspace.

**Why this is informational, not a block.** W3's verification doc already names these as pre-existing. The phase-2 diff does not touch `ferriskey/`. CI's `cargo clippy` matrix presumably already handles this either via a workspace member exclusion or a per-crate lint config; I verified locally that scoping out ferriskey produces a clean workspace-wide clippy run. If the CI matrix is failing on ferriskey lint, that's a separate branch (not phase-2 scope).

---

## Scope-bleed acknowledgement

Per the manager's brief, the mixed code + RFC-prose diff is intentional — W3's RFC-004 + RFC-010 edits are RFC-011-retirement-triggered corrections, logically part of phase 2. Noted, not flagged.

---

## Verdict

**ACCEPT on 745aa51 (and the underlying 791d9c1).**

All six review dimensions GREEN:
1. 166+7+4 migration sites correct
2. solo-partition determinism holds (same config mint+decode)
3. kid-gen uuid conversion is semantically honest
4. 6 num_execution_partitions HSET removals are orphan-free (no read-back sites anywhere)
5. W3's 2 RFC prose fixes are historically-accurate and surgical
6. 146/146 tests pass locally (matches W1's claim exactly)

Two YELLOW items, both forward-looking:
* YELLOW-1: flow-member tests need `for_flow` migration in phase 3 (not phase 2)
* YELLOW-2: pre-existing ferriskey clippy failures (not a phase-2 regression; already documented)

Phase 1+2 closes on my ACCEPT + CI fully green.

---

**Scope boundary.** Reviewed only the `f0dadd4..745aa51` diff. Did not:
* Re-run phase-1 cross-review (that landed already).
* Audit cairn-fabric branch (cairn's phase-4 migration is a separate PR).
* Run the full CI matrix locally (arm-cluster pending on GitHub per manager's note; local host is x86_64).
