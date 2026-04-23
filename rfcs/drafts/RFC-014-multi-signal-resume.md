# RFC-014: Multi-signal resume conditions (`all_of`, `count(n)`)

**Status:** Draft
**Author:** FlowFabric Team (Worker-2)
**Created:** 2026-04-23
**Tracks:** RFC-005 §Designed-for-deferred (lines 772–779) — the `all_of` + `count(n)` deferrals.
**Depends-on:** RFC-013 (Stage 1d `suspend` trait + base `ResumeCondition` shape)
**Extends:** RFC-004 §Resume Condition Model, RFC-005 §8 Signal Matching

---

## 0. Forward-compat contract with RFC-013

RFC-013 (parallel) is the canonical owner of the `suspend` trait signature and the **base** `ResumeCondition` shape. This RFC **extends** that base shape with multi-signal variants. The forward-compat contract is:

| Property required from RFC-013 | Why RFC-014 needs it | If RFC-013 doesn't deliver it |
|---|---|---|
| `ResumeCondition` is an **enum** (not an opaque JSON blob on the trait) | RFC-014 adds variants; a JSON blob defers parsing to Lua and makes Rust-side validation impossible. | RFC-014 becomes Lua-only — wire-format-defined. Rust typing is lost. Revisit. |
| `ResumeCondition` is **nestable / composable** (variants may hold `Vec<ResumeCondition>` or `Vec<WaitpointKey>`) | `AllOf` is a list of sub-conditions / waitpoints by definition. A flat non-composable enum cannot express it. | **BLOCKER.** RFC-014 cannot land. The alternative is a second parallel enum `MultiSignalCondition`, which we explicitly reject in §9. |
| `ResumeCondition` is **serde-stable** across engine and backend crates | Matching runs in Lua; the Rust side emits `resume_condition_json` per RFC-004 §Storage. | If serde is unstable RFC-014 must version-tag the JSON (§7). |
| `WaitpointSpec` remains **condition-agnostic** (carries only per-waitpoint matcher hints, not multi-signal topology) | Keeps the multi-signal structure at the `ResumeCondition` layer, not per-waitpoint. See §7. | RFC-014 would need to invert topology onto `WaitpointSpec`, which duplicates structure. |

**Action on blocker:** if RFC-013's adjudicated shape is non-nestable, this RFC parks at Draft and reopens only after an RFC-013 amendment. Do not ship a parallel `MultiSignalCondition` type.

---

## 1. Motivation

### 1.1 What consumers are asking for

Canonical patterns from `cairn-fabric` + real workflow consumers:

