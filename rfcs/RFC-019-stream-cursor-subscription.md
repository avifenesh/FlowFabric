# RFC-019: EngineBackend Stream-Cursor Subscription Surface

**Status:** Accepted
**Author:** FlowFabric Team
**Created:** 2026-04-24
**Accepted:** 2026-04-24 (owner adjudication of §Open Questions inline;
Stage A trait surface + opaque `StreamCursor` + `StreamEvent` + one
real Valkey impl (`subscribe_lease_history`) + one real Postgres impl
(`subscribe_completion`) shipping in the same PR.)
**Target:** v0.9.0 (Stage A); Stage B + Stage C follow-up issues filed.
**Related RFCs:** RFC-015 (stream + DurableSummary), RFC-017 (ff-server backend abstraction)
**Related Issues:** #282

---

## Summary

Define a cross-backend `EngineBackend` trait surface for subscribing to
long-lived, cursor-resumable event streams (`lease_history`,
`instance_tags`, `completion`, `signal_delivery`) so consumers like
cairn-fabric can drop direct `ferriskey::Client` imports and gain
Postgres-backend portability without rewriting their stream loops.

## Motivation

Issue #282 has the concrete shape. Cairn's
`crates/cairn-fabric/src/lease_history_subscriber.rs` today opens a
`ferriskey::{Client, Value}` directly and runs a hand-rolled non-blocking
XREAD + backoff poll loop because FF exposes no trait-level event-stream
subscription primitive. The sibling `instance_tag_backfill.rs` does the
same for HSCAN. Two of cairn's five direct `ferriskey` imports exist only
because of this hole; closing it deletes ~250 LOC consumer-side and is
the single largest mechanical LOC win in cairn's ferriskey decoupling.

The same pattern is duplicated **internally** in
`ff-engine/src/completion_listener.rs` for DAG-promotion — work that
belongs on the trait, not in an engine-internal module.

Porting either consumer to `ff-backend-postgres` today silently loses the
audit trail: PG has no XREAD; a `lease_history` table + LISTEN/NOTIFY is
natural but invisible without a trait surface.

## Goals

- One cross-backend trait method per named event-stream family, returning
  a Rust `Stream<Item = Result<StreamEvent, EngineError>>` so consumers
  get `tokio-stream` ergonomics.
- Durable cursors — consumers persist a `StreamCursor` and resume
  mid-stream after crash / reconnect.
- At-least-once delivery with inline hot-path event fields
  (`execution_id`, `attempt_index`, `timestamp`) so common consumers
  don't need a follow-up `describe_execution` round-trip.
- Shape reuses the RFC-017 pattern: default `Unavailable` impl, additive
  trait growth, per-backend specialization.

## Non-goals

- At-most-once / exactly-once semantics. Consumers dedup by `event_id`.
- Cross-stream transactions or global ordering across partitions.
  Per-partition (Valkey) / per-commit (Postgres) order is enough.
- Generic / polymorphic `subscribe_stream(family)` escape hatch. Each
  family is a named trait method; no `subscribe_stream(StreamFamily::X)`
  fall-through. See §Open Questions #5 — owner adjudicated
  **4 families + allow-list, no escape hatch** on 2026-04-24.
- Restructuring producer-side event emission (out of scope; RFC-015
  owns that).

## Alternatives Considered

### A. One trait method per stream family (recommended, accepted)

`subscribe_lease_history`, `subscribe_completion`,
`subscribe_signal_delivery`, `subscribe_instance_tags`. Four families,
owner-adjudicated allow-list, no generic escape hatch.

- Pro: type-safe — each method returns a concrete event enum consumer
  already understands.
- Pro: aligns with how RFC-017 grew the trait (method-per-operation).
- Con: trait growth per new stream family; mitigated by default
  `Unavailable` impl + allow-list RFC-amendment gate on new families.

### B. Polymorphic `subscribe_stream(family, cursor) -> Stream<StreamEvent>`

One method, parameterised by a `StreamFamily` enum. `StreamEvent` is a
sum type across all families.

- Pro: one trait method, no regrowth.
- Con: less type-safe — every consumer pattern-matches the full sum even
  though they care about one variant; adding a family is a
  breaking-ish change to the `StreamEvent` enum (mitigated by
  `#[non_exhaustive]` but still awkward).
