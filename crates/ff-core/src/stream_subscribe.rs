//! RFC-019 Stage A — cross-backend stream-cursor subscription surface.
//!
//! `EngineBackend::subscribe_{lease_history,completion,signal_delivery,
//! instance_tags}` return a [`StreamSubscription`] — a pinned
//! `tokio_stream::Stream` of `Result<StreamEvent, EngineError>`. The
//! cursor is an opaque byte blob the consumer persists across crashes
//! and hands back on resume; the first byte identifies the backend
//! family + version so cursors stay stable across backend upgrades.
//!
//! This module defines the wire types only — no trait default is here;
//! defaults live on the `EngineBackend` trait in
//! [`crate::engine_backend`]. Real impls live in `ff-backend-valkey`
//! (Valkey `XREAD BLOCK`-backed lease_history) and `ff-backend-postgres`
//! (`LISTEN/NOTIFY`-backed completion) per RFC-019 §Implementation Plan.
//!
//! Four-family allow-list (RFC-019 §Open Questions #5, owner-
//! adjudicated 2026-04-24): new families require an RFC amendment.

use std::pin::Pin;

use bytes::Bytes;
use tokio_stream::Stream;

use crate::engine_error::EngineError;
use crate::types::{ExecutionId, TimestampMs};

/// Opaque, backend-versioned cursor. Consumers persist the bytes, hand
/// them back on resume. The first byte MUST encode a backend-family +
/// version prefix so cursors stay stable across backend upgrades
/// (Valkey: `0x01`, Postgres: `0x02`, …). The remainder is backend-
/// specific.
///
/// [`StreamCursor::empty`] is the "start from tail / now" sentinel
/// every backend accepts. Cursor byte order is owner-adjudicated
/// opaque — consumers MUST NOT parse the bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamCursor(pub Bytes);

impl StreamCursor {
    /// Wrap arbitrary cursor bytes — typically only called by backend
    /// impls that just encoded a fresh cursor from a backend-native
    /// position (Valkey stream id, Postgres event_id).
    pub fn new(raw: impl Into<Bytes>) -> Self {
        Self(raw.into())
    }

    /// Borrow the raw cursor bytes for persistence. Consumers treat
    /// the slice as opaque.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Sentinel "start from tail" cursor. Backends interpret an empty
    /// cursor as "subscribe from now" (Valkey: XREAD `$`; Postgres:
    /// LISTEN from the current `max(event_id)`).
    pub fn empty() -> Self {
        Self(Bytes::new())
    }
}

/// Event families covered by the v0.9 allow-list (RFC-019 §Open
/// Questions #5). `#[non_exhaustive]` so v0.10+ families land without
/// breaking consumer match blocks, but the owner-adjudicated stance is
/// that new families require an RFC amendment — this is not a generic
/// escape hatch.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamFamily {
    LeaseHistory,
    Completion,
    SignalDelivery,
    InstanceTags,
}

/// Per-event payload.
///
/// - `family` identifies the event shape.
/// - `cursor` is the position this event occupies in the stream;
///   consumers persist it + hand it back on reconnect so replay begins
///   strictly after this event.
/// - `execution_id`, `attempt_index`, `timestamp` are inline hot fields
///   (RFC-019 §Open Questions #4, owner-adjudicated inline) so common
///   consumers do not need a follow-up `describe_execution` RPC.
/// - `payload` is the family-specific binary event body. Schema is
///   the backend's (not stabilised in Stage A — consumers parse via
///   family-specific helpers Stage B will ship).
///
/// `#[non_exhaustive]` so future inline metadata (e.g. `namespace`)
/// adds without a breaking change.
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

impl StreamEvent {
    /// Construct a minimal `StreamEvent`. The struct is
    /// `#[non_exhaustive]` so external crates cannot build via a
    /// literal — backends go through this constructor + builder.
    /// Optional inline metadata is added via `with_execution_id` /
    /// `with_attempt_index` so call-site shape stays stable across
    /// future field additions.
    pub fn new(
        family: StreamFamily,
        cursor: StreamCursor,
        timestamp: TimestampMs,
        payload: Bytes,
    ) -> Self {
        Self {
            family,
            cursor,
            execution_id: None,
            attempt_index: None,
            timestamp,
            payload,
        }
    }

    #[must_use]
    pub fn with_execution_id(mut self, id: ExecutionId) -> Self {
        self.execution_id = Some(id);
        self
    }

    #[must_use]
    pub fn with_attempt_index(mut self, idx: u32) -> Self {
        self.attempt_index = Some(idx);
        self
    }
}

