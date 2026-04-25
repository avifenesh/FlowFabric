# RFC-019: EngineBackend Stream-Cursor Subscription Surface

**Status:** Draft
**Author:** FlowFabric Team
**Created:** 2026-04-24
**Target:** v0.10.0
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
- Implementation code — this RFC is draft-level; Stage A–C plan below.
- Restructuring producer-side event emission (out of scope; RFC-015
  owns that).

## Alternatives Considered

### A. One trait method per stream family (recommended)

`subscribe_lease_history`, `subscribe_completion`,
`subscribe_signal_delivery`, `subscribe_instance_tags`,
(+`subscribe_stream` generic escape hatch, but capped at 5 for v0.10.0).

- Pro: type-safe — each method returns a concrete event enum consumer
  already understands (`LeaseHistoryEvent`, `CompletionEvent`, etc.).
- Pro: aligns with how RFC-017 grew the trait (method-per-operation).
- Con: trait growth per new stream family; mitigated by default
  `Unavailable` impl.

### B. Polymorphic `subscribe_stream(family, cursor) -> Stream<StreamEvent>`

One method, parameterised by a `StreamFamily` enum. `StreamEvent` is a
sum type across all families.

- Pro: one trait method, no regrowth.
- Con: less type-safe — every consumer pattern-matches the full sum even
  though they care about one variant; adding a family is a
  breaking-ish change to the `StreamEvent` enum (mitigated by
  `#[non_exhaustive]` but still awkward).

### C. Polling-only `read_stream_with_cursor(cursor) -> (events, next_cursor)`

What cairn-fabric does today by hand — consumer polls, tracks cursor.

- Pro: smallest trait surface.
- Con: wastes consumer effort on constant polling + cursor management;
  doesn't solve "XREAD blocks the multiplexed connection" — each
  consumer re-invents the dedicated-connection dance.

### D. HTTP SSE endpoint per family, skip the trait

`ff-observability-http` exposes `GET /v1/streams/lease_history`.

- Pro: language-neutral.
- Con: in-process Rust consumers (cairn, ff-engine DAG promoter) still
  need a trait path — can't force every in-tree consumer through HTTP
  just to share a transport.

## Recommendation: A + helper types, plus D as Stage C

Adopt Alternative A (one method per family) with a common
`StreamCursor` opaque-bytes helper, and layer Alternative D on top as
Stage C so language-neutral consumers get parity.

### Trait shape

```rust
/// Opaque, backend-versioned cursor. Consumers persist bytes, hand them
/// back on resume. Cursors are stable within a family × backend-version
/// but backends may rotate the encoding across major versions.
#[non_exhaustive]
pub struct StreamCursor(Vec<u8>);

/// Hot-path event fields inlined so common consumers skip a follow-up
/// describe_execution. Rich fields available via dedicated getters on
/// the backend.
#[non_exhaustive]
pub enum LeaseHistoryEvent {
    Expired   { cursor: StreamCursor, execution_id: ExecutionId, lease_id: LeaseId, prev_owner: WorkerInstanceId, at: SystemTime },
    Reclaimed { cursor: StreamCursor, execution_id: ExecutionId, lease_id: LeaseId, new_owner: WorkerInstanceId, at: SystemTime },
    Revoked   { cursor: StreamCursor, execution_id: ExecutionId, lease_id: LeaseId, by: RevokeSource, at: SystemTime },
}
// Mirror CompletionEvent, SignalDeliveryEvent, InstanceTagEvent.

trait EngineBackend {
    // ... existing methods ...

    async fn subscribe_lease_history(
        &self,
        filter: ScannerFilter,
        start_from: Option<StreamCursor>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<LeaseHistoryEvent, EngineError>> + Send>>, EngineError> {
        Err(EngineError::Unavailable { op: "subscribe_lease_history".into() })
    }

    // subscribe_completion, subscribe_signal_delivery, subscribe_instance_tags
    // same shape; each with a default Unavailable impl.
}
```

### Valkey impl (Stage B)

- Delegate to an internal `ValkeyBackend::tail_stream_v2`.
- Per subscription: `duplicate_connection()` so one blocking XREAD per
  sub does not starve the multiplexed FCALL connection.
- `XREAD BLOCK $ms STREAMS {partition}:lease_history <cursor>`.
- Cursor encodes `(partition_id, stream_id)` as opaque bytes plus a
  backend-version byte (`0x01`) prefix.

### Postgres impl (Stage B, partial)

- `subscribe_completion` wraps existing `completion_listener.rs`
  (LISTEN/NOTIFY on `completion_events` channel, dedicated pool
  connection).
- Other families stub `Unavailable`, follow-up issues filed.
- Payload > 8KB → row pointer + `SELECT` on a `lease_events` table (PG
  NOTIFY caps payload at 8KB per docs).

## Backend Semantics Appendix

| Aspect | Valkey | Postgres |
|---|---|---|
| Order guarantee | Per-partition XADD order | Per-commit NOTIFY order on the triggering table |
| Cross-partition order | Not guaranteed | Not guaranteed |
| Cursor encoding | `(partition_id, stream_id)` + version byte | `(partition_id, row_lsn)` + version byte |
| Resume after crash | XREAD from stored stream_id | LISTEN + replay `WHERE lsn > stored_lsn` |
| Connection cost | 1 blocking connection per subscription | 1 dedicated pool connection per subscription |
| Payload cap | None (Redis string) | 8KB NOTIFY payload → row-pointer fallback |

