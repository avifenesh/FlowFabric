# RFC-014 Round 1 — K (correctness) challenge

**Verdict:** DISSENT

I like the overall shape (nested enum + flat SET + path map). Four correctness
concerns block ACCEPT; each has a concrete minimal change to flip my vote.

## Per-section verdict

| Section | Signal | Notes |
|---|---|---|
| §0 Forward-compat | GREEN | Contract with RFC-013 is stated precisely. |
| §1 Motivation | GREEN | |
| §2.1 Enum shape | YELLOW | `Count.matcher` scope under nesting is unspecified — see K-1. |
| §2.2/2.3 Why / rejected | GREEN | |
| §3.1 Storage | YELLOW | Two keys added but no mention of TTL / orphan cleanup on `expire` / `cancel` paths — see K-2. |
| §3.2 Satisfier tokens | **RED** | `DistinctSources` token `src:<source_type>:<source_identity>` collapses the `source_type` axis in a way that breaks the §8 Q1 answer — see K-3. |
| §3.3 Matching algorithm | **RED** | Step 3/4 ordering assumes the `SADD` for a child node precedes parent's `evaluate_node`, but a leaf `Single`'s satisfaction is recorded as `wp:<id>` while `AllOf.evaluate_node` (§3.3 last bullet) reads `node:<child_path>` — inconsistent token shape. See K-4. |
| §3.4 Rationale | GREEN | |
| §4.1 Idempotency | GREEN | Two-layer dedup (signal-level + token-level) is correctly separated. |
| §4.2 AllOf re-fire | GREEN | |
| §4.3 Durability | YELLOW | Claim "handle never caches counts" — but the *resume* path (`claim_resumed_execution`) needs to know WHICH signal crossed the threshold for the continuation's `resumed_with` payload. Not addressed. See K-5. |
| §4.4 Diagnostic | GREEN | |
| §5 Errors | GREEN | Suspend-time validation set is well-scoped. |
| §6 Timeouts | YELLOW | "Synthetic timeout signal added to `satisfied_set`" — what token form? `timeout:<suspension_id>`? Unspecified; re-evaluation against `CountKind` is undefined. See K-6. |
| §7 Wire | GREEN | Version tag is good. |
| §8 Open questions | YELLOW | Q1 (system-source) is load-bearing for correctness; cannot ship Draft→Accepted with it open. |
| §9 Alternatives | GREEN | |
| §10 Plan | GREEN | |

## Concerns with concrete fixes

### K-1 — `Count.matcher` scope under nesting

§2.1: `Count { matcher: Option<ConditionMatcher>, waitpoints, ... }`. If the
`Count` is nested inside `AllOf`, does the matcher filter signals at delivery
time (pre-SADD) or at evaluation time (post-SADD)? §3.3 pseudocode does not
invoke `node.matcher` anywhere. Under the current text, a signal whose
`waitpoint_id` is in `member_map` contributes a satisfier token regardless of
the node's `matcher`.

**Minimal fix:** insert a step 2.5 in §3.3 pseudocode:

```
-- 2.5 Apply the node's local matcher, if any.
if node.matcher ~= nil and not match(signal, node.matcher) then
    return {effect = "signal_ignored_matcher_failed"}
end
```

And add `signal_ignored_matcher_failed` to §5.2.

### K-2 — Orphan `satisfied_set` / `member_map` on `cancel` / `expire`

§3.1 says "deleted on resume/cancel/timeout." Which key-owner deletes them?
Lua `cancel_execution` (RFC-013) + `expire_suspension` must explicitly DEL both
keys. Without a named owner, I expect leaks on the `expire` path specifically
because that path runs on the expirer thread, not the resume thread.

**Minimal fix:** add §3.1.1 naming the three deletion sites
(`resume_execution`, `cancel_execution`, `expire_suspension`) and an
integration test `expire_deletes_satisfied_set_and_member_map`.

### K-3 — `DistinctSources` collapses `source_type` axis

