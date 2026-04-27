//! SQLite-side thin wrapper over [`ff_core::handle_codec`].
//!
//! **RFC-023 Phase 2a.1.5 scaffold.** Mirrors
//! `ff_backend_postgres::handle_codec` / `ff_backend_valkey::handle_codec`
//! so the SQLite trait impls (Phase 2a.2+) have a symmetric
//! encode/decode path, and so the workspace has compile-time evidence
//! that the core codec is backend-agnostic once `BackendTag::Sqlite`
//! (wire byte `0x03`) is in play.
//!
//! # Tag-validation posture
//!
//! [`decode_handle`] rejects any handle not minted by the SQLite
//! backend with [`ValidationKind::HandleFromOtherBackend`] — symmetric
//! with the Valkey/Postgres wrappers. The guard runs *twice*: once on
//! the outer [`Handle::backend`] tag (before touching the opaque
//! bytes), and once on the `BackendTag` embedded inside the opaque
//! payload after core-decode. A mismatch between the outer tag and
//! the embedded tag means something upstream is lying; we treat that
//! as `HandleFromOtherBackend` too.
//!
//! At Phase 2a.1.5 the SQLite backend still returns `Unavailable` for
//! every trait method that touches a [`Handle`], so the bodies here
//! are unused by the live hot path today — they exist to lock in the
//! backend-tag invariant and to give Phase 2a.2 a ready-to-use API.

use ff_core::backend::{BackendTag, Handle, HandleKind, HandleOpaque};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::handle_codec::{decode as core_decode, encode as core_encode, HandlePayload};

/// Encode a [`HandlePayload`] into a SQLite-tagged [`Handle`].
#[allow(dead_code)] // Wired up in Phase 2a.2+ when Handle-bearing ops land.
pub(crate) fn encode_handle(payload: &HandlePayload, kind: HandleKind) -> Handle {
    let opaque: HandleOpaque = core_encode(BackendTag::Sqlite, payload);
    Handle::new(BackendTag::Sqlite, kind, opaque)
}

/// Decode a [`Handle`] under the SQLite-backend invariant. A
/// Valkey/Postgres-tagged handle decodes successfully at the core
/// codec layer but is rejected here with
/// `ValidationKind::HandleFromOtherBackend`.
#[allow(dead_code)] // Wired up in Phase 2a.2+.
pub(crate) fn decode_handle(handle: &Handle) -> Result<HandlePayload, EngineError> {
    if handle.backend != BackendTag::Sqlite {
        return Err(EngineError::Validation {
            kind: ValidationKind::HandleFromOtherBackend,
            detail: format!(
                "expected={:?} actual={:?}",
                BackendTag::Sqlite,
                handle.backend
            ),
        });
    }
    let decoded = core_decode(&handle.opaque)?;
    if decoded.tag != BackendTag::Sqlite {
        return Err(EngineError::Validation {
            kind: ValidationKind::HandleFromOtherBackend,
            detail: format!(
                "expected={:?} actual={:?} (embedded tag)",
                BackendTag::Sqlite,
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
            WorkerInstanceId::new("sqlite-worker-1"),
        )
    }

    #[test]
    fn encode_decode_round_trip() {
        let p = sample_payload();
        let h = encode_handle(&p, HandleKind::Fresh);
        assert_eq!(h.backend, BackendTag::Sqlite);
        assert_eq!(h.kind, HandleKind::Fresh);
        let back = decode_handle(&h).expect("round-trip");
        assert_eq!(back, p);
    }

    /// Handle minted by the Valkey backend must be rejected with
    /// `HandleFromOtherBackend` — the detail text names the foreign
    /// backend so operators can trace a cross-backend leak.
    #[test]
    fn rejects_valkey_tagged_handle() {
        let p = sample_payload();
        let valkey_opaque = ff_core::handle_codec::encode(BackendTag::Valkey, &p);
        let handle = Handle::new(BackendTag::Valkey, HandleKind::Fresh, valkey_opaque);
        let err = decode_handle(&handle).unwrap_err();
        match err {
            EngineError::Validation { kind, detail } => {
                assert_eq!(kind, ValidationKind::HandleFromOtherBackend);
                assert!(
                    detail.contains("Valkey"),
                    "detail should name the foreign backend, got {detail:?}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// Symmetric guard for Postgres-tagged handles.
    #[test]
    fn rejects_postgres_tagged_handle() {
        let p = sample_payload();
        let pg_opaque = ff_core::handle_codec::encode(BackendTag::Postgres, &p);
        let handle = Handle::new(BackendTag::Postgres, HandleKind::Fresh, pg_opaque);
        let err = decode_handle(&handle).unwrap_err();
        match err {
            EngineError::Validation { kind, detail } => {
                assert_eq!(kind, ValidationKind::HandleFromOtherBackend);
                assert!(
                    detail.contains("Postgres"),
                    "detail should name the foreign backend, got {detail:?}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// Guard the embedded-tag check: outer tag says Sqlite, but the
    /// opaque buffer was encoded with a foreign tag. The decoder must
    /// catch the mismatch on the second guard rather than accepting
    /// the payload.
    #[test]
    fn rejects_embedded_tag_mismatch() {
        let p = sample_payload();
        let valkey_opaque = ff_core::handle_codec::encode(BackendTag::Valkey, &p);
        // Outer tag lies: claims Sqlite, opaque is Valkey-encoded.
        let handle = Handle::new(BackendTag::Sqlite, HandleKind::Fresh, valkey_opaque);
        let err = decode_handle(&handle).unwrap_err();
        match err {
            EngineError::Validation { kind, detail } => {
                assert_eq!(kind, ValidationKind::HandleFromOtherBackend);
                assert!(
                    detail.contains("embedded tag"),
                    "detail should flag the embedded-tag mismatch, got {detail:?}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }
}
