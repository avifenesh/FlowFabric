//! Backend-agnostic codec for [`HandleOpaque`] payloads.
//!
//! **RFC-v0.7 Wave 1c.** Extracted from `ff-backend-valkey` so the
//! Postgres backend (Wave 4+) decodes the same wire shape. Worker-B
//! §4.1 flagged `handle_codec` as one of the top Valkey-shape-leaking
//! subsystems in the v0.7 migration master spec; this module is the
//! relocation target.
//!
//! # Responsibilities
//!
//! * Serialize a [`HandlePayload`] (attempt-cookie fields every
//!   backend needs to route an op) into the opaque byte buffer carried
//!   inside a [`Handle`]. See [`encode`].
//! * Parse that buffer back into a [`HandlePayload`] plus the
//!   [`BackendTag`] that minted it. See [`decode`].
//! * Expose a typed [`HandleDecodeError`] mapping cleanly to
//!   `EngineError::Validation { kind: Corruption, .. }` at the backend
//!   boundary. See [`From<HandleDecodeError> for EngineError`].
//!
//! # Wire format v2
//!
//! New-format opaque buffers are self-identifying — the leading byte
//! distinguishes pre-Wave-1c (Valkey-only) buffers from v2 buffers:
//!
//! ```text
//! v2 layout (Wave 1c+):
//!   [0]       u8    magic (0x02) — new-format marker
//!   [1]       u8    wire version (today: 0x01)
//!   [2]       u8    BackendTag wire byte (0x01 = Valkey, 0x02 = Postgres)
//!   [3..]             fields (see §Fields)
//!
//! v1 layout (pre-Wave-1c, Valkey-only — read-only compat):
//!   [0]       u8    wire version (0x01)
//!   [1..]             fields (§Fields)
//! ```
//!
//! ## §Fields
//!
//! ```text
//! u32(LE)  execution_id length
//! utf8     execution_id bytes
//! u32(LE)  attempt_index
//! u32(LE)  attempt_id length
//! utf8     attempt_id bytes
//! u32(LE)  lease_id length
//! utf8     lease_id bytes
//! u64(LE)  lease_epoch
//! u64(LE)  lease_ttl_ms
//! u32(LE)  lane_id length
//! utf8     lane_id bytes
//! u32(LE)  worker_instance_id length
//! utf8     worker_instance_id bytes
//! ```
//!
//! # Backward-compat
//!
//! v1 is Valkey-only: any pre-Wave-1c handle in-flight at upgrade time
//! decodes under `BackendTag::Valkey`. The magic byte `0x02` was
//! chosen to be disjoint from the v1 leading byte (`0x01` = version
//! tag) — the decoder switches on the first byte.

use crate::backend::{BackendTag, HandleOpaque};
use crate::engine_error::{EngineError, ValidationKind};
use crate::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, WorkerInstanceId,
};

/// v2 "new-format" marker. Disjoint from the v1 leading byte
/// (`V1_VERSION_TAG = 0x01`) so the decoder can switch cleanly.
const V2_MAGIC: u8 = 0x02;
/// v2 wire version. Bumped when field layout changes.
const V2_WIRE_VERSION: u8 = 0x01;
/// v1 (pre-Wave-1c) leading byte. Present only for the compat decode path.
const V1_VERSION_TAG: u8 = 0x01;

/// Decoded view of an encoded [`HandleOpaque`] — the minimum set of
/// fields every backend needs to construct its per-op KEYS + ARGV.
///
/// `#[non_exhaustive]`: fields grow additively when new attempt-cookie
/// state lands (e.g. a Wave-5 partition routing hint). Construct via
/// [`HandlePayload::new`]; field-access is via the pub fields.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HandlePayload {
    pub execution_id: ExecutionId,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub lease_ttl_ms: u64,
    pub lane_id: LaneId,
    pub worker_instance_id: WorkerInstanceId,
}

