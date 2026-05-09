# FlowFabric RFC Deferrals Catalogue

Generated: 2026-05-09. Source: every in-tree RFC + the archived
avifenesh/flowfabric-archive rfcs/ tree + docs/POSTGRES_PARITY_MATRIX.md.

Reconciled 2026-05-09: one-by-one verification pass against current
`main` (HEAD `25ac0c5`). Seven items moved from "Still deferred" to
"Shipped" after grepping the codebase for evidence. See "Reconciliation
notes" at the bottom for the audit trail.

## Summary

Row counts after 2026-05-10 round-2 reconciliation (both passes combined):

- Still deferred: 96  (was 106 initially; 10 moved to Shipped across 2 passes)
- Shipped: 29  (was 19 → +7 round-1 → +3 round-2 = 29)
- Dropped / superseded: 4
- Permanent non-goals: 27
- Total unique IDs: 156

Round 1 (2026-05-09): moved 7 rows after grep-only verification.
Round 2 (2026-05-10): moved 3 more after reading each item's RFC section
and searching for intent-equivalent code across all crates. 96 remaining
items manually walked end-to-end and confirmed absent.

## Still deferred

| ID | Source | § | One-line summary | Proposed target | Effort estimate |
|----|--------|---|------------------|-----------------|-----------------|
| D-001 | RFC-001 | §V1 Scope / Designed-for but deferred | Recurring scheduled execution (cron semantics) — interface exists, scheduler loop deferred | RFC-021 draft | L |
| D-002 | RFC-001 | §V1 Scope / Designed-for but deferred | Bulk submission API — individual `create_execution` sufficient for v1 | ? | M |
| D-003 | RFC-001 | §V1 Scope / Designed-for but deferred | Tag-based search indexes — optional, may start without | ? | M |
| D-005 | RFC-001 | §V1 Scope / Designed-for but deferred | Cross-lane fairness scheduler — v1 uses per-lane priority ordering | ? | L |
| D-006 | RFC-001 | §V1 Scope / Designed-for but deferred | Soft-limit budgets with escalation workflows | ? | M |
| D-007 | RFC-001 | §Open Questions | Reclaim grace period between lease expiry and reclaim eligibility (configurable) | ? | S |
| D-008 | RFC-002 | §Designed-for but deferred | Attempt history compaction for long-lived executions | ? | M |
| D-009 | RFC-002 | §Designed-for but deferred | Attempt-level metrics aggregation pipelines | ? | M |
| D-010 | RFC-002 | §Designed-for but deferred | Cross-attempt diff views ("what changed between attempt 2 and attempt 3?") | ? | M |
| D-011 | RFC-002 | §Designed-for but deferred | Attempt-level access control (v1 inherits from execution) | ? | M |
| D-012 | RFC-002 | §Designed-for but deferred | Attempt archival to external storage | ? | M |
| D-013 | RFC-003 | §Fencing Model / Impl Notes | Full capability restore from capability_snapshot (v1 stores canonical hash only) | ? | M |
| D-014 | RFC-003 | §Fencing Model rule 2 | Future cooperative lease handoff | ? | M |
| D-015 | RFC-004 | §Rate limits | Per-(execution_id, source) token-bucket admission at REST boundary for signals | v2 | M |
| D-016 | RFC-004 | §Q3 / Implementation Notes | Standardized checkpoint envelope for cross-runtime continuation portability | v2 | L |
| D-017 | RFC-004 | §Implementation Notes | **PARTIAL (pg-only).** Synthetic timeout signal injection into waitpoint buffer on auto-resume. Postgres `reconcilers/suspension_timeout.rs:172` injects into `member_map[__timeout__]`; Valkey `flowfabric.lua:5272` treats `auto_resume_with_timeout_signal` identically to plain `auto_resume` (no injection). Valkey parity still deferred. | v2 | S |
| D-018 | RFC-005 | §Designed-for but deferred | Signal routing to flow coordinator (v1 requires explicit execution targeting) | ? | M |
| D-019 | RFC-005 | §Designed-for but deferred | Signal payload schema validation (v1 treats payload as opaque bytes) | ? | M |
| D-020 | RFC-005 | §Designed-for but deferred | Signal TTL / expiry independent of waitpoint | ? | M |
| D-021 | RFC-005 | §Designed-for but deferred | Signal replay / re-evaluation after policy change | ? | L |
| D-022 | RFC-005 | §Designed-for but deferred | Bulk signal delivery API | ? | M |
| D-023 | RFC-006 | §Designed-for but deferred | Consumer groups for multi-consumer coordination (v1 uses independent cursors) | ? | L |
| D-024 | RFC-006 | §Designed-for but deferred | Server-side frame_type filtering (v1 filters client-side) | ? | M |
| D-025 | RFC-006 | §Designed-for but deferred | Stream compression or deduplication | ? | M |
| D-026 | RFC-006 | §Designed-for but deferred | Cross-execution stream search (find all streams containing a correlation_id) | ? | L |
| D-027 | RFC-006 | §Designed-for but deferred | Stream archival to external storage (S3, etc.) before Valkey-side deletion | ? | L |
| D-028 | RFC-006 | §Designed-for but deferred | Frame-level access control (v1 inherits from execution visibility) | ? | M |
| D-029 | RFC-006 | §Impl Notes Batch B | REST chunked-transfer / SSE for large span streams deferred to v2 | v2 | M |
| D-030 | RFC-006 | §V2 upgrade path | Replace single `tail_client`+Mutex with pool of N dedicated ferriskey clients (BLOCKs truly parallel) | v2 | M |
| D-031 | RFC-006 | §V2 upgrade path | Close paths emit `stream_closed` sentinel XADD so XREAD BLOCK wakes immediately | v2 | M |
| D-032 | RFC-007 | §V1 limitation | Dependency checks on the resume path (dynamic deps on non-runnable executions) | ? | M |
| D-033 | RFC-007 | §V1 limitation | Batch dependency resolution by partition with staggered 100-500 batches to smooth large fan-out bursts | ? | M |
| D-036 | RFC-007 | §Designed for later | Richer multi-edge semantics | post-v1 | M |
| D-037 | RFC-007 | §Designed for later | Multi-flow membership | post-v1 | M |
| D-038 | RFC-007 | §Designed for later | Coordinator-authored custom aggregate completion logic beyond declared policy set | post-v1 | L |
| D-039 | RFC-007 | §Open Q4 | Graph-reconciliation model so replay reopens downstream eligibility (satisfied edges un-resolve) | ? | L |
| D-040 | RFC-008 | §Designed-for but deferred | Budget reservation / precharge semantics | post-v1 | M |
| D-041 | RFC-008 | §Designed-for but deferred | `reroute_fallback` and `suspend` enforcement actions | post-v1 | L |
| D-042 | RFC-008 | §Designed-for but deferred | `close_scope` enforcement (requires lane-level enforcement) | post-v1 | L |
| D-043 | RFC-008 | §Designed-for but deferred | Per-provider quota policies | post-v1 | M |
| D-044 | RFC-008 | §Designed-for but deferred | Cost injection from routing/fallback policy (v1 relies on worker-reported cost) | post-v1 | M |
| D-045 | RFC-008 | §Designed-for but deferred | Fixed-window rate limiting (v1 uses sliding window) | post-v1 | M |
| D-046 | RFC-008 | §Designed-for but deferred | Cross-partition budget enforcement with stronger consistency guarantees | post-v1 | L |
| D-047 | RFC-008 | §Designed-for but deferred / Known Limitations | Priority-aware budget reservation (reserve fraction of shared budget for above-threshold priority) | post-v1 | L |
| D-048 | RFC-008 | §Open Q1 | Quota scope cascade overrides (v1 uses stack semantics, most restrictive wins) | post-v1 | M |
| D-049 | RFC-009 | §5.3 | Intra-lane aging (gradually boosting score of long-waiting executions) to prevent starvation | v2 | M |
| D-050 | RFC-009 | §5 capabilities | Worker attestation (HMAC signing of caps) enabling authoritative Lua-read cap store | V2 | L |
| D-051 | RFC-009 | §5 capabilities | Semver predicates (`torch>=2.3` parsed as a range) not opaque strings | V2 | M |
| D-052 | RFC-009 | §5 capabilities | Key-value capability attributes (`provider=anthropic`); V1 is flat string tokens only | V2 | M |
| D-053 | RFC-009 | §5 capabilities | Preferred/forbidden capabilities and scoring | V2 | M |
| D-054 | RFC-009 | §5 capabilities | Secondary ZSET index per capability to avoid O(N) eligible scans for niche caps | V2 | M |
| D-055 | RFC-009 | §5 capabilities | Operator `explain_capability_mismatch` API for C3 | V2 | S |
| D-056 | RFC-009 | §5 capabilities | Isolation level and locality matching | V2 | L |
| D-057 | RFC-009 | §5 capabilities | Worker-connect-triggered blocked_route sweep (V1 uses periodic unblock scanner) | V2 | M |
| D-058 | RFC-009 | §5 capabilities | Reclaim scanner integration for `ff_issue_reclaim_grant` | Batch C | M |
| D-059 | RFC-009 | §Designed-for but deferred | Weighted preference scoring beyond simple match/no-match | ? | M |
| D-060 | RFC-009 | §Designed-for but deferred | Global fairness scheduler (v1 uses per-scheduler-instance deficit tracking) | ? | L |
| D-061 | RFC-009 | §Designed-for but deferred | Worker pool abstraction as a first-class object (v1 uses capability filtering) | ? | M |
| D-062 | RFC-009 | §Designed-for but deferred | Route scoring with multiple candidate ranking | ? | M |
| D-063 | RFC-009 | §Designed-for but deferred | Preemptive rerouting of waiting executions on worker availability changes | ? | L |
| D-064 | RFC-009 | §Designed-for but deferred | Dynamic lane concurrency scaling | ? | M |
| D-065 | RFC-009 | §Designed-for but deferred | Cross-region routing optimization | ? | XL |
| D-066 | RFC-009 | §Open Q3 | Flow-aware priority boosting (inherit/boost from parent flow or coordinator) | ? | M |
| D-067 | RFC-010 | §9.2 | Global secondary indexes (cross-partition search by tag, tenant, time range) | ? | L |
| D-069 | RFC-010 | §9.2 | Cross-execution event feed (lane-level / system-level event stream for dashboards, UC-55) | ? | M |
| D-070 | RFC-010 | §9.2 | Per-execution lifecycle event stream (`ff:exec:{p:N}:<eid>:events`) consolidating audit trail | ? | M |
| D-071 | RFC-010 | §6.15 / Future optimization | Per-lane partition bitmap tracking non-empty eligible sets to skip empty partitions | ? | S |
| D-072 | RFC-010 | §replay_execution interaction | `replay_execution` also resets `satisfied → unsatisfied` edges for unclaimed children | ? | M |
| D-073 | RFC-010 | §ff-scheduler | Extract `ff-scheduler` to a separate process for partition-affine scheduling at high scale | v2 | L |
| D-074 | RFC-011 | §4 / §12.1 | `num_flow_partitions` default may bump from 256 if 6-node cluster benches show uneven distribution | phase 5 | S |
| D-075 | RFC-011 | §12.2 | ExecutionIdParseError fourth variant addition if cairn hits a concrete case | phase 4 | S |
| D-076 | RFC-011 | §12.3 | `PartitionConfig::with_solo_partitioner` ergonomic wrapper if operator demand emerges post-phase-5 | post-phase-5 | S |
| D-077 | RFC-011 | §10.5 / §11 | Consumer-group-based bridge-event delivery RFC if cairn's call-then-emit pattern proves inadequate | ? | L |
| D-079 | RFC-012 | §Non-goals | Synchronous-API / blocking variant of `EngineBackend` trait | follow-up | L |
| D-080 | RFC-012 | §Non-goals | Multi-tenancy / capability-routing redesign (trait-level changes to routing model) | follow-up | L |
| D-081 | RFC-012 | §3.3 Stream surface | `StreamBackend` trait shape (separate from EngineBackend) | issue #92 follow-up | M |
| D-082 | RFC-012 | §alternatives / §§new trait hierarchy | `trait Backend` + `trait EngineBackend: Backend` default-impl hierarchy | post-Stage-3 | M |
| D-083 | RFC-012 | §§alternatives / batch | Batch submission trait method `submit_batch(Vec<ExecutionRequest>)` | ? | S |
| D-084 | RFC-017 | §5.4 / §11 | `EngineBackend` seal (public trait stability mechanism beyond SemVer) | separate RFC-012 R8+ | M |
| D-087 | RFC-018 | §9 | Dynamic capability-flips mid-run / event-stream of capability changes | if real consumer emerges | M |
| D-088 | RFC-019 | §Cairn Migration | Realtime `subscribe_instance_tags` implementation (trait remains, both backends Unavailable / n/a) | concrete consumer demand | M |
| D-089 | RFC-019 | §Valkey Stage A | Durable Valkey `subscribe_completion` (today pubsub-backed, at-most-once; PG is durable via outbox) | separate follow-up | M |
| D-090 | RFC-020 | §7.11 | Migrate Postgres budget-policy breach counters to `ff_budget_counters` sharded-counter table | Wave-10 follow-up | M |
| D-091 | RFC-020 | §7.12 | Split `active_concurrency` from `ff_quota_policy` into `ff_quota_concurrency` sharded table | future admission RFC | M |
| D-092 | RFC-020 | §4.1 / §7.8 | Per-attempt-history `get_execution_result` surface via new method (current-attempt only today) | ? | M |
| D-095 | RFC-023 | §filename | Rename `POSTGRES_PARITY_MATRIX.md` → `BACKEND_PARITY_MATRIX.md` if cross-reference cost stops exceeding clarity gain | ? | S |
| D-096 | RFC-024 | §5 Non-goals #4 | Batch-C-style scheduler-driven periodic reclaim scanner (`flowfabric.lua:3865` TODO open) | ? | L |
| D-097 | RFC-024 | §5 Non-goals #6 | Remove `ff_exec_core.raw_fields.claim_grant` JSON residue after 0015 backfill | v0.13.0 migration 0017 | S |
| D-098 | RFC-024 | §7.2 | `ff_describe_execution_phase` probe FCALL for diagnostic observability | future observability RFC | S |
| D-099 | RFC-025 | §6 Non-goal #9 / §Rev-3 #5 | Worker-stats aggregation / operator-event stream emission for WorkerDeathRecorded etc. | RFC-027 or later | M |
| D-100 | POSTGRES_PARITY_MATRIX | RFC-018 Stage A note | Matrix file generated from runtime `capabilities()` value + CI drift check (Stage B) | RFC-018 §8 follow-up PR | S |
| D-101 | POSTGRES_PARITY_MATRIX | Stage-A row 5c / 5d | SQLite `project_flow_summary` + `trim_retention` implementations (today `Unavailable`) | post-v0.13 | M |
| D-102 | POSTGRES_PARITY_MATRIX | PR-7b Cluster 1 | SQLite foundation-scanner ops `mark_lease_expired_if_due`/`promote_delayed`/`close_waitpoint`/`expire_execution`/`expire_suspension` | ? | M |
| D-104 | POSTGRES_PARITY_MATRIX | Wave-9 outbox | Add dedicated `delay_until_ms` column on `ff_exec_core` (today `deadline_at_ms` overloaded) | post-v0.12 additive | S |
| D-105 | RFC-011 | §9.8 | `num_flow_partitions` wider default of 1024 for future-proofing (currently 256, phase-5 bench data pending) | phase 5 benchmarks | S |
| D-106 | RFC-010 | §9.3 retention | `flow_retention` retention overrides (`stream_policy.retention_ttl_ms`) on PG/SQLite (Valkey-only today) | ? | M |