1. **Human-in-the-loop approval with N reviewers** — `count(2-of-5)` reviewers must approve before the execution proceeds. Reviewers are distinct; one reviewer signalling twice must count once.
2. **Aggregate N callbacks from external webhooks** — an execution fans work out to N external systems, each posts back a callback signal. Execution resumes when all N land. Retry-sent duplicates of the same callback count once.
3. **All-of distinct event types** — a deployment waits on `db-migration-complete`, `cache-warmed`, `feature-flag-set`. Each is its own waitpoint. Execution resumes only when all three have fired.
4. **Per-waitpoint quorum** — `k-of-n` where the `n` is the *same waitpoint* receiving multiple signals from distinct sources. (Edge-level quorum in flow DAGs is RFC-016's concern; this RFC covers waitpoint-local quorum only.)

### 1.2 What v1 shipped

RFC-004 and RFC-005 shipped with a single-signal `ResumeCondition` + the `signal_match_mode` hint (`any` / `all` / `count(n)`) on the waitpoint. RFC-005 §283 explicitly flags: "the primary path is `signal_match_mode = any` with a single matcher. The multi-signal machinery exists for correctness but complex multi-condition evaluation is a v1-should-have, not a must-have."

Today consumers expressing pattern (3) must spin an internal coordinator execution that waits single-signal, re-emits, and rechains. That is a workaround, not a primitive.

### 1.3 What is **not** in scope (inherited deferrals stay deferred)

(See §1.4 below for the canonical worked examples of patterns 1–3.)

RFC-005 §Designed-for-deferred lists seven deferrals. This RFC takes two: `all_of` and `count(n)`. The remaining five **stay deferred** and must not drift into scope:

- Signal routing to flow coordinator — RFC-016 concern.
- Signal payload schema validation — separate RFC (validation engine).
- Signal TTL — separate RFC (signal lifecycle).
- Signal replay — separate RFC (observability/debug tooling).
- Bulk signal delivery — separate RFC (API surface).

§9 names these explicitly under "Out of scope by choice, not oversight."

### 1.4 Canonical worked examples

The three §1.1 patterns map to these concrete builder-form constructions
(see §10.3 for the full builder API):

**Pattern 1 — 2-of-5 human reviewers (canonical style: shared waitpoint +
`DistinctSources`).** Reviewer identity comes from policy at runtime;
creating a waitpoint per reviewer forces a fan-out the policy engine cannot
commit to at suspend-time. Preferred shape:

```rust
let cond = ResumeCondition::count(2)
    .distinct_sources()
    .on_waitpoint(wp_reviewers);  // one waitpoint, N signals land on it
```

A signal's `source_identity` is the reviewer's user id. Two signals from
the same reviewer dedup at the token layer (§4.1).

**Pattern 2 — N external callbacks from the same webhook endpoint.** Each
callback carries its own `idempotency_key` / `signal_id`. Use
`DistinctSignals`:

```rust
let cond = ResumeCondition::count(n)
    .distinct_signals()
    .on_waitpoint(wp_webhook);
```

**Pattern 3 — all-of distinct event types (heterogeneous subsystems).** Each
subsystem owns its own waitpoint. Use `all_of_waitpoints`:

```rust
let cond = ResumeCondition::all_of_waitpoints([
    wp_db_migration_complete,
    wp_cache_warmed,
    wp_feature_flag_set,
]);
```

**When to prefer multi-waitpoint over shared-waitpoint.** Use one waitpoint
per source when the subsystems are heterogeneous (different matchers,
different payload schemas, different TTLs). Use a shared waitpoint when the
signals are homogeneous and only the source_identity distinguishes them (the
2-of-5 pattern).

---

## 2. Extended `ResumeCondition` enum

### 2.1 Proposed shape (atop RFC-013's base)

Assuming RFC-013 settles a base of the form:

```rust
// Provided by RFC-013 (base; RFC-014 does not own this):
pub enum ResumeCondition {
    Single { waitpoint_key: WaitpointKey, matcher: ConditionMatcher },
    // ... RFC-013 may add Timeout-only, OperatorOnly, etc.
}
```

RFC-014 adds two variants:

```rust
pub enum ResumeCondition {
    Single { waitpoint_key: WaitpointKey, matcher: ConditionMatcher },

    /// All listed sub-conditions must be satisfied. Order-independent.
    /// Once satisfied, further signals to member waitpoints are observed
    /// but do not re-open satisfaction.
    AllOf {
        members: Vec<ResumeCondition>,  // nestable
    },

    /// At least `n` *distinct satisfiers* (see CountKind) must match.
    /// `n` must be ≥ 1 and ≤ upper bound derived from CountKind (see §5).
    Count {
        n: u32,
        kind: CountKind,
        /// Optional: constrains which signals participate. If `None`, any
        /// signal delivered to any waitpoint in `waitpoints` counts (subject
        /// to CountKind).
        matcher: Option<ConditionMatcher>,
        waitpoints: Vec<WaitpointKey>,
    },
}

pub enum CountKind {
    /// n distinct waitpoint_keys in `waitpoints` must be satisfied.
    /// Idempotent: same waitpoint fired twice counts once.
    DistinctWaitpoints,

    /// n distinct signal_ids accepted across the waitpoint set.
    /// Suitable for "N callbacks from same webhook endpoint,
    /// each callback has its own idempotency_key".
    DistinctSignals,

    /// n distinct `source_identity` values (from signal.source_identity,
    /// RFC-005 §Signal fields). Suitable for "2-of-5 reviewers" where
    /// each reviewer's source_identity is their user id.
    DistinctSources,
}
```

### 2.2 Why this shape, not alternatives

- **`AllOf` nests `ResumeCondition`** rather than `Vec<WaitpointKey>` so `AllOf { members: [Count{…}, Single{…}] }` is expressible. Non-nestable alternative forces a 2D topology where consumers can't say "2-of-3 reviewers AND db-migration-complete." Nestable wins.
- **`Count.kind` is explicit.** Implicit "distinct by what?" caused real pain in prior workflow engines (Argo, Temporal). We make the discriminant a required enum field, not a mode string.
- **`Count.waitpoints` is required** (non-empty). A `Count(n)` with no declared waitpoint set is meaningless — see §5 (error taxonomy).
- **`Count.matcher` is optional.** Lets consumers say "2 signals named `approval` from distinct sources against this one waitpoint" without duplicating the matcher on every sub-condition.

### 2.3 Variants we explicitly did not add

- `AnyOf` — **not** added. `AnyOf { members: [a, b] }` is expressible as `Count { n: 1, kind: DistinctWaitpoints, waitpoints: [a, b] }`. Adding a third variant with overlapping semantics bloats the matching algorithm in §3.
- `Quorum { k, n }` — rejected. `Count { n: k, waitpoints: [...n waitpoints] }` covers it. Per-flow-edge quorum is RFC-016's concern.
- `NotOf` / negation — rejected. Negative conditions require a timeout to ever fire, and RFC-004's timeout-behavior already covers "advance if no signal arrived." Adding negation here doubles the algorithm's state space for one pattern already served.

---

## 3. Lua storage + matching algorithm

### 3.1 Storage model (extends RFC-010 §Waitpoint keys)

RFC-010 §64 already defines `ff:exec:{p:N}:<execution_id>:suspension:current` HASH with a `resume_condition_json` field. RFC-014 adds two new keys, scoped to the active suspension:

| Key | Type | Lifetime | Fields |
|---|---|---|---|
| `ff:exec:{p:N}:<execution_id>:suspension:current:satisfied_set` | SET | created at `suspend_execution`, deleted on resume/cancel/timeout | Populated with "satisfier tokens" — see §3.2. Membership is the durable satisfaction state. |
| `ff:exec:{p:N}:<execution_id>:suspension:current:member_map` | HASH | same lifetime | Static map from `waitpoint_id` → `condition_path` (a JSON path like `"members[0]"` or `"members[1]"`). Written once at suspend-time; read at each signal delivery to locate which `AllOf` / `Count` node a signal affects. |

#### 3.1.1 Cleanup owners

Both keys are created at `suspend_execution` commit. They MUST be deleted
by each of the three suspension-terminating paths, named explicitly:

| Terminator | Deletion site |
|---|---|
| `resume_execution` (signal-driven, §3.3 root-satisfied path) | Lua `ff_deliver_signal.lua` at `close_and_resume` |
| `cancel_execution` (RFC-013) | Lua `ff_cancel_execution.lua` |
| `expire_suspension` (timeout expirer thread, RFC-004 §timeout-behavior) | Lua `ff_expire_suspension.lua` |

Phase-2 integration test list (§10.2) includes
`expire_deletes_satisfied_set_and_member_map` and
`cancel_deletes_satisfied_set_and_member_map` to catch owner drift.

#### 3.1.2 Key budget impact

- Before RFC-014: suspension owns `{current HASH, waitpoints ZSET}`.
- After RFC-014: suspension owns `{current HASH, waitpoints ZSET,
  satisfied_set SET, member_map HASH}`. Net delta: +2 keys per active
  suspension.
- All four keys share the `{p:N}:<execution_id>` hash-tag → co-located on
  the same cluster shard, accessible from a single Valkey Function call.
- Key count is O(1) in condition complexity: nested depth and waitpoint-set
  size do not add keys (both are encoded inside the SET/HASH values, not
  as separate keys).

Rationale:
- **SET** gives O(1) "have we seen this satisfier before" (§4 idempotency).
- **HASH** is a static lookup table, not touched by signal delivery except read. Write-once at suspend commit.
- Both keys live **in Valkey, not in the handle**, so worker crashes between signal 2 and signal 3 of a `count(3)` don't lose the count (§4 replay).

### 3.2 Satisfier tokens

A satisfier token is the element stored in `satisfied_set`. Format depends on `CountKind`:

| CountKind | Token format | Example |
|---|---|---|
| `DistinctWaitpoints` | `"wp:<waitpoint_id>"` | `wp:WP-abc123` |
| `DistinctSignals` | `"sig:<signal_id>"` | `sig:SG-def456` |
| `DistinctSources` | `"src:<source_type>:<source_identity>"` | `src:user:alice@example.com`, `src:system:operator-override` |

**Source-type inclusion is intentional.** `source_type` is part of the
token precisely so `src:user:alice` and `src:system:alice` count as
distinct satisfiers. This closes §8 Q1: system-origin signals (e.g.
operator overrides via `send_signal`) DO count toward a `DistinctSources`
count. Operator tooling surfaces source_type breakdowns on
`evaluate_resume_conditions` (§4.4) so "2 operator overrides" is visibly
distinguishable from "2 user approvals."

Consumers that want "user-only" quorum encode the constraint via the
`Count.matcher` field (§2.1), e.g. `matcher = source_type == "user"`. No
separate `CountKind::DistinctUserSources` variant is introduced (rejected
as overlapping with matcher-based filtering).

**Token shape at leaves under `AllOf`.** For a direct `Single` child of
`AllOf`, the satisfier token is `wp:<waitpoint_id>` — the `wp:` token IS
the satisfaction marker, no synthetic `node:<path>` is emitted. For a
non-`Single` child (`AllOf` or `Count`), once the child is satisfied a
synthetic `node:<child_path>` token is SADDed. See §3.3 step 4 for the
ordering and §3.3's restated `AllOf.evaluate_node` rule.

### 3.3 Matching algorithm (extends RFC-005 §8.3)

Lua pseudocode lives in `ff-script/src/functions/signal.rs`'s `deliver_signal` path, replacing the single-matcher check at RFC-005 §262–281:

```
on deliver_signal(signal, execution_id):
    -- Parse condition topology. NOTE: "parse" is per-invocation only —
    -- Valkey Functions are stateless across calls, so there is no
    -- cross-call cache. Parse cost is O(condition size) ≤ O(8 KiB) per
    -- signal (§5.1 cap). The parsed `cond` is a function-local table for
    -- the duration of this single deliver_signal invocation only.
    local cond       = parse_resume_condition(HGET suspension:current resume_condition_json)
    local member_map = HGETALL suspension:current:member_map
    local satisfied  = SMEMBERS suspension:current:satisfied_set

    -- 1. Identify which condition node this signal pertains to.
    local node_path = member_map[signal.waitpoint_id]
    if node_path == nil then
        return {effect = "signal_ignored_not_in_condition"}
    end

    local node = descend(cond, node_path)

    -- 2. Compute the satisfier token for this signal against this node.
    local token = satisfier_token(signal, node.kind or "DistinctWaitpoints")

    -- 2.5 Apply the node's local matcher, if any. A signal that fails the
    --     matcher is recorded on the waitpoint (RFC-005 semantics) but
    --     does not contribute a satisfier token to this node.
    if node.matcher ~= nil and not match(signal, node.matcher) then
        return {effect = "signal_ignored_matcher_failed"}
    end

    -- 3. Idempotency: SADD returns 0 if already present.
    local added = SADD suspension:current:satisfied_set token
    if added == 0 then
        return {effect = "appended_to_waitpoint_duplicate"}
    end

    -- 4. Re-evaluate satisfaction of every node on the path to the root.
    --    Satisfaction propagates upward: a non-leaf satisfied child adds
    --    its synthetic `node:<child_path>` token to the parent's
    --    satisfied_set. Leaf `Single` children need no synthetic token —
    --    their `wp:<id>` IS the satisfaction marker (see §3.2 + the
    --    restated AllOf rule below).
    local path = ancestor_path(node_path)   -- e.g. ["members[0]", ""]
    for _, p in ipairs(path) do
        local n = descend(cond, p)
        if evaluate_node(n, satisfied_set) then
            if p == "" then
                -- root satisfied → close suspension, mark resume-eligible
                return close_and_resume(execution_id, signal)
            elseif not is_single_leaf(n) then
                SADD suspension:current:satisfied_set ("node:" .. p)
            end
        end
    end

    return {effect = "appended_to_waitpoint"}
```

`evaluate_node` rules (restated for clarity — leaf `Single` case is
distinct from non-leaf):

- `Single`: satisfied iff `wp:<waitpoint_id>` ∈ `satisfied_set`.
- `AllOf { members }`: satisfied iff, for each member `m` at child_path `cp`:
  - if `m` is a `Single`, `wp:<m.waitpoint_id>` ∈ `satisfied_set`;
  - else (`m` is `AllOf` or `Count`), `node:<cp>` ∈ `satisfied_set`.
- `Count { n, kind, waitpoints }`: satisfied iff
  `|{tokens in satisfied_set matching this node's kind+waitpoints}| >= n`.

### 3.4 Why a flat SET + path map, not nested HASHes

A nested-structure-per-node (one SET per condition sub-node) would be cleaner reading, but requires **N extra Valkey keys** per suspension. Flat `satisfied_set` + `member_map` keeps Valkey key count at `O(1)` per suspension regardless of condition complexity. Evaluation cost is `O(depth)` per signal — bounded by a hard depth cap (§5 invariant 5.4).

---

## 4. Idempotency + replay

### 4.1 Idempotency contract

**Same signal delivered twice must count once.** Enforcement is the `SADD` return value at step 3 above — if the token already existed, we return `appended_to_waitpoint_duplicate` and do not re-evaluate. This is stronger than RFC-005's existing `idempotency_key`-based dedup because:

1. `idempotency_key` dedup happens at signal acceptance (signal-level).
2. Token-based dedup happens at satisfaction (condition-level).

Both are kept. Example of why: a `DistinctSources` count may accept two *different* signals with different signal_ids and different idempotency_keys but the **same source_identity** — RFC-005's dedup doesn't fire, but RFC-014's SADD does. Correct behavior: both signals are accepted and recorded (RFC-005 semantics preserved), but only the first increments the count.

### 4.2 `AllOf` re-fire semantics

If a waitpoint inside an `AllOf` is fired, then fired again later, the second signal:
- Is accepted (RFC-005 semantics).
- Is appended to the waitpoint's signal list (RFC-005 §list_waitpoint_signals).
- Does NOT re-satisfy the parent — parent is already satisfied.
- Returns effect `appended_to_waitpoint_duplicate`.

This preserves the existing invariant "waitpoint close is one-way" from RFC-004 §State Transitions.

### 4.3 Durability + replay

**Requirement:** if a worker crashes after 2 of 3 `count(3)` signals have satisfied, replay must see the 2 already-counted.

Design:
- `satisfied_set` is a Valkey SET. It survives worker crashes by construction.
- The handle (worker-local) does NOT carry satisfaction state. On replay (`claim_resumed_execution`), the worker reads the handle's continuation pointer (RFC-004), does not consult satisfaction state directly, and proceeds.
- Third signal arrives → Lua reads `satisfied_set` (2 tokens present) → SADDs token 3 → `evaluate_node` finds count ≥ 3 → resumes execution.

The durable path is self-healing: Lua is the single source of truth; the handle never caches counts.

### 4.4 Non-crash replay: `evaluate_resume_conditions` operator tool

RFC-005 §323 defines `evaluate_resume_conditions(execution_id)` as an operator diagnostic. RFC-014 extends its output to return the per-node satisfaction state:

```
{
  condition: <parsed tree>,
  satisfied_set: [...tokens],
  nodes: [
    { path: "", kind: "AllOf", satisfied: false, remaining_members: 1 },
    { path: "members[0]", kind: "Count(2-of-3)", satisfied: true  },
    { path: "members[1]", kind: "Single", satisfied: false }
  ]
}
```

This is Class C (derived/read-only). No state transitions from this call.

The operator-facing output includes per-`DistinctSources` source_type
breakdowns (e.g. `{ "user": 1, "system": 1 }`) so "2 operator overrides"
is visibly distinguishable from "2 user approvals" (see §3.2 on Q1
closure).

### 4.5 Resume payload under multi-signal

RFC-004's `resumed_with` payload surfaces what signal(s) unblocked the
suspension. For a `Single`, this is the single signal. For multi-signal
conditions it is two fields:

| Field | Meaning |
|---|---|
| `closer_signal_id` | The signal whose acceptance + SADD caused `evaluate_node` at the root to flip to satisfied. Exactly one per resume. |
| `all_satisfier_signals` | The subset of `list_waitpoint_signals` whose satisfier tokens are present in `satisfied_set` at close time. For a `Count(3, DistinctSources)`, this is three signals (one per source). For `AllOf { Count(3), Single }`, this is four signals. |

Consumers that need only "who closed it" read `closer_signal_id`. Consumers
that need the full satisfier set (e.g. to attribute approvals in an audit
log) iterate `all_satisfier_signals`. Signals that landed but did not
contribute a satisfier (duplicates, matcher-failed) are NOT in
`all_satisfier_signals` but remain visible via `list_waitpoint_signals`.

---

## 5. Error taxonomy

### 5.1 Validation at `suspend_execution` (early detection)

| Error (new) | Condition | When |
|---|---|---|
| `count_exceeds_waitpoint_set` | `Count { n, waitpoints }` with `n > waitpoints.len()` and `kind = DistinctWaitpoints` | At suspend-time (Rust-side validation before Valkey call). Impossible condition → reject synchronously. |
| `count_n_zero` | `n == 0` | Suspend-time. Degenerate — caller should use timeout-only condition. |
| `allof_empty_members` | `AllOf { members: [] }` | Suspend-time. Trivially satisfied condition is ambiguous; force caller to spell intent. |
| `count_waitpoints_empty` | `Count { waitpoints: [], ... }` | Suspend-time. Meaningless: no waitpoints can satisfy. |
| `condition_depth_exceeded` | Recursive nest depth > 4 | Suspend-time. Hard cap. |
| `condition_size_exceeded` | Total serialized `resume_condition_json` > 8 KiB | Suspend-time. Bounds Lua parse cost. |

#### 5.1.1 Error payload shape

All new kinds map to a single structured error type:

```rust
pub enum EngineError {
    // ...existing variants...
    InvalidCondition {
        kind: ConditionErrorKind,
        detail: String,  // human-readable: "depth 5 exceeds cap 4 at path members[0].members[0].members[0]"
    },
}
```

`detail` is consumer-facing and MUST be specific enough to locate the
offending subtree (depth, path, cardinality values). Consumer error-surfacing
code (e.g. cairn's reviewer panel) renders the `detail` string verbatim.

### 5.2 Late-detection (at signal delivery)

| Error (existing, clarified by RFC-014) | Condition |
|---|---|
| `invalid_resume_condition` (RFC-004 §412) | Extended: also fires if `resume_condition_json` fails to parse in Lua at suspend (catch-all). |
| `signal_ignored_not_in_condition` (new) | Signal delivered to a waitpoint whose `waitpoint_id` is not in `member_map`. Signal is recorded, no satisfaction attempted. Returns this effect for observability. |
| `signal_ignored_matcher_failed` (new) | Signal delivered to a waitpoint whose `waitpoint_id` IS in `member_map`, but the local node's `Count.matcher` (§2.1) rejects it. Signal is recorded on the waitpoint's signal list (RFC-005 semantics) but does not contribute a satisfier token. |

### 5.3 Why early detection for `count_exceeds_waitpoint_set`

RFC-014 chooses **suspend-time** validation for size/cardinality errors for three reasons:

1. Suspend-time is synchronous in the worker; error propagates to user code. Late detection at signal time leaves the execution in a permanently-unsatisfiable state until timeout fires — silent failure.
2. The validation is cheap: `n <= waitpoints.len()` is O(1).
3. Consumer debuggability: a test "suspend with `count(5)` on 3 waitpoints" should fail loudly, not hang until timeout.

`DistinctSignals` and `DistinctSources` **cannot** be validated at suspend time (no upper bound on arriving signals/sources is known). Those rely on timeout behavior (§6) for non-termination.

### 5.4 Invariants

1. `satisfied_set` cardinality monotonically increases during suspension lifetime.
2. Satisfaction is one-way: `evaluate_node(n) == true` at time T implies `evaluate_node(n) == true` at all T' > T (until suspension closes).
3. Signal acceptance and satisfaction evaluation are atomic within a single Valkey Function call (Class A), consistent with RFC-005 §306.
4. No nested condition exceeds depth 4. (Soft-cap; consumers have not presented a real pattern > 2.)
5. `member_map` is write-once — read-only after `suspend_execution` commits.

### 5.5 Cap rationale (depth 4, size 8 KiB)

Both caps are soft — bumping either requires only a validator constant
change, not a wire-format or schema change. The current values are chosen
as:

- **Depth 4.** Real consumer patterns top out at depth 2 (`AllOf { Count,
  Single }` = depth 2). Depth 4 gives 2× headroom for future patterns
  without inviting arbitrary recursion. Per-signal cost is O(depth); at
  depth 4 the walk is four `evaluate_node` calls, each O(|members|) or
  O(|tokens|). Fits in a single `deliver_signal` Function budget.
- **Size 8 KiB.** Roughly 10% of Valkey's default ARGV soft limit (64 KiB
  per parameter), leaving headroom for other ARGV fields in the same
  `suspend_execution` call (waitpoint specs, continuation payload,
  idempotency key). A condition at 8 KiB can encode ~200 nested `Single`s
  or ~40 `Count`s with 5 waitpoints each — above observed real need.

If a future consumer hits either cap, lifting them is a single-PR change
(validator constant + the cap-rationale paragraph here). No persisted
data migrates.

---

## 6. Interaction with timeouts

### 6.1 Choice: synthetic timeout-signal, not terminal failure

When a `Count(3)` has a timeout and 2 satisfiers have arrived by timeout, **we generate a synthetic `timeout` signal** that is passed to `evaluate_node` as an additional token source, and we set `timeout_behavior` to decide what the node does with it.

The `timeout_behavior` values (from RFC-004 §254) already support this:

| `timeout_behavior` | Effect on partially-satisfied multi-signal condition |
|---|---|
| `fail` | Execution transitions to terminal failure regardless of partial satisfaction. (Most strict.) |
| `auto_resume_with_timeout_signal` | Synthetic timeout token added to `satisfied_set`. Condition re-evaluated. If the condition treats the timeout token as satisfying (consumer opt-in via matcher, §6.2), resumes. Otherwise falls through to `fail`. |
| `cancel` | Execution cancels; no partial work propagated. |
| `expire` | Suspension expires; execution state is what RFC-004 defines. |
| `escalate` | Operator-notified, no auto-advance. |

**This RFC does NOT introduce a new timeout mode.** It defines how existing modes compose with multi-signal conditions.

**Timeout token form.** The synthetic timeout token is
`timeout:<suspension_id>` — exactly one token per suspension, regardless of
`CountKind`. A `Count { n, kind }` node that sees the timeout token
short-circuits to satisfied (i.e. the timeout token is treated as a
universal node-satisfier), it does NOT increment the distinct-satisfier
count. Rationale: a timeout has no source_identity / signal_id /
waitpoint_id, so it cannot contribute a distinct satisfier under any
`CountKind`; treating it as a node-level short-circuit keeps the token
algebra clean. The short-circuit fires only when
`timeout_behavior == auto_resume_with_timeout_signal`; under `fail` the
token is not SADDed and the execution transitions to failure directly.

Consumers that want "resume only if ≥k of n signals arrived AND timeout
fired" must express that as application-layer logic on the resumed
worker — see §6.2.

### 6.2 Consumer opt-in for partial-count resume

The pattern "resume with whatever count we have if timeout fires" is expressible **without** a new variant: put the timeout-handling logic in the resumed worker code. The execution resumes with `timeout_behavior = auto_resume_with_timeout_signal`, and the worker inspects `list_waitpoint_signals` to see what actually arrived. This keeps the condition language minimal.

If a consumer wants *condition-level* "count(3) OR timeout-partial", they must write it as two sequential suspensions — one `Count(3)` with `fail` timeout, a retry-handler that runs a second suspension with a shorter timeout. This is verbose but expressible.

**Idiomatic surface.** To make the common "wait for 2-of-5 with 30m
timeout; on partial-quorum, escalate" pattern ergonomic, the
`SuspensionTimedOut` error variant carries `partial_satisfiers` so the
caller can branch without a second `evaluate_resume_conditions` round-trip:

```rust
match self.suspend(spec, cond_count_2_of_5, timeout_30m).await {
    Ok(resume) => handle_full_quorum(resume),
    Err(EngineError::SuspensionTimedOut { partial_satisfiers, .. }) =>
        handle_partial(partial_satisfiers),
}
```

`partial_satisfiers: Vec<SatisfierToken>` echoes the `satisfied_set`
contents at timeout time. This is an additive error-field change; no new
condition variant and no new `timeout_behavior` value.

### 6.3 Why not introduce `PartialCount` variant

Rejected: adds a 4th variant to handle a policy decision (what to do on timeout with N<required), bloating the enum with orthogonal concern. Policy belongs in `timeout_behavior`, topology in `ResumeCondition`.

---

## 7. Wire format + interactions

### 7.1 Does `WaitpointSpec` need condition-level info?

**No.** `ResumeCondition` owns the topology; `WaitpointSpec` stays per-waitpoint (name, matcher hints, payload schema hints).

At `suspend_execution` time:
- The worker constructs `Vec<WaitpointSpec>` (one per waitpoint to create) and `ResumeCondition` (the topology).
- `ResumeCondition` references waitpoints by `WaitpointKey`, which the suspend call resolves to `waitpoint_id`s.
- Lua receives `resume_condition_json` + `waitpoint_specs_json` as separate ARGV fields, writes `suspension:current:member_map` from their intersection.

This cleanly separates "what waitpoints exist" from "how their signals combine," matching RFC-004 §§Object Definition where these are already two fields.

### 7.2 Serde stability

`resume_condition_json` is the wire format. We version-tag it:

```json
{
  "v": 1,
  "kind": "AllOf",
  "members": [
    { "v": 1, "kind": "Single", "waitpoint_key": "..." },
    { "v": 1, "kind": "Count", "n": 2, "count_kind": "DistinctSources",
      "waitpoints": ["..."] }
  ]
}
```

Lua rejects `v > 1` with `invalid_resume_condition`. Future RFCs increment `v`.

### 7.3 Interaction with RFC-013

RFC-013 owns the typed `ResumeCondition` on the `suspend` trait. RFC-014 variants flow through it. The SDK forwarder (RFC-012 stage 1d) serializes to `resume_condition_json` and the Valkey backend writes it unchanged. No additional trait method is introduced by RFC-014.

### 7.4 Interaction with RFC-016 (flow DAGs, future)

RFC-016 addresses flow-edge quorum (DAG join semantics). RFC-014 handles waitpoint-local quorum only. If a flow edge needs `k-of-n` across *executions*, that is a flow-level join, not a waitpoint condition. The two layers compose: a flow-join waits on one-child-execution-per-branch; each branch may use RFC-014 locally.

### 7.5 Interaction with RFC-005 `send_signal`

`send_signal` (RFC-005 §310) signature is unchanged. The matching logic it invokes expands to §3.3. Existing single-signal consumers see no behavior change because the default `ResumeCondition::Single` branch of the matching algorithm reduces to RFC-005 §8.3.

---

## 8. Open questions

All prior open questions are closed as of round-1 challenge revisions:

1. **Closed — system-source signals count.** `CountKind::DistinctSources`
   counts signals with `source_type = "system"` as distinct satisfiers;
   `source_type` is part of the token (§3.2). Consumers that want
   user-only quorum use `Count.matcher = source_type == "user"`.
2. **Closed — no short-circuit.** Per-signal root evaluation is O(depth ≤ 4);
   the "maybe ready" queue alternative adds complexity without measurable
   win. Design stays as §3.3 step 4.
3. **Closed — shared waitpoint is canonical.** The shared-waitpoint form
   (`Count(2, DistinctSources, waitpoints=[wp_reviewers])`) is the
   canonical 2-of-5 shape (§1.4). No `WaitpointSpec.expected_signal_count`
   hint is added; signal TTL is out of scope per §1.3.

---

## 9. Alternatives rejected

### 9.1 Parallel `MultiSignalCondition` type (not an enum variant extension)

A separate top-level type passed alongside `ResumeCondition`:

```rust
// REJECTED
fn suspend(
    spec: Vec<WaitpointSpec>,
    cond: ResumeCondition,          // single-signal from RFC-013
    multi: Option<MultiSignalCondition>,  // new, parallel
);
```

Rejected because:
- Two types that both describe "when does this resume" is an API smell. Consumers must learn both; trait signatures grow.
- Composition (`AllOf { members: [count, single] }`) is awkward across two types.
- If RFC-013 delivers a non-nestable `ResumeCondition`, the correct move is to amend RFC-013, not fork the shape in RFC-014.

### 9.2 Condition-expressed-as-JSONata (or similar external predicate language)

Rejected: violates RFC-004 §Invariants (no external predicates in v1). Lua would execute untrusted expressions; scope creep into validation engine; debuggability loss.

### 9.3 Per-waitpoint `expected_signal_count` on `WaitpointSpec`

An alternative to `Count { n, waitpoints: [wp] }` where the `wp` itself carries `expected_signal_count = 3`. Rejected:
- Duplicates topology into per-waitpoint fields.
- Multi-waitpoint count (`Count { n: 2, waitpoints: [wp1, wp2, wp3] }`) is not expressible.
- RFC-004 `WaitpointSpec` is already at its size limit; adding count semantics there inflates an unrelated type.

### 9.4 Adding `AnyOf`, `NotOf`, `Quorum` variants

All expressible via `Count`. Extra variants ≡ extra matching paths in Lua. See §2.3.

### 9.5 Out of scope by choice, not oversight

The following RFC-005 §Designed-for-deferred items are **explicitly not in scope** for RFC-014. Adding them here would bloat an already-dense RFC and block on unrelated designs:

- **Signal routing to flow coordinator** — RFC-016 (flow DAGs).
- **Signal payload schema validation** — separate RFC (type registry / schema engine).
- **Signal TTL** — separate RFC (signal lifecycle + GC).
- **Signal replay** — separate RFC (debug/observability tooling).
- **Bulk signal delivery** — separate RFC (API surface; ties into batch ergonomics).

None of these are blockers for multi-signal resume. Multi-signal works with v1's signal semantics; these are additive future concerns.

---

## 10. Implementation plan

**Dependency:** RFC-013 must land first with a nestable `ResumeCondition` enum. If RFC-013 ships a non-nestable shape, this RFC parks pending an RFC-013 amendment.

### Phase 1 — Rust-side enum + validation

**Crates touched:** `ff-core` (types), `ff-sdk` (wire), `ff-backend-valkey` (argv build).

- Add `AllOf` + `Count` variants + `CountKind` enum to `ResumeCondition`.
- Add suspend-time validators (§5.1) returning `EngineError::InvalidCondition { kind }` variants.
- Wire-level serde with `v: 1` version tag (§7.2).
- Unit tests: validator rejects impossible conditions; serde roundtrip; nesting depth bound.

**Exit:** `cargo test -p ff-core -p ff-sdk` green. No Lua changes yet.

### Phase 2 — Lua evaluator

**Files:** `ff-script/src/functions/signal.rs`, `ff-script/lua/ff_deliver_signal.lua` (or equivalent), `ff-script/lua/ff_suspend_execution.lua`.

- `suspend_execution` writes `suspension:current:member_map`.
- `deliver_signal` replaces its single-matcher branch with the §3.3 algorithm.
- `evaluate_resume_conditions` (RFC-005 §323) extended per §4.4.
- Integration tests in `crates/ff-backend-valkey/tests/`:
  - `count_2_distinct_sources_resumes_on_second_source`
  - `count_2_distinct_sources_ignores_duplicate_source`
  - `count_2_distinct_sources_counts_system_signal` (Q1 closure)
  - `count_matcher_filters_user_only_sources` (matcher-based filtering)
  - `allof_three_waitpoints_resumes_when_all_fired`
  - `allof_replay_after_partial_count_preserves_state`
  - `count_exceeds_waitpoint_set_rejected_at_suspend`
  - `timeout_with_partial_count_uses_timeout_behavior`
  - `timeout_token_short_circuits_count_node` (§6.1 token form)
  - `suspension_timed_out_error_carries_partial_satisfiers` (§6.2 idiom)
  - `expire_deletes_satisfied_set_and_member_map` (§3.1.1 owner)
  - `cancel_deletes_satisfied_set_and_member_map` (§3.1.1 owner)
  - `resume_payload_exposes_closer_and_all_satisfiers` (§4.5)
- Cluster-mode integration tests in
  `crates/ff-backend-valkey/tests/cluster/`:
  - `satisfied_set_colocated_with_suspension_in_cluster_mode`
  - `member_map_colocated_with_suspension_in_cluster_mode`

**Exit:** integration tests green against Valkey 8.x; `evaluate_resume_conditions` diagnostic reflects correct partial state.

### Phase 3 — SDK ergonomics

**Crates:** `ff-sdk`.

Public builder API (full surface; doctests cover each entry point):

```rust
impl ResumeCondition {
    pub fn single(wp: WaitpointKey, matcher: ConditionMatcher) -> Self;

    /// Explicit all-of with heterogeneous members (nested Count, etc.).
    pub fn all_of(members: impl IntoIterator<Item = ResumeCondition>) -> Self;

    /// Shorthand for the common `AllOf { members: [Single, Single, ...] }`
    /// shape (pattern 3 — heterogeneous-subsystems-each-fired).
    pub fn all_of_waitpoints(
        wps: impl IntoIterator<Item = WaitpointKey>,
    ) -> Self;

    pub fn count(n: u32) -> CountBuilder;
}

impl CountBuilder {
    pub fn distinct_waitpoints(self) -> Self;
    pub fn distinct_signals(self) -> Self;
    pub fn distinct_sources(self) -> Self;
    pub fn with_matcher(self, m: ConditionMatcher) -> Self;

    /// Multi-waitpoint terminator (pattern 3 variant).
    pub fn on_waitpoints(
        self,
        wps: impl IntoIterator<Item = WaitpointKey>,
    ) -> ResumeCondition;

    /// Single-waitpoint terminator (canonical pattern 1 / 2 shape).
    pub fn on_waitpoint(self, wp: WaitpointKey) -> ResumeCondition;
}
```

- Doc tests in `ff-sdk/src/suspend.rs` covering each §1.4 canonical
  pattern: 2-of-5 reviewers (shared waitpoint), N webhook callbacks
  (distinct signals), all-of heterogeneous subsystems.
- Smoke example in `examples/` that runs a 2-of-3 approval flow
  end-to-end against a published artifact.

**Exit:** scratch-project smoke (per the "smoke after publish" memory) validates the three patterns against a published artifact.

### Phase 4 — Observability

- `evaluate_resume_conditions` output wired to `ff-board-sdk` (if/when that lands).
- Metric: `ff_suspension_condition_depth` histogram (per §5.4 invariant, catch drift).
- Tracing: per-signal log of `effect` including new `appended_to_waitpoint_duplicate` and `signal_ignored_not_in_condition`.

**Exit:** operator can answer "why is this execution still suspended?" via `evaluate_resume_conditions` without reading Lua.

### Non-goals during implementation

- No change to `send_signal` / `send_signal_to_waitpoint` signatures.
- No change to waitpoint creation API.
- No change to `idempotency_key` semantics (§4.1 keeps both dedup layers).
- No new Valkey keys beyond `satisfied_set` + `member_map` per §3.1.

---

## References

- RFC-004 §Resume Condition Model — base condition schema.
- RFC-005 §8 Signal Matching — existing single-signal algorithm this RFC extends.
- RFC-005 §Designed-for-deferred (lines 772–779) — the deferral catalog this RFC partially addresses.
- RFC-010 §Waitpoint keys (line 64) — storage conventions for `suspension:current`.
- RFC-010 §Lua helpers (line 780) — `initialize_condition(json)` is where parsing lives.
- RFC-012 §Stage 1d — `suspend` trait migration (parallel with RFC-013).
- RFC-013 (parallel) — base `ResumeCondition` shape + `suspend` trait signature.
- RFC-016 (future) — flow DAG edge quorum; composes with, does not supersede, RFC-014.
- PR #200 — `deliver_signal` + `claim_resumed_execution` landing.
