# RFC-009 V2 cap-routing — real-world impact analysis (2026-04-23)

Author: Worker EEE (investigation only, no code changes)
Scope: Assess whether the thrash residual tracked in issue #11 is a real
production concern or a bench-contrived artifact, and recommend a #11
disposition.

## Mechanism

Cap-routing v1 uses a **block-on-mismatch + periodic sweep** shape
(RFC-009 §7.5, `rfcs/RFC-009-scheduling.md:370-386`):

1. Worker polls `claim_next` (default 50 ms in bench,
   `benches/harness/src/bin/cap_routed.rs:789`).
2. Scheduler / SDK inline-claim path selects the top of
   `ff:idx:{p:N}:lane:<L>:eligible` and calls `ff_issue_claim_grant`
   with the worker's caps CSV in ARGV[9]
   (`rfcs/RFC-009-scheduling.md:372-376`).
3. Lua does subset check atomically. On mismatch it returns
   `capability_mismatch` with a single HSET
   (`last_capability_mismatch_at`) — **no ZSET mutation**
   (`rfcs/RFC-009-scheduling.md:378-381`).
4. The caller (ff-scheduler or ff-sdk inline path) invokes
   `ff_block_execution_for_admission(reason="waiting_for_capable_worker")`
   which ZREMs from `eligible` and ZADDs to `blocked:route`
   (`rfcs/RFC-009-scheduling.md:378`,
   `RFC-009-scheduling.md:566-572`).
5. `ff-engine::scanner::unblock` fires every `unblock_interval`
   (default 5 s — `crates/ff-engine/src/lib.rs:139`). For each member
   in `lane_blocked_route`, it:
   - reads the exec's `required_capabilities` CSV
     (`crates/ff-engine/src/scanner/unblock.rs:605-615`);
   - compares against the **union** of every connected worker's caps
     via SSCAN `ff:idx:workers` + fan-out GET `ff:worker:<id>:caps`
     (`crates/ff-engine/src/scanner/unblock.rs:680-757`);
   - if the union covers required, FCALLs `ff_unblock_execution` →
     exec returns to `eligible`
     (`crates/ff-engine/src/scanner/unblock.rs:283, 292-313`).

**Thrash condition.** The union check is over the *fleet*, not the
*next poller*. Once promoted back to `eligible`, whichever worker
happens to poll first claims. If that worker is **incapable**
(union matched because some *other* worker in the fleet has the cap),
the exec is re-blocked. Each 5 s sweep is a fresh roll of the dice
weighted by the fraction of capable vs incapable workers:

- P(capable wins) ≈ n_capable / n_total (at identical poll rates).
- Expected cycles to converge ≈ n_total / n_capable.
- Wall-time per stranded exec ≈ unblock_interval × expected cycles.

**Exact triggers:**

- Capable : incapable ratio is **small** (≤ ~1:9). Thrash grows
  geometrically.
- Required caps are satisfied by **some but not all** connected workers
  (if NO worker has the cap, union check returns false and the exec
  stays blocked — no thrash, just correct starvation).
- Multiple incapable workers poll faster than the sweep interval (always
  true in practice: 50 ms poll vs 5 s sweep).
- Capability-set cardinality: **irrelevant** to thrash. The subset check
  is pass/fail regardless of whether required is `[A]` or `[A,B,C]`.
  What matters is the fraction of workers satisfying the whole set.

## Synthetic bench profile

`benches/harness/src/bin/cap_routed.rs` scenario 5, three modes
(`cap_routed.rs:865-918`):

| Mode | Workers | Caps distribution | Thrash-driving ratio |
|---|---|---|---|
| `happy` | 100, each advertises full universe | P(capable) = 1.0 | None — expected 1.0 convergence |
| `partial` | 10 "power" + 90 "regular" | For ~50% of tasks: P(capable) = 10/100 = 0.10 | Expected cycles = 10; at 5 s sweep → ~50 s/task; 300 s deadline → geometric tail ~2-10% stranded |
| `scarce` | 1 omnibus + 99 single-cap | For ~10% of tasks: P(capable) = 1/100 = 0.01 | Expected cycles = 100; at 5 s sweep → ~500 s/task > deadline; ~5-15% stranded |

