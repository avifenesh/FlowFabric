# RFC-018: `EngineBackend` capability discovery surface

**Status:** Accepted
**Author:** FlowFabric Team (manager single-agent draft)
**Created:** 2026-04-24
**Acceptance:** 2026-04-24 — owner-adjudicated inline on §9 open
questions (granularity: coarse; version: struct; sync; no event
stream). Stage A implementation ships in the same PR as this
promotion — `BackendIdentity` + `Capability` + `CapabilityStatus` +
`CapabilityMatrix` + `Version` land in `ff-core`, trait default
method added, `ValkeyBackend` + `PostgresBackend` populate real
matrices. Stage B (derived parity matrix) and Stage C (HTTP
`GET /v1/capabilities` + ff-sdk cache) remain follow-ups; closes
issue #277 for v0.9.0.
**Target release:** v0.9.0 (Stage A); Stage B/C land per §8.
**Related RFCs:** RFC-012 (EngineBackend trait landing + Round-7 stabilisation), RFC-017 (ff-server backend abstraction + staged Postgres parity)
**Related issues:** #277 (cairn PG-backend operator-UI blocker — driver for this RFC), #298 (`cancel_flow_wait` Unavailable shape on PG), #282 (backend-parity discovery tracking), #297 (token-budget example surfaced the "what does this backend support" gap)

---

## Summary

Consumers of `Arc<dyn EngineBackend>` (and HTTP clients hitting `ff-server`) today have no typed way to ask "what can this backend actually do?" before dispatching. They discover capability gaps empirically — they call a trait method, receive `EngineError::Unavailable { op }`, and surface a generic transport error to the operator. This RFC adds a first-class capability-discovery surface: a `BackendIdentity` tuple + a `CapabilityMatrix` method on the trait, mirrored by `GET /v1/capabilities` on `ff-server`. The `POSTGRES_PARITY_MATRIX.md` artifact stops being hand-maintained and becomes a renderer over the in-code matrix — drift between code and doc drops to zero.

---

## 1. Motivation

### 1.1 Cairn's operator-UI problem (verbatim from #277)

> Operator clicks Cancel, cairn dispatches, FF returns 503, cairn surfaces a generic transport error. The UI has no way to pre-hide or grey-render features.

This is the driving pain. When a cairn deployment is pointed at a Postgres-backed `ff-server` during the RFC-017 Stage A–D migration window, seven or more HTTP routes return `501` / `503` for unimplemented trait methods (default `Unavailable` impls). Cairn's operator UI renders every action as a button, clicks it, gets an error, and surfaces "backend transport failure" — indistinguishable from a network partition. Operators have no way to distinguish "the backend cannot do this yet" from "the backend is down."

Today cairn works around this by parsing `docs/POSTGRES_PARITY_MATRIX.md` **out-of-band at release time** and hard-coding a feature flag table per ff-server version. Every RFC-017 stage bump requires a cairn release bump just to refresh the flag table. The matrix is a doc artifact; drift between matrix and code is tracked only by human review. Bringing this knowledge in-band — greppable from the running backend — closes the loop.

### 1.2 Not a one-off — recurring pattern

The "I tried to call X, got Unavailable, now what?" shape shows up repeatedly across recent issues:

- **#297 (token-budget example)** — consumer example hit `report_usage_admin` returning `Unavailable` on Postgres and had no structured way to pre-check.
- **#298 (`cancel_flow_wait`)** — concrete example where a specific backend variant returns `Unavailable`; consumer code fell back to a generic error path instead of rendering a targeted "this backend does not support timeout-based cancel-wait" message.
- **#277 (parent)** — the general operator-UI greying problem above.

The fact that three independent issues converge on the same gap means this is a missing primitive, not a one-off UX tweak.

### 1.3 Seed already exists

`EngineBackend::backend_label() -> &'static str` (`crates/ff-core/src/engine_backend.rs:924`, from RFC-017 §5.4) is the first backend-identity surface. It returns a static string today (`"valkey"` / `"postgres"` / `"unknown"`). This RFC builds out the neighbouring surface rather than reinventing it.

