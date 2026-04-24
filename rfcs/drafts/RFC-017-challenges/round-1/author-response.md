# RFC-017 — Author response to round-1 challenges (K + L)

**Author:** RFC-017 consolidator
**Date:** 2026-04-23
**Branch:** `rfc/017-consolidated` (revision push on PR #261)
**Scope:** addresses K (`rfc/017-debate-K`) + L (`rfc/017-debate-L`) in a single pass.

Format per finding: **CONCEDE** / **ARGUE-BACK** / **REVISE**. REVISE notes the exact §-anchor that changes in the master RFC.

---

## K's findings

### F1 — Trait-count arithmetic
**CONCEDE + REVISE.** Multiple inconsistencies: (a) the RFC claimed 30 base methods, actual count on `origin/main` is **31** (`awk '/trait EngineBackend/,/^}/' | grep -cE '^\s*(async\s+)?fn\s+\w+'` = 31); (b) summary line said 44, §5 said 45. Locked to **base 31 + 14 new = 45**, with `report_usage_admin` counted explicitly as one of the 14 (it IS a new method, not a folded extension). Updated §summary, §2.3, §5, §15. +15 is wrong; +14 is right because `report_usage_admin` is one of the fourteen, not a fifteenth.

### F2 — `#[non_exhaustive]` discipline
**CONCEDE.** Memory rule `non_exhaustive_needs_constructor` is load-bearing. v0.8 is a semver break anyway, so upgrading existing Args types now is free. Added §5.1.1 "Struct discipline" enumerating every Args/Result that gets `#[non_exhaustive]` + a `pub fn new(...)` constructor, and the existing Args promoted at Stage A.

### F3 — T/M/H dishonestly scored
**PARTIAL CONCEDE.** K is right on three rows: `list_pending_waitpoints` H not M; `shutdown_prepare` M not T (see F12); boot relocation needs an explicit ordering sub-row at H. `get_execution_result` upgrade to H: ARGUE-BACK — it's an existing `GET <key>` primitive; Postgres stores it as a bytea column; mechanical, not architectural. Keep M. `create_execution` idempotency_ttl wire impact: CONCEDE — clarified in §4 that the field lifts as an `Option<Duration>` with default preservation, with NO HTTP-wire change (the literal stays server-side inside the default).

### F4 — `report_usage_admin` smuggled in
**CONCEDE.** Promoted to a first-class row in §5 within the "budget admin" group; counted explicitly in the 14; feature-flag placement locked to `admin` in §5.2 with an explicit note that single-binary deployments compile both `core` + `admin`.

### F5 — `PendingWaitpointInfo` schema break
**CONCEDE.** K + L both flagged; reframing §8 as a **schema rewrite**, not a redaction. Preserved operationally-load-bearing fields (`waitpoint_key`, `required_signal_names`, `activated_at`) per K's observation that the reviewer UI filters on them. `state: String` stays as-is with `#[serde(alias)]` path to `WaitpointStatus` deferred to a follow-up; this RFC does only the HMAC redaction + additive `token_kid`/`token_fingerprint`. Net: additive redaction, not a wholesale rewrite.

### F6 — D7 `connect_with_metrics` invariant gap
**CONCEDE.** Added §3.3 D7 contract: `connect_with_metrics` is idempotent; `ensure_library` respects `skip_library_load`; boot ordering contract documented on `EngineBackend` trait doc-comment.

### F7 — 429 `Retry-After` pinning
**CONCEDE.** `EngineError::ResourceExhausted { pool, max, retry_after_ms: Option<u32> }`; `ServerError::from` maps `retry_after_ms` to the `Retry-After` header. §6.

### F8 — `Server::scheduler` dual-dispatch window
**CONCEDE.** §7 now states exactly which reads keep the field alive (metrics reads deferred to Stage C retirement; Stage A/B does NOT call any `scheduler_metrics()` path on Postgres). Q4 closed — field retires at Stage D, not v0.8.

### F9 — 7 silent 501s on Postgres A-D
**CONCEDE.** Adopted option (c) from the prompt: `FF_BACKEND=postgres` refuses to boot during Stages A-D. `Server::start_with_metrics` validates `backend.backend_label()` against a `BACKEND_STAGE_READY` const and returns `ServerError::BackendNotReady { backend, stage }` until Stage E (= v0.8.0 cleanup) ships. Stage plan in §9 updated. Also folds L's F9-overlap ask: `POSTGRES_PARITY_MATRIX.md` committed in-tree at Stage A + `test_postgres_parity_no_unavailable` mechanical gate at Stage D.

### F10 — `start_with_backend` config ownership
**CONCEDE.** §11 now states `partition_config`, `lanes`, `waitpoint_hmac_secret` move from `ServerConfig` into `BackendConfig::Valkey(...)` at v0.8; v0.7.x shim carries them for back-compat. No new trait methods needed — they flow through the `BackendConfig` enum match in `start_with_metrics`, not through trait calls.

### F11 — Q1/Q3/Q5 not owner-scoped
**CONCEDE.** Closed Q1 (split — the `Handle` sum-type pollutes every worker call-site), Q3 (separate method — binary payloads don't belong on snapshot), Q5 (not a design choice; trait is public and semver-protected by default, sealing is out of scope for RFC-017). §12 shrunk to Q2 + Q4.

### F12 — `shutdown_prepare` T → M/H
**CONCEDE.** Bumped to **H** in §4 row 13. Signature changes to `shutdown_prepare(&self, grace: Duration) -> Result<(), EngineError>`; server wraps with its own top-level timeout; Postgres impl spec'd in §5 (UNLISTEN * + pool drain with grace budget) — NOT defaulted. §14.8 promoted from "awareness" to a mandatory Stage B CI test.

---

## L's findings

### L-1 — Postgres parity matrix + mechanical gate
**CONCEDE.** Overlaps K's F9. Resolved together: `POSTGRES_PARITY_MATRIX.md` committed at Stage A merge (in-tree, not side-artifact); `test_postgres_parity_no_unavailable` iterates every HTTP-exposed method and asserts no `Unavailable` at Stage D gate. Plus F9 hard-gates `FF_BACKEND=postgres` boot until Stage E, closing the silent-501 window entirely.

### L-2 — Stage cadence + consumer guidance
**CONCEDE.** Added §9.3 "Consumer migration cadence": wall-clock estimates per stage; cairn mock-impl update cadence (**once at Stage A with `Unavailable` defaults, not four times**); trait-stability freeze during Stages B/C; dashboard-selector advice gated on "every instance ≥ Stage A".

### L-3 — `list_pending_waitpoints` middle-path
**CONCEDE.** L's preferred split was "functional-plus-warn OR hard-redact, not empty-string". Prompt directs hard-redact in v0.8.0. Adopted: v0.7.x keeps the field functional + emits `Deprecation: ff-017` header + server-side audit log per read (gives cairn a dashboard signal); v0.8.0 hard-removes. This is L's preferred *functional-plus-warn* pattern — safer than empty-string, and v0.8 is where the break lands cleanly.

### L-4 (from §6 delta list) — per-op cross-backend operational semantics appendix
**CONCEDE.** Added §16 "Per-op cross-backend semantics" with one paragraph each for `rotate_waitpoint_hmac_secret_all` (fan-out vs transactional), `cancel_flow` dispatch (pending Q2), `replay_execution` (variadic KEYS vs SQL UNNEST). `backend_label()` required in audit event payloads.

### L-5 (from §6 delta list) — Q5 owner resolution before Stage A
**PARTIAL CONCEDE.** Q5 closed per F11 (no sealing, trait is public + semver-protected). The "cairn-facing commitment" it encoded is now a statement in §10, not an open question.

---

## Open questions after revision

Down from 5 → 2:
- **Q2** — `cancel_flow` dispatch ownership (header-only vs end-to-end on trait). Real scope tradeoff; owner-scoped.
- **Q4** — `Server::scheduler` field retirement timing. Resolved in F8 above to Stage D. **Closed.**

Net: **Q2 only**.

## Nothing escalated to owner beyond Q2

Every other decision was obvious-enough under `feedback_decide_obvious_escalate_tradeoffs` to settle without owner involvement. Q2 stays open because it changes the scheduler's Postgres transaction contract, which is a real cross-cutting tradeoff.