impl HandlePayload {
    /// Construct a payload. All fields are mandatory at Wave 1c — the
    /// `#[non_exhaustive]` guard lets future fields land additively.
    #[allow(clippy::too_many_arguments)] // all 8 fields are required attempt-cookie state
    pub fn new(
        execution_id: ExecutionId,
        attempt_index: AttemptIndex,
        attempt_id: AttemptId,
        lease_id: LeaseId,
        lease_epoch: LeaseEpoch,
        lease_ttl_ms: u64,
        lane_id: LaneId,
        worker_instance_id: WorkerInstanceId,
    ) -> Self {
        Self {
            execution_id,
            attempt_index,
            attempt_id,
            lease_id,
            lease_epoch,
            lease_ttl_ms,
            lane_id,
            worker_instance_id,
        }
    }
}

/// Output of [`decode`] — the decoded [`HandlePayload`] plus the
/// [`BackendTag`] embedded in the opaque buffer (or inferred from the
/// v1 compat path).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodedHandle {
    pub tag: BackendTag,
    pub payload: HandlePayload,
}

impl DecodedHandle {
    pub fn new(tag: BackendTag, payload: HandlePayload) -> Self {
        Self { tag, payload }
    }
}

/// Typed decode-failure classification. Mapped to
/// `EngineError::Validation { kind: Corruption, .. }` at the backend
/// boundary via the [`From`] impl.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HandleDecodeError {
    /// Buffer shorter than required by the current read position.
    Truncated { needed: usize, at: usize, have: usize },
    /// Trailing bytes after the last expected field.
    TrailingBytes { pos: usize, len: usize },
    /// v2 wire-version byte did not match a supported version.
    BadWireVersion { got: u8 },
    /// v1-path version byte did not match the expected tag.
    BadV1Version { got: u8 },
    /// v2 backend-tag byte did not map to a known [`BackendTag`].
    BadBackendTag { got: u8 },
    /// A length-prefixed string had invalid UTF-8.
    InvalidUtf8 { field: &'static str },
    /// A parsed id/field failed its own validation (e.g. `ExecutionId::parse`).
    ParseField { field: &'static str, detail: String },
}

impl std::fmt::Display for HandleDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandleDecodeError::Truncated { needed, at, have } => write!(
                f,
                "truncated handle: needed {needed} bytes at offset {at}, have {have}"
            ),
            HandleDecodeError::TrailingBytes { pos, len } => {
                write!(f, "trailing bytes in handle: pos={pos}, len={len}")
            }
            HandleDecodeError::BadWireVersion { got } => {
                write!(f, "handle v2 wire-version byte {got:#x} not recognised")
            }
            HandleDecodeError::BadV1Version { got } => write!(
                f,
                "handle v1 version byte {got:#x} not recognised (expected {V1_VERSION_TAG:#x})"
            ),
            HandleDecodeError::BadBackendTag { got } => {
                write!(f, "handle backend-tag byte {got:#x} not recognised")
            }
            HandleDecodeError::InvalidUtf8 { field } => {
                write!(f, "handle field `{field}` is not valid UTF-8")
            }
            HandleDecodeError::ParseField { field, detail } => {
                write!(f, "handle field `{field}` parse failed: {detail}")
            }
        }
    }
}

impl std::error::Error for HandleDecodeError {}

impl From<HandleDecodeError> for EngineError {
    fn from(err: HandleDecodeError) -> Self {
        EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("handle_codec: {err}"),
        }
    }
}

/// Encode a [`HandlePayload`] into a [`HandleOpaque`] tagged with
/// `tag`. Produces a v2 buffer — callers decoding via [`decode`] always
/// get the tag they encoded.
pub fn encode(tag: BackendTag, payload: &HandlePayload) -> HandleOpaque {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    buf.push(V2_MAGIC);
    buf.push(V2_WIRE_VERSION);
    buf.push(tag.wire_byte());
    write_fields(&mut buf, payload);
    HandleOpaque::new(buf.into_boxed_slice())
}