Task volumes: 1000 tasks, 10-cap universe, 50 ms worker poll
(`cap_routed.rs:125-128, 789`). Deadline 300 s (`cap_routed.rs:177`).

**Inflection point.** Empirically from mode definitions:
- 100 capable / 100 total → 1.00 (trivial).
- 10 / 100 → 0.95 ± 0.03 (observed, `cap_routed.rs:97`).
- 1 / 100 → 0.90 ± 0.04 (observed, `cap_routed.rs:98`).
- Inferred: >= 50% capable → no stranding within 300 s
  (expected cycles ≤ 2, wall time ≤ 10 s, geometric tail vanishes).

The floor is roughly **25% capable** for convergence within 300 s
at the 5 s sweep cadence (E[cycles]=4, E[wall]=20 s, tail << 1%).

## Production patterns

### Cairn (only real consumer today)

Cairn spawns **typed workers per agent-kind**. A worker pool is N
instances of the *same* agent-kind, each advertising the *same* cap
set. Tasks are stamped with the capabilities that agent needs.

Implication: for any given task requirement `R`, workers in cairn's
fleet are either **all capable** (the pool that matches `R`) or
**all incapable** (every other pool). A task requiring `R` lives on a
lane associated with the matching pool — so the fleet-visible fraction
of capable workers per lane is effectively **1.0**, not 0.1.

Cap-routing thrash requires a *mixed* fleet on a single lane where
some workers satisfy and others don't. Cairn's deployment shape
doesn't naturally produce that mix: workers are lane-homogeneous.

**Verdict for cairn:** thrash is structurally absent. Cairn has not
reported this issue and has no reason to (consistent with the status
summary, `rfcs/drafts/rfc-009-status-summary.md:32`).

### Hypothetical: multi-tenant agent platform (cairn-shaped)

Same as cairn. Homogeneous pools per lane. No thrash.

### Hypothetical: generic queue-style workload (no caps)

`required_capabilities` is empty. `check_route_cleared` short-circuits
to `true` (`unblock.rs:612-615`); `ff_issue_claim_grant` subset check
is trivially satisfied (empty set ⊆ any set). Zero caps → zero thrash.
This is the BullMQ/Sidekiq migration path. Unaffected.

### Hypothetical: heterogeneous analytics pool

Many workers with overlapping-but-not-identical cap sets on a single
shared lane. E.g. 100 data-job workers; 30 have `gpu`, 70 have `cpu`.
Tasks requiring `gpu` land on the shared lane with 30% capable
fraction. At 30% capable, E[cycles] ≈ 3, E[wall] ≈ 15 s per
exclusive task. Convergence within 300 s approaches 100% (geometric
tail vanishes fast).

Only becomes problematic if the capable fraction drops below ~10-15%
on a high-traffic lane. In practice this is a **lane-design smell** —
the operator should partition gpu / cpu onto separate lanes. But it's
not impossible.

### Hypothetical: pathological mix

The bench's 10% and 1% capable modes. Real-world equivalent: a rare
capability (e.g. a specialized model, an airgap-sandbox worker) mixed
onto a general lane with many irrelevant workers. The RFC already
warns against this shape — it's what "use separate lanes for
different priority tiers" (`RFC-009-scheduling.md:152`) generalizes
to for caps.

### Hypothetical: transient imbalance

During a deploy, a capable worker pool restarts. For a short window
(seconds to tens of seconds), the capable-fraction drops. If the
deploy co-occurs with task submission to that lane, tasks thrash
briefly then converge once the pool comes back. Benign — the bench
already models this geometry and shows ≥95% convergence at 90%
incapable.

## User experience if hit

**What the user sees.** Some fraction of tasks sit in
`lane_blocked_route` longer than the 5 s sweep interval. At p50 of an
adversarial distribution, tasks converge in 1-3 cycles (5-15 s). At
p99 and into the tail, they can take minutes. At the 300 s deadline,
a few percent are still blocked.

