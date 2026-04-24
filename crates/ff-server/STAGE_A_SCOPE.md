# Wave 8 — RFC-017 Stage A kickoff scope

**Branch:** `impl/017-stage-a`
**Worktree:** `/home/ubuntu/ff-worker-impl-017-stage-a`
**Watchdog:** 5 hours
**RFC:** [`rfcs/RFC-017-ff-server-backend-abstraction.md`](../../rfcs/RFC-017-ff-server-backend-abstraction.md) §9 Stage A.
**Q2 adjudicated (2026-04-23):** header-only `cancel_flow` + caller-side async `describe_flow` polling — see RFC §16 `cancel_flow` paragraph.

---

## What Stage A delivers

Per RFC-017 §9 (Stage A — Trait expansion + parity wiring, weeks 0-2):

1. **Add `Arc<dyn EngineBackend>` as a `Server` field alongside the existing `ferriskey::Client` fields.** Both must be present simultaneously during the Stage A transition — this is NOT the cutover. See RFC §13.1 for why we rejected "keep `client: Client` alongside `backend: Arc<dyn>`" as the **end state**, but §9 explicitly allows the dual-field shape as a **transitional** posture for Stage A only. Stages B-E progressively retire `client` / `tail_client`.

2. **Migrate 2-3 handler methods to route through trait methods.** Pick the easiest wins from RFC §4 migration table (Trivial class):
   - `describe_execution` (handler row 3, trait: existing `describe_execution`, effort T)
   - `list_executions` (handler row 4, trait: existing `list_executions`, effort T)
   - `deliver_signal` (handler row 16, trait: existing `deliver_signal`, effort T)

   Pick any 2-3 of these (or other Trivial-class rows). Leave the remaining 10+ handlers calling the existing `Client`-based path; Stages B/C/D migrate the rest.

3. **Add `Server::start_with_backend(config, backend, metrics, ...)` entry point.** Parallel to existing `Server::start_with_metrics`. `start_with_backend` takes a caller-provided `Arc<dyn EngineBackend>` and wires it into the new `Server.backend` field; the existing `client` / `tail_client` / `engine` / `scheduler` fields are still constructed from config as today (Stage A is dual-path). Follow RFC §5.1 signature conventions: `#[non_exhaustive]` args structs with builder-style constructors (§5.1.1).

4. **Stage A CI gate (mandatory per RFC §14.1).**
   - **Parity matrix test per migrated handler** (`crates/ff-backend-valkey/tests/parity_stage_a.rs`): invoke each migrated handler via both the legacy `Client` path and the new `backend` trait path; assert identical observable state transitions. 1 happy-path + 1 error-path per migrated method.
   - **`backend_label` smoke:** `assert_eq!(valkey.backend_label(), "valkey");`
   - **`shutdown_prepare` smoke:** `assert!(valkey.shutdown_prepare(Duration::from_secs(5)).await.is_ok());`
   - **`MockBackend` ergonomics (RFC §14.5):** lives in `ff-server`'s test module; implements every `EngineBackend` method via `Arc<Mutex<ScriptedResponses>>`; migrated handler tests run without a Valkey container. Ship alongside the trait expansion.

## What Stage A does NOT deliver (explicit out-of-scope)

- Do NOT migrate all 13 handler families (Stages B/C/D).
- Do NOT retire `Client` / `tail_client` / `Engine` fields (Stage E).
- Do NOT add new trait methods (Stage A uses only methods already on `EngineBackend` today at `ff-core::engine_backend`).
- Do NOT touch `cancel_flow` (Stage C; header-only semantics per §16).
- Do NOT touch `list_pending_waitpoints` (Stage B schema rewrite per §8).
- Do NOT touch ingress `create_flow` / `add_execution_to_flow` / etc. (Stage D).
- Do NOT migrate boot path (Stage D; `connect_with_metrics` relocation).
- Do NOT make `FF_BACKEND=postgres` runnable (Stage E hard-gate per §9.0).

## Success criteria (goal-driven, verifiable)

1. `cargo check --workspace` clean.
2. `cargo test -p ff-server --all-features` green (new parity-matrix tests + existing tests both pass).
3. `cargo clippy --workspace --all-targets -- -D warnings` clean.
4. `Server::start_with_metrics` and `Server::start_with_backend` both callable; existing binary entry point (`main.rs`) unchanged (Stage A is non-breaking additive).
5. `MockBackend` available under `#[cfg(test)]` with at least one handler test using it in place of a real Valkey container.

## Kickoff commit (this commit)

This first commit on `impl/017-stage-a` lays down the scope document and a one-line marker on `Server` — it does NOT yet add the `backend` field or migrate handlers. That follows in subsequent commits on this same branch (executed by the watchdog'd agent per owner's Wave 8 kickoff).

## References

- RFC: `rfcs/RFC-017-ff-server-backend-abstraction.md`
- Trait: `crates/ff-core/src/engine_backend.rs`
- Current `Server`: `crates/ff-server/src/server.rs` line 121
- RFC-017 PR: #261 (merged)
- Promotion PR: #262 (open, owner-merges)