/// Decode a [`HandleOpaque`] into a [`DecodedHandle`]. Accepts both
/// v2 buffers (leading `V2_MAGIC`) and legacy v1 Valkey buffers
/// (leading `V1_VERSION_TAG`); the v1 path returns
/// `tag = BackendTag::Valkey`.
pub fn decode(opaque: &HandleOpaque) -> Result<DecodedHandle, HandleDecodeError> {
    let bytes = opaque.as_bytes();
    let mut cur = Cursor::new(bytes);
    let lead = cur.read_u8()?;
    let tag = match lead {
        V2_MAGIC => {
            let wire_version = cur.read_u8()?;
            if wire_version != V2_WIRE_VERSION {
                return Err(HandleDecodeError::BadWireVersion { got: wire_version });
            }
            let tag_byte = cur.read_u8()?;
            BackendTag::from_wire_byte(tag_byte)
                .ok_or(HandleDecodeError::BadBackendTag { got: tag_byte })?
        }
        V1_VERSION_TAG => BackendTag::Valkey,
        other => return Err(HandleDecodeError::BadV1Version { got: other }),
    };
    let payload = read_fields(&mut cur)?;
    cur.expect_eof()?;
    Ok(DecodedHandle { tag, payload })
}

// ── Field serialization ─────────────────────────────────────────────────

fn write_fields(buf: &mut Vec<u8>, f: &HandlePayload) {
    write_str(buf, &f.execution_id.to_string());
    buf.extend_from_slice(&f.attempt_index.0.to_le_bytes());
    write_str(buf, &f.attempt_id.to_string());
    write_str(buf, &f.lease_id.to_string());
    buf.extend_from_slice(&f.lease_epoch.0.to_le_bytes());
    buf.extend_from_slice(&f.lease_ttl_ms.to_le_bytes());
    write_str(buf, f.lane_id.as_str());
    write_str(buf, f.worker_instance_id.as_str());
}

