# RFC-014 Round 1 — M (implementation) challenge

**Verdict:** DISSENT

The storage model is reasonable. The per-signal re-evaluation cost is
bounded. But three implementation costs are underspecified in ways that
will either drift at implementation time or force a follow-up RFC. Fix
those in-doc and I flip to ACCEPT.

## Per-section verdict

| Section | Signal | Notes |
|---|---|---|
| §0 Forward-compat | GREEN | |
| §1 Motivation | GREEN | |
| §2.1 Enum shape | GREEN | Wire size bounded by §5.1 8 KiB cap — good. |
| §3.1 Storage | YELLOW | Two keys per suspension is fine; key-count budget not stated — see M-1. |
| §3.2 Satisfier tokens | GREEN | String tokens are Valkey-native (SET members), no serialization overhead. |
| §3.3 Algorithm | YELLOW | `parse_resume_condition` — is this re-parsed on every signal, or cached in Function-scope? §3.3 says "cached" but doesn't say what happens on Function reload. See M-2. |
| §3.4 Rationale | GREEN | |
| §4 Idempotency | GREEN | |
| §5.1 Error table | YELLOW | Depth cap = 4 and size cap = 8 KiB are asserted but not justified by measurement — see M-3. |
| §6 Timeouts | GREEN | |
| §7.1 `WaitpointSpec` unchanged | GREEN | Good — keeps the existing shape. |
| §7.2 Serde version tag | GREEN | |
| §8 Open questions | GREEN | |
| §9 Alternatives | GREEN | |
| §10 Plan | **RED** | Phase 2 integration tests omit Valkey-cluster / hash-slot co-location tests. See M-4. |

## Concerns with concrete fixes

### M-1 — Per-suspension key budget not stated

RFC-010 tracks a per-suspension key budget. Today a suspension owns
`suspension:current` (HASH) and waitpoint-list keys. RFC-014 adds
`satisfied_set` (SET) and `member_map` (HASH). §3.1 says "+2 keys per
suspension regardless of condition complexity" but doesn't roll up the total
or confirm the cluster-shard impact.

**Minimal fix:** add §3.1.2 "Key budget impact":

- Before RFC-014: suspension owns {current HASH, waitpoints ZSET}.
- After RFC-014: suspension owns {current HASH, waitpoints ZSET,
  satisfied_set SET, member_map HASH}.
- All keys share the `{p:N}:<execution_id>` hash-tag → same shard, same
  Function call.
- Impact on `ff_suspension_key_count` metric (if one exists) — state
  whether the metric name is stable.

This is two sentences; the reader-confidence payoff is high.

### M-2 — Function-scope parse cache

§3.3: "Load condition topology (parsed once, cached in function-scope
table)." Function-scope == per-Valkey-Function-invocation or per-execution?
Valkey Functions are stateless across calls; "cached" must mean within the
single `deliver_signal` call. If the author means cross-call caching,
that's a cluster correctness bug (different shard → different cache).

**Minimal fix:** clarify §3.3 line 1 to: "Parse `resume_condition_json`
locally inside this `deliver_signal` invocation. No cross-call caching —
Valkey Functions are stateless across invocations."

Parse cost is O(condition size) ≤ O(8 KiB) per signal. At depth 4 and
typical condition size < 1 KiB, this is well within budget.

### M-3 — Depth cap 4 and size cap 8 KiB are unjustified

§5.1: depth > 4 rejected, size > 8 KiB rejected. Where do 4 and 8 KiB come
from? Reasoning I can reconstruct:

- depth 4 → O(depth) walk per signal = 4 node evaluations. Fine.
- 8 KiB → roughly 200 nested `Single`s or 40 `Count`s with 5 waitpoints
  each. Above real need, below ARGV limits.

But these are asserted caps without trace. If a future consumer hits them,
we're in "lift the cap" territory without understanding why it was set.

**Minimal fix:** add §5.5 "Cap rationale":

- Depth 4 = max realistic compositional depth (§8 Q2 notes consumers have
  not presented > 2). 4 gives 2× headroom.
- Size 8 KiB = 10% of Valkey's 64 KiB ARGV soft limit per parameter, leaves
  headroom for other ARGV fields in the same `suspend_execution` call.
- Both caps are soft: bumping either requires only a validator constant
  change, not a wire-format change.

Named so the next person who wants to lift them knows what they're
trading.

### M-4 — Cluster / hash-slot tests missing from Phase 2

§10.2 lists integration tests. None assert Valkey-cluster behavior
specifically. RFC-014 adds two keys that MUST co-locate with
`suspension:current` via hash-tag. If the tag derivation drifts (see past
hash-tag-drift bugs on this project), signals cross-shard and Lua fails
with `CROSSSLOT`.

**Minimal fix:** add to §10.2 integration test list:

- `satisfied_set_colocated_with_suspension_in_cluster_mode` — asserts both
  new keys hash to the same slot as `suspension:current` under
  `valkey-cluster` topology.
- `member_map_colocated_with_suspension_in_cluster_mode` — same for
  `member_map`.

These belong in `crates/ff-backend-valkey/tests/cluster/`. This is the
same class of test RFC-012 added for the flow/exec co-location and
caught drift early. RFC-014 inherits the pattern.

## Flip-to-ACCEPT requirements

- M-1: state the key budget delta.
- M-2: clarify "cached" means within-call only.
- M-3: add §5.5 cap rationale.
- M-4: add cluster co-location tests to §10.2.

All are doc-only; no design changes. If the author makes these edits I
ACCEPT.
