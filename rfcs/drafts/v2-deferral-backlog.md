# v2 deferral backlog (post-v0.5.0)

Sweep date: 2026-04-23. Scope: `rfcs/**`. Excludes smoke reports, audit records, release-saga, session-end, obsolescence-sweep, and per-round K/L/M/P challenge logs.

## Summary

Sweep covered 12 primary RFCs (`RFC-001`..`RFC-012` + amendments) and 16 drafts (observability-batteries, tower-layer-surface, handler-di, apalis-comparison/deep-dive, ff-board-sdk-prerequisites, stage-1c-plan / scope-audit, 87-88-scope-carveout, RFC-012-amendment-namespace, rfc-009-v2-real-world-analysis, rfc-009-status-summary, post-stage-1c-test-coverage, issue-43-scheduler-perf, bridge-event-audit, README, 0.4.0-release-prep-checklist). The numbered `0.4.0-*-smoke/audit/readiness/runtime`, `main-*`, `scenario-*`, `release-saga`, `session-end`, `open-issues-obsolescence-sweep`, and `RFC-012-amendment-*-challenge` files were excluded per the task's scope rules.

Raw counts after dedup:

- **A. Large items (RFC-worthy):** 11
- **B. Trait/API additions:** 14
- **C. Operational / observability:** 10
- **D. Open design questions (no decision yet):** 18
- **E. Already-obsolete deferrals (close out):** 6
- **F. Aspirational / uncertain:** 7

**Total open for v2 consideration: ~53 distinct items** (after collapsing the many `reason_code`-level deferrals inside RFC-004/005 into single bullets). **Candidate v2 bundle** (large items with named triggers or blocked-on-consumer-ask): ~8 items in section A. **Candidate incremental v0.5.x work** (small, scoped, no design debate needed): ~14 items across B and C. Section E lists items to close out — the biggest surprise is that RFC-010 §9.2 still lists **SQLite local mode** and **multi-backend abstraction** as "deferred because not yet built," which RFC-012 + Stage 1c now partially invalidate; the wording should be updated.

The other surprise: RFC-012 §R7.6.1 defers `suspend` to Stage 1d, and the amendment-117 M-challenge already adjudicates `(b) defer` — but Stage 1d itself has no RFC. Promoting Stage 1d to an RFC is arguably the largest single v2 design surface still undocumented.

---

## A. Large items (RFC-worthy on their own)

- **[RFC-012 §R7.6.1 / §R7.1]** *Stage 1d: `suspend` trait migration + `ConditionMatcher`↔`WaitpointSpec` / typed `ResumeCondition` input-shape rework.* `crates/ff-backend-valkey/src/lib.rs:1240-1249` is stubbed `EngineError::Unavailable` today; SDK `parse_suspend_result` at `task.rs:1557-1562` constructs `Suspended` exhaustively from wire bytes with no handle access. Unblocked by: owner decision on the `SuspendOutcome` shape (Option (a) shape-reservation vs (b) defer, M-challenge leans (b)); requires parallel SDK forwarder rewrite.

- **[RFC-010 §9.2 + RFC-012 §5.2 / §R7.2]** *Experimental `ff-backend-postgres` (Stage 2, formerly Stage 3).* "A Postgres backend becomes implementable as a drop-in crate" (RFC-012 §1.2 line 87). Unblocked by: Stage 1 trait landing (done), Stage 1c migration (in progress). Namespace amendment already designed for `pg_catalog.pg_namespace` check (RFC-012-amendment-namespace.md:216).

- **[RFC-010 §9.2]** *SQLite local mode.* "The partition model and key schema are designed to be implementable on a single-node SQLite backend ... Deferred because Valkey is the primary backend and the abstraction layer is not yet built." (RFC-010:2882). Now partially unblocked by RFC-012; needs its own RFC.

- **[RFC-001 §Designed-for-deferred line 1857 + RFC-009 §7 row `enqueue_scheduled` line 472]** *Recurring scheduled execution (cron / schedule loop).* "interface exists, scheduler loop deferred." V1 requires external cron; no trigger beyond consumer ask.

