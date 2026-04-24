//! Postgres-side thin wrapper over [`ff_core::handle_codec`].
//!
//! **RFC-v0.7 Wave 1c scaffold.** Mirrors
//! `ff_backend_valkey::handle_codec` so the Postgres trait impls
//! (Waves 4+) have a symmetric encode/decode path and so the
//! workspace has compile-time evidence that the core codec is
//! backend-agnostic.
//!
//! At Wave 1c the Postgres backend still returns `Unavailable` for
//! every trait method that touches a [`Handle`], so the bodies here
//! are unused by the live hot path today — they exist to lock in the
//! backend-tag invariant (a Postgres backend never accepts a
//! Valkey-tagged handle) and to give Wave 4 a ready-to-use API.

use ff_core::backend::{BackendTag, Handle, HandleKind, HandleOpaque};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::handle_codec::{decode as core_decode, encode as core_encode, HandlePayload};

/// Encode a [`HandlePayload`] into a Postgres-tagged [`Handle`].
#[allow(dead_code)] // Wired up in Wave 4+ when Handle-bearing ops land.
pub(crate) fn encode_handle(payload: &HandlePayload, kind: HandleKind) -> Handle {
    let opaque: HandleOpaque = core_encode(BackendTag::Postgres, payload);
    Handle::new(BackendTag::Postgres, kind, opaque)
}

/// Decode a [`Handle`] under the Postgres-backend invariant. A
/// Valkey-tagged handle decodes successfully at the core codec layer
/// but is rejected here with `ValidationKind::HandleFromOtherBackend`.
#[allow(dead_code)] // Wired up in Wave 4+.
pub(crate) fn decode_handle(handle: &Handle) -> Result<HandlePayload, EngineError> {
    if handle.backend != BackendTag::Postgres {
        return Err(EngineError::Validation {
            kind: ValidationKind::HandleFromOtherBackend,
            detail: format!(
                "expected={:?} actual={:?}",
                BackendTag::Postgres,
                handle.backend
            ),
        });
    }
    let decoded = core_decode(&handle.opaque)?;
    if decoded.tag != BackendTag::Postgres {
        return Err(EngineError::Validation {
            kind: ValidationKind::HandleFromOtherBackend,
            detail: format!(
                "expected={:?} actual={:?} (embedded tag)",
                BackendTag::Postgres,
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
    use ff_core::types::{
        AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, WorkerInstanceId,
    };

    fn sample_payload() -> HandlePayload {
        HandlePayload::new(
            ExecutionId::solo(&LaneId::new("default"), &PartitionConfig::default()),
            AttemptIndex::new(1),
            AttemptId::new(),
            LeaseId::new(),
            LeaseEpoch(1),
            30_000,
            LaneId::new("default"),
            WorkerInstanceId::new("pg-worker-1"),
        )
    }

    #[test]
    fn round_trip_fresh() {
        let p = sample_payload();
        let h = encode_handle(&p, HandleKind::Fresh);
        assert_eq!(h.backend, BackendTag::Postgres);
        let back = decode_handle(&h).expect("round-trip");
        assert_eq!(back, p);
    }

    /// Wave 1c spec: a Valkey-tagged handle presented to the Postgres
    /// backend decodes at the core codec layer but the backend
    /// rejects with `HandleFromOtherBackend`.
    #[test]
    fn valkey_handle_rejected_as_other_backend() {
        let p = sample_payload();
        let valkey_opaque = ff_core::handle_codec::encode(BackendTag::Valkey, &p);
        let handle = Handle::new(BackendTag::Valkey, HandleKind::Fresh, valkey_opaque);
        let err = decode_handle(&handle).unwrap_err();
        match err {
            EngineError::Validation { kind, .. } => {
                assert_eq!(kind, ValidationKind::HandleFromOtherBackend);
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }
}