**Correctness vs latency.** Pure **latency** issue. Every stranded
task is still claimable — it's just waiting for the sweep + race to
favor a capable poller. No task is lost, no task enters a dead state,
no task is claimed by an incapable worker (the subset check in Lua is
authoritative, `RFC-009-scheduling.md:730-733`). The
`last_capability_mismatch_at` HSET gives operators a recency signal
for diagnosis (`RFC-009-scheduling.md:368`).

**Eventual drain.** Yes. Given enough time (~E[cycles] × 5 s), every
task converges. The only true starvation is when NO capable worker
exists in the fleet — and that's surfaced by
`last_capability_mismatch_at` growing without bound, which is correct
behavior (the task legitimately cannot run).

**Workarounds available now:**
1. **Provision more capable workers** on the affected lane — each
   additional capable worker linearly raises P(capable) and
   geometrically shrinks the tail.
2. **Partition caps onto separate lanes** — the canonical fix. A lane
   with homogeneous cap requirements has no mixed-fleet thrash by
   construction.
3. **Lower `unblock_interval`** — at 1 s sweep, wall time per
   stranded task drops 5×. Trade-off: more scanner Valkey load.
4. **Kill incapable workers / set `min_isolation`** — reduces the mix.

## Verdict

**Possible real-world issue under narrow patterns; no natural cairn
pattern produces it; V2 not urgent.**

Key evidence:

- The thrash is a **pure latency tail** on adversarial mixed fleets,
  not a correctness bug. Every stranded task eventually drains.
- The geometric tail flattens fast: at ≥25% capable fraction the 300 s
  deadline is essentially never hit; the bench's 10% and 1% cases are
  explicit stress tests, not capacity targets
  (`cap_routed.rs:86-91, 96-98`).
- Cairn's workers-per-lane homogeneity means a cairn pool sees
  capable fraction ≈ 1.0 per lane. No cairn report because no cairn
  workload produces the shape.
- Workarounds (provision more capable workers; partition caps onto
  separate lanes; shorten sweep) are first-class operator tools.
- The `last_capability_mismatch_at` scalar already gives operators a
  debugging surface if they hit it (`RFC-009-scheduling.md:368`).

The only scenario a real user **would** hit this: a heterogeneous
worker pool where a rare capability is mixed onto a general lane and
the operator either can't or won't partition. That's a lane-design
anti-pattern, documented implicitly by RFC-009's `explain_capability_mismatch`
future API (`RFC-009-scheduling.md:545`) and the mis-design warning
in §4.3.

## Recommendation

**Keep #11 open, trigger-based. Do NOT resume V2 work now.**

Consistent with session posture ("no speculative fixes without
trigger") and the prior status summary
(`rfcs/drafts/rfc-009-status-summary.md:30-36`). Specifically:

1. **Do not preemptively implement Option A or Option B.** Both cost
   non-trivial schema/Lua churn; neither fixes a correctness issue;
   the bench thresholds already match what v1 ships.
2. **Trigger to resume V2:**
   - A consumer (cairn or otherwise) reports stranded tasks in
     production with P(capable) > 0 (i.e., a latency complaint, not
     a true-starvation complaint).
   - We want to advertise `correct_routing_rate ≥ 0.99` under
     adversarial mixes as a 0.x release claim.
   - A heterogeneous-pool consumer (pattern 3 above) appears in the
     roadmap.
3. **When resumed:** start with Option A (worker-connect-triggered
   sweep) — already listed as deferred in RFC-009 §7.5
   (`RFC-009-scheduling.md:400`). Lower blast radius than Option B's
   caps-selectivity priority bias, which requires server-side caps
   access in the Lua claim path.
4. **Docs action (cheap, do now):** add a one-line operator note to
   the RFC-009 §7.5 block-on-mismatch paragraph pointing at the
   "provision more capable workers / partition caps by lane"
   mitigation, so the first consumer who asks has a clear answer
   before V2 ships.

The residual is real, documented, bench-tested, and not currently
hurting any user. Close-as-accepted-limitation is tempting, but
keeping #11 open with a clear trigger is the more honest shape — the
issue will resurface if a real deployment shows mixed-fleet latency
tails, and we want a live tracker in place for that report.
