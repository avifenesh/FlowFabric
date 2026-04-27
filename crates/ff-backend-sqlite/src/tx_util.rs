//! Shared transaction + parsing helpers for the SQLite backend.
//!
//! Extracted from `backend.rs` + `suspend_ops.rs` so both modules
//! share one implementation — avoids subtle drift (error messages,
//! parsing rules, txn semantics) between the hot-path and the
//! suspend/signal bodies (Copilot review #PR-378).

use std::time::{SystemTime, UNIX_EPOCH};

use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::types::ExecutionId;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::errors::map_sqlx_error;

/// Unix-millis wall clock.
pub(crate) fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

/// Decompose `{fp:N}:<uuid>` into `(partition_index, uuid)`.
///
/// Enforces the `u16` partition-index invariant explicitly —
/// negative or `> u16::MAX` values surface as
/// `EngineError::Validation(InvalidInput)` rather than silently
/// flowing into an `i64` partition_key bind.
pub(crate) fn split_exec_id(eid: &ExecutionId) -> Result<(i64, Uuid), EngineError> {
    let s = eid.as_str();
    let rest = s
        .strip_prefix("{fp:")
        .ok_or_else(|| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("execution_id missing `{{fp:` prefix: {s}"),
        })?;
    let close = rest.find("}:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id missing `}}:`: {s}"),
    })?;
    let part_u16: u16 = rest[..close].parse().map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id partition index not in u16 range: {s}"),
    })?;
    let uuid = Uuid::parse_str(&rest[close + 2..]).map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id UUID invalid: {s}"),
    })?;
    Ok((i64::from(part_u16), uuid))
}

/// Acquire a pooled connection and issue `BEGIN IMMEDIATE`.
pub(crate) async fn begin_immediate(
    pool: &SqlitePool,
) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, EngineError> {
    let mut conn = pool.acquire().await.map_err(map_sqlx_error)?;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(conn)
}

/// Commit on success, best-effort rollback on commit failure so the
/// returned connection is clean when the pool recycles it.
pub(crate) async fn commit_or_rollback(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
) -> Result<(), EngineError> {
    if let Err(e) = sqlx::query("COMMIT")
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)
    {
        let _ = sqlx::query("ROLLBACK").execute(&mut **conn).await;
        return Err(e);
    }
    Ok(())
}

/// Best-effort rollback on an error path.
pub(crate) async fn rollback_quiet(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) {
    let _ = sqlx::query("ROLLBACK").execute(&mut **conn).await;
}
