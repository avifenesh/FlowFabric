# FlowFabric RFC Deferrals

Generated 2026-05-09, deeply reconciled 2026-05-10 against `main`.

Every row below is an item the RFC explicitly deferred, punted to v2/v3,
or left as a follow-up, and which is **still absent from the codebase**.
Verified by reading the originating RFC section and searching for intent-
equivalent code (renamed symbols, cross-crate diffusion, semantic
equivalents) across every `crates/` directory plus the Lua library.

Source RFCs: RFC-001..012 + RFC-017..019 (archived at
`avifenesh/flowfabric-archive`), RFC-020, RFC-023, RFC-024, RFC-025
(in-tree), plus `docs/POSTGRES_PARITY_MATRIX.md`.

**Count: 96 items still deferred.**

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
| D-032 | RFC-007 | §V1 limitation | Dependency checks on the resume path (dynamic deps on non-runnable executions) — tracked at #525 | ? | M |
| D-033 | RFC-007 | §V1 limitation | Batch dependency resolution by partition with staggered 100-500 batches to smooth large fan-out bursts | ? | M |
| D-036 | RFC-007 | §Designed for later | Richer multi-edge semantics | post-v1 | M |
| D-037 | RFC-007 | §Designed for later | Multi-flow membership / subflow nesting — tracked at #524 (RFC-design-sketch) | post-v1 | M |
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
| D-057 | RFC-009 | §5 capabilities | Worker-connect-triggered blocked_route sweep (V1 uses periodic unblock scanner) — tracked at #526 | V2 | M |
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