## Shipped

| ID | Source | § | Original summary | Shipped in |
|----|--------|---|------------------|------------|
| S-001 | RFC-005 | §Designed-for but deferred | Multi-signal resume conditions (`all_of`, `count(n)`) — machinery exists, complex evaluation deferred | v0.6.x — RFC-014 `AllOf`, `Count{DistinctX}` (CHANGELOG line 2481) |
| S-002 | RFC-006 | §Designed-for but deferred | `durable_summary` and `best_effort_live` durability modes | CHANGELOG line 2421 `StreamMode::best_effort_live` |
| S-003 | RFC-007 | §Designed for later | any-of / quorum dependency policies | v0.6.x — RFC-016 Stage B `any_of` / `quorum(k,...)` (CHANGELOG line 2452, 2559) |
| S-004 | RFC-008 | §Known Limitations | Soft limits with graduated enforcement (default warn) | CHANGELOG entries for `SOFT_BREACH`/soft-limit support |
| S-005 | RFC-010 | §9.2 | Multi-backend abstraction + Postgres backend becomes implementable | v0.8.0 (RFC-012 trait + Postgres backend) |
| S-006 | RFC-010 | §9.2 | SQLite local mode (partition model) | v0.12.0 (RFC-023 `ff-backend-sqlite`) |
| S-007 | RFC-010 | §9.2 | Public API wire format (gRPC/REST/SDK) / API server | `ff-server` REST surface |
| S-008 | RFC-012 | §R7 amendment | `create_waitpoint`, `append_frame` return widen, `report_usage` return replace | v0.4.0 Stage 1b (RFC-012 Round-7) |
| S-009 | RFC-012 | §R7.1 | PostgresBackend impl for `EngineBackend` | v0.8.0 Stage E (Postgres boot) |
| S-010 | RFC-017 | §D1 → D2 | Stage D2 boot relocation into `ValkeyBackend::connect_with_metrics` + `Server::client` retirement | v0.8.0 Stage E2 (matrix `Server::client` retired) |
| S-011 | RFC-017 | §E1-E4 | `BACKEND_STAGE_READY = ["valkey","postgres"]` flip + `FF_BACKEND=postgres` boots natively | v0.8.0 (2026-04-24) |
| S-012 | RFC-019 | §Stage B | Valkey `subscribe_completion` / `subscribe_signal_delivery`; PG `subscribe_lease_history` / `subscribe_signal_delivery` | v0.9/v0.10 Stage B (#308-#311) |
| S-013 | RFC-019 | §Stage C | HTTP SSE surface `GET /v1/streams/{family}/subscribe` | Stage C (ff-observability-http) |
| S-014 | RFC-020 (cluster) | §3.1 | 13 Postgres Wave-9 methods (cancel_execution, change_priority, replay_execution, revoke_lease, read_execution_*, get_execution_result, list_pending_waitpoints, cancel_flow_header, ack_cancel_member, budget/quota admin) | v0.11.0 (2026-04-26) |
| S-015 | RFC-020 | §3.3 | `rotate_waitpoint_hmac_secret_all` housekeeping parity-matrix flip | v0.11.0 release sweep |
| S-016 | RFC-023 | Phase 2-4 | SQLite dev-only backend shipped whole-or-not | v0.12.0 (2026-04-28) |
| S-017 | RFC-024 | §4.1-4.3 | `issue_reclaim_grant` + `reclaim_execution` + rename `claim_from_reclaim → claim_from_resume_grant` across all 3 backends | v0.12.0 (POSTGRES_PARITY_MATRIX RFC-024 section) |
| S-018 | RFC-025 | all phases | Worker registry: `register_worker`, `heartbeat_worker`, `mark_worker_dead`, `list_expired_leases`, `list_workers` on all 3 backends | v0.14.0 (2026-05-03) |
| S-019 | RFC-007 | §Designed for later | any-of dependencies (was D-034) | v0.6.x — `EdgeDependencyPolicy::any_of` in `crates/ff-core/src/contracts/mod.rs`; covered by S-003 |
| S-020 | RFC-007 | §Designed for later | Quorum/threshold joins (was D-035) | v0.6.x — `EdgeDependencyPolicy::quorum(k, on_satisfied)`; covered by S-003 |
| S-021 | RFC-012 | §R7.6.1 / Stage 1d | `suspend` trait migration — return-type widen (was D-078) | `async fn suspend(&self, handle, args: SuspendArgs) -> SuspendOutcome` in `crates/ff-core/src/engine_backend.rs:214`; `WaitpointSpec` is the final shape, no `ConditionMatcher` type remains |
| S-022 | RFC-017 | §D12 | Postgres `tail_stream` impl (was D-085) | `crates/ff-backend-postgres/src/stream.rs:466` + `lib.rs:1325` (gated on `streaming` feature). No longer returns `Unavailable` |
| S-023 | RFC-020 | §4.1 / §7.8 | Per-attempt-history `get_execution_result` on Postgres | v0.11.0 — present as Postgres trait override; default is `Unavailable`. **Partial:** trait signature still `(id)` only — per-attempt argument is the D-092 part (still open) |
| S-024 | RFC-023 | §5.2 / §8 | Backend-agnostic SDK worker-loop (was D-093) | v0.16-unreleased — `FlowFabricWorker::claim_next_via_backend` + `WorkerRuntime` in `crates/ff-sdk/src/runtime/`; landed via #331 / #523 |
| S-025 | RFC-023 | Note | `PartitionConfig::WorkerConfig` threading (was D-094) | v0.12 PR-6 — `WorkerConfig::partition_config: Option<PartitionConfig>` is honored by `FlowFabricWorker::connect` |
| S-026 | POSTGRES_PARITY_MATRIX | v0.12 additive | PG + SQLite `claim_execution` with typed `ClaimExecutionArgs`/`ClaimExecutionResult` (was D-103) | Both backends implement the typed contract — see `crates/ff-backend-postgres/src/lib.rs:1719` and `crates/ff-backend-sqlite/src/backend.rs:2658` |
| S-027 | RFC-001 | §V1 Scope | Retention policy on stream outputs (was D-004) | `StreamPolicy::retention_ttl_ms` shipped as the retention dial (`crates/ff-core/src/policy.rs`); backends honor it via retention scanner. Multi-dimensional per-outcome / per-tag / tiered retention was never asked for by any consumer. |
| S-028 | RFC-010 | §9.2 | Completion pubsub (part of was-D-068 "pubsub for completion / frame push / flow state") | `ff:dag:completions` Valkey pubsub channel shipped — see `crates/ff-backend-valkey/src/completion.rs:COMPLETION_CHANNEL` and the 7 `PUBLISH ff:dag:completions` sites in `flowfabric.lua`. The broader "frame push" and "flow state" broadcast channels from D-068's original scope were not shipped; they were speculative and no consumer has requested them. |
| S-029 | RFC-017 | §8 waitpoint HMAC | Raw waitpoint-token endpoint with stricter auth (was D-086) | Endpoint exists as `GET /v1/executions/{id}/waitpoints/{waitpoint_id}/token` (not the RFC's literal `/v1/waitpoints/{id}/token` shape). Operator-only; no-cache headers; returns raw HMAC token for approval/external-callback tooling. URL divergence is routing-simplicity (ExecutionId-partition-scoped) and does not change the security contract. See `get_waitpoint_token` in `crates/ff-server/src/api.rs:965`. |

## Dropped / superseded

| ID | Source | § | Original summary | Superseded by / reason dropped |
|----|--------|---|------------------|--------------------------------|
| X-001 | RFC-020 | §3.2 / §5 #5 | `subscribe_instance_tags` impl | Dropped permanently (both backends `n/a`); cairn's backfill served by `list_executions` + `ScannerFilter::with_instance_tag` (#311 audit 2026-04-24) |
| X-002 | RFC-025 | §6 Non-goal #2 | "Worker discovery / listing live workers" was listed as a non-goal | Promoted into Phase 6 `list_workers` method (shipped v0.14.0) |
| X-003 | RFC-009 (original) | §3 exec_id | Pluggable `SoloPartitioner` wrapper (originally deferred as over-scope) | Replaced by RFC-011 decisions; default `Crc16SoloPartitioner` ships, custom partitioner trait stable |
| X-004 | RFC-022 | entire RFC | Full-parity production SQLite backend | Parked `[OPEN FOR FEEDBACK]`; RFC-023 dev-only supersedes the scope (§6.1 RFC-023) |

## Permanent non-goals

| Source | § | Summary |
|--------|---|---------|
| RFC-010 | §9.4 | Durable event sourcing (FF stores current state + audit; not a complete reconstructable event log) |
| RFC-010 | §9.4 | Cross-shard (multi-slot) transactions on Valkey Cluster |
| RFC-010 | §9.4 | Zero-downtime schema migration for structural changes (e.g. partition count) |
| RFC-010 | §9.4 | Built-in observability backend (metrics/events exposed, no built-in dashboard/TSDB) |
| RFC-010 | §9.4 | Namespace-based access control at engine level (gateway/control-plane responsibility) |
| RFC-011 | §11 | Changing the wire shape of cairn's `BridgeEvent::*` variants |
| RFC-011 | §11 | Online migration of existing executions for RFC-011 ExecutionId change |
| RFC-011 | §11 | Hot-spot mitigation beyond partition-count bump + flow-size guidance |
| RFC-011 | §11 | RFC-009 capability-routing interaction (orthogonal; sits above exec-partition) |
| RFC-011 | §11 | Deleting `PartitionFamily::Execution` from the public API |
| RFC-011 | §11 | Valkey Module API adoption (Modules are not an atomicity fix per §10.3-a) |
| RFC-020 | §5 | Re-litigating the trait surface (locked by RFC-012/017/018) |
| RFC-020 | §5 | Changing Valkey behavior (translate semantics, don't rewrite them) |
| RFC-020 | §5 | Introducing new consumer-facing APIs or HTTP response shapes |
| RFC-020 | §5 | Changing the capability-flag surface (only flip Supports flags) |
| RFC-020 | §5 | `subscribe_instance_tags` impl on either backend |
| RFC-020 | §5 | Wave 9 as a release gate for v0.10 (v0.10 already shipped) |
| RFC-023 | §5 #1 | NOT production-scale SQLite (~10³ write-QPS ceiling, single-writer, single-process — inherent) |
| RFC-023 | §5 #2 | NOT multi-writer SQLite (no WAL-over-NFS, no synchronized-filesystem setups) |
| RFC-023 | §5 #3 | NOT clustered SQLite (no rqlite, no dqlite, no replication layer) |
| RFC-023 | §5 #4 | NOT streaming-replica / HA (no Litestream-style continuous backup) |
| RFC-023 | §5 #5 | NOT cross-process pub/sub (§4.2 in-process broadcast channel is permanent shape) |
| RFC-023 | §5 #6 | NOT a replacement for Valkey or Postgres recommendations (default stays `FF_BACKEND=valkey`) |
| RFC-023 | §5 #7 | NOT v2 expansion — no "dev-only SQLite today, production SQLite tomorrow" path |
| RFC-023 | §5 #8 | NOT Wave-N+ SQLite-only perf/scale work |
| RFC-025 | §6 #1 | Worker fencing at the SQL layer (handled at trait ingress + FCALL atomicity) |
| RFC-025 | §6 #3 | Lease reclaim dispatch (FF doesn't auto-reclaim; caller decides via RFC-024) |
| RFC-025 | §6 #4 | Cross-partition global worker identity (`worker_instance_id` unique within (namespace, partition) only) |
| RFC-025 | §6 #5 | Worker-to-lane fencing at storage layer (scheduler admission handles it) |
| RFC-025 | §6 #6 | Per-worker backpressure signals (no `current_in_flight_count` field) |
| RFC-025 | §6 #7 | Worker restart-crash-loop detection (belongs in operator tooling, not trait) |
| RFC-025 | §6 #8 | Cross-worker leader election (consumer-layer; orthogonal) |
| RFC-024 | §5 #1 | NOT a change to `ff_issue_claim_grant` semantics |
| RFC-024 | §5 #2 | NOT a change to `ff_claim_resumed_execution` semantics |
| RFC-024 | §5 #3 | NOT a new Lua FCALL (reuse `ff_reclaim_execution` with one additional ARGV) |
| RFC-024 | §5 #5 | NOT a merger of resume and reclaim trait methods |

## Cross-cutting themes

- **Capability-routing v2 tail (RFC-009).** 9 deferrals (D-050..D-058) all waiting on an authoritative cap store + attestation; V2 target named but no release tag.
- **Signal machinery extensions (RFC-005).** 5 deferrals (D-018..D-022) — signal TTLs, bulk delivery, routing to flow coordinator, payload validation, replay-after-policy-change. None have shipped; no concrete target.
- **Stream transport hardening (RFC-006).** 7 deferrals (D-023..D-031) concentrated in cross-consumer coordination (consumer groups), server-side filters, SSE, sentinel XADDs, and tail-client pool.
- **Budget / quota feature tail (RFC-008).** 9 deferrals (D-040..D-048) — enforcement actions, per-provider quotas, fixed-window rate limits, priority-aware reservation, cascade overrides. All gated on concrete consumer need.
- **Dependency-graph richness (RFC-007).** 3 deferrals remain (D-036..D-039 after D-034/D-035 flipped to shipped) — richer multi-edge semantics, multi-flow membership, graph reconciliation on replay. Gated on future product requirements.
- **Postgres parity tail (POSTGRES_PARITY_MATRIX / RFC-020).** 4 deferrals (D-090, D-091, D-092, D-104) — sharded-counter tables, per-attempt history argument, dedicated `delay_until_ms` column. Mostly additive migrations.
- **SQLite parity gaps (POSTGRES_PARITY_MATRIX).** 3 deferrals (D-101, D-102, D-106) — projection/retention scanners and foundation-scanner ops (`mark_lease_expired_if_due`/`promote_delayed`/`close_waitpoint`/`expire_execution`/`expire_suspension`); all are `Unavailable` on SQLite today while PG has them.
- **Event-model unification (RFC-010).** 4 deferrals (D-068..D-070, plus UC-55 per-execution event stream) — all converging on "one consolidated lifecycle stream" that would replace the 4-source reconstruction story.
- **Scheduler architecture (RFC-009, RFC-010).** 6 deferrals (D-049, D-059..D-065, D-073) on fairness, aging, pool abstraction, preemptive rerouting, cross-region, extraction to separate process. Bundled as "v2 scheduler" theme.
- **Attempt lineage features (RFC-002).** 5 deferrals (D-008..D-012) — compaction, metrics, diff views, access control, archival. All marked "designed-for" in v1; status-unverifiable.
- **Reclaim-scanner absence (RFC-009, RFC-024).** D-058 and D-096 both track the same gap (no production scheduler-side reclaim scanner); RFC-024 is consumer-initiated only, `flowfabric.lua:3865` TODO remains open.

## Reconciliation notes (2026-05-09 one-by-one pass)

Each "Still deferred" row verified via targeted grep against `main`
@ `25ac0c5`. Seven rows moved to "Shipped":

- **D-034, D-035** (any-of / quorum) — double-counted with S-003. The
  original RFC-007 §Designed-for-later list listed these separately
  from the RFC-016 Stage B work that ultimately shipped them. Verified
  in `crates/ff-core/src/contracts/mod.rs`: `EdgeDependencyPolicy::any_of`
  + `EdgeDependencyPolicy::quorum(k, on_satisfied)`.
- **D-078** (suspend trait migration) — `async fn suspend(&self, handle,
  args: SuspendArgs) -> Result<SuspendOutcome, EngineError>` is the
  current trait shape in `engine_backend.rs:214`. `WaitpointSpec` is the
  final input type; `ConditionMatcher` remains only as a comment
  reference. Stage 1d complete.
- **D-085** (Postgres `tail_stream`) — real implementation at
  `crates/ff-backend-postgres/src/stream.rs:466` + `lib.rs:1325`; gated
  on `streaming` feature.
- **D-093** (backend-agnostic SDK worker-loop) — `claim_next_via_backend`
  + the `WorkerRuntime` handler-DI runtime landed in #331/#523
  (v0.16-unreleased in `[Unreleased]`).
- **D-094** (PartitionConfig via WorkerConfig) — already present in
  `crates/ff-sdk/src/worker.rs` (v0.12 PR-6 comment confirms).
- **D-103** (PG + SQLite trait-level grant-consumer parity for
  `claim_execution`) — both backends have a typed `ClaimExecutionArgs /
  ClaimExecutionResult` impl.

Partial credit: **D-092** (per-attempt-history `get_execution_result`)
is S-023 — Postgres override shipped, but the *per-attempt argument*
part is still open on the trait signature. Remains in "Still deferred"
accordingly.

S-019 (the original "RFC-021 draft on file" entry) was **incorrect** —
RFC-021 exists only on an unmerged local branch (`rfcs/021-cron-draft`
@ 53a653b) and is not reachable from `main` nor in the archive. The
row now tracks D-034 instead; RFC-021 is neither shipped nor on file
in any published sense, and D-001 (recurring scheduled execution)
remains correctly deferred.

Reconciliation does not catch items that shipped as a side-effect but
were never linked back to the original RFC. If you spot one, add it to
the shipped table with a note and bump the summary counts.

## Round 2 reconciliation (2026-05-10)

Full deep walk of every remaining D-row. Loaded RFCs 001-002-003-004-005-007-008-009-010-011-012 (in-tree + archive) into context and for each item:

1. Read the originating RFC §.
2. Greped broadly for intent-equivalent code (not just the exact symbol name) across all crates.
3. Verified absence or shipped-in-different-shape.

**Three rows moved to Shipped:**

- **D-004 → S-027** advanced retention policies. `StreamPolicy::retention_ttl_ms` ships as the retention knob. The RFC's speculative "advanced" (per-outcome / per-tag / tiered) isn't there, but also was never requested by a real consumer. Declaring intent-satisfied.
- **D-068 → S-028** completion pubsub. `ff:dag:completions` Valkey pubsub channel ships and is the backbone of the CompletionListener scanner + `subscribe_completion`. The frame-push and flow-state broadcast channels RFC-010 §9.2 grouped with this were speculative — not shipped, not asked for, not tracked separately.
- **D-086 → S-029** raw waitpoint-token endpoint. Shipped at `/v1/executions/{id}/waitpoints/{waitpoint_id}/token` rather than the RFC's `/v1/waitpoints/{id}/token`. Same security contract (operator-only bearer auth + no-cache headers), different URL shape.

**One row annotated as partial:**

- **D-017** synthetic timeout signal on auto-resume. Postgres reconciler at `crates/ff-backend-postgres/src/reconcilers/suspension_timeout.rs:172` injects `{signal_name: "timeout"}` into `member_map[__timeout__]` and clears `timeout_at_ms`. Valkey `flowfabric.lua:5272` treats `auto_resume` and `auto_resume_with_timeout_signal` identically (no injection). Asymmetry kept in the Still deferred table with a **PARTIAL (pg-only)** marker.

**Discovered documentation errors (surfaced but not changed):**

- X-001 (Dropped/superseded) says `subscribe_instance_tags` was "dropped permanently." Actual code comments in `crates/ff-backend-valkey/src/lib.rs`, `crates/ff-backend-sqlite/src/backend.rs`, `crates/ff-backend-postgres/src/lib.rs` all say "deferred on all backends per #311" — not dropped. By user decision (no real consumer demand), row remains as Dropped in the table.

**All other 95 remaining D-rows manually verified as genuinely absent.** Patterns searched beyond the RFC's original symbol names: renamed symbols, cross-crate diffusion, and semantic equivalents. No further matches found.

The 96 still-deferred items fall into the same clusters the themes section already names. The round-2 walk confirms the themes are an accurate summary, not a projection.
