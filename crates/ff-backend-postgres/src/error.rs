//! `sqlx::Error` → [`EngineError`] mapping for the Postgres backend.
//!
//! **RFC-v0.7 Wave 0.** This is a map-sketch: enough surface for
//! subsequent waves' trait-method bodies to route transport faults
//! through `EngineError::Transport { backend: "postgres", .. }` plus
//! the two typed cases the design-question matrix already pinned:
//!
//! * `SqlState::UniqueViolation` (`23505`) → [`EngineError::Conflict`]
//!   — mirrors the Valkey backend's conflict classification for
//!   duplicate-key writes (e.g. waitpoint id collision, unique
//!   index violation on `ff_completion_event` etc.).
//! * `SqlState::SerializationFailure` (`40001`) / `DeadlockDetected`
//!   (`40P01`) → [`EngineError::Contention`] — per Q11 (isolation
//!   level default) these are the retryable serialization faults
//!   SERIALIZABLE can raise; callers retry per RFC-010 §10.7.
//!
//! Every other `sqlx::Error` boxes through `Transport` as a safe
//! default. Future waves refine (e.g. `RowNotFound` → `NotFound`,
//! connection-pool-closed → `Unavailable`).

use ff_core::engine_error::{ConflictKind, ContentionKind, EngineError};

/// Convert a `sqlx::Error` into an [`EngineError`].
///
/// Kept as a free function (rather than a `From` impl) so call sites
/// can thread extra context (method label, key, etc.) through
/// [`ff_core::engine_error::backend_context`] alongside. A blanket
/// `From<sqlx::Error> for EngineError` is also provided below for
/// ergonomics in `?` chains where no extra context is needed.
pub fn map_sqlx_error(err: sqlx::Error) -> EngineError {
    if let Some(db_err) = err.as_database_error()
        && let Some(code) = db_err.code()
    {
        // SQLSTATE codes are 5-char strings. Match the Q11-pinned
        // pair first; everything else falls through to Transport.
        match code.as_ref() {
            // unique_violation
            "23505" => {
                return EngineError::Conflict(ConflictKind::RotationConflict(
                    db_err.message().to_string(),
                ));
            }
            // serialization_failure / deadlock_detected
            "40001" | "40P01" => {
                return EngineError::Contention(ContentionKind::LeaseConflict);
            }
            _ => {}
        }
    }
    EngineError::Transport {
        backend: "postgres",
        source: Box::new(err),
    }
}

impl From<sqlx::Error> for PostgresTransportError {
    fn from(err: sqlx::Error) -> Self {
        PostgresTransportError(err)
    }
}

/// Thin newtype over `sqlx::Error` so a blanket
/// `From<sqlx::Error> for EngineError` doesn't conflict with
/// ff-core's own orphan rules when downstream crates add their own.
/// Backend-internal call sites use [`map_sqlx_error`] directly; this
/// wrapper exists for future cases where we want to attach a
/// typed source to a different `EngineError` variant.
#[derive(Debug)]
pub struct PostgresTransportError(pub sqlx::Error);

impl std::fmt::Display for PostgresTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "postgres transport: {}", self.0)
    }
}

impl std::error::Error for PostgresTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