fn read_fields(cur: &mut Cursor<'_>) -> Result<HandlePayload, HandleDecodeError> {
    let execution_id_str = cur.read_str("execution_id")?;
    let execution_id = ExecutionId::parse(&execution_id_str).map_err(|e| {
        HandleDecodeError::ParseField {
            field: "execution_id",
            detail: e.to_string(),
        }
    })?;
    let attempt_index = AttemptIndex::new(cur.read_u32()?);
    let attempt_id_str = cur.read_str("attempt_id")?;
    let attempt_id =
        AttemptId::parse(&attempt_id_str).map_err(|e| HandleDecodeError::ParseField {
            field: "attempt_id",
            detail: e.to_string(),
        })?;
    let lease_id_str = cur.read_str("lease_id")?;
    let lease_id = LeaseId::parse(&lease_id_str).map_err(|e| HandleDecodeError::ParseField {
        field: "lease_id",
        detail: e.to_string(),
    })?;
    let lease_epoch = LeaseEpoch(cur.read_u64()?);
    let lease_ttl_ms = cur.read_u64()?;
    let lane_id_str = cur.read_str("lane_id")?;
    let lane_id = LaneId::new(lane_id_str);
    let worker_str = cur.read_str("worker_instance_id")?;
    let worker_instance_id = WorkerInstanceId::new(worker_str);
    Ok(HandlePayload {
        execution_id,
        attempt_index,
        attempt_id,
        lease_id,
        lease_epoch,
        lease_ttl_ms,
        lane_id,
        worker_instance_id,
    })
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    // Clamp rather than panic: if a caller somehow hands in a
    // >4 GiB string (impossible at the SDK's attempt-cookie scale),
    // we write a u32::MAX length prefix + the first 4 GiB of bytes.
    // decode() will detect the trailing truncation.
    let (len, take) = match u32::try_from(bytes.len()) {
        Ok(n) => (n, bytes.len()),
        Err(_) => (u32::MAX, u32::MAX as usize),
    };
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&bytes[..take]);
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], HandleDecodeError> {
        if self.pos + n > self.bytes.len() {
            return Err(HandleDecodeError::Truncated {
                needed: n,
                at: self.pos,
                have: self.bytes.len(),
            });
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, HandleDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, HandleDecodeError> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(b))
    }

    fn read_u64(&mut self) -> Result<u64, HandleDecodeError> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(b))
    }

    fn read_str(&mut self, field: &'static str) -> Result<String, HandleDecodeError> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| HandleDecodeError::InvalidUtf8 { field })
    }

    fn expect_eof(&self) -> Result<(), HandleDecodeError> {
        if self.pos != self.bytes.len() {
            return Err(HandleDecodeError::TrailingBytes {
                pos: self.pos,
                len: self.bytes.len(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::PartitionConfig;

    fn sample_payload() -> HandlePayload {
        HandlePayload {
            execution_id: ExecutionId::solo(
                &LaneId::new("default"),
                &PartitionConfig::default(),
            ),
            attempt_index: AttemptIndex::new(3),
            attempt_id: AttemptId::new(),
            lease_id: LeaseId::new(),
            lease_epoch: LeaseEpoch(7),
            lease_ttl_ms: 30_000,
            lane_id: LaneId::new("default"),
            worker_instance_id: WorkerInstanceId::new("worker-1"),
        }
    }

    #[test]
    fn round_trip_valkey_v2() {
        let p = sample_payload();
        let opaque = encode(BackendTag::Valkey, &p);
        // v2 leading byte.
        assert_eq!(opaque.as_bytes()[0], V2_MAGIC);
        assert_eq!(opaque.as_bytes()[1], V2_WIRE_VERSION);
        assert_eq!(opaque.as_bytes()[2], BackendTag::Valkey.wire_byte());
        let decoded = decode(&opaque).expect("round-trip");
        assert_eq!(decoded.tag, BackendTag::Valkey);
        assert_eq!(decoded.payload, p);
    }

    #[test]
    fn round_trip_postgres_v2() {
        let p = sample_payload();
        let opaque = encode(BackendTag::Postgres, &p);
        assert_eq!(opaque.as_bytes()[2], BackendTag::Postgres.wire_byte());
        let decoded = decode(&opaque).expect("round-trip");
        assert_eq!(decoded.tag, BackendTag::Postgres);
        assert_eq!(decoded.payload, p);
    }

    /// Regression guard: an opaque buffer produced by the pre-Wave-1c
    /// Valkey-only codec (leading byte = 0x01, no magic, no embedded
    /// backend tag) must still decode cleanly as `BackendTag::Valkey`.
    /// Non-negotiable per Wave 1c spec.
    #[test]
    fn old_v1_format_decodes_as_valkey() {
        let p = sample_payload();
        // Synthesise a v1 buffer byte-for-byte identical to what the
        // pre-Wave-1c `encode_handle` in ff-backend-valkey produced.
        let mut buf: Vec<u8> = Vec::new();
        buf.push(V1_VERSION_TAG); // single version byte, no magic, no tag
        write_fields(&mut buf, &p);
        let opaque = HandleOpaque::new(buf.into_boxed_slice());
        let decoded = decode(&opaque).expect("v1 compat decode");
        assert_eq!(decoded.tag, BackendTag::Valkey);
        assert_eq!(decoded.payload, p);
    }

    #[test]
    fn truncated_handle_rejected() {
        // Only the magic byte — read_u8 for wire_version will fail.
        let opaque = HandleOpaque::new(Box::new([V2_MAGIC]));
        let err = decode(&opaque).unwrap_err();
        assert!(matches!(err, HandleDecodeError::Truncated { .. }));
    }

    #[test]
    fn bad_v2_wire_version_rejected() {
        // v2 magic, unknown wire version.
        let opaque = HandleOpaque::new(Box::new([V2_MAGIC, 0xFF, 0x01]));
        let err = decode(&opaque).unwrap_err();
        assert!(matches!(err, HandleDecodeError::BadWireVersion { got: 0xFF }));
    }

    #[test]
    fn bad_backend_tag_rejected() {
        // v2 magic + valid wire version + bogus tag byte.
        let opaque = HandleOpaque::new(Box::new([V2_MAGIC, V2_WIRE_VERSION, 0xFE]));
        let err = decode(&opaque).unwrap_err();
        assert!(matches!(err, HandleDecodeError::BadBackendTag { got: 0xFE }));
    }

    #[test]
    fn bad_leading_byte_rejected() {
        // Neither v2 magic nor v1 version tag.
        let opaque = HandleOpaque::new(Box::new([0xAB]));
        let err = decode(&opaque).unwrap_err();
        assert!(matches!(err, HandleDecodeError::BadV1Version { got: 0xAB }));
    }

    #[test]
    fn decode_error_maps_to_validation_corruption() {
        let err: EngineError = HandleDecodeError::Truncated {
            needed: 4,
            at: 1,
            have: 1,
        }
        .into();
        match err {
            EngineError::Validation { kind, detail } => {
                assert_eq!(kind, ValidationKind::Corruption);
                assert!(detail.contains("handle_codec"));
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }
}