- **Rejected** by owner: cairn has four well-known families and no
  consumer has asked for a generic escape hatch. Pay trait-growth cost
  per family via RFC amendment; keep type-safety.

### C. Polling-only `read_stream_with_cursor(cursor) -> (events, next_cursor)`

What cairn-fabric does today by hand — consumer polls, tracks cursor.

- Pro: smallest trait surface.
- Con: wastes consumer effort on constant polling + cursor management;
  doesn't solve "XREAD blocks the multiplexed connection" — each
  consumer re-invents the dedicated-connection dance.
- **Rejected** — this is the status quo.

### D. HTTP SSE endpoint per family, skip the trait

`ff-observability-http` exposes `GET /v1/streams/lease_history`.

- Pro: language-neutral.
- Con: in-process Rust consumers (cairn, ff-engine DAG promoter) still
  need a trait path — can't force every in-tree consumer through HTTP
  just to share a transport.
- **Deferred** — Stage C lands the SSE surface on top of A.

## Recommendation: A + helper types, plus D as Stage C

Adopt Alternative A (one method per family) with a common
`StreamCursor` opaque-bytes helper, and layer Alternative D on top as
Stage C so language-neutral consumers get parity.

### Trait shape (as accepted + implemented in Stage A)

```rust
use bytes::Bytes;

/// Opaque, backend-versioned cursor. Consumers persist bytes, hand them
/// back on resume. The first byte encodes a backend-family + version
/// prefix so cursors stay stable across backend upgrades (Valkey: `0x01`,
/// Postgres: `0x02`, …). Rest is backend-specific.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamCursor(pub Bytes);

#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamFamily {
    LeaseHistory,
    Completion,
    SignalDelivery,
    InstanceTags,
}

/// Per-event payload. `family` identifies the event shape; `payload` is
/// the family-specific binary event data (backend encodes). Hot fields
/// are inline (execution_id, attempt_index, timestamp) so common
/// consumers don't need a follow-up describe_execution.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct StreamEvent {
    pub family: StreamFamily,
    pub cursor: StreamCursor,
    pub execution_id: Option<ExecutionId>,
    pub attempt_index: Option<u32>,
    pub timestamp: TimestampMs,
    pub payload: Bytes,
}

pub type StreamSubscription = Pin<Box<
    dyn Stream<Item = Result<StreamEvent, EngineError>> + Send
>>;

trait EngineBackend {
    // ... existing methods ...

    async fn subscribe_lease_history(
        &self,
        _cursor: StreamCursor,
    ) -> Result<StreamSubscription, EngineError> {
        Err(EngineError::Unavailable { op: "subscribe_lease_history" })
    }

    async fn subscribe_completion(
        &self,
        _cursor: StreamCursor,
    ) -> Result<StreamSubscription, EngineError> {
        Err(EngineError::Unavailable { op: "subscribe_completion" })
    }

    async fn subscribe_signal_delivery(
        &self,
        _cursor: StreamCursor,
    ) -> Result<StreamSubscription, EngineError> {
        Err(EngineError::Unavailable { op: "subscribe_signal_delivery" })
    }

    async fn subscribe_instance_tags(
        &self,
        _cursor: StreamCursor,
    ) -> Result<StreamSubscription, EngineError> {
        Err(EngineError::Unavailable { op: "subscribe_instance_tags" })
    }
}
```

### Valkey impl (Stage A — this PR — lease_history only)

- Delegate to `ValkeyBackend::subscribe_lease_history`.
- Per subscription: `duplicate_connection()` so one blocking XREAD per
  sub does not starve the multiplexed FCALL connection.
- `XREAD BLOCK 5000 STREAMS {partition}:lease_history <cursor>`.
- Cursor encoding: first byte `0x01` (Valkey family prefix), next
  8 bytes = stream id `ms` as BE `u64`, next 8 bytes = stream id `seq`
  as BE `u64`. Empty cursor ⇒ "begin at `$`" (tail-only).
- Fan-out across partitions: single call tails the local partition the
  backend was configured with. Cross-partition consumers subscribe
  per-partition and merge consumer-side. Documented in method
  doc-comment.
