//! In-process pub/sub wiring (RFC-023 §4.2).
//!
//! Phase 2b.1 wires the five broadcast-channel outboxes (Group D.1).
//! Each channel corresponds to one outbox table; subscribers (Phase
//! 2b.2 / later) hold a `Receiver` and wake on post-commit emits.
//!
//! # Ordering / durability invariant (RFC-023 §4.2 A2)
//!
//! The broadcast channel is **wakeup only** — not the durable
//! record of what happened. Events land as rows on the matching
//! outbox table **inside** the writing transaction; the
//! `emit_post_commit` helper fires AFTER `tx.commit()` returns, with
//! the row(s) just committed as the payload. A late subscriber that
//! missed a broadcast tick can still recover every event by tailing
//! the outbox table `WHERE event_id > cursor`. That is the RFC-019
//! cursor-resume contract.
//!
//! # Partition routing
//!
//! Each [`OutboxEvent`] carries the partition key it landed on; a
//! partition-scoped subscriber filters on its own `OutboxEvent::partition_key`.
//! The post-commit emit never drops the event (broadcast `send`
//! returns `Ok` when there are zero receivers and `Err(SendError)`
//! only when the receiver count is zero — the producer side is
//! infallible from our POV).

use tokio::sync::broadcast;

/// Capacity of each broadcast channel's ring buffer. Tuned conservatively
/// for the dev-only single-writer envelope; late subscribers catch up via
/// the outbox table, so lost-wakeup only means an extra outbox poll on
/// the catch-up side (not a lost event).
const DEFAULT_CAPACITY: usize = 256;

/// One wakeup broadcast payload, one per outbox row written.
///
/// Carries enough identification for a subscriber to filter on its
/// partition / execution of interest without re-reading the outbox
/// row. The full row (including any non-identifying payload columns
/// — e.g. `outcome`, `event_type`, `details`) is fetched by the
/// subscriber from the outbox table using `event_id` as a cursor,
/// matching the RFC-019 catch-up shape.
#[derive(Clone, Debug)]
pub(crate) struct OutboxEvent {
    /// Auto-incrementing outbox row id (monotonic per table). Read by
    /// Phase 2b.2 `subscribe_*` consumers as the catch-up cursor; the
    /// producer side fills it from `last_insert_rowid()`.
    #[allow(dead_code)] // Phase 2b.2 subscribers read this
    pub(crate) event_id: i64,
    /// Partition the write targeted. SQLite currently runs with
    /// `num_flow_partitions = 1` (§4.1 A3) so this is always `0`
    /// today, but the field exists for topology symmetry with PG
    /// and for future partition fan-out if the invariant ever
    /// relaxes.
    #[allow(dead_code)] // Phase 2b.2 partition-scoped subscribers filter on this
    pub(crate) partition_key: i64,
}

/// Per-backend in-process wakeup channels. One broadcast channel per
/// outbox family; each `Sender` is held on `SqliteBackendInner` and
/// subscribers derive `Receiver` handles via `Sender::subscribe()`
/// (Phase 2b.2+).
pub(crate) struct PubSub {
    /// Lease lifecycle (acquired / renewed / reclaimed / revoked) —
    /// rides `ff_lease_event`.
    pub(crate) lease_history: broadcast::Sender<OutboxEvent>,
    /// Completion (success / failed / cancelled / expired / skipped) —
    /// rides `ff_completion_event`.
    pub(crate) completion: broadcast::Sender<OutboxEvent>,
    /// Signal delivery — rides `ff_signal_event`.
    pub(crate) signal_delivery: broadcast::Sender<OutboxEvent>,
    /// Append-frame durable stream wakeups — rides `ff_stream_frame`.
    /// Phase 2b.2's `tail_stream` consumer subscribes to this.
    pub(crate) stream_frame: broadcast::Sender<OutboxEvent>,
    /// Operator events (priority_changed / replayed / flow_cancel_requested)
    /// — rides `ff_operator_event`.
    pub(crate) operator_event: broadcast::Sender<OutboxEvent>,
}

impl PubSub {
    pub(crate) fn new() -> Self {
        let (lease_history, _) = broadcast::channel(DEFAULT_CAPACITY);
        let (completion, _) = broadcast::channel(DEFAULT_CAPACITY);
        let (signal_delivery, _) = broadcast::channel(DEFAULT_CAPACITY);
        let (stream_frame, _) = broadcast::channel(DEFAULT_CAPACITY);
        let (operator_event, _) = broadcast::channel(DEFAULT_CAPACITY);
        Self {
            lease_history,
            completion,
            signal_delivery,
            stream_frame,
            operator_event,
        }
    }

    /// Emit one post-commit wakeup. Swallows the `SendError` that
    /// appears when no subscriber is attached (the RFC-019 contract
    /// is that durable replay covers missed wakeups — a no-receiver
    /// emit is NOT an error from the producer's POV).
    pub(crate) fn emit(sender: &broadcast::Sender<OutboxEvent>, ev: OutboxEvent) {
        let _ = sender.send(ev);
    }
}

impl Default for PubSub {
    fn default() -> Self {
        Self::new()
    }
}
