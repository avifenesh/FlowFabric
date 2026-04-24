//! Valkey-side thin wrapper over [`ff_core::handle_codec`].
//!
//! **RFC-v0.7 Wave 1c.** The byte-layout + wire-version logic moved to
//! `ff_core::handle_codec` so both this crate and the Postgres backend
//! decode the same shape. This file now (a) adapts the core codec's
//! [`HandlePayload`] / [`DecodedHandle`] types to the Valkey-internal
//! `HandleFields` alias the rest of the crate already names, (b)
//! builds / validates the `Handle` wrapper (backend tag = Valkey +
//! [`HandleKind`]), and (c) maps [`HandleDecodeError`] into
//! [`EngineError`] at the trait boundary.
//!
//! Wire-format compatibility note: v1 (pre-Wave-1c) Valkey handles
//! decode cleanly on the core codec's v1-compat path. The v2 format
//! (leading `0x02` magic + embedded `BackendTag`) is produced by all
//! encode paths from Wave 1c onward. See `ff_core::handle_codec` for
//! the full wire spec.

use ff_core::backend::{BackendTag, Handle, HandleKind, HandleOpaque};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::handle_codec::{decode as core_decode, encode as core_encode, HandlePayload};

// Keep the crate-internal alias so the rest of ff-backend-valkey keeps
// naming the payload as `HandleFields` without churn.
pub(crate) type HandleFields = HandlePayload;

/// Encode a `HandleFields` into a `Handle` carrying a Valkey-tagged
/// `HandleOpaque`. Kind is caller-supplied (`Fresh` for `claim_next` /
/// `claim_from_grant`, `Resumed` for `claim_from_reclaim_grant`,
/// `Suspended` for `suspend`).
pub(crate) fn encode_handle(fields: &HandleFields, kind: HandleKind) -> Handle {
    let opaque: HandleOpaque = core_encode(BackendTag::Valkey, fields);
    Handle::new(BackendTag::Valkey, kind, opaque)
}

/// Decode a `Handle` into its backing `HandleFields`. Returns
/// `EngineError::Validation` on any shape / version / parse mismatch
/// — the Valkey backend will not route an op on a malformed handle.
///
/// Also enforces the backend-tag invariant: a handle minted by
/// another backend (e.g. Postgres) is rejected with
/// `ValidationKind::HandleFromOtherBackend` without attempting to
/// parse the rest of the buffer.
pub(crate) fn decode_handle(handle: &Handle) -> Result<HandleFields, EngineError> {
    if handle.backend != BackendTag::Valkey {
        return Err(EngineError::Validation {
            kind: ValidationKind::HandleFromOtherBackend,
            detail: format!(
                "expected={:?} actual={:?}",
                BackendTag::Valkey,
                handle.backend
            ),
        });
    }
    let decoded = core_decode(&handle.opaque)?;
    // The core codec also carries the backend tag in the buffer. If
    // the embedded tag disagrees with the Handle's `backend` field,
    // something up-stream is lying — treat that as
    // `HandleFromOtherBackend` too.
    if decoded.tag != BackendTag::Valkey {
        return Err(EngineError::Validation {
            kind: ValidationKind::HandleFromOtherBackend,
            detail: format!(
                "expected={:?} actual={:?} (embedded tag)",
                BackendTag::Valkey,
                decoded.tag
            ),
        });
    }
    Ok(decoded.payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::partition::PartitionConfig;
    use ff_core::types::{AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId,
        WorkerInstanceId};

    fn sample() -> HandleFields {
        HandlePayload::new(
            ExecutionId::solo(&LaneId::new("default"), &PartitionConfig::default()),
            AttemptIndex::new(3),
            AttemptId::new(),
            LeaseId::new(),
            LeaseEpoch(7),
            30_000,
            LaneId::new("default"),
            WorkerInstanceId::new("worker-1"),
        )
    }

    #[test]
    fn round_trip_fresh() {
        let fields = sample();
        let handle = encode_handle(&fields, HandleKind::Fresh);
        assert_eq!(handle.backend, BackendTag::Valkey);
        assert_eq!(handle.kind, HandleKind::Fresh);
        let decoded = decode_handle(&handle).expect("round-trip");
        assert_eq!(decoded, fields);
    }

    #[test]
    fn round_trip_resumed() {
        let fields = sample();
        let handle = encode_handle(&fields, HandleKind::Resumed);
        assert_eq!(handle.kind, HandleKind::Resumed);
        let _ = decode_handle(&handle).expect("resumed round-trip");
    }

    #[test]
    fn rejects_postgres_tagged_handle() {
        // Postgres-tagged handle presented to Valkey decode path.
        let fields = sample();
        let pg_opaque = ff_core::handle_codec::encode(BackendTag::Postgres, &fields);
        let handle = Handle::new(BackendTag::Postgres, HandleKind::Fresh, pg_opaque);
        let err = decode_handle(&handle).unwrap_err();
        match err {
            EngineError::Validation { kind, .. } => {
                assert_eq!(kind, ValidationKind::HandleFromOtherBackend);
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }
}
