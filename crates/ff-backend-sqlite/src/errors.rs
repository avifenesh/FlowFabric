//! SQLite error classification — paralleling
//! `ff-backend-postgres::is_retryable_serialization`.
//!
//! RFC-023 §4.3: `SQLITE_BUSY` / `SQLITE_BUSY_TIMEOUT` / `SQLITE_LOCKED`
//! map to retry. Non-retryable kinds (`SQLITE_CORRUPT`, `SQLITE_FULL`,
//! etc.) surface as hard errors. Phase 1a lands the skeleton; Wave 9
//! SERIALIZABLE ops wrap it once real impls exist.

#[cfg(test)]
use sqlx::error::Error as SqlxError;

/// Return `true` when the sqlx error is a transient busy-contention
/// fault that is safe to retry. Mirrors the shape of PG's
/// `is_retryable_serialization` classifier.
///
/// Phase 1a exposes the classifier as a skeleton; real usage lands
/// alongside Wave 9 SERIALIZABLE ops in Phase 2+.
#[allow(dead_code)] // Phase 1a skeleton — wired in Phase 2+.
pub fn is_retryable_sqlite_busy(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        // sqlx's SQLite driver surfaces extended result codes via the
        // `code()` accessor. The three SQLITE_BUSY / SQLITE_LOCKED
        // families are decimal 5 and 6 in the base result codes.
        if let Some(code) = db_err.code() {
            return matches!(code.as_ref(), "5" | "6" | "517" | "261");
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
