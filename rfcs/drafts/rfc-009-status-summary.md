# RFC-009 status summary (2026-04-23)

Author: Worker XX (investigation only)
Source: `rfcs/RFC-009-scheduling.md` (created 2026-04-14, last touched 2026-04-14 round-21), issue #11 (open, "RFC-009 deferred"), obsolescence sweep `rfcs/drafts/open-issues-obsolescence-sweep.md`.

## RFC-009 summary

RFC-009 defines the scheduling, fairness, and admission layer that sits between execution state and atomic lease acquisition. Its load-bearing pieces are: (1) the **claim-grant model** — scheduler computes candidate cross-partition, writes a short-lived `ff:exec:{p:N}:<id>:claim_grant` hash, atomic Valkey Function `ff_claim_execution` consumes it; (2) **priority + fairness** — per-lane composite score, cross-lane weighted-deficit round-robin, cross-tenant fair-share with symmetric deficit clamp `[-2w, +2w]`; (3) **capability matching** — V1 ships exact-string subset match with worker-supplied caps in ARGV, plus block-on-mismatch + caps-union unblock scanner; (4) the **lane/queue facade** with 4 states (`intake_open`/`paused`/`draining`/`disabled`) and all submission modes (enqueue, delayed, scheduled, bulk, enqueue_and_wait via XREAD BLOCK on lease_history). It also defines `ff_block_execution_for_admission` / `ff_issue_reclaim_grant` and the admission-rejection → RFC-001 blocking-reason mapping.

## Issue #11 framing

Issue #11 is NOT "RFC-009 is deferred as a whole." It is titled **"RFC-009: cap-routing convergence — V2 options"** and tracks a single known residual in the shipped v1 capability-routing: the `cap_routed --mode partial` bench leaves ~4.5% of exclusive tasks stranded in `lane_blocked_route` at a 300 s deadline because of promote↔reblock thrash driven by the `n_capable / n_total` poll-rate ratio. RFC-009 §7.5 pre-declared v1 as best-effort. Issue proposes two V2 fixes — **Option A**: worker-connect-triggered `blocked_route` sweep (Batch C item 6 in RFC-009 deferred list); **Option B**: priority-bias eligible ZSET by caps-selectivity so capable workers see rare-cap tasks first. Acceptance criteria are explicit: partial-mode ≥ 0.99, scarce-mode ≥ 0.95, happy-mode unchanged at 1.0, no regression in `scheduler_burn_fcalls_per_correct_claim`. The obsolescence sweep (`open-issues-obsolescence-sweep.md:64`) classifies #11 as LIVE: "Explicit acceptance criteria not met; v1 best-effort shipped; V2 Option A / B not implemented."

## What has landed since

RFC-009 was written 2026-04-14 in the initial 10-RFC drop. Since then:

- **RFC-011 (exec/flow hash-slot co-location, 2026-04-18/19, #23/#26/#29).** Changed `ExecutionId` shape to `{fp:N}:<uuid>` and reorganized partitioning around flow co-location. §3.5 "Scheduler / claim routing" is a routing-through amendment, not a redesign — the claim-grant mechanism and fairness model are unchanged. Touches RFC-009 mechanically (keys now under `{fp:N}` for flow execs, `{p:N}` legacy/solo path replaced by `SoloPartitioner`), not semantically.
- **RFC-012 (EngineBackend trait, #102 / #107 / #114 / #119 / amendments).** Stage 1a moved FCALL ops behind a backend trait; scheduler is a consumer of the trait. Claim-grant / issue-grant primitives keep their semantics, they just route through a backend method now.
- **Scheduler perf work (#43, PR #86, SHA `183c10f`).** Bounded partition scan + rotation cursor — implements the partition-affine rotation that RFC-009 §Open Questions §1 anticipated.
- **Scheduler-routed claim endpoint (PR #42, `7c653f9`).** Realized the scheduler side of the claim cycle end-to-end in `ff-scheduler`, matching RFC-009 §3.1.
- **#122 / ScannerFilter + namespace hook (closed).** Unrelated to RFC-009 — key-isolation for multi-tenant, did not touch scheduling.
- **cap_routed bench thresholds (`17426db`).** Relaxed per-mode targets to match the documented v1 best-effort floor; this is what #11 is tracking to tighten back up in V2.

## Overlap verdict

**Intact — not subsumed.** RFC-009's design surface (claim-grant, fairness, capability matching, lane facade, admission-rejection mapping) has been implemented, not replaced. RFC-011 re-partitioned the keyspace underneath RFC-009's primitives without changing its mechanism. RFC-012 wrapped the FCALLs in a trait without changing their semantics. #122 is orthogonal. The residual tracked by #11 is one bullet under RFC-009 §7.5 "deferred to V2" (worker-connect-triggered sweep) + one novel idea (caps-selectivity priority bias); it is not an obsoleting of RFC-009 itself.

## Recommendation

**Keep #11 open; keep RFC-009 as an accepted RFC; reframe the discussion.** Three concrete actions for the manager conversation:

1. **Relabel #11.** The issue title ("V2 options") is accurate, but the session shorthand "RFC-009 deferred" is misleading — it makes RFC-009 sound like an unshipped RFC when in fact RFC-009 is shipped and #11 is a V2 optimization ticket. Consider retitling to `cap-routing convergence: worker-connect sweep + caps-selectivity bias (RFC-009 V2)` so future readers don't assume the whole RFC is deferred.
2. **Pick a trigger, not a schedule.** V2 work is not time-critical — happy-mode is 1.0, scarce/partial are documented best-effort with bench thresholds relaxed to match. Trigger candidates: (a) a consumer reports stranded tasks in production, (b) cairn-fabric's cap-routing workload tightens requirements, (c) we want to advertise `correct_routing_rate ≥ 0.99` under adversarial distributions as a release claim. Without one of these, this is backlog.
3. **When picked up, start with Option A.** RFC-009 §7.5 already names "Worker-connect-triggered `blocked_route` sweep" as a deferred item — that's the lower-blast-radius fix (local to worker startup, no schema change, no new Lua path). Option B (caps-selectivity priority bias) requires server-side caps-union access in the Lua claim path and is a bigger schema change; defer until Option A's production telemetry proves it insufficient.

No edits to RFC-009, no PR. Discussion-ready.
