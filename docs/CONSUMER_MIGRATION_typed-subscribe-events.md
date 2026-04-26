# Consumer migration — typed `EngineBackend::subscribe_*` events

**Applies to:** the v0.10 release that lands RFC-019 Stage C.

## What changed

The four `EngineBackend::subscribe_*` trait methods now return
family-specific typed subscriptions instead of a generic
`StreamSubscription<StreamEvent>` with an opaque byte payload.
Consumers `match` on typed variants directly; no more
NUL-delimited / JSON payload parsing on the consumer side.

| Trait method                  | New return type                                     |
|-------------------------------|-----------------------------------------------------|
| `subscribe_lease_history`     | `LeaseHistorySubscription` → `LeaseHistoryEvent`    |
| `subscribe_completion`        | `CompletionSubscription` → `CompletionEvent`        |
| `subscribe_signal_delivery`   | `SignalDeliverySubscription` → `SignalDeliveryEvent`|
| `subscribe_instance_tags`     | `InstanceTagSubscription` → `InstanceTagEvent`      |

The untyped `StreamEvent`, `StreamSubscription`, and `StreamFamily`
are removed from `ff_core::stream_subscribe`. The cursor codec
(`StreamCursor`, `encode_valkey_cursor`, `decode_valkey_cursor`,
`encode_postgres_event_cursor`, `decode_postgres_event_cursor`) is
unchanged — persisted cursors remain valid across the upgrade.

## Before (v0.9)

```rust
use ff_core::engine_backend::EngineBackend;
use ff_core::stream_subscribe::StreamCursor;
use futures::StreamExt;

let mut sub = backend
    .subscribe_lease_history(StreamCursor::empty())
    .await?;
while let Some(item) = sub.next().await {
    let event = item?;
    // Consumers had to know the Lua wire shape to do anything useful:
    let payload = std::str::from_utf8(&event.payload)?;
    if payload.contains("event\0reclaimed") {
        // ... hand-parse the NUL-delimited field map
    }
}
```

## After (v0.10)

The `subscribe_lease_history` / `subscribe_completion` /
`subscribe_signal_delivery` methods also gained a `filter` parameter
(issue #282) — pass `&ScannerFilter::default()` for the unfiltered
behaviour shown below. `subscribe_instance_tags` does **not** take a
filter and still returns `EngineError::Unavailable` on both backends
(#311 producer wiring deferred).

```rust
use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::EngineBackend;
use ff_core::stream_events::LeaseHistoryEvent;
use ff_core::stream_subscribe::StreamCursor;
use futures::StreamExt;

let mut sub = backend
    .subscribe_lease_history(StreamCursor::empty(), &ScannerFilter::default())
    .await?;
while let Some(item) = sub.next().await {
    match item? {
        LeaseHistoryEvent::Reclaimed { execution_id, new_lease_id, at, .. } => {
            record_reclaim(execution_id, new_lease_id, at);
        }
        LeaseHistoryEvent::Expired { execution_id, prev_owner, at, .. } => {
            alert_operator(execution_id, prev_owner, at);
        }
        LeaseHistoryEvent::Revoked { execution_id, revoked_by, .. } => {
            tracing::info!(%execution_id, %revoked_by, "lease revoked");
        }
        _ => {} // Acquired / Renewed / (future variants)
    }
}
```

## Cursor persistence

Cursors encoded under v0.9 remain valid; consumers that persisted
`StreamEvent::cursor` bytes hand them back to `subscribe_*` with no
change. Access via `event.cursor()` on the enum (lease_history) or
`event.cursor` on the struct (completion / signal_delivery).

## Gotchas

- `LeaseHistoryEvent::{Acquired, Renewed, Reclaimed}::lease_id` /
  `new_lease_id` are `Option<LeaseId>`. Valkey populates them; the
  Postgres outbox typically carries `None` today because the
  attempt identity is `(lease_epoch, attempt_index, execution_id)`
  rather than a stable lease UUID. Match accordingly.
- `LeaseHistoryEvent::Revoked::revoked_by` is `String` today for
  forward compatibility; may promote to a typed enum post-v0.10
  once the taxonomy settles. Known values: `"operator"`,
  `"reconciler"`, `"backend"`.
- `SignalDeliveryEvent::effect` on Postgres is always
  `SignalDeliveryEffect::Satisfied` until the outbox schema gains an
  `effect` column. Valkey surfaces the actual Lua producer's effect
  string via `SignalDeliveryEffect::from_wire`.
- `subscribe_instance_tags` still returns
  `EngineError::Unavailable` on both backends (producer wiring
  deferred per #311). The trait method exists for surface uniformity.

## Enums are `#[non_exhaustive]`

Every typed event enum + outcome enum is `#[non_exhaustive]`.
Consumer `match` statements must include a wildcard arm; future
additive variants (e.g. a new `LeaseHistoryEvent::Throttled`) ship
in a minor version without breaking your build.
