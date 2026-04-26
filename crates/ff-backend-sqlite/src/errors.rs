//! SQLite error classification — paralleling
//! `ff-backend-postgres::is_retryable_serialization`.
//!
//! RFC-023 §4.3: `SQLITE_BUSY` / `SQLITE_BUSY_TIMEOUT` / `SQLITE_LOCKED`
//! map to retry. Non-retryable kinds (`SQLITE_CORRUPT`, `SQLITE_FULL`,
//! etc.) surface as hard errors. Phase 1a landed the skeleton;
//! Phase 2a.1 wires it into [`crate::retry::retry_serializable`]
//! and re-exports [`MAX_ATTEMPTS`] alongside the classifier so
//! Wave-9 op modules can pull both symbols from a single path.

#[cfg(test)]
use sqlx::error::Error as SqlxError;

/// Re-export of the retry budget so callers have one import path for
/// classifier + budget + helper.
pub use crate::retry::MAX_ATTEMPTS;

/// Return `true` when the sqlx error is a transient busy-contention
/// fault that is safe to retry. Mirrors the shape of PG's
/// `is_retryable_serialization` classifier.
///
/// Wave-9 SERIALIZABLE ops wrap the classifier via
/// [`crate::retry::retry_serializable`].
pub fn is_retryable_sqlite_busy(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        // sqlx's SQLite driver surfaces the *extended* result code via
        // `DatabaseError::code()` (see `sqlx_sqlite::SqliteError` —
        // uses `sqlite3_extended_errcode`). We must match the primary
        // codes (5, 6) AND every extended code whose low 8 bits are
        // 5 or 6 so BUSY_RECOVERY / BUSY_SNAPSHOT / BUSY_TIMEOUT /
        // LOCKED_SHAREDCACHE / LOCKED_VTAB all classify as retryable.
        if let Some(code) = db_err.code()
            && let Ok(n) = code.parse::<i32>()
        {
            let primary = n & 0xff;
            return primary == 5 || primary == 6;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_db_error_is_not_retryable() {
        let err = SqlxError::RowNotFound;
        assert!(!is_retryable_sqlite_busy(&err));
    }
}