- Reconnect semantics: if the dedicated connection drops, the stream
  yields `Err(EngineError::StreamDisconnected { cursor: last_seen })`
  and ends; the consumer reconnects with the cursor they observed.

Other three families (`subscribe_completion`, `subscribe_signal_delivery`,
`subscribe_instance_tags`) stay default `Unavailable` on Valkey for Stage
A. Follow-up issues track their implementation.

### Postgres impl (Stage A — this PR — completion only)

- `subscribe_completion` wraps existing
  `ff_backend_postgres::completion::subscribe` (LISTEN/NOTIFY on
  `ff_completion` channel, dedicated pool connection). Inherits
  reconnect handling + replay-from-cursor via `last_seen_event_id`.
- Cursor encoding: first byte `0x02` (Postgres family prefix), next
  8 bytes = `event_id` as BE `i64`.
- Other families stub `Unavailable`; follow-up issues filed.

## Backend Semantics Appendix

| Aspect | Valkey | Postgres |
|---|---|---|
| Order guarantee | Per-partition XADD order | Per-commit NOTIFY order on the triggering table |
| Cross-partition order | Not guaranteed | Not guaranteed |
| Cursor encoding | `0x01` + (stream_id ms, seq) | `0x02` + `event_id` |
| Resume after crash | XREAD from stored stream_id | LISTEN + replay `WHERE event_id > stored` |
| Connection cost | 1 blocking connection per subscription | 1 dedicated pool connection per subscription |
| Payload cap | None (Redis string) | 8KB NOTIFY payload → row-pointer fallback |

Consumers needing global ordering must merge application-side. Cursor
bytes are opaque and stable within a (family, backend major version);
major-version bumps may invalidate persisted cursors — consumers
reconcile by restarting from `StreamCursor::empty()` (full replay
permitted by at-least-once contract).

## Cairn Migration Path

- **Today:** `lease_history_subscriber.rs` (~200 LOC) opens
  `ferriskey::Client`, hand-rolls non-blocking XREAD + backoff poll.
- **v0.9.0 (Stage A shipped):** rewrite to
  `backend.subscribe_lease_history(cursor)` iteration. Direct
  `ferriskey` dep drops. Cursor persisted via cairn's existing
  checkpoint module.