/// Shape of a subscription stream.
///
/// Errors surface as `Err` items (not panics / stream-end); the
/// terminal `Err(EngineError::StreamDisconnected { cursor })` is the
/// owner-adjudicated disconnect contract (RFC-019 §Open Questions #2):
/// the consumer reconnects by re-calling the relevant `subscribe_*`
/// method with the cursor they observed. Non-terminal errors may be
/// followed by further events.
pub type StreamSubscription =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, EngineError>> + Send>>;

// ─── Valkey cursor codec ───
//
// Layout: `0x01` ++ ms_be(8) ++ seq_be(8). 17 bytes total. Empty
// `StreamCursor` (zero-length) means "tail from `$`" (current end).
//
// These helpers live in ff-core rather than ff-backend-valkey because
// the wire shape is part of the RFC-019 contract — a Postgres cursor
// can never decode as a Valkey cursor (different first byte) and
// consumers migrating between backends discard + restart cleanly.

/// Valkey family prefix byte (RFC-019 §Backend Semantics Appendix).
pub const VALKEY_CURSOR_PREFIX: u8 = 0x01;

/// Postgres family prefix byte.
pub const POSTGRES_CURSOR_PREFIX: u8 = 0x02;

/// Encode a Valkey stream id `(ms, seq)` into a cursor.
pub fn encode_valkey_cursor(ms: u64, seq: u64) -> StreamCursor {
    let mut buf = Vec::with_capacity(17);
    buf.push(VALKEY_CURSOR_PREFIX);
    buf.extend_from_slice(&ms.to_be_bytes());
    buf.extend_from_slice(&seq.to_be_bytes());
    StreamCursor::new(buf)
}

/// Decode a Valkey cursor back to `(ms, seq)`. Returns `None` for the
/// empty / tail-from-now cursor. Returns `Err` for malformed bytes
/// (wrong prefix, truncated).
pub fn decode_valkey_cursor(cursor: &StreamCursor) -> Result<Option<(u64, u64)>, &'static str> {
    let bytes = cursor.as_bytes();
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() != 17 || bytes[0] != VALKEY_CURSOR_PREFIX {
        return Err("stream_subscribe: cursor does not belong to the Valkey backend");
    }
    let mut ms = [0u8; 8];
    let mut seq = [0u8; 8];
    ms.copy_from_slice(&bytes[1..9]);
    seq.copy_from_slice(&bytes[9..17]);
    Ok(Some((u64::from_be_bytes(ms), u64::from_be_bytes(seq))))
}

/// Encode a Postgres `event_id` (i64, from `ff_completion_event`) into
/// a cursor.
pub fn encode_postgres_event_cursor(event_id: i64) -> StreamCursor {
    let mut buf = Vec::with_capacity(9);
    buf.push(POSTGRES_CURSOR_PREFIX);
    buf.extend_from_slice(&event_id.to_be_bytes());
    StreamCursor::new(buf)
}

/// Decode a Postgres event cursor back to `event_id`. `None` =
/// start-from-tail.
pub fn decode_postgres_event_cursor(cursor: &StreamCursor) -> Result<Option<i64>, &'static str> {
    let bytes = cursor.as_bytes();
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() != 9 || bytes[0] != POSTGRES_CURSOR_PREFIX {
        return Err("stream_subscribe: cursor does not belong to the Postgres backend");
    }
    let mut event_id = [0u8; 8];
    event_id.copy_from_slice(&bytes[1..9]);
    Ok(Some(i64::from_be_bytes(event_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valkey_cursor_roundtrip() {
        let c = encode_valkey_cursor(1_700_000_000_000, 42);
        assert_eq!(c.as_bytes()[0], VALKEY_CURSOR_PREFIX);
        let (ms, seq) = decode_valkey_cursor(&c).unwrap().unwrap();
        assert_eq!(ms, 1_700_000_000_000);
        assert_eq!(seq, 42);
    }

    #[test]
    fn valkey_empty_is_tail() {
        let c = StreamCursor::empty();
        assert!(decode_valkey_cursor(&c).unwrap().is_none());
    }

    #[test]
    fn valkey_rejects_postgres_cursor() {
        let c = encode_postgres_event_cursor(7);
        assert!(decode_valkey_cursor(&c).is_err());
    }

    #[test]
    fn postgres_cursor_roundtrip() {
        let c = encode_postgres_event_cursor(12345);
        assert_eq!(c.as_bytes()[0], POSTGRES_CURSOR_PREFIX);
        assert_eq!(decode_postgres_event_cursor(&c).unwrap(), Some(12345));
    }

    #[test]
    fn postgres_rejects_valkey_cursor() {
        let c = encode_valkey_cursor(1, 1);
        assert!(decode_postgres_event_cursor(&c).is_err());
    }
}