§3.2 token: `src:<source_type>:<source_identity>`. The SET lookup treats
`src:user:alice` and `src:system:alice` as distinct, which is correct. But §8
Q1 asks whether system-origin signals count toward a `DistinctSources(n)` — if
the answer is "count, but note in operator tooling," then the token must
include source_type (it does). But if the answer is "DistinctSources is
user-scoped," the token SHOULD be `src:<source_identity>` alone and
source_type="system" signals should be rejected before SADD. The RFC cannot
ship with this still open because it determines the token format, which is a
persisted wire-level artifact.

**Minimal fix:** either

(a) close Q1 as "system signals do count; source_type is part of token" —
which the current token format implies — and remove Q1 from §8, OR

(b) add a new `CountKind::DistinctUserSources` and keep `DistinctSources` as
the permissive form, explicitly noting which kind the cairn 2-of-5 pattern
maps to.

My recommendation: (a). It matches §1.1 pattern 1 and is unambiguous.

### K-4 — Token shape inconsistency at leaves

§3.2: "For `AllOf` members, the token is always `wp:` form." §3.3 last
bullet: "`AllOf.evaluate_node` satisfied iff every member's synthetic
`node:<child_path>` ∈ `satisfied_set` (leaf `Single` children use `wp:<id>`)."
These two statements are consistent but subtle; a Lua implementer reading §3.3
alone would not know to special-case leaf `Single`.

**Minimal fix:** restate as two rules in §3.3:

```
AllOf.evaluate_node(node):
  for each member m at child_path:
    if m is Single:
      require wp:<m.waitpoint_id> in satisfied_set
    else:
      require node:<child_path> in satisfied_set
```

This also changes §3.3 step 4: the "SADD node:... if satisfied" emission is
skipped for leaf `Single` nodes (their `wp:` token IS the satisfaction
marker).

### K-5 — Resume continuation: which signal "closed" a Count?

RFC-004's `resumed_with` payload surfaces the signal that unblocked the
suspension. For `Count(3)`, the "third" signal is the closer — but what about
`AllOf { Count(3), Single }`? Three signals arrive to the Count (closing it);
later the Single arrives (closing the root). The closer of the root is the
Single. But a consumer reading `resumed_with` may want to see all three Count
signals too. Currently undefined.

**Minimal fix:** add §4.5 "Resume payload under multi-signal":

- `resumed_with.closer_signal_id` = the signal that closed the root.
- `resumed_with.all_satisfier_signals` = `list_waitpoint_signals` restricted
  to signals whose tokens are in `satisfied_set` at close time.

Consumer then knows both "who closed it" and "full satisfier set."

### K-6 — Timeout token form + interaction with CountKind

§6.1 says synthetic timeout signal enters `satisfied_set`. Token form?
`timeout:<suspension_id>` is one token, but `Count(3, DistinctSources)` needs
three distinct source tokens — one synthetic timeout token adds exactly 1 to
the count. So `auto_resume_with_timeout_signal` on a `Count(3)` with 2
satisfiers becomes 3 only if the synthetic timeout counts as a distinct
source, which is semantically weird (timeout has no source_identity).

**Minimal fix:** §6.1 must state:

- Timeout token is `timeout:<suspension_id>` (one global token).
- `Count` nodes treat the timeout token as satisfying the *node*, not adding
  to the count. I.e., if `timeout:` ∈ `satisfied_set` AND
  `timeout_behavior == auto_resume_with_timeout_signal`, node short-circuits
  to satisfied regardless of `n`.
- Consumers wanting "resume if ≥2 of 3 arrived" can inspect
  `list_waitpoint_signals` on the resumed worker per §6.2.

This keeps the token algebra clean and matches §6.2's stated position.

## Flip-to-ACCEPT requirements

All six (K-1 through K-6) must be addressed in revisions. K-3 (close Q1) and
K-4 (token shape) and K-6 (timeout token) are the blocking ones; K-1, K-2,
K-5 are correctness gaps but small.