- **`instance_tag_backfill.rs`:** audited 2026-04-24 (#311) — one-shot
  backfill served by `list_executions` + `ScannerFilter::with_instance_tag(..)`
  pagination (client-side filter for v0.9; a future `list_executions`
  filter parameter is the natural efficiency win, not a subscription).
  `subscribe_instance_tags` deferred: trait method remains, both
  backends return `Unavailable`, parity matrix row flipped to `n/a`.
  Resume implementation only on concrete consumer demand for a
  realtime tag-churn stream — not speculatively.
- **LOC delta:** ~250 LOC deleted consumer-side; 2 of 5 direct
  `ferriskey` imports removed.

## Implementation Plan

### Stage A — Trait + types + 2 real impls (this PR)

- Add `StreamCursor`, `StreamFamily`, `StreamEvent`, `StreamSubscription`
  in `ff_core::stream_subscribe`.
- Add `EngineError::StreamDisconnected { cursor }` +
  `EngineError::StreamBackpressure` non-exhaustive variants.
- Add four `subscribe_*` methods with default `Unavailable` impl on
  `EngineBackend`.
- Valkey `subscribe_lease_history` — real impl via
  `duplicate_connection()` + `XREAD BLOCK`.
- Postgres `subscribe_completion` — wraps existing
  `PostgresBackend::subscribe` LISTEN/NOTIFY machinery.
- Other six (family × backend) combinations stay default `Unavailable`;
  follow-up issues filed.

### Stage B — Fill the matrix

- Valkey: `subscribe_completion`, `subscribe_signal_delivery`.
  (`subscribe_instance_tags` deferred — see §Cairn Migration Path.)
- Postgres: `subscribe_lease_history`, `subscribe_signal_delivery`.
  (`subscribe_instance_tags` deferred — see §Cairn Migration Path.)
- ff-engine's internal `completion_listener.rs` DAG-promoter migrated
  to consume the trait method (deduplicating the pattern on the way in).

### Stage C — HTTP SSE surface

- `ff-observability-http` exposes
  `GET /v1/streams/{family}/subscribe?cursor=<base64>` using SSE.
- Language-neutral consumers get parity with in-process Rust consumers.
- Reuses backend trait under the hood — no additional semantics.

## Open Questions — Owner Adjudicated (2026-04-24)

1. **Cursor type** → **Opaque bytes with backend-version prefix.**
   `StreamCursor(Bytes)`; first byte encodes a backend family + version
   prefix (`0x01` Valkey, `0x02` Postgres). Preserves backend evolution
   freedom; typed enum rejected as needless surface area.

2. **Disconnect contract** → **Consumer-side reconnect.** The backend
   emits `Err(EngineError::StreamDisconnected { cursor })` and the
   stream ends. Consumers re-call `subscribe_*(cursor)` with the last
   observed cursor to resume. Auto-reconnect inside the backend
   rejected — hides failure modes + complicates retry policy ownership.

3. **Backpressure** → **Pull via `Stream` trait.** Consumers drive via
   `StreamExt::next`. `StreamExt::ready_chunks` covers batch consumers.
   Push-based / broadcast-with-drop-oldest rejected — hides dropped
   events. A `StreamBackpressure` error variant is reserved for
   backends that do surface lag (currently unused; Stage B may wire it
   for instance_tags HSCAN-paced subscribers).

4. **Inline metadata** → **Yes, inline hot fields.**
   `StreamEvent` carries `execution_id`, `attempt_index`, `timestamp`
   inline; rich fields via `describe_execution`. Minimal-event
   alternative rejected — doubles round-trips for the 90% consumer
   (status-badge update in a UI, recovery audit trail, etc.).

5. **Family cap** → **4 families + allow-list; no generic escape
   hatch.** `lease_history`, `completion`, `signal_delivery`,
   `instance_tags`. A fifth family requires a fresh RFC amendment.
   Generic `subscribe_stream(StreamFamily::X)` rejected — adding
   families should be a considered trait-surface decision, not a
   runtime dispatch.

## Backwards Compatibility

Additive trait methods with default `Unavailable` impl. Cairn adopts
incrementally; no existing FF consumer breaks. Cursor encoding is
opaque and version-prefixed — cursor persistence survives minor-version
upgrades; major-version backend bumps may force a cursor reset
(permitted under at-least-once). `EngineError` gains two
`#[non_exhaustive]` variants (`StreamDisconnected`, `StreamBackpressure`);
non-exhaustive was pre-existing so this is not a break.

## Risks

- **Valkey connection-pool sizing.** Each subscription holds a
  dedicated blocking connection. Ops-side: document the N-subscribers
  → N-extra-connections multiplier; default cap + operator override.
- **Postgres NOTIFY 8KB payload cap.** Already addressed upstream —
  `subscribe_completion` uses the outbox-table + `event_id` cursor
  pattern (NOTIFY carries only the cursor; subscriber `SELECT`s the
  row). Consumers inherit this behaviour.
- **Consumer cursor drift.** If a consumer persists a cursor then the
  backend's event retention window expires before resume, the
  consumer sees `Err(StreamDisconnected)` (Valkey) or an empty
  replay (Postgres event rows already trimmed). Reconcile via
  snapshot + forward from current tail.
- **Trait growth.** Four methods + owner-adjudicated allow-list. New
  families require RFC amendment + explicit method addition; default
  `Unavailable` impl keeps out-of-tree backends compiling.

## Links

- Issue: #282 — EngineBackend: subscribe_lease_history trait method.
- Prior art: `ff-engine/src/completion_listener.rs` (internal DAG
  promoter — subscription pattern to be deduplicated into trait in
  Stage B).
- Consumer: `crates/cairn-fabric/src/lease_history_subscriber.rs` (the
  ~200 LOC about to be deleted).
- Related: `crates/cairn-fabric/src/instance_tag_backfill.rs`.
- RFC-015: stream durability modes (producer-side complement).
- RFC-017: ff-server backend abstraction (trait-growth pattern
  template).