- **[RFC-010 §9.2 line 2894, 2913-2929]** *Cross-execution event feed / per-execution unified lifecycle event stream.* Recommended v1 extension that was deferred. Addresses UC-55, simplifies `enqueue_and_wait`, enables real-time dashboards. Current fragmentation across 4 sources (lease history, waitpoint signal stream, suspension record, exec_core) is documented inline.

- **[RFC-009 §7.5 line 392-403 + rfc-009-v2-real-world-analysis.md + rfc-009-status-summary.md]** *Capability-routing V2: Option A (worker-connect-triggered `blocked_route` sweep) and Option B (caps-selectivity priority bias).* Tracked under #11 (closed as accepted-limitation 2026-04-22). Trigger: production consumer reports stranded tasks in adversarial cap distribution.

- **[RFC-009 §7.5 line 392]** *Worker attestation (HMAC signing of worker caps).* Enables option (b) where Lua reads an authoritative cap store. Explicitly deferred; required for untrusted-worker threat model.

- **[RFC-draft-observability-batteries §3 line 120-146 + ff-board-sdk-prerequisites.md]** *Web UI admin board (ff-board).* "A web UI in 0.5 — Defer to 0.6+ RFC" (line 158). Prereq list (read-APIs, OTEL sink decision) exists; write-ops (cancel/reclaim/signal) explicitly deferred to v2 (ff-board-sdk-prerequisites.md:36).

