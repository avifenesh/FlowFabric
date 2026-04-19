# Cross-review — RFC-011 phase 3 (W3)

Reviewer: Worker-3. Branch: `feat/rfc011-phase3-atomic`. Single
commit `c1e8678` on top of `60c9b78`. Independent of W1's review.

## Overall: **ACCEPT with 2 YELLOW**

Phase 3 closes the §5.5 atomicity gap. Single atomic FCALL; 5
strong behavioral tests; reader-site invariants rewritten from
"safe-against-orphan" to "true-by-construction"; issue #21 closed on
GitHub; §4 gap-report LANDED block + §5 table flipped to CLOSED. No
REDs.

Two YELLOWs, both documentation/UX nits (neither blocks merge):

1. Already-member branch defensive HSET (W2 self-flag #1) — scope
   widening vs strict RFC §7.3 idempotency invariant; needs text
   amendment.
2. No pre-flight `exec_partition == flow_partition` check in
   `add_execution_to_flow` — wrong-partition callers hit raw
   `CROSSSLOT` instead of typed `PartitionMismatch`. Additive ~5 LOC
   polish.

## Dimension 1 — acceptance gates (trust-but-verify)

All green on local machine:

```
$ cargo check --workspace --all-targets --exclude ferriskey    ✓ clean
$ cargo clippy --workspace --exclude ferriskey -- -D warnings  ✓ clean
$ cargo test -p ff-core --lib       70/70
$ cargo test -p ff-script --lib     30/30
$ cargo test -p ff-sdk --lib        23/23
$ cargo test -p ff-server --lib     5/5
$ cargo test -p ff-test --tests -- --test-threads=1    151/151
  - e2e_flow_atomicity: 5/5 ← NEW (phase 3 contribution)
  - e2e_lifecycle: 102/102
  - waitpoint_tokens: 13/13
  - e2e_api: 23/23 (was 16)
  - admin_rotate_api: 5/5
  - pending_waitpoints_api: 4/4
  - result_api: 3/3
```

`ferriskey` test-compile errors excluded (pre-existing unrelated —
`compression::CommandCompressionBehavior::description`,
`AuthenticationInfo.iam_config`).

W2's 151/151 claim verified on my machine, 1.95 rustc. Matches.

## Dimension 2 — grep assertions

| Grep                                                              | Expected | Actual |
|-------------------------------------------------------------------|:--------:|:------:|
| `hset.*exec_core.*flow_id` in `crates/ff-server`                  | 0        | **0**  |
| `HSET.*exec_core.*flow_id` in `crates/ff-server`                  | 0        | **0**  |
| `ExecutionId::new(` in code (excluding docstrings + review docs)  | 0        | **0**  |
| `num_execution_partitions` in code (excluding retirement annotations) | 0    | **0**  |
| `HSET flow_id` outside FCALL at `crates/ff-server/src/server.rs`  | 0        | **0**  |
| `e2e_lifecycle.rs` `num_execution_partitions` hits (phase-2 cleanup) | 0     | **0** (matches phase-2 merge) |

All 6 "should-be-zero" greps pass. Phase-2 HSET at server.rs:1279 is
deleted; phase-3 only adds exec_core as KEYS[4] to the atomic FCALL.

## Dimension 3 — test meaningfulness (signature catch)

Each of the 5 new `crates/ff-test/tests/e2e_flow_atomicity.rs` tests
asserts Valkey-state values, not path-matches. If any atomic invariant
was silently broken by an impl change, the tests would fail loudly.

### Test 1 — `test_atomicity_happy_path_commits_both_writes` (L156-187)

- Pre-call: asserts both `hget_flow_id(&eid) == None` AND
  `sismember_flow == false` (both writes absent).
- Call returns Ok.
- Post-call: asserts both `hget_flow_id(&eid) == Some(flow_id)` AND
  `sismember_flow == true`.

Both writes pinned. A impl that forgot the HSET (like the pre-RFC-011
version would, if re-introduced) fails the first post-call assertion.

### Test 2 — `test_atomicity_flow_not_found_commits_nothing` (L189-223)

This is manager's key test. Verified it actually proves nothing was
written:

- Creates exec_core but NOT flow_core.
- Calls `add_execution_to_flow` → expects `flow_not_found` error.
- Post-call: asserts `hget_flow_id(&eid) == None` AND
  `sismember_flow(flow_id, eid) == false`.

Manager's concern: "could a concurrent writer pollute the state it
reads back?" Answer: **no.** `#[serial_test::serial]` annotation +
test cleanup ensures no concurrent writer on the same keyspace. Even
without serial, each test reads back the specific `(eid, flow_id)`
it minted — uuid-scoped, no cross-contamination possible. If the
impl accidentally wrote before the guard, the HGET would return the
mistaken value.

Validates-before-writing discipline is pinned at the Valkey-state
level, not at the path-match level.

### Test 3 — `test_atomicity_repeat_call_is_idempotent` (L225-272)

Pre- + post-condition asserts `node_count == "1"` (not `>=1`). Catches
double-increment on retry — the specific bug a naively-implemented
Lua function would introduce if it skipped the members-set SISMEMBER
guard.

**YELLOW 1 interaction:** this test DOES exercise the already-member
branch (second call hits it), but the assertion `hget_flow_id(&eid)
== Some(flow_id)` passes whether the defensive HSET at `lua/flow.lua:182`
runs or not, because the first call already stamped the correct value.
Test does NOT catch a pathological "already-member path silently
rewrites to a DIFFERENT flow_id" scenario. See YELLOW 1 below.

### Test 4 — `test_atomicity_concurrent_distinct_execs` (L274-340)

Two `tokio::task`s call `add_execution_to_flow` on the same flow with
distinct execs concurrently. Assertions:
- Both execs are in members_set.
- Both exec_cores carry flow_id.
- `node_count == "2"` exactly (no lost update).

Catches lost-update via non-atomic increment. HINCRBY is atomic but
a pre-atomic version that did SADD + HINCRBY across separate calls
could race.

### Test 5 — `test_atomicity_flow_id_visible_to_subsequent_reads` (L342-end)

Asserts that immediately after a successful call, 3 sequential reads
all see the stamped `flow_id`. Pins the "no visibility lag" property.

### Dimension 3 verdict

All 5 tests behavioral, none path-match only. Manager's "could
pass with non-atomic impl" check satisfies. GREEN.

## Dimension 4 — `lua/flow.lua` docstring rewrite

`lua/flow.lua:114-136` entirely rewritten. Old state:
```
TWO-PHASE CONTRACT (cairn P3.6 §5.5)
exec_core lives on {p:N} ... this FCALL runs on {fp:N} ... we CANNOT
touch exec_core ... The caller (ff-server `add_execution_to_flow`) ...
runs this FCALL first and then issues an `HSET exec_core flow_id` as
a separate command. Orphan window: ... Reader safety: ... scanner ...
```

New state (L114-136):
```
ff_add_execution_to_flow  (on {fp:N} — single atomic FCALL)
Add a member execution to a flow AND stamp the flow_id back-pointer
on exec_core in one atomic commit. Per RFC-011 §7.3, exec keys
co-locate with their parent flow's partition under hash-tag routing,
so exec_core shares the `{fp:N}` hash-tag with flow_core / members_set
/ flow_index. All four KEYS hash to the same slot; no CROSSSLOT.

Validates-before-writing: flow_not_found / flow_already_terminal
early-returns fire BEFORE any write ...

Invariant (post-RFC-011): a successful call commits BOTH the flow-
index updates AND the exec_core.flow_id stamp in one atomic unit.
Readers can assume exec_core.flow_id == flow_id iff the exec is in
members_set. The pre-RFC-011 two-phase contract + §5.5 orphan-window
+ issue #21 reconciliation-scanner plan are all superseded.
```

Zero "two-phase" / "orphan-window" / "reconciliation-scanner" prose
remaining as CURRENT-REALITY CLAIMS. All mentions tag as "pre-RFC-011
... retired" or "all superseded." Surgical, matches surgical-changes
discipline. GREEN.

## Dimension 5 — reader-invariant rewrites

`describe_execution` (server.rs ~:1895) and `replay_execution`
(server.rs ~:2080) reader comments rewritten:

Old: "Treat empty as 'no flow affinity known at read time' — safe for
this reader ... See `add_execution_to_flow` rustdoc for the
orphan-direction and reader-safety catalogue."

New: "Reader invariant (RFC-011 §7.3): `flow_id` on exec_core is
stamped atomically with membership by `add_execution_to_flow`'s single
FCALL. Empty iff the exec has no flow affinity (solo-path
create_execution — never added to a flow)."

**Post-atomic the invariant is TRUE BY CONSTRUCTION** — empty flow_id
means solo exec, full stop. No orphan-possibility language retained
as live. "is_skipped_flow_member" branch at replay_execution gates on
`!flow_id_str.is_empty()`, so solo execs correctly fall back. Matches
"true by construction, not half-measure" standard. GREEN.

## Dimension 6 — gap-report §4 "LANDED" + §5 CLOSED row

`benches/perf-invest/bridge-event-gap-report.md`:

- §4 LANDED block added (L334-349): describes the phase-3 shape,
  mentions exec_core as KEYS[4], notes validates-before-writing Lua
  discipline, cites `crates/ff-test/tests/e2e_flow_atomicity.rs` for
  the atomicity tests, and states "Issue #21 is closed as superseded."

- §5 row for §5.5 flipped from `YES (atomicity, not emit)` +
  description to `**CLOSED** (landed phase 3)` + new description
  pointing at RFC-011 §7.3 + e2e_flow_atomicity.rs. No intermediate
  state left in the row.

Both updates match expectation. GREEN.

## Dimension 7 — Issue #21 closure text

Checked GitHub: `gh issue view 21 --json state,title` returns
`{"state":"CLOSED","title":"crash-recovery scanner for flow_id / flow
membership consistency (cairn P3.6 §5.5)"}`. W2 already closed it on
the remote — no dangling-ref.

Citation pattern in docs: "issue #21, now superseded" / "issue #21 is
closed as superseded" — declarative, doesn't depend on specific merge
SHA. Survives whenever phase 3 merges; reader can `gh issue view 21`
for context. W2 handled this cleanly. GREEN.

## YELLOW 1 — already-member branch defensive HSET (W2 self-flag #1)

**Site:** `lua/flow.lua:181-185`.

```lua
if redis.call("SISMEMBER", K.members_set, A.execution_id) == 1 then
    redis.call("HSET", K.exec_core, "flow_id", A.flow_id)
    local nc = redis.call("HGET", K.flow_core, "node_count") or "0"
    return ok_already_satisfied(A.execution_id, nc)
end
```

The HSET at line 182 runs even when the exec is already a member.
W2's docstring calls it "defensive heal for pre-phase-3 orphans during
rolling upgrade."

**RFC §7.3.1 Test 3 spec:**
```
1. Successful add_execution_to_flow (test 1).
2. Second call with same (flow_id, eid) → OK_ALREADY_SATISFIED.
3. Assert exec_core.flow_id unchanged; node_count still "1".
```

RFC says "exec_core.flow_id **unchanged**" on idempotent retry. W2's
impl always HSETs `flow_id = A.flow_id` on that branch. **The
observable post-state is identical** (same value written, so the
"unchanged" assertion still holds) — test 3 passes. But the strict
RFC invariant is "no write, not just same value." Scope widening.

**Latent correctness risk:** if a corrupted state had `exec.flow_id =
flow_B` with `exec in flow_A.members_set` (not reachable via
`add_execution_to_flow` under normal use — would require manual
Valkey tampering or a cairn-side bug adding the same exec to two
flows), the defensive HSET **silently rewrites** from B to A. No
warning, no error. Masks a real bug.

Naturally-reachable? Only via cross-flow membership. Cairn currently
maintains exec-belongs-to-exactly-one-flow invariant in
`RunService::create_run`/`TaskService::create_task` — no code path
adds the same exec to two flows. So the risk is hypothetical under
current consumers.

**Disposition:** YELLOW — accept the defensive heal as deliberate
rolling-upgrade safety, BUT:

1. **RFC §7.3 text amendment needed.** Update §7.3.1 Test 3 spec to
   reflect "same-value rewrite is acceptable under the defensive-heal
   contract; cross-flow rewrite is never reached via normal
   consumer paths." Make the scope widening explicit so future
   maintainers don't treat line 182 as pure idempotency.

2. **Optional defensive guard:** add an `if old_flow_id ~= "" and
   old_flow_id ~= A.flow_id then error_reply("cross_flow_membership")
   end` before line 182. Catches the hypothetical cross-flow
   corruption loudly. ~2 LOC. Not blocking; file a follow-up if any
   consumer hits it.

W2 self-flagged this honestly; the YELLOW is on the spec-text side,
not the impl.

## YELLOW 2 — no pre-flight exec_partition == flow_partition check

**Site:** `crates/ff-server/src/server.rs:1279-1317`
(`add_execution_to_flow` body).

Method computes `flow_partition(args.flow_id)` and
`execution_partition(args.execution_id)` independently, threads both
partitions into KEYS, but never asserts they're equal. If a caller
passes a solo-minted exec or a for_flow-minted-against-a-different-flow
id, the resulting KEYS span slots → Valkey returns raw `CROSSSLOT`
error → bubbles up as `ServerError::ValkeyContext(CrossSlot...)`.

Not a correctness issue — the op fails safely. But the error surface
is raw-Valkey instead of a typed `ServerError::PartitionMismatch` with
a clear diagnostic ("execution_id was minted for partition X but
flow_id hashes to partition Y; use ExecutionId::for_flow(&flow_id)
at the minting site").

**Disposition:** YELLOW — additive ~5 LOC polish. Insert before the
FCALL:

```rust
if exec_partition.index != partition.index {
    return Err(ServerError::PartitionMismatch {
        exec_partition: exec_partition.index,
        flow_partition: partition.index,
    });
}
```

Turns CROSSSLOT into a clean typed error. Good ops UX, zero semantics
change. Not blocking; follow-up commit works.

## Regression check

- `cargo build --workspace --exclude ferriskey`: clean.
- `cargo test -p ff-test --tests`: 151/151 (100% pass).
- 5 should-be-zero greps still pass (Dimension 2 table above).
- No new `#[allow(...)]`, no `unsafe`, no new `Box::pin`. No stowaways.
- Issue #21 closed on GitHub.

No regressions.

## Vote

**ACCEPT phase 3** with the two documented YELLOWs (neither blocking).
Phase 3 delivers the atomicity guarantee the §5.5 gap report named as
the cap-stone fix, via a surgical ~550-LOC commit (160 Rust
rewrite/delete + 80 Lua rewrite + 370 new atomicity test + 100 doc
updates). Test meaningfulness passes the "could a non-atomic impl
pass this test?" check.

If W1 also ACCEPTs (their ping indicates same-direction ACCEPT with
two different YELLOWs), phase 3 closes + PR#25 bundles phase-1 + phase-2
+ phase-3 for merge to main.

Post-merge follow-ups for the YELLOWs:

- YELLOW 1: §7.3 text amendment in a trailing RFC-011-revision-4
  commit on rfc/011-revision-4, or inline the amendment into the
  phase-5 release-note consolidation.
- YELLOW 2: 5-LOC `PartitionMismatch` check — can land as its own
  commit on feat/rfc011-phase3-atomic before PR#25 merges OR as a
  phase-5 polish (operator UX improvement, not correctness).

Independent of W1. Did not read W1's full review doc before writing.
