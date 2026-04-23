# RFC-012 Stage 1c — Plan (v4 draft)

Worker SS-v4, 2026-04-22. Anchors on `stage-1c-scope-audit.md` (commit `47ef2b0`). Revises v3; no source edits, no PR.

## v3 → v4 changelog

- **Trait feature-gate strategy (new subsection under Scope).** Adopts owner decision: `ff-core`'s `EngineBackend` trait is feature-gated internally for backend-implementer ergonomics; consumer API (via `ff-sdk`) is unchanged. Default features = all-on. Backend crates opt into a subset to declare which methods they own.
- **Risk 6 reframed.** v3 Risk 6 ("trait facade growing to 25-30 methods") split into two axes: (a) consumer-facing facade concern — still ~25-30 methods since `ff-sdk` pulls default features; (b) implementer flexibility — minimal backend (e.g. hypothetical Postgres without suspension/streaming) implements ~18, no `Err(Unavailable)` stubs needed.
- **Tranche scope.** Feature-flag scaffolding folded into tranche 1 (alongside `BackendConfig` split, #93). Each new trait method in later tranches lands with its `#[cfg(feature = …)]` declared; `ValkeyBackend` impl mirrors.
- **Effort estimate.** 32-46h → **34-50h** (+2-4h for feature-flag definitions, `ValkeyBackend` all-features verification, CI matrix plumbing).
- **CI.** New open question: CI feature-matrix. Publish-list drift CI (#132) does not catch feature-set drift.
- **CHANGELOG.** Additive; consumers see no change.

## Scope

### Hot-path migration (from scope audit §Hot-path inventory)

~22 sites / ~8-10 trait methods. 17 `FlowFabricWorker` sites in `crates/ff-sdk/src/worker.rs`, 2 free-fn reshapes (`read_stream`, `tail_stream` — closes 3 of BB's 6 #87 sites), 1 admin free-fn reshape (`rotate_waitpoint_hmac_secret_all_partitions` — additional leak BB did not list), 2 `snapshot.rs` pipeline sites. Scope-audit §FCALL table confirms 7 FCALL migrations + 1 direct-transport (`xread_block`) = 8 Stage-1c-proper migrations. Comparable to Stage 1b's 8 methods (landed ~24h).

Four `ClaimedTask` ops (`create_pending_waitpoint`, `append_frame`, `suspend`, `report_usage`) remain gated on #117 and are out of scope unless #117 lands first.

### `BackendConfig` carveout (#93)

Per scope-audit §WorkerConfig split: 4 fields move (`host`, `port`, `tls`, `cluster`), 8 stay (`worker_id`, `worker_instance_id`, `namespace`, `lanes`, `capabilities`, `lease_ttl_ms`, `claim_poll_interval_ms`, `max_concurrent_tasks`). `WorkerConfig::new` back-compat shim: **break** (per v3 open Q1 recommendation, pre-1.0 CHANGELOG-only posture, ~20 first-party call sites).

### `BackendTimeouts` / `BackendRetry` wiring

Types at `crates/ff-core/src/backend.rs:570-587`; currently dead code. Thread `config.timeouts.request` → `ClientBuilder::request_timeout`, `config.timeouts.connect` → `ClientBuilder::connect_timeout`, retry → transient-error wrapper. Write tests that verify plumbing (the namespace-field dead-code lesson).

### Trait feature-gate strategy (new, v4)

**Motivation.** The `EngineBackend` trait is trending toward 25-30 methods. We want backend implementers (`ff-backend-valkey` today; hypothetical `ff-backend-postgres` later) to compile only the slice of the trait they actually implement — no `Err(Unavailable)` stubs, no forced surface. Consumers (`cairn`, other `ff-sdk` callers) must continue to see the full trait with zero toggles.

**Mechanism.** Feature flags on `ff-core`:

- Candidate groupings (exact names chosen at implementation time): `core` (claim / lease / ack — always on), `streaming` (`read_stream` / `tail_stream`), `suspension` (`suspend` / `create_pending_waitpoint` / HMAC rotate), `budget` (`report_usage`), possibly `signals` (`deliver_signal`). Final grouping driven by which methods cluster on the same storage primitive.
- `default = [all features]`. A `default-features = true` dependency sees the full trait.
- Each `EngineBackend` method declaration carries the matching `#[cfg(feature = "…")]`. `ValkeyBackend`'s `impl` methods mirror the same gates. A build with all features is the contract; `ValkeyBackend` tests run that way in CI.

**Consumer posture.**

```toml
# ff-sdk/Cargo.toml — unchanged by Stage 1c
[dependencies]
ff-core = { workspace = true }   # default features on
```

`ff-sdk` re-exports the full trait surface; its downstream users (`cairn`, test harnesses) are feature-oblivious.

**Backend posture.**

```toml
# Hypothetical minimal backend, not in this stage
[dependencies]
ff-core = { workspace = true, default-features = false, features = ["core"] }
```

Trait surface in that crate is a strict subset. A Postgres backend that cannot do `suspend` compiles `ff-core` without `suspension`; its `impl EngineBackend for PostgresBackend` does not need to declare those methods because the trait itself does not declare them at that feature set.

```toml
# ff-backend-valkey — enables everything
[dependencies]
ff-core = { workspace = true }   # or features = [all], explicit
```

**Non-goals (v4).**

- No crate-split (ff-sdk-core extraction from MM's audit) — post-Postgres scoped.
- No `ff-backend-postgres` shape design — this stage only establishes the trait accommodates it.
- No commitment to the candidate feature names listed above; those are picked during implementation based on storage-primitive clustering.

### Connect-path primitives

Per scope-audit Risk 5 / open Q4: absorb PING, alive-key `SET NX PX`, caps-index `SET/SADD/DEL/SREM` into `ValkeyBackend::connect` / a `backend.register_worker(&config)` method. Precedent: `ValkeyBackend::connect` already loads `ff:config:partitions` (`lib.rs:133-148`). Likely lands gated on `core` feature.

### Snapshot pipeline (snapshot.rs:77, 810)

Per scope-audit Risk 4 / open Q3: expose a documented `pub(crate)` `ValkeyBackend::pipeline()` escape hatch rather than a trait method. Pipelines are Valkey-shaped; abstracting them into the trait forces an awkward Postgres shape. Recommended over `backend.pipeline_snapshot()`. Not feature-gated (Valkey-crate-internal).

## Tranches

1. **Tranche 1 — BackendConfig + feature-flag scaffold.** `BackendConfig` carveout (#93), `BackendTimeouts`/`BackendRetry` wiring, `ff-core` feature flag declarations with `default = [all]`, CI matrix entries, CHANGELOG. **+2-4h for feature-flag setup vs v3.** Target: ~9-13h.
2. **Tranche 2 — Claim / lifecycle methods.** `backend.claim`, `backend.claim_resumed`, `backend.issue_claim_grant`, `backend.block_execution`, connect-path primitives bundled into `backend.register_worker`. Each method declared with its `cfg`. `ValkeyBackend` impl mirrors. Target: ~10-14h.
3. **Tranche 3 — Signal + stream + admin.** `backend.deliver_signal`, reshape `read_stream`/`tail_stream` as `FlowFabricWorker` methods (closes #87 for 3 sites), reshape `rotate_waitpoint_hmac_secret_all_partitions`. `streaming` / `suspension` feature flags exercised here. Target: ~10-15h.
4. **Tranche 4 — Snapshot pipeline + `client()` closure.** `pub(crate) ValkeyBackend::pipeline()` escape, `snapshot.rs` rewires, `FlowFabricWorker::client()` removed, `backend()` narrows return type. Target: ~5-8h.

CI gate boundary: each tranche is a separate PR with workspace-wide `cargo test` green (per feedback_rfc_phases_vs_ci).

## Effort estimate

| Bucket | v3 | v4 |
|---|---|---|
| Hot-path migration | 18-24h | 18-24h |
| `BackendConfig` carveout (#93) | 3-5h | 3-5h |
| `BackendTimeouts`/`BackendRetry` wiring | 2-4h | 2-4h |
| Admin `rotate_waitpoint_hmac_secret_all_partitions` | 2-3h | 2-3h |
| Feature-gate scaffold + CI matrix | — | 2-4h |
| Snapshot pipeline + `client()` closure | 5-8h | 5-8h (unchanged) |
| Buffer (tests, doc, CHANGELOG) | 2-2h | 2-2h |
| **Total** | **32-46h** | **34-50h** |

Still within one Worker-1 sprint at the upper end; tranched PRs keep any single review digestible.

## Risks

1. **Lua impact: none.** All 8 in-scope migrations preserve FCALL names + KEYS/ARGV shape (scope-audit Risk 1).
2. **`BackendTimeouts::keepalive` → ferriskey API gap.** If ferriskey exposes no keepalive-interval knob, field is dead code on landing. Must verify empirically before tranche 1 lands (v3 open Q2, unchanged).
3. **Snapshot pipeline abstraction.** Recommended resolution: `pub(crate) ValkeyBackend::pipeline()` escape hatch (not a trait method). Design fixed in v4; no longer an open Q.
4. **Connect-path primitive bundling.** Recommended: absorb into `backend.register_worker`. Design fixed in v4.
5. **`#88` error-type wrapping bundled or separate.** Recommendation (unchanged from v3): separate PR per BB's Option A.
6. **Trait surface growth → 25-30 methods** *(reframed in v4)*:
   - **Consumer-facing facade concern.** `ff-sdk` pulls default features → consumers always see ~25-30 methods. This is a real API-surface concern (discoverability, doc weight). Mitigation: group methods on the trait by feature with rustdoc section headers.
   - **Implementer flexibility.** Backend crates opt into a feature subset. A minimal Postgres-style backend implements ~18 methods; no `Err(Unavailable)` stubs. No forced surface.
   - Net: the "facade" risk is real for consumers but contained; the "too many methods to implement" risk dissolves at the backend crate level.
7. **Feature-flag maintenance cost.** Each new trait method requires `cfg`-gate decisions at add-time. Cheap if groupings are stable; expensive if we re-organize. Mitigation: name groupings after storage primitives (suspension, streaming, budget) rather than consumer features — primitives change slower than APIs.

## Open questions

1. **`WorkerConfig::new` back-compat shim: keep or break?** Recommend break. Needs owner confirmation.
2. **`BackendTimeouts::keepalive`** — verify ferriskey `ClientBuilder` exposes a keepalive knob before tranche 1 lands; else drop the field.
3. **`#88` error-type wrapping** — bundle or separate PR? Recommend separate.
4. **`#117` timing.** If it lands before Stage 1c starts, bundle the four `ClaimedTask` ops; else defer to 1d.
5. **Feature-flag groupings** — exact names/boundaries. Candidates: `core`, `streaming`, `suspension`, `budget`, `signals`. Decide at tranche 1 based on storage-primitive clustering; documented in CHANGELOG.
6. **CI feature-matrix** *(new, v4)*: CI must exercise at least:
   - `cargo check -p ff-core --no-default-features`
   - `cargo check -p ff-core --no-default-features --features "core"` (minimal backend shape)
   - `cargo test -p ff-core` (all features — default)
   - `cargo test -p ff-backend-valkey` (all features — default)

   Publish-list drift CI (#132) does not cover feature-set drift. Add a 3-4-entry matrix job to `ci.yml`. Cost estimate included in the +2-4h.

## CHANGELOG posture

Additive. Consumers (`ff-sdk` downstream) see no API change. Backend authors gain the ability to opt into a subset. Document candidate feature names once finalized.

## Ready-for-implementation checklist

- [x] Scope anchored on `stage-1c-scope-audit.md`.
- [x] Tranche boundaries align with CI gates (per feedback_rfc_phases_vs_ci).
- [x] Snapshot + connect-path design calls resolved (were v3 open Qs 3+4).
- [x] Feature-gate strategy folded in, trait-crate-internal only.
- [ ] Owner confirms `WorkerConfig::new` break (open Q1).
- [ ] Empirical check: ferriskey keepalive knob (open Q2).
- [ ] CI feature-matrix approved (open Q6, new in v4).

**Plan is ready for Stage 1c implementation spawn** once open Qs 1, 2, 6 are resolved. Feature-gate addition does not re-open any v3-resolved question; it only adds open Q6 (CI-side).
