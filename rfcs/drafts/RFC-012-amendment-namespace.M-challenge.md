# Round-8 CHALLENGE against RFC-012 namespace amendment (draft v4)

**Challenger:** Worker M. Third independent adversarial review; K did round-1 (v2),
L did round-2 (v3), M does round-3 (v4). Challengers cycled to avoid capture.
**Tree head:** `origin/main` `2552f5d`.
**Posture:** pre-1.0, cairn sole consumer, CHANGELOG-only, no BC shims. Bias
check: v4 dropped the migration tool + rollback + warning gate in response to
the owner posture clarification. Does it still satisfy correctness properties
that survive the posture change?

Bottom line: v4 cleaned up K+L findings cleanly, but the **ARGV-append
mechanism is wire-breaking against three existing Lua functions** that compute
offsets from `#args` or parity of `#args`. The §R6.2.2 surface inventory also
undercounts by ~79 `format!("ff:…")` sites living outside `ff-core::keys`
(ff-engine scanners + ff-sdk + ff-server), which v4 silently omits. Fix
those plus a small §R6.6 consistency bug and land.

---

## MUST-FIX

### #M1: Appending namespace to ARGV breaks `ff_report_usage_and_check`, `ff_set_execution_tags`, `ff_set_flow_tags`, and the `ff_resolve_dependency` skipped-flow path

v4 §R6.2.4 line 85: *"each FCALL gains a **trailing** ARGV slot carrying the
namespace prefix. `ARGV[#ARGV]` (the last element) is `namespace_prefix`."*

The append-not-prepend choice is correct for **positional-index-read** functions
(which is most of the library). But four registered Lua functions compute slots
from `#args` or require `#args` parity, and the append silently corrupts them:

- `crates/ff-script/src/flowfabric.lua:3042` — `ff_set_execution_tags`:
  ```lua
  local n = #args
  if n == 0 or (n % 2) ~= 0 then
    return err("invalid_input", "tags must be non-empty even-length key/value pairs")
  end
  …
  redis.call("HSET", K.tags_key, unpack(args))
  ```
  Appending namespace makes `#args` odd whenever the caller passed an
  even-length key/value list. The parity check fails and every tag-set call
  errors out. Even if the parity check were patched, `unpack(args)` would
  pass the namespace string as a stray HSET argument, corrupting the hash.

- `crates/ff-script/src/flowfabric.lua:7033` — `ff_set_flow_tags`: identical
  `n = #args` parity guard + `unpack(args)`.

- `crates/ff-script/src/flowfabric.lua:5489-5499` — `ff_report_usage_and_check`:
  ```lua
  local dim_count = require_number(args[1], "dim_count")
  local now_ms = args[2 * dim_count + 2]
  local dedup_key = args[2 * dim_count + 3] or ""
  ```
  Rust caller builds `[dim_count, dims…, deltas…, now_ms, dedup_key]`
  (`budget.rs:199-218`). The Lua reads by fixed offset, so appending namespace
  to `ARGV[#ARGV]` does *not* shift `dedup_key`; dedup still resolves at
  `2*dim_count+3`. **This one happens to survive**, but only because the Rust
  caller remains the authority on positions. Flag as "audit required" not
  "known-safe" — the next edit that adds another trailing optional field to
  this function will collide.

- `crates/ff-script/src/flowfabric.lua:6919` — `ff_resolve_dependency` skipped-
  flow branch: `local num_edges = #args - 2`. Appending namespace makes
  `num_edges` off-by-one high; the loop then reads `args[2 + num_edges]` which
  is the namespace string, treated as an edge id. Silent correctness bug.

**Required fix.** §R6.2.4 must:

1. Acknowledge the three `#args`-dependent sites by name and state the
   required remediation: **rewrite each to pop the trailing namespace slot
   before any `#args` check or `unpack`**. Canonical fix:
   ```lua
   local NS = table.remove(args)   -- pops tail, decrements #args
   -- existing #args / unpack / offset logic continues unchanged
   ```
   This restores positional invariants without shifting indices. Forbid any
   remediation that leaves namespace in-band while computing `#args`.

2. Extend the §R6.2.4 Lua boilerplate example to use `table.remove(args)`
   rather than `ARGV[#ARGV]` read-in-place, since the read-in-place pattern
   is only correct for functions that neither iterate nor parity-check
   `#args`. The uniform pattern is safer as a project rule.

3. Add a Stage 1c test obligation: a CI grep-gate that rejects any new
   `#args` / `#ARGV` / `unpack(args)` read in the Lua library unless
   preceded by `table.remove(args)` on the same function-body scope.

