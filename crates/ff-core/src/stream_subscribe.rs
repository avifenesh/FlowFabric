//! RFC-019 Stage A/B/C — cross-backend stream-cursor subscription
//! surface (shared primitives).
//!
//! This module defines the [`StreamCursor`] every `subscribe_*`
//! method accepts, plus the codec helpers backends use to
//! encode/decode their native stream positions (Valkey `(ms, seq)`,
//! Postgres `event_id`). Family-specific typed event enums + per-
//! family subscription aliases live in [`crate::stream_events`].
//!
//! Four-family allow-list (RFC-019 §Open Questions #5, owner-
//! adjudicated 2026-04-24): new families require an RFC amendment.

use bytes::Bytes;

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
