//! Internal [`HandleOpaque`] encoding for the Valkey backend.
//!
//! **RFC-012 Stage 1b, option A (synth_handle per op).** ff-sdk's
//! `ClaimedTask` does not yet refactor its shape onto a `Handle` —
//! that lands in Stage 1d. For Stage 1b the SDK encodes the minimal
//! attempt-cookie fields into a `HandleOpaque` on every forwarder
//! entry, and this crate decodes them back on every FCALL. The
//! format is private to this crate — no consumer ever sees the bytes
//! — so it can change freely across stages.
//!
//! # Wire layout (version-tagged)
//!
//! ```text
//! [0]       u8    version tag (today: 0x01)
//! [1..5]    u32   execution_id length (LE)
//! [5..]     utf8  execution_id bytes
//! [..+4]    u32   attempt_index (LE)
//! [..+4]    u32   attempt_id length (LE)
//! [..]      utf8  attempt_id bytes
//! [..+4]    u32   lease_id length (LE)
//! [..]      utf8  lease_id bytes
//! [..+8]    u64   lease_epoch (LE)
//! [..+8]    u64   lease_ttl_ms (LE)
//! [..+4]    u32   lane_id length (LE)
//! [..]      utf8  lane_id bytes
//! [..+4]    u32   worker_instance_id length (LE)
//! [..]      utf8  worker_instance_id bytes
//! ```
//!
//! Length fields are `u32`s so any string up to 4 GiB encodes losslessly
//! — far past realistic ExecutionId / UUID / lane lengths. The version
//! tag is a forward-compat door: if the field set changes between
//! Stage 1b and 1d, a future `0x02` branch can land additively without
//! breaking in-flight handles (handles do not persist across process
//! boundaries today, so even a hard switch would be safe — but the tag
//! is cheap insurance).

use ff_core::backend::{BackendTag, Handle, HandleKind, HandleOpaque};
use ff_core::engine_error::EngineError;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, WorkerInstanceId,
};

/// Decoded view of a `HandleOpaque` — the minimum set of fields every
/// Stage 1b forwarder needs to construct its Lua KEYS + ARGV.
///
/// Produced by [`encode_handle`] (via `ClaimedTask::synth_handle`) and
/// consumed by [`decode_handle`] on each trait method entry.
#[derive(Clone, Debug)]
pub(crate) struct HandleFields {
    pub execution_id: ExecutionId,
    pub attempt_index: AttemptIndex,
    pub attempt_id: AttemptId,
    pub lease_id: LeaseId,
    pub lease_epoch: LeaseEpoch,
    pub lease_ttl_ms: u64,
    pub lane_id: LaneId,
    pub worker_instance_id: WorkerInstanceId,
}

const VERSION_TAG: u8 = 0x01;

/// Encode a `HandleFields` into a `Handle` carrying a Valkey-tagged
/// `HandleOpaque`. Kind is caller-supplied (`Fresh` for `claim_next` /
/// `claim_from_grant`, `Resumed` for `claim_from_reclaim_grant`).
pub(crate) fn encode_handle(fields: &HandleFields, kind: HandleKind) -> Handle {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    buf.push(VERSION_TAG);
    // Every string field here is produced by an SDK type's `.to_string()`
    // / `.as_str()`: UUID = 36 bytes, LaneId/WorkerInstanceId are
    // config-bounded (WorkerConfig validator rejects >1KiB today),
    // ExecutionId prefixed partition tag + UUID ≈ 50 bytes. None
    // approach 4 GiB. `write_str` clamps at `u32::MAX`; on the
    // vanishingly improbable path of an attacker-controlled
    // oversized caller, the encoded slot length is clamped to
    // u32::MAX and the trailing bytes are dropped — the `decode_handle`
    // side detects the truncation (`Validation(InvalidInput)`) on the
    // next op and the worker fails loudly rather than silently.
    write_str(&mut buf, &fields.execution_id.to_string());
    buf.extend_from_slice(&fields.attempt_index.0.to_le_bytes());
    write_str(&mut buf, &fields.attempt_id.to_string());
    write_str(&mut buf, &fields.lease_id.to_string());
    buf.extend_from_slice(&fields.lease_epoch.0.to_le_bytes());
    buf.extend_from_slice(&fields.lease_ttl_ms.to_le_bytes());
    write_str(&mut buf, fields.lane_id.as_str());
    write_str(&mut buf, fields.worker_instance_id.as_str());
    Handle::new(BackendTag::Valkey, kind, HandleOpaque::new(buf.into_boxed_slice()))
}