Without this, Stage 1c ships with three broken FCALLs.

### #M2: §R6.2.2 surface inventory silently omits ~79 `format!("ff:…")` sites outside `ff-core::keys`

v4 §R6.2.2 lists:
- 83 `format!("ff:…")` sites in `keys.rs` ✓
- 4 Rust-literal sites in `ff-script::functions::lease` ✓
- 13 Lua-literal key sites + 4 PUBLISH sites ✓

Evidence the inventory is incomplete — `grep -rn 'format!("ff:' crates/ff-engine/src/ crates/ff-sdk/src/ crates/ff-server/src/` returns **79 hits** across 15 files:

- `crates/ff-engine/src/budget.rs:56-57`
- `crates/ff-engine/src/scanner/suspension_timeout.rs:138-180` (multiple)
- `crates/ff-engine/src/scanner/pending_wp_expiry.rs:131,142`
- `crates/ff-engine/src/scanner/budget_reconciler.rs:121-123`
- `crates/ff-engine/src/scanner/index_reconciler.rs:125`
- `crates/ff-engine/src/scanner/flow_projector.rs:146-148,202`
- `crates/ff-engine/src/scanner/delayed_promoter.rs:111`
- `crates/ff-engine/src/scanner/retention_trimmer.rs` (multiple)
- `crates/ff-engine/src/scanner/{quota_reconciler,lease_expiry,attempt_timeout,dependency_reconciler,budget_reset,unblock}.rs`
- `crates/ff-sdk/src/worker.rs`
- Plus `pub const COMPLETION_CHANNEL: &str = "ff:dag:completions"` at
  `crates/ff-engine/src/completion_listener.rs:43` (subscribe side — must match
  the namespaced publish name from Lua).

These sites ARE cross-namespace leak surfaces: scanners iterate indices and
materialise exec/flow/attempt keys by string-formatting bare `ff:` — a
namespaced deploy will have its scanners reading/writing the unprefixed
keyspace while its FCALL path writes to the prefixed keyspace. **Split-brain
at the scanner layer.**

Cairn's read-layer filter (the defense-in-depth referenced in §R6.1) does
not protect scanner writes — scanners mutate `deps_meta`, `active-index`
ZSETs, budget-reset hashes, etc. A scanner that reads `ff:idx:{p:N}:…` (bare)
while FCALLs write `<ns>:ff:idx:{p:N}:…` will reconcile against an empty
index and drop/rewrite state.

**Required fix.**

1. §R6.2.2 add a row: **"Rust-literal `ff:` sites outside `ff-core::keys` and
   `ff-script`"** — enumerate the 15 files + ~79 sites, or cite the
   `grep -rn 'format!("ff:' crates/ff-engine/ crates/ff-sdk/ crates/ff-server/`
   incantation and count. These must be threaded in Stage 1c or the scanner
   layer runs unnamespaced.

2. §R6.4 Stage 1c scope extend to name `ff-engine::scanner::*` (13 files) +
   `ff-engine::budget` + `ff-engine::completion_listener` + `ff-sdk::worker`
   explicitly. Current wording lists only `keys.rs` + `ff-script` + Lua.

3. Note the subscribe-side invariant for `COMPLETION_CHANNEL`: the engine's
   SUBSCRIBE must use the namespaced channel name; the Lua PUBLISH (covered
   in §R6.2.4) namespaces the publish side; both sides must stay in sync or
   completion events are lost. Currently `completion_listener.rs:43` is a
   bare `&'static str` constant — it becomes a `&Namespace`-parameterised
   function call in Stage 1c.

Without this, Stage 1c lands, the invariant §R6.2.1 is declared satisfied,
and a scanner quietly reads the wrong keyspace in production.

### #M3: §R6.6 `CLIENT SETNAME` follow-up contradicts v4 changelog + posture

v4 changelog line 15 implies the CLIENT SETNAME follow-up is gone
(v3→v4 removed triple-placement rollback and the whole rollback section,
and the L-challenge #LD1 landed v3's "out-of-scope callout"). But v4 §R6.6
line 232-234 still says:

```
- Does NOT modify `CLIENT SETNAME` / connection identification (see §R6.6 follow-up note below).

**Follow-up not-in-scope:** per-namespace `CLIENT SETNAME ff-worker-<ns>-<wiid>`
for operator `CLIENT LIST` visibility. Cheap, ~1 line per connection setup.
Propose as separate RFC/issue when operator demand appears.
```

Not a correctness bug — but the round-7→round-8 prompt for M specifically
flagged "v4 dropped the `CLIENT SETNAME` follow-up note entirely (user
decision)." It did not. §R6.6 still carries the note. Either the owner's
decision was to *keep* the note (benign) or to *remove* it (then §R6.6
needs editing). Clarify in v5 or confirm §R6.6 is intended.

