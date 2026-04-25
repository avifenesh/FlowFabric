//! RFC-019 Stage C — Typed stream events.
//!
//! Stage A/B shipped the cursor machinery + backend adapters emitting
//! `StreamEvent { payload: Bytes, … }`, forcing every consumer to
//! parse a backend-shaped byte blob (Valkey: NUL-delimited field map,
//! Postgres: `serde_json`). This module promotes each family to a
//! typed enum so consumers `match` instead of parsing.
//!
//! One enum + one subscription alias per family, paralleling the
//! four-family allow-list in [`crate::stream_subscribe::StreamFamily`]:
//!
//! | Family           | Event enum                 | Subscription alias           |
//! |------------------|----------------------------|------------------------------|
//! | LeaseHistory     | [`LeaseHistoryEvent`]      | [`LeaseHistorySubscription`] |
//! | Completion       | [`CompletionEvent`]        | [`CompletionSubscription`]   |
//! | SignalDelivery   | [`SignalDeliveryEvent`]    | [`SignalDeliverySubscription`] |
//! | InstanceTags     | [`InstanceTagEvent`]       | [`InstanceTagSubscription`]  |
//!
//! Each event carries the same inline-hot fields the Stage A
//! `StreamEvent` envelope carried (`cursor`, `execution_id` where
//! applicable, `timestamp`) plus family-specific fields promoted out
//! of the old opaque payload.
//!
//! `#[non_exhaustive]` is applied to every variant-bearing enum and
//! every event struct so new variants / fields land additively. The
//! owner-adjudicated v0.9 four-family allow-list stays in force — new
//! families require an RFC amendment.

use std::pin::Pin;

use tokio_stream::Stream;

use crate::engine_error::EngineError;
use crate::stream_subscribe::StreamCursor;
use crate::types::{ExecutionId, LeaseId, SignalId, TimestampMs, WaitpointId, WorkerInstanceId};

// Rustdoc reminder: every `#[non_exhaustive]` struct below MUST pair
// with a public constructor so external backend crates (ff-backend-
// valkey, ff-backend-postgres) can still build instances. Adding a
// new field to a non_exhaustive struct is additive at call sites that
// use the constructor; callers that build via struct literal would
// break, which is exactly what `non_exhaustive` is there to prevent.

// ─── LeaseHistory ─────────────────────────────────────────────────

/// Per-event payload of `subscribe_lease_history`.
///
/// Each variant carries the fence triple fields the Lua producer
/// writes (`lease_id`, plus attempt/worker correlation where
/// available) and the monotonic `at` timestamp derived from the
/// backend's native stream id / event id.
///
/// `revoked_by` is `String` today for forward compatibility; we may
/// promote to a typed enum post-v0.10 once the revocation-source
/// taxonomy settles. Known producers emit `"operator"`,
/// `"reconciler"`, or `"backend"` today.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseHistoryEvent {
    Acquired {
        cursor: StreamCursor,
        execution_id: ExecutionId,
        lease_id: LeaseId,
        worker_instance_id: Option<WorkerInstanceId>,
        at: TimestampMs,
    },
    Renewed {
        cursor: StreamCursor,
        execution_id: ExecutionId,
        lease_id: LeaseId,
        worker_instance_id: Option<WorkerInstanceId>,
        at: TimestampMs,
    },
    Expired {
        cursor: StreamCursor,
        execution_id: ExecutionId,
        lease_id: Option<LeaseId>,
        prev_owner: Option<WorkerInstanceId>,
        at: TimestampMs,
    },
    Reclaimed {
        cursor: StreamCursor,
        execution_id: ExecutionId,
        new_lease_id: LeaseId,
        new_owner: Option<WorkerInstanceId>,
        at: TimestampMs,
    },
    Revoked {
        cursor: StreamCursor,
        execution_id: ExecutionId,
        lease_id: Option<LeaseId>,
        /// String today for forward compat; may promote to typed
        /// enum post-v0.10 once taxonomy settles. Known values:
        /// `"operator"`, `"reconciler"`, `"backend"`.
        revoked_by: String,
        at: TimestampMs,
    },
}

impl LeaseHistoryEvent {
    /// Position cursor to persist + replay from.
    pub fn cursor(&self) -> &StreamCursor {
        match self {
            Self::Acquired { cursor, .. }
            | Self::Renewed { cursor, .. }
            | Self::Expired { cursor, .. }
            | Self::Reclaimed { cursor, .. }
            | Self::Revoked { cursor, .. } => cursor,
        }
    }

    /// Execution the event pertains to.
    pub fn execution_id(&self) -> &ExecutionId {
        match self {
            Self::Acquired { execution_id, .. }
            | Self::Renewed { execution_id, .. }
            | Self::Expired { execution_id, .. }
            | Self::Reclaimed { execution_id, .. }
            | Self::Revoked { execution_id, .. } => execution_id,
        }
    }

    /// Monotonic backend-stamped time of the event.
    pub fn at(&self) -> TimestampMs {
        match self {
            Self::Acquired { at, .. }
            | Self::Renewed { at, .. }
            | Self::Expired { at, .. }
            | Self::Reclaimed { at, .. }
            | Self::Revoked { at, .. } => *at,
        }
    }
}