---

## 2. Goals / Non-goals

**Goals:**

1. **Consumer-callable typed query** — "does this backend support operation X?" with a stable, typed answer (not a stringly-typed probe, not a try-it-and-catch pattern).
2. **Static + dynamic split** — compile-time-known capabilities (static) are cleanly separated from runtime-configurable capabilities (dynamic). A Postgres instance with `LISTEN/NOTIFY` disabled reports the LISTEN-dependent capabilities differently from one with it enabled.
3. **Greppable from `ff-server` HTTP** — language-neutral consumers (UC-62 ecosystem: non-Rust operator tooling, cairn's JS UI layer, out-of-process inspectors) get the same answer as in-process `Arc<dyn>` consumers via `GET /v1/capabilities`.
4. **Parity matrix becomes derived** — `docs/POSTGRES_PARITY_MATRIX.md` is rendered from the in-code matrix; drift between code and doc drops to zero.

**Non-goals:**

1. **No auto-degrade / auto-reroute.** Capability query is read-only. We do not introduce a "backend lacks X, silently dispatch to compatible alternative Y" shim; that is policy and belongs to the consumer. If the consumer wants a fallback, they build it explicitly on top of the query.
2. **No per-method versioning.** Capabilities report "supported" / "unsupported" at the granularity this RFC defines. Method-level semver is cairn's release-pinning concern (they already pin `ff-sdk` versions); not ours.
3. **No event-stream of capability changes.** Backend capabilities are deployment-lifetime-stable; cache at startup. Revisit if a real consumer need emerges.
4. **No per-tenant / per-deployment gating.** That is UC-48 quota land (rate-limits + auth-scoped feature flags), not capability discovery.
5. **No dynamic capability flips mid-run.** Backends either support a capability for the lifetime of the process or they do not. Re-examine if evidence demands it.
6. **No implementation code in this RFC.** Draft only; staged implementation lives in follow-up PRs per §8.

---

## 3. Alternatives considered

### A. Enrich `backend_label()` → `BackendIdentity { family, version, capabilities: &[&'static str] }`

Smallest possible change. Widen the existing `backend_label()` return to a small struct carrying family + version + a slice of static capability strings that the backend author curates.

- **Pros:** Minimal trait growth (one existing method widened, no new methods). Compile-time-known list; zero allocations on the query path. Covers cairn's static operator-UI greying use case at ~80%.
- **Cons:** Only captures *static* capabilities. Cannot express runtime-gated ones — e.g. "this Postgres instance has LISTEN/NOTIFY enabled," "this Valkey cluster has the Redis-module scripting surface loaded." Stringly-typed (either bare `&'static str` or a parallel enum). Does not directly render to JSON for the HTTP surface without an adapter layer.

### B. New trait method `capabilities_matrix() -> CapabilityMatrix`

Strongly typed struct with per-capability rows. Each row is `(Capability, CapabilityStatus)` where both sides are enums.

- **Pros:** Most discoverable; consumer gets a typed matrix they can iterate. Supports runtime-gated caps (the backend probes at `connect_with_metrics` time and populates the matrix). Renders trivially to the HTTP surface.
- **Cons:** Biggest trait growth (one new required-override method, default returns empty). Each new primitive that consumers care about adds an enum variant.

### C. Per-method `X::capability() -> Capability` sentinel

Pattern from some std traits (e.g. `Iterator::size_hint`). Every trait method carries a sibling sentinel method returning its own capability row.

- **Pros:** Capability discovery co-located with the method it describes.
- **Cons:** Trait surface explosion — 50+ existing methods × 2 = 100+ methods. Every consumer callsite pays an ergonomic cost. **Rejected** — surface-area cost is not justified by any concrete consumer benefit.

### D. HTTP-only `GET /v1/capabilities`

Skip the trait entirely; expose capabilities only via the HTTP handler. `ff-server` queries its own backend internals to assemble the JSON.

- **Pros:** Zero trait growth.
- **Cons:** Rust-level consumers (cairn-fabric, `flowfabric` umbrella-crate users, any embedded `Arc<dyn EngineBackend>` consumer) pay an HTTP hop for information that is otherwise in-process. Direct-trait consumers (tests, workers, cairn-fabric's internal boot path) cannot use it at all. Breaks the established pattern of "trait first, HTTP is a thin wrapper."

---

## 4. Recommended design: B with a static fallback via A

Adopt **B**'s typed matrix as the primary surface, with **A**'s `BackendIdentity` as the cheap static fallback — they compose.

### 4.1 `BackendIdentity`

Widen the existing backend-identity surface to a small typed tuple:

```rust
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BackendIdentity {
    pub family: &'static str,              // "valkey" | "postgres" | "<custom>"
    pub version: Version,                  // struct, not semver string — see §9
    pub stage: &'static str,               // RFC-017 stage this backend was certified at, e.g. "E" / "A"
    pub capabilities: &'static [Capability], // static-known caps; subset of the matrix below
}
```

- `family` is the existing `backend_label()` return, now a field.
- `version` is the backend crate's own version, surfaced for consumer version-pinning diagnostics.
- `stage` is the RFC-017 stage the backend reports itself certified at — a consumer can refuse to boot against a backend below its required stage.
- `capabilities` is a compile-time slice — the minimum-viable static answer. Backends that have nothing dynamic override only this field and inherit the default `capabilities_matrix()` which lifts the slice into matrix form.

### 4.2 `Capability` enum

Coarse-grained enum, one entry per "cairn grey-rendering unit" — the granularity at which the operator UI wants to pre-hide or grey-render a feature:

```rust
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Capability {
    // Claim + lifecycle
    ClaimForWorker,
    ClaimFromReclaim,
    SuspendResumeByCount,

    // Cancel
    CancelExecution,
    CancelFlow,
    CancelFlowWaitTimeout,      // #298 driver — header-only vs wait-timeout split
    CancelFlowWaitIndefinite,

    // Streams
    StreamRead,
    StreamTail,
    StreamDurableSummary,

    // Signals + waitpoints
    DeliverSignal,
    ListPendingWaitpoints,
    RotateWaitpointHmac,

    // Budget + quota
    ReportUsage,                // worker-path
    ReportUsageAdminPath,       // admin-path (#297 driver)
    ResetBudget,

    // Ingress
    CreateFlow,
    CreateExecution,
    StageDependencyEdge,
    ApplyDependencyToChild,

    // Diagnostics
    Ping,
    BackendIdentity,            // trivially true on anything post-RFC-018
    // Room to grow; `#[non_exhaustive]` on the enum keeps additions minor.
}
```

~20 coarse caps covering operator-UI greyable features. Intentionally not per-method (Alternative C); see §9 open question on granularity.

### 4.3 `CapabilityStatus`

```rust
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityStatus {
    Supported,
    Unsupported,
    Partial { note: &'static str },    // e.g. "flow-level only, no member wait"
    Unknown,                            // backend has not reported; treat as try-and-catch
}
```

Four states. `Partial` carries a static note so `ff-server`'s JSON response can render human-readable hints; `Unknown` is the safe default when a backend predates RFC-018.

### 4.4 `CapabilityMatrix`

```rust
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityMatrix {
    pub family: &'static str,
    pub version: Version,
    pub caps: BTreeMap<Capability, CapabilityStatus>,
}
```

`BTreeMap` (not `HashMap`) so iteration order is stable — cairn's operator UI can render rows deterministically, and the `ff-server` JSON response is byte-stable under the same backend state. Every capability the backend *knows about* appears as a row; capabilities the backend has never heard of are absent (consumer treats absent as `Unknown`).

### 4.5 Trait additions

```rust
// ── Added in RFC-018 ──

/// Static-known backend identity. Cheap: returns a struct owned by the
/// backend's static `const`. Consumers that only need family+version
/// (e.g. for metrics dimensioning) call this; consumers that need the
/// full capability matrix call `capabilities_matrix()`.
///
/// Default impl synthesizes from `backend_label()` + empty caps so
/// pre-RFC-018 out-of-tree backends keep compiling.
fn identity(&self) -> BackendIdentity {
    BackendIdentity {
        family: self.backend_label(),
        version: Version::UNKNOWN,
        stage: "unknown",
        capabilities: &[],
    }
}

/// Full capability matrix, including runtime-gated entries.
///
/// Sync (see §9 on sync-vs-async). Backends that need a runtime probe
/// (Postgres LISTEN/NOTIFY availability, Valkey module presence) do
/// that probe once at `connect_with_metrics` time and cache the
/// resulting matrix; `capabilities_matrix()` returns the cached copy.
///
/// Default impl lifts `identity().capabilities` into matrix form with
/// every entry marked `Supported`. Backends that have anything dynamic
/// override.
fn capabilities_matrix(&self) -> CapabilityMatrix {
    let id = self.identity();
    let caps = id.capabilities
        .iter()
        .copied()
        .map(|c| (c, CapabilityStatus::Supported))
        .collect();
    CapabilityMatrix { family: id.family, version: id.version, caps }
}
```

**Note:** `backend_label()` is retained — `identity()` is strictly additive. Consumers that already call `backend_label()` for metrics continue to work; `identity().family` is the richer alternative.

---

## 5. HTTP surface

`GET /v1/capabilities` on `ff-server`:

```json
{
  "family": "postgres",
  "version": { "major": 0, "minor": 10, "patch": 0 },
  "stage": "E",
  "caps": [
    { "name": "CreateFlow",           "status": "supported" },
    { "name": "CancelFlowWaitTimeout","status": "partial", "note": "flow-level only, no member wait" },
    { "name": "StreamTail",           "status": "unsupported" }
  ]
}
```

Served directly from `self.backend.capabilities_matrix()`. Language-neutral consumers (cairn's JS UI, operator tooling in non-Rust stacks, the out-of-process inspector from UC-62) get the same answer as in-process consumers. Wire format stable under additive `Capability` enum growth (non-exhaustive on the wire = extra row types, which existing consumers ignore).

No auth required beyond whatever `ff-server` already gates reads with — capability information is not sensitive, and cairn's UI needs it pre-login to render a welcome screen.

---

## 6. Migration for consumers

### 6.1 Cairn-fabric (primary driver)

**Today:** parses `docs/POSTGRES_PARITY_MATRIX.md` at cairn release time. Every RFC-017 stage bump requires a cairn release bump.

**After RFC-018 Stage A:** cairn calls `backend.capabilities_matrix()` at its own startup (once, per `Arc<dyn EngineBackend>` — backend identity stable for deployment lifetime), caches the result keyed by `(family, version)`, and drives operator-UI greying off the cache. Stage bumps no longer force cairn releases; cairn's UI automatically adapts to whatever the running backend reports.

**Fallback for pre-RFC-018 backends:** cairn's cached matrix is empty on a backend predating this RFC (default-impl `identity()` returns empty capabilities). Cairn treats empty as "unknown — dispatch and catch `Unavailable`," i.e. exactly today's behaviour. Additive migration; cairn does not need to block on every backend upgrading in lockstep.

### 6.2 Static (compile-time-check) consumers

Consumers that want compile-time verification ("this code path is unreachable unless `Capability::CancelFlowWaitTimeout` is in the set") expose an associated const on the concrete backend type:

```rust
impl PostgresBackend {
    pub const CAPABILITIES: &'static [Capability] = &[
        Capability::CreateFlow,
        Capability::CreateExecution,
        // ...
    ];
}
```

Consumers that statically bind to the concrete type (tests, embedded deploys) check against this const. Consumers that bind via `Arc<dyn>` use the runtime matrix. Both paths agree on content.

### 6.3 Fallback for backends that have not yet implemented

Default trait impls (§4.5) return empty capabilities. Consumers treat empty-or-missing as `CapabilityStatus::Unknown` — i.e., fall back to pre-RFC-018 behaviour (dispatch and catch). **Nothing breaks.** A backend adopts RFC-018 at its own cadence.

---

## 7. Backwards compatibility

- **Trait additions are default-impl'd.** Out-of-tree `impl EngineBackend for X` blocks continue to compile unchanged; they just report empty capabilities.
- **`backend_label()` is preserved.** Existing consumers that key metrics on it keep working.
- **`POSTGRES_PARITY_MATRIX.md` becomes a derived artifact.** The file continues to exist at the same path with the same shape; a small `rfcs/scripts/render_parity_matrix.rs` tool regenerates it from the in-code matrix of both backends. Hand-edits are rejected by a CI check (`scripts/check_parity_matrix_up_to_date.sh`) that re-renders and diffs. See §8 Stage B.
- **Wire format on `GET /v1/capabilities` is additive.** New `Capability` enum variants render as new JSON rows; pre-RFC-018 consumers ignore unknown rows. Existing rows never change shape.
- **Semver impact:** trait additions with default impls are a minor bump. `BackendIdentity` + `Capability` + `CapabilityMatrix` + `CapabilityStatus` are new public types in `ff-core` — additive, no break.
- **No wire changes** outside the new `GET /v1/capabilities` route.

---

## 8. Implementation plan

Three stages, each CI-gated at the workspace boundary per `feedback_rfc_phases_vs_ci`. Stage A is the v0.10.0 minimum; B + C can slip one minor behind without blocking the release.

### Stage A — Trait additions + Valkey/Postgres matrices (v0.10.0)

- Land `BackendIdentity`, `Capability` (non-exhaustive enum), `CapabilityStatus`, `CapabilityMatrix`, `Version` in `ff-core`.
- Add `identity()` + `capabilities_matrix()` to `EngineBackend` with default impls (§4.5).
- `ValkeyBackend::identity()` + `capabilities_matrix()` fill in the concrete caps by inspecting its own feature flags + inherent methods.
- `PostgresBackend::identity()` + `capabilities_matrix()` likewise. Runtime-probe `LISTEN/NOTIFY` once at `connect_with_metrics` time and cache; `capabilities_matrix()` returns the cached copy.
- CI gate: `cargo test --workspace` green on the trait additions; new unit test asserts each in-tree backend reports a non-empty matrix with every currently-impl'd trait method represented.

**Deliverable:** consumers can query. Nothing else changes.

### Stage B — Parity matrix renderer (v0.10.x patch or v0.11.0)

- Land `rfcs/scripts/render_parity_matrix.rs` (or equivalent — placement TBD in implementation PR). Reads both backends' `capabilities_matrix()` from a test harness, renders the existing `docs/POSTGRES_PARITY_MATRIX.md` shape.
- Add `scripts/check_parity_matrix_up_to_date.sh` to CI: re-runs the renderer, diffs against checked-in file, fails on drift.
- Land a one-time commit regenerating `POSTGRES_PARITY_MATRIX.md` from the matrix (expected to be a no-op diff if Stage A populated the matrices accurately).
- CI gate: drift check green; workspace green.

**Deliverable:** parity-matrix drift drops to zero. Hand-edits of the doc fail CI.

### Stage C — `GET /v1/capabilities` HTTP handler + ff-sdk cache (v0.11.0)

- `ff-server` handler: `GET /v1/capabilities` → `self.backend.capabilities_matrix()` → JSON per §5.
- `ff-sdk` adds a startup-time `fetch_capabilities()` helper that calls the HTTP endpoint and caches the result (keyed by base URL + backend identity).
- Cairn-fabric consumes the helper; removes the out-of-band parity-matrix parse.
- CI gate: new HTTP test asserts the JSON shape; cairn-fabric consumer CI green.

**Deliverable:** language-neutral consumers have an in-band query path. Cairn's release-coupling story is resolved.

---

## 9. Open questions — owner adjudication (2026-04-24)

All four open questions closed inline per the draft's own
recommendations. Rejected options preserved verbatim in §3
Alternatives (for posterity) and below (with the accepted choice
marked) so future re-evaluation has the full record.

1. **Capability granularity — ~20 coarse or 50+ fine?** This RFC proposes coarse (one per operator-UI-greyable feature, ~20 entries). Fine would be one per trait method (~50+, tracking RFC-017 §2.3's 51-method post-expansion count). **Recommend coarse** — cairn's UI grey-rendering is on operator-visible features (the "Cancel Flow" button), not per-method (the user does not see `cancel_flow_header` vs `cancel_flow_wait`). Coarse also keeps `Capability` enum growth slow; fine couples it to every trait-surface change. If owner picks fine, Stage A scope doubles.

   **Accepted: coarse.** Stage A ships ~25 variants
   (`crates/ff-core/src/capability.rs::Capability`). Driver (#277)
   is UI grey-rendering, which is operator-visible-feature granular,
   not trait-method granular. `#[non_exhaustive]` keeps the enum
   growable in follow-up stages.

2. **`Version` semantics — semver string, or `struct { major, minor, patch }`?** This RFC proposes `struct` for compile-time comparison (consumers can write `if backend.identity().version >= Version { major: 0, minor: 10, patch: 0 }` without parsing). Semver string is one-line simpler but forces consumers to pull a semver-parsing dep. **Recommend struct.**

   **Accepted: struct.** Stage A ships
   `ff_core::capability::Version { major, minor, patch }` with
   `#[derive(Ord, PartialOrd)]` + const constructor. Consumers do
   direct comparison without a semver-parsing dep. Pre-release /
   build-metadata suffixes not in scope for Stage A.

3. **Sync vs async `capabilities_matrix()`?** This RFC proposes sync. Rationale: backend-static info should not require a probe on every query; dynamic probes happen once at `connect_with_metrics` time and cache the result. Sync signature keeps the caller ergonomics clean (no `.await` on a diagnostic call). **Recommend sync.** Open because a hypothetical future backend might want to refresh on every call (e.g. a cluster where capabilities genuinely flip when membership changes); that backend would have to re-poll on connect and accept a possibly-stale answer until next reconnect — acceptable for our current needs.

   **Accepted: sync.** Backends that need a runtime probe
   (Postgres `LISTEN/NOTIFY` availability, Valkey module presence)
   do it at `connect*` time and cache. The trait method returns
   the cached matrix — consumers pay no `.await` on a diagnostic
   call.

4. **Event-stream of capability changes?** A consumer could subscribe to capability-flip events rather than polling. **Recommend no** — backend capabilities do not change across a deployment's lifecycle (a Postgres instance either has LISTEN/NOTIFY or does not, for its whole boot cycle). Cache at startup. Revisit only if a real consumer use case emerges post-Stage A.

   **Accepted: no.** Cache at startup. Revisit only if a real
   consumer use case emerges post-Stage A — no stub hook added in
   this PR so the trait surface stays lean.

---

## 10. Links

- Issue [#277](https://github.com/avifenesh/FlowFabric/issues/277) — cairn PG-backend operator-UI blocker (driver)
- Issue [#282](https://github.com/avifenesh/FlowFabric/issues/282) — backend-parity discovery tracking
- Issue [#298](https://github.com/avifenesh/FlowFabric/issues/298) — `cancel_flow_wait` Unavailable shape (concrete example)
- Issue [#297](https://github.com/avifenesh/FlowFabric/issues/297) — token-budget example (UI affordance motivation)
- `rfcs/RFC-017-ff-server-backend-abstraction.md` — trait expansion + Postgres parity staging
- `rfcs/RFC-012-engine-backend-trait.md` — trait landing + Round-7 stabilisation
- `crates/ff-core/src/engine_backend.rs:924` — existing `backend_label()` seed this RFC widens
- `docs/POSTGRES_PARITY_MATRIX.md` — hand-maintained today; derived artifact after Stage B
- `examples/token-budget/` — consumer example that surfaced the "what does this backend support" gap
- `examples/umbrella-quickstart/` — UI-affordance motivation for language-neutral HTTP discovery

---

**End of RFC-018 draft.** Seeking owner + cairn sign-off before promotion to `rfcs/RFC-018-*.md` and staged implementation.
