# Consumer migration — v0.9 → v0.10

**Scope.** Three source-breaking surface changes ship in v0.10 for
consumers of `EngineBackend` + the flowfabric umbrella. Two additive
methods and one additive test-fixtures feature also ship. This
document consolidates all five so cairn (and any other direct
consumer) upgrades in one sweep.

A full per-change listing lives in the `[0.10.0]` section of
`CHANGELOG.md`; this doc focuses on the code-edit checklist.

## Source-breaking changes

### 1. `capabilities_matrix()` → `capabilities()` (#277, PR #320)

**Rename + reshape.** The v0.9 capability-discovery API shipped a
`BTreeMap<Capability, CapabilityStatus>` map keyed by an enum. v0.10
reshapes to a flat named-field struct per cairn's original ask:

```rust
// v0.9:
use flowfabric::core::capability::{Capability, CapabilityMatrix, CapabilityStatus};
let caps: CapabilityMatrix = backend.capabilities_matrix();
if matches!(caps.get(Capability::CancelExecution), CapabilityStatus::Supported) {
    render_cancel_button();
}

// v0.10:
use flowfabric::core::capability::Capabilities;
let caps: Capabilities = backend.capabilities();
if caps.supports.cancel_execution {
    render_cancel_button();
}
```

**What moved.**

* `Capability` + `CapabilityStatus` enums are **removed**. Import
  paths to either are dead; the compiler will flag each callsite.
* `CapabilityMatrix` is **renamed** to `Capabilities`. The
  `BTreeMap` becomes the flat [`Supports`] struct under
  `caps.supports`.
* `Supports` is `#[non_exhaustive]` with a `none()` constructor +
  `Default`. Never struct-literal — future additive bools land as
  fields without a `.supports = Supports { ... }` break.

**Partial-status nuance.** v0.9 had three discriminants
(`Supported` / `PartialSupport` / `Unavailable`). v0.10 collapses to
a single bool per method. The "partial" case (e.g. non-durable
cursor on Valkey `subscribe_completion`) moved to rustdoc on the
trait method and `docs/POSTGRES_PARITY_MATRIX.md`. The flat bool
answers the one consumer-visible question: **is this callable at
all.** Fine-grained semantic differences are read from the rustdoc,
not from the capability value.

### 2. Typed `subscribe_*` event enums (#282 Stage C, PR #321)

See `docs/CONSUMER_MIGRATION_typed-subscribe-events.md` for the full
before/after snippet. In short: the four `subscribe_*` trait methods
now return family-specific typed subscriptions (`LeaseHistoryEvent`,
`CompletionEvent`, `SignalDeliveryEvent`, `InstanceTagEvent`) instead
of `StreamSubscription<StreamEvent>` with an opaque byte payload.
Every enum + outcome type is `#[non_exhaustive]`; consumer `match`
arms must include a wildcard.

### 3. `ScannerFilter` required param on three `subscribe_*` methods (#282, PR #325)

`subscribe_lease_history`, `subscribe_completion`, and
`subscribe_signal_delivery` gain a required `filter: &ScannerFilter`
parameter.

```rust
// v0.9:
let sub = backend.subscribe_lease_history(cursor).await?;

// v0.10 (unfiltered — old behaviour):
let sub = backend
    .subscribe_lease_history(cursor, &ScannerFilter::default())
    .await?;

// v0.10 (tag-restricted — cairn-style per-tenant isolation):
let filter = ScannerFilter::new().with_instance_tag("tenant", "acme");
let sub = backend.subscribe_lease_history(cursor, &filter).await?;
```

`subscribe_instance_tags` keeps its v0.9 signature (no filter arg) —
producer wiring is deferred per #311 and the method still returns
`EngineError::Unavailable` on both backends.

**Backend semantics.**

* **Valkey** reuses the existing #122 `FilterGate` (per-event HGET on
  `ff:exec:{p}:<eid>:tags`). No network-shape change; tags live on
  the same execution hash as before.
* **Postgres** filters inline against new denormalised `namespace` +
  `instance_tag` columns on `ff_lease_event` (migration 0008) and
  `ff_signal_event` (migration 0009). **Required migration before
  upgrade:** run `sqlx migrate run` against your Postgres database
  so the new columns and backfill trigger are in place before
  serving v0.10 requests. Migrations are forward-only; no rollback.

Return types are unchanged — filtering happens inside the backend
stream before yielding, not via an envelope wrapper. `is_noop()` on
the filter short-circuits back to the unfiltered fast path.

## Additive changes (no consumer edits required)

### 4. `EngineBackend::suspend_by_triple` (#322, PR #327)

New trait method; ships with a default impl returning
`EngineError::Unavailable { op: "suspend_by_triple" }`. Out-of-tree
`EngineBackend` impls keep compiling without edits. Valkey and
Postgres override to forward to their existing `suspend` path
sourced from a fence triple instead of a worker `Handle`. Adopters:
cairn's pause-by-operator / enter-waiting-approval /
cancel-with-timeout-record call sites that today go via raw
FCALL glue.

### 5. `ff_core::handle_codec::v1_handle_for_tests` (#323, PR #326)

New test-only fixture gated behind a new `test-fixtures` feature on
`ff-core` (default **off**). Synthesises a pre-Wave-1c (v1) handle
byte buffer for cross-version compat tests (e.g. decoding event logs
persisted under FF 0.3 under FF 0.9+). Enable via
`[dev-dependencies] ff-core = { version = "0.10", features =
["test-fixtures"] }`. Never leaks into release builds.

## Upgrade checklist

- [ ] `cargo update -p ff-core -p ff-sdk -p flowfabric` (etc.) to
      pick up v0.10.
- [ ] Postgres: `sqlx migrate run` against every deployment that
      backs a v0.10 consumer (migrations 0008 + 0009 add the
      denormalised filter columns).
- [ ] Rename every `capabilities_matrix()` call to `capabilities()`;
      rewrite `caps.get(Capability::X)` reads as `caps.supports.x`.
      Delete `use ... {Capability, CapabilityMatrix, CapabilityStatus}`
      imports.
- [ ] Add `, &ScannerFilter::default()` to every `subscribe_lease_history`
      / `subscribe_completion` / `subscribe_signal_delivery` call that
      should preserve v0.9 unfiltered behaviour. Audit for call sites
      that should become per-tenant via `with_instance_tag(..)`.
- [ ] Replace every `StreamEvent { payload, .. }` byte-parse with a
      typed-enum `match`. Ensure every `match` has a wildcard arm
      (each enum is `#[non_exhaustive]`).
- [ ] Optional: adopt `suspend_by_triple` in cairn call sites that
      hold a lease fence triple but no worker `Handle`.