/// Stream of typed lease-history events.
pub type LeaseHistorySubscription =
    Pin<Box<dyn Stream<Item = Result<LeaseHistoryEvent, EngineError>> + Send>>;

// ─── Completion ──────────────────────────────────────────────────

/// Outcome classification for a completion event.
///
/// `#[non_exhaustive]` so future terminal states (e.g. `Suspended`
/// promoted to a terminal distinct from `Cancelled`) land additively.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionOutcome {
    Success,
    Failure,
    Cancelled,
    /// Outcome the backend surfaced but this crate version does not
    /// recognise. The raw string is retained so operator tooling can
    /// still log it.
    Other(String),
}

impl CompletionOutcome {
    /// Map a wire-string outcome to the typed enum. Unknown strings
    /// fall through to [`CompletionOutcome::Other`] rather than an
    /// error — completion events are advisory and a new producer
    /// variant must not crash old subscribers.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "success" => Self::Success,
            "failure" => Self::Failure,
            "cancelled" => Self::Cancelled,
            other => Self::Other(other.to_string()),
        }
    }
}

/// Per-event payload of `subscribe_completion`.
///
/// Completion events are terminal state transitions surfaced to
/// downstream DAG consumers. Postgres carries a durable event-id
/// cursor; Valkey Stage B rides pubsub + always yields
/// [`StreamCursor::empty`] (no durable replay).
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct CompletionEvent {
    pub cursor: StreamCursor,
    pub execution_id: ExecutionId,
    pub outcome: CompletionOutcome,
    pub at: TimestampMs,
}

impl CompletionEvent {
    /// Construct a `CompletionEvent`. Backend adapters go through
    /// this constructor instead of struct-literal syntax so future
    /// additive fields land as builder methods without breaking
    /// call sites.
    pub fn new(
        cursor: StreamCursor,
        execution_id: ExecutionId,
        outcome: CompletionOutcome,
        at: TimestampMs,
    ) -> Self {
        Self {
            cursor,
            execution_id,
            outcome,
            at,
        }
    }
}

/// Stream of typed completion events.
pub type CompletionSubscription =
    Pin<Box<dyn Stream<Item = Result<CompletionEvent, EngineError>> + Send>>;

// ─── SignalDelivery ──────────────────────────────────────────────

/// Effect of a signal delivery on the target waitpoint.
///
/// `#[non_exhaustive]` for future effects (e.g. `Throttled`,
/// `Coalesced`).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalDeliveryEffect {
    /// The signal was delivered and its waitpoint transitioned to
    /// satisfied.
    Satisfied,
    /// The signal was buffered against a pending waitpoint.
    Buffered,
    /// The signal was deduped against an earlier delivery with the
    /// same idempotency key.
    Deduped,
    /// Effect surfaced by the backend but not recognised by this
    /// crate version. Raw string preserved for observability.
    Other(String),
}

impl SignalDeliveryEffect {
    pub fn from_wire(s: &str) -> Self {
        match s {
            "satisfied" => Self::Satisfied,
            "buffered" => Self::Buffered,
            "deduped" => Self::Deduped,
            other => Self::Other(other.to_string()),
        }
    }
}

/// Per-event payload of `subscribe_signal_delivery`.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct SignalDeliveryEvent {
    pub cursor: StreamCursor,
    pub execution_id: ExecutionId,
    pub signal_id: SignalId,
    pub waitpoint_id: Option<WaitpointId>,
    pub source_identity: Option<String>,
    pub effect: SignalDeliveryEffect,
    pub at: TimestampMs,
}

impl SignalDeliveryEvent {
    /// Construct a `SignalDeliveryEvent`. Backend adapters use this
    /// constructor so future additive fields are non-breaking.
    pub fn new(
        cursor: StreamCursor,
        execution_id: ExecutionId,
        signal_id: SignalId,
        waitpoint_id: Option<WaitpointId>,
        source_identity: Option<String>,
        effect: SignalDeliveryEffect,
        at: TimestampMs,
    ) -> Self {
        Self {
            cursor,
            execution_id,
            signal_id,
            waitpoint_id,
            source_identity,
            effect,
            at,
        }
    }
}

/// Stream of typed signal-delivery events.
pub type SignalDeliverySubscription =
    Pin<Box<dyn Stream<Item = Result<SignalDeliveryEvent, EngineError>> + Send>>;

// ─── InstanceTags ────────────────────────────────────────────────

/// Per-event payload of `subscribe_instance_tags`.
///
/// Producer wiring is deferred (see #311); trait-level surface exists
/// for API uniformity across the four families. Every backend's
/// default impl returns `EngineError::Unavailable`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstanceTagEvent {
    /// Tag attached to an instance-scoped grouping.
    Attached {
        cursor: StreamCursor,
        execution_id: ExecutionId,
        tag: String,
        at: TimestampMs,
    },
    /// Tag cleared from an instance-scoped grouping.
    Cleared {
        cursor: StreamCursor,
        execution_id: ExecutionId,
        tag: String,
        at: TimestampMs,
    },
}

/// Stream of typed instance-tag events.
pub type InstanceTagSubscription =
    Pin<Box<dyn Stream<Item = Result<InstanceTagEvent, EngineError>> + Send>>;