**Required fix.** Either delete the §R6.6 follow-up paragraph, or update
the v3→v4 changelog to accurately state the follow-up note was retained.
One-line edit either way.

---

## DESERVES-DEBATE

### #MD1: `Namespace::new("")` → `Ok(EMPTY)` vs. `InvalidChar(' ')` for `"   "` — documented but asymmetric

v4 §R6.3 line 169: *"`new("")` → `Ok(EMPTY)`. Whitespace-only input (`"   "`)
errors via `InvalidChar(' ')` — callers trim their own config input."*

This is fine as a policy. But the rustdoc in §R6.3 (lines 140-150) is thin:

```rust
/// Construct and validate. Empty input returns `Namespace::EMPTY`
/// without error — empty is a supported production config, not
/// a caller bug.
pub fn new(s: impl Into<String>) -> Result<Self, NamespaceError> {
```

The doc says "empty input returns EMPTY" but doesn't say "whitespace is NOT
normalised to empty — `.trim()` is the caller's responsibility." A caller
reading `env::var("NS").unwrap_or_default()` with `NS=" "` in the env file
(common container pattern — `NS= ` or `NS="  "` in docker-compose) gets
`InvalidChar(' ')` unexpectedly.

**Debate.** Two positions:

- **(A, v4's position)** Document the policy in rustdoc and let callers
  `.trim()` themselves. Simplest. Preserves the "one constructor" posture
  from L#3 resolution.
- **(B)** Have `new` pre-`.trim()` silently. Whitespace collapses to empty.
  One-line change. No asymmetry for callers.

**Worker M lean: A, but rustdoc MUST state the whitespace-is-not-normalised
rule.** The current rustdoc is "empty is allowed" — implying a caller might
reasonably expect "empty-after-trim" is also allowed. Fix by appending:
*"Whitespace-only input is not normalised to empty: `Namespace::new("   ")`
errors with `InvalidChar(' ')`. Trim caller-provided config before passing."*

Not a correctness bug in v4 — just rustdoc thinness that invited L#3 and
will invite the same mistake from Stage 1c implementers if unwritten.

### #MD2: No test-strategy commitment for §R6.2.1 isolation invariant

v4 has no §R6.x that says *how Stage 1c will verify two backends with
different namespaces stay disjoint.* The invariant §R6.2.1 is a correctness
contract; a correctness contract without a verification commitment is a
promise on paper.

Natural test: spin two backends `A` (ns=`tenant_a`) and `B` (ns=`tenant_b`)
against a single Valkey, exercise the full EngineBackend surface on both,
assert `SCAN MATCH tenant_a:*` and `SCAN MATCH tenant_b:*` are disjoint and
that `SCAN MATCH ff:*` (bare, unprefixed) is empty.

**Debate.**

- **(A)** Amend §R6.4 Stage 1c row to add a line: *"Integration-test
  commitment: Stage 1c ships a two-backend disjointness test
  (`tests/namespace_isolation.rs` in `ff-backend-valkey`) that fails if any
  key/channel leaks across namespaces."*
- **(B)** Leave test scope to Stage 1c planning; the RFC doesn't prescribe
  test details.

**Worker M lean: A.** The invariant is the whole point of the amendment.
An implementer could satisfy §R6.2.2's threading scope mechanically and
still miss a leak (cf. #M2's scanner surface). A disjointness test is
the mechanical verification of the invariant. Commit to it in the RFC,
not in a follow-up planning doc.

### #MD3: v4 drops migration tool, but cairn's "what happens to my existing `ff:*` data" path is unstated

v4 §R6.5 line 211-212: *"Operators wanting to switch an existing deploy to
use a namespace: FLUSHDB-equivalent of the relevant partitions + redeploy
with new `BackendConfig.namespace`. Any in-flight state is lost."*

This is correct for the "switch" case. But cairn's actual path is different:
cairn is currently running with `namespace=EMPTY` (implicit, because the
feature doesn't exist yet). When Stage 1c lands, cairn's binary upgrades.
Their existing data lives at `ff:*`. Do they:

- (a) keep running with `namespace=EMPTY` (byte-identical keys per §R6.2.3),
  in which case nothing changes — **this is the intended path and v4 is
  silent that "keep namespace EMPTY" is the zero-migration option**, or
- (b) adopt a namespace, which requires FLUSHDB — losing their in-flight
  work.

The CHANGELOG bullet "Existing deploys without a namespace config continue
producing the same keys (byte-identical)" (v4 §R6.5 line 219) hints at (a)
but buries it. The §R6.5 prose describes only (b).

**Debate.** Add a §R6.5 lead sentence:

> *"Cairn's zero-migration path: keep `BackendConfig.namespace = EMPTY` (the
> default). Keys remain byte-identical. Adopting a non-EMPTY namespace is
> opt-in and requires a keyspace reset."*

or leave cairn's path to the CHANGELOG and the amendment's adoption-
coordination conversation (peer-team boundary).

**Worker M lean: add the lead sentence.** One sentence. Reassures peer-team
reviewers (cairn) without crossing the boundary. Current §R6.5 implies
migration is a FLUSHDB regardless of namespace choice, which is factually
wrong for the EMPTY-stays-EMPTY case.

---

## NITS

### #MN1: v4 §R6.2.2 row 6 `ff:noop:` audit deferral

Line 60: *"Pre-fix scratchpads (`ff:noop:` sentinel) | `flowfabric.lua:1299,1522` | 2 sites (string-contains check; prefix-aware audit needed)"*.

L already noted this wasn't enumerated crisply. v4 acknowledges "prefix-aware
audit needed" but doesn't commit to doing it. Either rename the sentinel,
fix-at-Stage-1c, or strike the "prefix-aware audit needed" parenthetical and
resolve. Open-ended TODOs in an accepted RFC drift into the implementation
phase as silent hazards. Nit, not a MUST — Stage 1c planning can audit.

### #MN2: Proposed rename targets (`idem_domain`, `exec_domain`, `tag_domain`) don't exist in the codebase — safe to claim

`grep -rn 'idem_domain\|exec_domain\|tag_domain' crates/` returns empty. The
proposed Stage 1c renames are collision-free. v4 §R6.2.6 is safe.

### #MN3: `fcall_with_namespace` helper — placement and trait bound not specified

v4 §R6.2.4 line 96: *"a thin helper (`fcall_with_namespace(cmd, keys, args, &namespace)`) centralises the append."*

Today's `ff_function!` macro (`crates/ff-script/src/macros.rs:34-71`) already
centralises FCALL construction — it builds `argv: Vec<String>` and calls
`conn.fcall::<Value>(name, &key_refs, &argv_refs)`. Extending the macro to
append `namespace.as_str()` to every generated `argv` is a **5-line macro
edit**, which is strictly better than a new free function helper (because
it catches all callers including the `ff_function!`-generated ones).

The manual FCALL sites (`ff_report_usage_and_check`, the tags functions, one
in budget_reset) are few enough to edit inline. The `fcall_with_namespace`
free helper adds a path that manual sites must remember to use; extending
the macro guarantees generated sites are covered.

**Suggested:** v4 §R6.2.4 should name the macro edit as the primary vehicle
(`ff_function!` macro appends namespace automatically) and the helper as
the secondary vehicle (for manual FCALL sites). Minor but clarifies the
implementation path.

### #MN4: Stage 2 Postgres check — `pg_catalog.pg_namespace` privilege note is fine

v4 §R6.4 line 199 adopts L#6's `pg_catalog.pg_namespace` correction. Per
Postgres docs, `pg_namespace` is readable by `PUBLIC` (default grant to
`pg_database_owner` via cluster-bootstrap); the `SELECT 1 FROM pg_namespace
WHERE nspname = $1` visibility returns rows regardless of USAGE on the
named schema. Unlike `information_schema.schemata`, it's genuinely
privilege-independent for existence checks. v4's claim stands.

### #MN5: `Namespace` `new_or_empty` deletion — clean

K#KD3 + L#3 arc closed cleanly. v4 has single `new` constructor + `EMPTY`
const + `new("")` maps to `Ok(EMPTY)`. The whitespace asymmetry lives (see
#MD1) but the helper-deletion was right.

---

## Affirmative no-findings (vs prior rounds + new angles)

- **K#K1 (Lua not transparent):** v4 §R6.2.4 now correctly states Lua-side
  propagation via ARGV tail. Fix direction correct modulo #M1 above (which
  is a *new* gap in v4's append-direction claim, not a K regression).
- **K#K2 (migration-tool SCAN pattern broken):** v4 deleted the tool. Fix
  by removal. Correct per posture.
- **K#K3 (enumerated leak surfaces):** v4 §R6.2.2 adds inventory table.
  Structure correct — but #M2 shows inventory is still incomplete beyond
  K's original enumeration.
- **K#K4 (6 key contexts, not 3):** v4 lists all 6. Clean.
- **K#K5 (#90 interaction):** v4 §R6.4 accurately says Lua PUBLISH sites
  persist; namespace threading applies regardless of merge order. Clean.
- **K#K6 (encoding rationale):** v4 §R6.2.3 states rationale. Clean.
- **L#1 (ARGV[1] prepend wire-break):** v4 reverses to append. Fix direction
  correct. But append introduces #M1's new collision with `#args`-dependent
  functions — still wire-breaking, just via a different mechanism.
- **L#2 (count table errors):** v4 corrects to 83 + 19. Line cites still
  point to `impl` openers rather than `new` defs at some rows (L#LN1-style
  nit) but v4's table is substantially accurate.
- **L#3 (whitespace leak in `new_or_empty`):** v4 deletes `new_or_empty`.
  Clean (modulo #MD1 rustdoc thinness).
- **L#4 (no-hash-tag cluster-slot):** v4 §R6.2.3 discusses the 3 affected
  keys explicitly. Greenfield posture acknowledges slot-change is correct.
  Clean.
- **L#5 (rollback hazard modes):** v4 deletes the rollback section entirely
  per posture. Correct per posture; the four hazard modes (double-execution,
  stuck suspensions, un-resolvable waitpoints, orphaned indices) become
  CHANGELOG scope rather than RFC scope. Clean.
- **L#6 (Postgres check):** v4 adopts `pg_catalog.pg_namespace`. Clean.
- **L#7 (`Namespace` placement):** v4 §R6.3 line 124 names `ff-core`.
  Clean.
- **L#LD1 (CLIENT SETNAME):** v4 §R6.6 retains follow-up note. The v3→v4
  changelog is slightly misleading on this (#M3) but the section itself is
  consistent with L's preferred resolution.
- **L#LD2 (`usage_dedup_key`):** v4 §R6.2.2 row 2 lists it as a free fn to
  thread. Clean.
- **L#LD3 (Stage 1a.0/1a.1 ordering):** v4 fuses into Stage 1c single
  landing. Clean per v3→v4 changelog.
- **Invariant #10 cross-namespace key *value* references:** checked — no
  FCALL writes an `ff:` key name as the *value* of another key. All cross-
  key references are reconstructed by Rust/Lua from tag + eid. Namespacing
  the key does not leak via value embedding. Clean.
- **`BackendConfig` `#[non_exhaustive]`:** verified at `backend.rs:610`.
  Struct-literal construction is forbidden cross-crate per Rust reference.
  v4's additive-field claim stands. Clean.
- **Proposed rename targets:** no collisions (#MN2). Clean.
- **ARGV-append via `ff_function!` macro feasibility:** macro at
  `macros.rs:60` builds `argv: Vec<String>` locally — appending namespace
  is a one-line edit. v4's "helper centralises the append" claim is
  realistic (see #MN3 for placement refinement). Clean.
- **#91 (PartitionKey) interaction:** v4 still says disjoint. Confirmed:
  PR #91 merged at `2552f5d` on main; touched `contracts.rs` + `partition.rs`
  + DTOs, not `keys.rs` + Lua. Disjoint holds. Clean.

---

## Summary for owner

v4 is very close. Three MUST-FIX items:

- **#M1** (append collides with `#args`-dependent Lua functions — wire-break,
  same class as L#1 reversed) — requires §R6.2.4 remediation: mandate
  `table.remove(args)` pattern, name the three affected functions.
- **#M2** (§R6.2.2 inventory omits ~79 `format!("ff:…")` sites in ff-engine
  scanners + ff-sdk + ff-server — scanner split-brain if unthreaded) —
  requires §R6.2.2 row + §R6.4 Stage 1c scope extension.
- **#M3** (§R6.6 `CLIENT SETNAME` follow-up contradicts v3→v4 changelog) —
  one-line consistency fix.

Three DESERVES-DEBATE: #MD1 (whitespace rustdoc), #MD2 (isolation test
commitment), #MD3 (cairn zero-migration path sentence).

Verdict: **CHALLENGE — v4 not ready to land.** Expect v5 with #M1–#M3
resolved. If #M1 and #M2 are addressed, the amendment is landable; MD1/MD3
are RFC-polish, MD2 is a test-scope commitment the owner picks.