- **[RFC-012 §3.2 line 242 + §1.2 bullet 6]** *`StreamBackend` / `CompletionBackend` trait extraction (issues #90, #92).* RFC-012 names StreamBackend's existence; its shape lives in the #92 follow-up RFC. Stream cursors need a cursor type that XRANGE markers don't map to row-based backends without a translation layer. CompletionBackend covers completion pubsub (#90).

- **[RFC-011 §11 line 977]** *Consumer-group-based bridge-event delivery.* "Orthogonal to §5.5 atomicity. Cairn's bridge-event observation story is a separate design surface; filed as a potential follow-up RFC if cairn's current call-then-emit pattern proves inadequate in production." Trigger: production pain report from cairn.

- **[RFC-012 §3.2 + §7.19 + §7.20 line 1119-1123]** *`AdminBackend` trait (not defined in RFC-012).* Named as the home for `describe_signals(execution_id)` cross-execution observation, `issue_claim_grant` escape hatch (§7.1), and other admin-scope ops. Shape undefined.

---

## B. Trait/API additions

- **[RFC-012 §7.4 line 1002-1006]** *`Batch`/`submit_batch` op for multi-execution submission.* "Lean: defer. Add when a consumer asks."

- **[RFC-012 §7.14 line 1066-1080]** *Split `BudgetBackend` from `EngineBackend` for LLM-heavy consumers.* "Keep `report_usage` on `EngineBackend`... revisit with evidence" if real LLM-heavy consumer surfaces with batching/pool needs.

- **[RFC-012 §6.6 line 717]** *Shared impls across backends via a base `trait Backend { default impls } + trait EngineBackend: Backend`.* "Deferred to post-Stage-3 when the Postgres backend has materialised and shared surfaces become visible."

- **[RFC-012 §7.15 + §8.19]** *Native `async fn` in traits (drop `#[async_trait]`).* "Revisit in the post-2024-edition-stabilised era when native `async fn` gains object-safety support; the revisit is additive."

- **[RFC-012 §7.17 / §8.20 line 1088-1096]** *Swap `Handle.opaque` from `Box<[u8]>` to `bytes::Bytes`.* "Additive From impls let us swap to `Bytes` later without a breaking change."

- **[RFC-012 §4.1 line 505 + §8 response line 1160]** *Compile-time phantom-typed `Handle<Fresh>` / `Handle<Resumed>`.* "Reviewers who want compile-time phantom-typed `Handle<Fresh>` vs `Handle<Resumed>` can argue it as additive later; it is not needed now."

- **[RFC-001 §Designed-for-deferred line 1858-1862]** *Bulk submission API, tag-based search indexes, advanced retention policies, cross-lane fairness scheduler, soft-limit budgets with escalation workflows.*

- **[RFC-002 §Designed-for-deferred line 888-894]** *Attempt history compaction, attempt-level metrics aggregation pipelines, cross-attempt diff views, attempt-level access control, attempt archival to external storage.*

- **[RFC-003 line 961-965]** *Cooperative atomic lease handoff, richer per-class lease-duration policy, selective relaxation for progress/stream appends without full lease validation.*

- **[RFC-004 line 1197-1202 + line 1224]** *Ordered signal matching, richer operator-driven condition rewrites, multi-waitpoint suspension graphs inside one episode, cross-execution waitpoint joins. Also: synthetic timeout-signal injection, standardized checkpoint envelope for continuation metadata (line 1212).*

- **[RFC-005 §Designed-for-deferred line 772-779]** *Multi-signal resume conditions (`all_of`, `count(n)`) with complex evaluation, signal routing to flow coordinator, signal payload schema validation, signal TTL independent of waitpoint, signal replay / re-evaluation after policy change, bulk signal delivery API.*

- **[RFC-006 §Designed-for-deferred line 657-665]** *`durable_summary` and `best_effort_live` durability modes, consumer groups for multi-consumer coordination, server-side `frame_type` filtering, stream compression/deduplication, cross-execution stream search, stream archival to external storage (S3), frame-level access control.* Also: chunked-transfer/SSE for tail (line 735, 770); `xread_block` per-call dedicated socket to remove Mutex on tail_client (line 826-832); sentinel close-path emit (line 884); client-side frame-level sequence idempotency (line 88).

- **[RFC-007 line 196-201 + line 854-860]** *Any-of dependencies, quorum dependencies, threshold joins, richer multi-edge semantics, multi-flow membership, coordinator-authored custom aggregate completion logic. Also: fan-out batching mitigation (line 852), resume-path dependency checks (line 309), replay-reopens-downstream graph reconciliation (line 868).*

- **[RFC-008 §Designed-for-deferred line 763-774 + §Known-limitations line 794]** *Soft limits with graduated enforcement, budget reservation/precharge semantics, `reroute_fallback` and `suspend` enforcement, `close_scope` enforcement, per-provider quota policies, cost injection from routing policy, fixed-window rate limiting, cross-partition stronger consistency, priority-aware budget reservation.* PagerDuty/Slack/webhook integration for `escalation_target` noted at line 89.

---

## C. Operational / observability

- **[RFC-010 §9.4 line 3002]** *Engine-level per-operation authentication (signed tokens, per-worker ACLs) — post-v1 extension.* Trusted-network assumption currently load-bearing.

- **[RFC-010 §9.4 line 2978]** *Namespace-based access control enforced at engine level.* Today namespace is metadata; enforcement delegated to API gateway.

- **[RFC-010 §9.2 line 2892]** *Public API wire format (gRPC/REST/SDK) for the engine — transport layer deferred.*

- **[RFC-004 line 1171]** *Per-(execution_id, source) token-bucket admission at the REST boundary for signal traffic.* "Deferred v2 feature if operators see partition starvation from hostile signal traffic."

- **[RFC-009 §7.5 line 394-401]** *Semver predicates (`torch>=2.3` parsed as a range), key-value capability attributes (`provider=anthropic`), preferred/forbidden capabilities + scoring, secondary ZSET index per capability, operator `explain_capability_mismatch` API, isolation level and locality matching, reclaim scanner integration for `ff_issue_reclaim_grant`.*

- **[RFC-010 §8.3 line 2678]** *Extraction of `ff-scheduler` crate to a separate process (partition-affine scheduling at high scale).* Embedded in ff-server in v1.

- **[RFC-010 §6.11 area line 912]** *Per-lane partition bitmap tracking non-empty eligible sets (scanner optimization) / random partition sampling for high partition counts.*

- **[RFC-draft-observability-batteries line 210]** *Bundled Grafana dashboard JSON (0.5.x if release train has room; defer if 0.5.0 is full).*

- **[RFC-draft-handler-di §3.3 line 147-151]** *`ExecutionPolicy`, `Capabilities`, `Payload<T>` extractors.* "Deliberately NOT in 0.5.0. Hot take; defer to user feedback."

- **[RFC-draft-handler-di §3.3 line 155]** *`#[derive(FromTask)]` macro for composed extractor structs.* "Low priority, can land later."

---

## D. Open design questions (no decision yet)

- **[RFC-001 §Open-Questions line 1868]** *Reclaim grace period: configurable grace period between lease expiry and reclaim eligibility?*

- **[RFC-002 §Open-Questions line 898]** *Compaction threshold for attempt records — product/operational decision.*

- **[RFC-003 §Open-Questions line 971-972]** *(1) Should lease-duration policy resolve primarily from execution policy, lane defaults, or merged policy with hard caps? (2) Should `lease_revoked` render as a distinct operator-facing state or remain an ownership-state nuance?*

- **[RFC-006 §Open-Questions line 671]** *Merged view pagination: opaque cursor or structured `(attempt_index, sequence)` tuples?*

- **[RFC-007 §Open-Questions line 862-868]** *(1) Should `aggregate_threshold` completion be a first-class v1 policy or remain designed-for? (3) How much edge-level data-passing metadata should be standardized vs left opaque? (4) Replay-of-completed-upstream graph-reconciliation model.*

- **[RFC-009 §Open-Questions line 972]** *Flow-aware priority boosting — should the scheduler inherit or boost priority from the parent flow / coordinator execution?*

- **[RFC-011 §12.1 line 983-988]** *`num_flow_partitions` default: 256 vs 512 vs 1024.* Decision protocol: phase 5 benchmarks against 6-node clusters.

- **[RFC-011 §12.2 line 990-995]** *`ExecutionIdParseError` variant granularity — which error callers most want to handle distinctly.*

- **[RFC-011 §12.3 line 997-1005]** *Custom `SoloPartitioner` adoption trigger + default-partitioner algorithm revision if ≥3 operators independently report collisions.*

- **[RFC-012 §7.1 + §7.2 + §7.8 + §7.11 + §7.12 + §7.13]** *Claim split vs combined (leaning combined); `CapabilitySet` bitfield vs stringly-typed (leaning stringly); trait location (leaning new `ff-backend-api` crate); feature-flag the trait (leaning no); `Backend` super-trait composition; backend-specific extension points.* All open with leans, not locked.

- **[RFC-012 §R7.6 line 916-918]** *`suspend` deferred — Stage 1d tracking (still open per §R7.6.1).*

- **[RFC-draft-handler-di §9 line 285-297]** *6 owner questions:* (1) Missing-state policy compile-time vs run-start, (2) Extractor failure → fail vs skip, (3) `WorkerRuntime` vs `WorkerBuilder` naming, (4) Spawn policy override, (5) Handler return `IntoCompleteOutput` blanket, (6) Feature-flag name.

- **[RFC-draft-tower-layer-surface §7 line 279-306]** *7 owner questions:* (1) Default feature set (leaning `default = []`), (2) Circuit-breaker trigger scope, (3) Rate-limit granularity, (4) `MetricsSink` vs `ff-observability` bridge, (5) Layer-ordering enforcement for `PanicCatchLayer`, (6) Semver of `EngineBackendLayer` trait, (7) Higher-op trait interplay.

- **[RFC-draft-tower-layer-surface §3.6 line 183-199]** *"Explicitly NOT bundled" — `RetryLayer` (reconsider in v2 with clear name `TransportRetryLayer`), `TimeoutLayer` (rejected permanently), Sentry/Prometheus exporter layers (rejected per OTEL lock).* v2 reconsideration target: `RetryLayer` only.

- **[RFC-draft-observability-batteries §Open-questions line 217-239]** *5 owner questions:* Sentry feature naming, `metrics-endpoint` crate location, web-UI commitment (0.6+ deferred acceptable?), Grafana JSON in-tree vs out, env-var prefix.

- **[ff-board-sdk-prerequisites §7 line 139-146]** *Decisions on read-API surface (list-executions, list-suspended, lane throughput, lease-renewal) — cross-backend X1/X2/X3 owner locks needed.*

- **[stage-1c-plan-draft §Open-questions line 117-131]** *6 questions:* `WorkerConfig::new` back-compat shim keep-or-break, `BackendTimeouts::keepalive` ferriskey gap, `#88` error-type wrapping bundle-or-separate, `#117` timing, feature-flag groupings, CI feature-matrix.

- **[87-88-scope-carveout §Open-questions line 95-103]** *Cross-backend sealing questions; `ff-readiness-tests::probe_server_version` migration to backend trait at Stage 1c+.*

---

## E. Already-obsolete deferrals (landed in v0.4.x / v0.5.0 — close out)

- **[RFC-010 §9.2 line 2884]** *"Multi-backend abstraction ... deferred because v1 is Valkey-native and the interface boundary is not yet hardened."* RFC-012 Stages 0-1 have landed the boundary. Update RFC-010 §9.2 text.

- **[RFC-012 §R7.2 / §R7.3]** *`create_waitpoint`, `append_frame` return widen, `report_usage` return replace.* Landed in round-7 amendment (3 of 4 Stage-1b deferrals closed per §R7.1). Only `suspend` (Stage 1d) remains.

- **[RFC-012 §R7 `AdmissionDecision`]** *`AdmissionDecision` type removed.* Completed in round-7 (§R7.3). Close out.

- **[apalis-deep-architecture-dive recommendation #1]** *`SharedFfBackend` factory + worker one-liner.* Recommended for 0.4.x fold-in; check against current v0.5.0 SDK.

- **[RFC-009 §7 row `enqueue_scheduled` line 472 "V1"]** *Single delayed execution creation.* Landed. The recurring-creation bullet stays open (see A).

- **[rfc-009-status-summary line 20]** *Scheduler perf work (#43, PR #86) — bounded partition scan + rotation cursor.* "Implements the partition-affine rotation that RFC-009 §Open Questions §1 anticipated." Close RFC-009 §Open Q1 if not already.

---

## F. Aspirational / uncertain

- **[RFC-007 §Open-Q line 866]** *`aggregate_threshold` as first-class v1 policy.* "Remain designed-for until a concrete product requires it."

- **[RFC-010 §9.2 line 2888]** *Pub/sub for real-time notifications (SUBSCRIBE channels) instead of polling.* V1 uses polling; "can reduce latency for request/reply and live dashboards" but no driving consumer.

- **[RFC-010 §9.2 line 2886]** *Global secondary indexes (cross-partition search by tag, tenant, time range).* "Full-text or faceted search requires an external index." No consumer ask.

- **[RFC-001 §Designed-for-deferred line 1859]** *Tag-based search indexes — "optional, may start without."* Candidate for punt or delete.

- **[RFC-012 §7.16 + §8.20 line 1125-1138]** *Postgres-first prototype before ratifying trait.* Closed as "trait-first with Stage 3 as pressure-test" but the alternative-worldview remains recorded. Unlikely to re-open.

- **[apalis-deep-architecture-dive §Recommendations line 292-303]** *Medium-scope 0.5.0 items:* FromRequest-style extractors on higher-op trait (#135 — overlaps with RFC-draft-handler-di), client-local tower-compatible layer surface (overlaps with RFC-draft-tower-layer-surface), capability-matrix `features_table!`-style rustdoc.

- **[RFC-011 §9.7 line 460]** *`PartitionConfig::with_solo_partitioner` ergonomic wrapper.* "Defer ... to a follow-up RFC if operator demand emerges post-phase-5 benchmarks." No demand signal yet.

---

## Notes

- RFC-012 §R7.6.3 names only `create_waitpoint` for Stage 1b; the naming trail is preserved in `RFC-012-amendment-117-deferrals.L-challenge.md`.
- Several "post-v1" mentions in RFC-008 (lines 331-333, 800, 802) use "post-v1" as a temporal anchor that now means "post-v0.5.0" given FlowFabric's current release lane.
- RFC-009 §7.5's `block-on-mismatch` invariant applies to the reclaim path (line 401) but is still listed as deferred — this is an architectural-invariant commitment with no code; belongs in B if Worker attestation ever lands.
- "Trusted network assumption" (RFC-010 §9.4 line 3002) is foundational; any hardening proposal replaces it rather than extending.