/// Decode a `Handle` into its backing `HandleFields`. Returns
/// `EngineError::Validation` on any shape / version / parse mismatch
/// — the Valkey backend will not route an op on a malformed handle.
pub(crate) fn decode_handle(handle: &Handle) -> Result<HandleFields, EngineError> {
    if handle.backend != BackendTag::Valkey {
        return Err(invalid("handle backend tag is not Valkey"));
    }
    let bytes = handle.opaque.as_bytes();
    let mut cur = Cursor::new(bytes);
    let version = cur.read_u8()?;
    if version != VERSION_TAG {
        return Err(invalid(&format!(
            "handle version tag {version:#x} not recognised (expected {VERSION_TAG:#x})"
        )));
    }
    let execution_id_str = cur.read_str()?;
    let execution_id = ExecutionId::parse(&execution_id_str)
        .map_err(|e| invalid(&format!("handle execution_id parse failed: {e}")))?;
    let attempt_index = AttemptIndex::new(cur.read_u32()?);
    let attempt_id_str = cur.read_str()?;
    let attempt_id = AttemptId::parse(&attempt_id_str)
        .map_err(|e| invalid(&format!("handle attempt_id parse failed: {e}")))?;
    let lease_id_str = cur.read_str()?;
    let lease_id = LeaseId::parse(&lease_id_str)
        .map_err(|e| invalid(&format!("handle lease_id parse failed: {e}")))?;
    let lease_epoch = LeaseEpoch(cur.read_u64()?);
    let lease_ttl_ms = cur.read_u64()?;
    let lane_id_str = cur.read_str()?;
    let lane_id = LaneId::new(lane_id_str);
    let worker_str = cur.read_str()?;
    let worker_instance_id = WorkerInstanceId::new(worker_str);
    cur.expect_eof()?;
    Ok(HandleFields {
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
    // `decode_handle` will detect the trailing truncation and return
    // `EngineError::Validation(InvalidInput)` on the first op — the
    // worker fails loudly at op time rather than on the spawn-path
    // stack, which matches the crate-wide strict-parse posture
    // (gemini-code-assist review finding on PR #119: avoid `expect`
    // on storage-adjacent data paths).
    let (len, take) = match u32::try_from(bytes.len()) {
        Ok(n) => (n, bytes.len()),
        Err(_) => (u32::MAX, u32::MAX as usize),
    };
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&bytes[..take]);
}

fn invalid(msg: &str) -> EngineError {
    EngineError::Validation {
        kind: ff_core::engine_error::ValidationKind::InvalidInput,
        detail: format!("invalid Valkey backend handle: {msg}"),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], EngineError> {
        if self.pos + n > self.bytes.len() {
            return Err(invalid(&format!(
                "truncated handle: needed {n} bytes at offset {pos}, have {len}",
                pos = self.pos,
                len = self.bytes.len()
            )));
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, EngineError> {
        Ok(self.take(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, EngineError> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(b))
    }

    fn read_u64(&mut self) -> Result<u64, EngineError> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(b))
    }

    fn read_str(&mut self) -> Result<String, EngineError> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| invalid(&format!("handle string is not valid UTF-8: {e}")))
    }

    fn expect_eof(&self) -> Result<(), EngineError> {
        if self.pos != self.bytes.len() {
            return Err(invalid(&format!(
                "trailing bytes in handle: pos={pos}, len={len}",
                pos = self.pos,
                len = self.bytes.len()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::partition::PartitionConfig;
    use ff_core::types::FlowId;

    fn sample() -> HandleFields {
        HandleFields {
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
    fn round_trip_fresh() {
        let fields = sample();
        let handle = encode_handle(&fields, HandleKind::Fresh);
        assert_eq!(handle.backend, BackendTag::Valkey);
        assert_eq!(handle.kind, HandleKind::Fresh);
        let decoded = decode_handle(&handle).expect("round-trip");
        assert_eq!(decoded.execution_id, fields.execution_id);
        assert_eq!(decoded.attempt_index, fields.attempt_index);
        assert_eq!(decoded.attempt_id, fields.attempt_id);
        assert_eq!(decoded.lease_id, fields.lease_id);
        assert_eq!(decoded.lease_epoch, fields.lease_epoch);
        assert_eq!(decoded.lease_ttl_ms, fields.lease_ttl_ms);
        assert_eq!(decoded.lane_id, fields.lane_id);
        assert_eq!(decoded.worker_instance_id, fields.worker_instance_id);
    }

    #[test]
    fn round_trip_resumed() {
        let fields = sample();
        let handle = encode_handle(&fields, HandleKind::Resumed);
        assert_eq!(handle.kind, HandleKind::Resumed);
        let _ = decode_handle(&handle).expect("resumed round-trip");
    }

    #[test]
    fn truncated_handle_rejected() {
        // Only version tag, no fields.
        let handle = Handle::new(
            BackendTag::Valkey,
            HandleKind::Fresh,
            HandleOpaque::new(Box::new([VERSION_TAG])),
        );
        let err = decode_handle(&handle).unwrap_err();
        assert!(format!("{err}").contains("truncated"));
    }

    #[test]
    fn bad_version_rejected() {
        let handle = Handle::new(
            BackendTag::Valkey,
            HandleKind::Fresh,
            HandleOpaque::new(Box::new([0xFF])),
        );
        let err = decode_handle(&handle).unwrap_err();
        assert!(format!("{err}").contains("version tag"));
    }

    // FlowId is used elsewhere in the crate; touch it here to silence
    // an unused-import warning if the tests file is added standalone.
    #[allow(dead_code)]
    fn _flow_id_touch() -> FlowId {
        FlowId::new()
    }
}