Consumers needing global ordering must merge application-side. Cursor
bytes are opaque and stable within a (family, backend major version);
major-version bumps may invalidate persisted cursors — consumers
reconcile by restarting from `None` (full replay permitted by
at-least-once contract).

## Cairn Migration Path

- **Today:** `lease_history_subscriber.rs` (~200 LOC) opens
  `ferriskey::Client`, hand-rolls non-blocking XREAD + backoff poll.
- **v0.10.0:** replaced with
  `backend.subscribe_lease_history(filter, cursor)` iteration. Direct
  `ferriskey` dep drops. Cursor persisted via cairn's existing
  checkpoint module.
- **`instance_tag_backfill.rs`:** audited separately — it's one-shot
  backfill, likely served by `list_executions` with the right filter;
  may not need a subscription at all. Handled via
  `subscribe_instance_tags` only if audit shows a subscription is
  warranted.
- **LOC delta:** ~250 LOC deleted consumer-side; 2 of 5 direct
  `ferriskey` imports removed.

## Implementation Plan

### Stage A — Trait + types (additive, no break)

- Add `StreamCursor`, `LeaseHistoryEvent`, `CompletionEvent`,
  `SignalDeliveryEvent`, `InstanceTagEvent` to `ff-core::engine_backend`.
- Add five `subscribe_*` methods with default `Unavailable` impl.
- Land with a minimal in-memory test backend that implements the full
  surface for trait-conformance tests.

### Stage B — Valkey full + Postgres partial

- Valkey: all five families using `tail_stream_v2` + `duplicate_connection()`.
- Postgres: `subscribe_completion` wrapping existing
  `completion_listener.rs`; other families stubbed `Unavailable` with
  follow-up issues filed.
- ff-engine's internal `completion_listener.rs` DAG-promoter migrated to
  consume the trait method (deduplicating the pattern on the way in).

### Stage C — HTTP SSE surface

- `ff-observability-http` exposes
  `GET /v1/streams/{family}/subscribe?cursor=<base64>` using SSE.
- Language-neutral consumers get parity with in-process Rust consumers.
- Reuses backend trait under the hood — no additional semantics.

## Open Questions for Owner

1. **Cursor type:** opaque bytes (recommended, version-byte prefixed) or
   typed enum (`StreamCursor::Valkey { partition, stream_id }`)?
   Opaque preserves backend evolution freedom; typed is more
   introspectable during debugging.
2. **Disconnect contract:** recommend `Stream` yields
   `Err(EngineError::StreamDisconnected { cursor })` on backend error,
   leaving reconnect to consumer. Alternative: auto-reconnect inside
   the backend with a max-retry policy. Owner call on which side owns
   the retry loop.
3. **Backpressure:** recommend pull via `Stream` trait (natural Rust
   idiom, `StreamExt::ready_chunks` for batch consumers). Push-based
   (channel with drop-oldest on lag) is the alternative — simpler for
   high-fanout consumers but hides dropped events.
4. **Inline vs follow-up metadata:** recommend inline hot fields
   (`execution_id`, `attempt_index`, `timestamp`); rich fields via
   dedicated getters. Alternative: minimal event (ids only) + always
   require `describe_execution` — simpler trait, more round-trips.
5. **Family cap:** five families for v0.10.0 (lease_history,
   completion, signal_delivery, instance_tags, + one generic
   escape hatch?) — or stick to four and defer the generic form until
   a consumer asks.

## Backwards Compatibility

Additive trait methods with default `Unavailable` impl. Cairn adopts
incrementally; no existing FF consumer breaks. Cursor encoding is
opaque and version-prefixed — cursor persistence survives minor-version
upgrades; major-version backend bumps may force a cursor reset
(permitted under at-least-once).

## Risks

- **Valkey connection-pool sizing.** Each subscription holds a
  dedicated blocking connection. Ops-side: document the N-subscribers
  → N-extra-connections multiplier; default cap + operator override.
- **Postgres NOTIFY 8KB payload cap.** Events exceeding the cap must
  use a row-pointer pattern (NOTIFY carries an id; subscriber SELECTs
  the row). Doubles round-trips for large events; acceptable since
  lease-history events are small.
- **Consumer cursor drift.** If a consumer persists a cursor then the
  backend's event retention window expires before resume, the
  consumer sees `Err(StreamCursorExpired)` and must reconcile via
  snapshot + forward from current tail.
- **Trait growth.** Five methods → more if new families land. Mitigate
  by requiring an RFC amendment for each new family and keeping the
  default `Unavailable` impl.

## Links

- Issue: #282 — EngineBackend: subscribe_lease_history trait method.
- Prior art: `ff-engine/src/completion_listener.rs` (internal DAG
  promoter — subscription pattern to be deduplicated into trait).
- Consumer: `crates/cairn-fabric/src/lease_history_subscriber.rs` (the
  ~200 LOC about to be deleted).
- Related: `crates/cairn-fabric/src/instance_tag_backfill.rs`.
- RFC-015: stream durability modes (producer-side complement).
- RFC-017: ff-server backend abstraction (trait-growth pattern
  template; Stage E scheduler field removal is the analogue here).
