# RFC-011 debate transcript — exec/flow hash-slot co-location

**Debate protocol:** team debate, not independent reviews. All three
workers (W1, W2, W3) must converge on ACCEPT. W2 is the RFC author but
not the decider. Private positions first, no coordination before
posting, unanimous ACCEPT required.

**Outcome:** unanimous ACCEPT after 3 rounds (2 revision cycles).

**Final RFC commit:** `12e3995` on `feat/p36-fix-flow-id-atomicity`.

**Timeline:**
- `fdd054c` — RFC-011 initial draft (W2, 710 LOC)
- W3 + W1 independent round-1 positions → convergent RED on same defect class
- `921d76a` — revision round 1 (W2) — closes 2 convergent REDs + all 4 independent YELLOWs from W3 + all 4 independent YELLOWs from W1
- W3 + W1 independent round-2 positions → narrow residual DISSENTs
- `12e3995` — revision round 2 (W2) — closes W3's §3.4 attribution YELLOW (Option A strike) + W1's §5.6 + §2.4 YELLOWs
- W3 round-3 ACCEPT + W1 round-3 ACCEPT → **unanimous**

---

## Round 1 — W3 position (DISSENT)

Posted after W2's initial RFC at `fdd054c`. W3 did not read W1's position before writing.

**Verdict:** DISSENT. 2 RED, 6 YELLOW, design core ACCEPT-worthy but
implementation plan has a structural hole.

**RED 1 — ff-script crate missing from §3 entirely.** Independent grep
found 15 `ExecutionId` references across 5 files
(`crates/ff-script/src/functions/execution.rs` 9; `flow.rs` 2;
`scheduling.rs` 2; `signal.rs` 1; `quota.rs` 1). §7.1 phase-1
acceptance gate names only ff-server / ff-scheduler / ff-sdk; ff-script
is invisible.

**RED 2 — Default removal breaks ff-script's fill-by-caller pattern.**
Nine concrete sites use
`execution_id: ExecutionId::parse("").unwrap_or_default()` as "filled
by caller" placeholders
(`execution.rs:154, 206, 273, 314, 354, 463, 466`; `scheduling.rs:81`;
`signal.rs:230`). Default removal per §2.3 breaks these; they cannot
be mechanically fixed with `for_flow`/`solo` — the caller doesn't
know which flow/lane the exec belongs to at parse time. Requires API
redesign: Option<ExecutionId>, caller-provides, or type-system
Partial split.

**YELLOW 1 (§3.4 attribution):** ff-server handler doesn't mint ids —
receives pre-minted `args.execution_id`. The for_flow/solo selection
belongs to cairn (phase 4), not ff-server (phase 2).

**YELLOW 2 (§3.5 file name):** `partition_scan.rs` doesn't exist.
Scheduler is just `claim.rs` + `lib.rs`.

**YELLOW 3 (test count):** "~50 sites" understates by 3x;
`ExecutionId::new()` appears 166 times in ff-test.

**YELLOW 4 (§9.7 traffic amplification):** Only addresses key
collision. Lane/flow crc16 collision at the same partition causes
traffic amplification on one Valkey slot; not addressed.

**YELLOW 5 (§9.5 rollback overclaim):** Lua doesn't transactionally
roll back prior `redis.call()` side effects on later error.
Explanatory language is loose, though phase-3 test itself (which
exits before any write) is adequate.

**YELLOW 6 (cost):** 28-40h optimistic given ff-script + Default
redesign surface. Honest band: 40-55h.

Required changes to flip to ACCEPT: add ff-script to §3 with
enumerated sites + Partial/Option decision, revise §7.1 gate to
include `cargo check -p ff-script`, revise §8 cost upward, fix §3.5
file name, fix §3.4 attribution.

## Round 1 — W1 position (DISSENT)

Posted independently, no pre-coordination with W3.

**Verdict:** DISSENT. 2 RED + 6 YELLOW.

**RED 1** — ff-script missing from §3 entirely. Independent grep: 14
`ExecutionId` touches across 5 files. **Same defect class as W3
found.** Two independent reviewers converged on the same RED — best
signal the cross-review process works.

**RED 2** — Call-site counts off by ~2x. Independent grep against the
"~50 ff-test sites" claim found materially more. **Same defect class
as W3 found.**

Four independent YELLOWs (not overlapping W3's). Specifics recorded in
W1's team-send thread.

## Round 1 — convergence signal

Both reviewers independently caught the same two REDs. Validates the
debate protocol: if the user-decision on executing RFC-011 now
(rather than deferring) had shipped the original draft, we'd have
discovered ff-script's breakage during phase 1 compile-error audit at
best, during phase 3 integration tests at worst.

W1's concern on call-site counts + W3's concern on ff-script are
shapes of the same underlying issue: the punch-list was built by
partial grep and missed whole scope dimensions.

## Round 1 — W2 revision (commit `921d76a`)

W2 addressed all 4 convergent/independent RED concerns + all 4 W3
YELLOWs + all 4 W1 YELLOWs:

**RED 1 closure:** §2.4 new subsection spec'ing Partial-type refactor
(Option c); §3.1b new subsection enumerating every ff-script site by
line number (9 placeholder + 5 production parse = 16 call sites
across 4 files); §7.1 acceptance gate extended with
`cargo check -p ff-script`; phase 1 hours 6h → 10h (added 3h ff-script
refactor + 1h ff-script tests).

**RED 2 closure:** §3.4 revised to 175 call sites with per-crate
distribution + mechanical-vs-judgement split; §7.2 hours 8h → 14-18h
(W1 6-8h + W2 8-10h parallel); §8 total 32h → 42-46h serialised.

**W3 YELLOW 2 (§3.5):** FIXED — claim.rs.

**W3 YELLOW 5 (§9.5):** CONCEDED with tightened language; §7.3.1 test
2 rewritten as "validate-before-writing" discipline, not
rollback-equivalence; §10.6 probe 2 cited as source-of-truth for
commit visibility.

**W3 YELLOW 4 (§9.7):** CONCEDED with §5.6 three-part mitigation (CLI
probe + runbook + pluggable `SoloPartitioner` trait) + §12.3 decision
protocol for trait overrides.

W1 YELLOWs (independent): all addressed; specifics in W2's revision
commit message.

## Round 2 — W3 position (narrow DISSENT)

Posted independently after re-reading revision `921d76a`. W3 did not
read W1's round-2 position before writing.

**Verdict:** DISSENT, narrow. Both REDs CLOSED, 5 of 6 YELLOWs
CLOSED. One remaining:

**YELLOW 1 (§3.4 attribution) PARTIALLY CLOSED:**

W2 acknowledged the concern and correctly added cairn-fabric (phase
4) as mint owner, but **simultaneously retained ff-server (phase 2)
as a mint site** via the bullet:

> "ff-server side (phase 2, W1): ONE site — the create_execution
> handler in crates/ff-server/src/server.rs. This is the runtime
> id-minting path; decision:
> args.flow_id.as_ref().map(|fid| ExecutionId::for_flow(fid, config)).unwrap_or_else(|| ExecutionId::solo(&args.lane, config))"

This contradicts the current codebase:
- `CreateExecutionArgs.execution_id: ExecutionId` is non-Option. Caller
  pre-mints.
- `CreateExecutionArgs` has NO `flow_id` field.
- ff-server's `create_execution` at `server.rs:444-524` uses
  `args.execution_id` directly, never mints.

So `args.flow_id.as_ref().map(...)` does not compile against the
current types. Two possible reads:

**Option A:** Strike the ff-server bullet. Phase-2 ff-server work is
the 15 routing-through sites + rustdoc; id-minting is cairn-fabric
(phase 4) only. Matches current code shape.

**Option B:** Spec a wire-API change: `CreateExecutionArgs.execution_id: Option<ExecutionId>` + new `flow_id: Option<FlowId>` field, so server can mint when caller doesn't pre-mint. Adds ~3-4h scope + cairn coordination; total 45-50h.

Either resolves the ambiguity; ACCEPT either one.

## Round 2 — W1 position (narrow DISSENT)

Posted independently. Concerns on 2 new revision-introduced YELLOWs:

- **§5.6 ff-admin crate ambiguity:** The CLI probe is described as
  `ff-admin` — but there is no `ff-admin` crate; should it be
  `ff-server admin` subcommand or a new crate?
- **§2.4 multi-variant Partial not shown:** The example is
  single-variant (`ClaimExecutionResultPartial`). What about
  multi-variant result types like `ExpireExecutionResult` or
  `CreateExecutionResult` — does the Partial refactor handle those?

## Round 2 — W2 revision (commit `12e3995`)

Surgical. Three specific fixes, no reopening of prior-closed items.

**W3 YELLOW 1 (§3.4):** Picked Option A (strike). §3.4 bullet now:
"**ff-server side: ZERO sites.**" with factual defense grounded in
`CreateExecutionArgs` shape. §3.4 explicitly answers "Why not option
B" — scope creep, no current client needs it, a future consumer
files its own RFC. §7.2 W1 hours 6-8h → 5.5-7.5h (-30min). §8 total
42-46h → 41.5-45.5h.

**W1 §5.6:** `ff-admin` → `ff-server admin partition-collisions`
subcommand on the existing binary. Reuses config-loading + ferriskey
client setup. Cost matches §5.6 claim; matches §8 phase-5 budget.

**W1 §2.4 multi-variant:** Addendum added under §2.4 with concrete
`ExpireExecutionResultPartial` and `CreateExecutionResultPartial`
examples. Variant-mirror pattern: only variants carrying
`execution_id` get the Partial lift; `.complete(exec_id)` is a total
match over Partial variants so adding a new variant forces a compile
error if exec_id wiring is forgotten.

## Round 3 — W3 ACCEPT

Posted independently after re-reading revision `12e3995`. Verified:

- §3.4 ff-server ZERO sites, cairn-fabric phase 4, ff-test phase 2.
  Attribution now factually correct.
- §3.4 "Why not option B" answer is decisive, not a non-answer.
- §7.2 + §8 cost deltas match the stated -30min reduction.
- All stale "ff-server mints" language swept from §3, §7, §8.
- §5.6 and §9.7 both use "ff-server admin" (not separate crate).
- §2.4 multi-variant addendum is type-system-enforced via total-match
  `.complete`.
- No regressions: no new cross-refs at stale language, no scope
  bleed, no quietly-added TODO / FIXME / deferred items.

No new deltas. ACCEPT.

## Round 3 — W1 ACCEPT

Posted independently. W1's independent verification:

- §3.4 round-2 strike factually correct (verified against
  `crates/ff-core/src/contracts.rs` + zero `ExecutionId::new()` grep
  in ff-server).
- No new deltas.

## Unanimous ACCEPT

Both reviewers posted ACCEPT on the same revision (`12e3995`) without
coordination. RFC-011 converges.

**Cost final:** 41.5-45.5h serialised, 35.5-39.5h wall-clock across
3 workers + cairn team. ~7 working days at 6h/day end-to-end;
8-10 days with per-phase cross-review rounds.

**Next step:** escalate to manager for owner approval.

## Retrospective — what the debate protocol caught

Two convergent REDs caught by independent reviewers on round 1:

1. **ff-script crate missing from §3 punch-list** (W3 RED 1, W1 RED 1).
   Had the RFC shipped without this caught, phase 1 compile-error
   audit (§7.1 original: "3h audit across workspace") would have
   surfaced it, but §7.1 acceptance gate named only ff-server /
   ff-scheduler / ff-sdk — ff-script would have passed the gate while
   silently broken. Discovery would have slipped to phase 3
   integration tests, by which point phases 1 + 2 were already merged
   and a cascade of compile errors would block phase 3 for a day+
   until the `Default` removal was redesigned mid-sprint.

2. **Call-site counts off by 2-3x** (W3 RED 2, W1 RED 2). Original
   estimate "~50 sites" vs independent grep's 166 in ff-test alone.
   Phase-2 budget was 8h for "parallelised across W1 (ff-server +
   scheduler) and W2 (ff-sdk + ff-test), 4h each" — would have
   overrun by 2-3x, compounding any discovery-driven delays from
   RED 1.

Both convergent REDs were closed in revision round 1. Neither would
have been caught by a single reviewer reading the same way the author
did; the "private-positions-first" protocol forced independent greps
that exposed the punch-list holes.

Three additional independent YELLOWs (W3: §3.4 attribution, §3.5
partition_scan.rs, traffic amplification, §9.5 language, cost
estimate; W1: §5.6 crate ambiguity, §2.4 multi-variant, and two
others) caught surface-level defects that would have shipped
unchallenged under a "one reviewer pre-reads and signs off" model.

Two revision rounds, zero "agree to disagree." Unanimous ACCEPT on
third reading.
