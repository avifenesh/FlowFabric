//! SERIALIZABLE retry helper for SQLite per RFC-023 §4.3.
//!
//! Mirrors `ff-backend-postgres::operator::retry_serializable`: up to
//! [`MAX_ATTEMPTS`] iterations of the closure, re-running when the
//! returned error classifies as a transient busy-contention fault
//! (`SQLITE_BUSY` / `SQLITE_BUSY_TIMEOUT` / `SQLITE_LOCKED`). All
//! non-retryable kinds (`SQLITE_CORRUPT`, `SQLITE_FULL`, misuse, etc.)
//! propagate immediately.
//!
//! Backoff shape matches the PG reference (`5ms * 2^attempt`) — 5ms,
//! 10ms between the three attempts. Deliberately short: under the
//! single-writer envelope §4.1 targets, busy contention resolves in
//! tens of milliseconds at most, and the caller already absorbed the
//! SQLite driver's per-statement busy-wait budget before the error
//! surfaced here.
//!
//! Wired into Wave-9 SERIALIZABLE ops in Phase 2a.2 / 2a.3:
//! `cancel_flow`, `cancel_flow_header`, `ack_cancel_member`,
//! `change_priority`, `replay_execution`, `complete_attempt`,
//! `fail_attempt`, `deliver_signal`, plus the scanner-supervisor's
//! budget-reconcile path.

use std::time::Duration;

/// Retry budget paralleling
/// `ff-backend-postgres::operator::MAX_ATTEMPTS` and
/// `CANCEL_FLOW_MAX_ATTEMPTS` (RFC-023 §4.3 / RFC §4.2 template).
pub const MAX_ATTEMPTS: u32 = 3;

/// Trait letting the retry helper classify an arbitrary error type
/// as retryable. Implemented for `sqlx::Error` out of the box.
///
/// Op-level wrappers that translate `sqlx::Error` → `EngineError`
/// before entering the retry loop implement this trait on their
/// translated error to keep the classification at a single source of
/// truth.
pub trait IsRetryableBusy {
    fn is_retryable_busy(&self) -> bool;
}

impl IsRetryableBusy for sqlx::Error {
    fn is_retryable_busy(&self) -> bool {
        crate::errors::is_retryable_sqlite_busy(self)
    }
}

/// Runs `f` up to [`MAX_ATTEMPTS`] times, retrying when the error
/// classifies as transient busy contention. Between retries, sleeps
/// `5ms * 2^attempt` to match the PG reference shape.
pub async fn retry_serializable<F, Fut, T, E>(mut f: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: IsRetryableBusy,
{
    let mut last: Option<E> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match f().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                if err.is_retryable_busy() {
                    if attempt + 1 < MAX_ATTEMPTS {
                        let ms = 5u64 * (1u64 << attempt);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    last = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }
    // Safe: MAX_ATTEMPTS >= 1 and the loop only falls through via
    // the retryable branch, which always populates `last`.
    Err(last.expect("retry loop exited without populating last error"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::error::{DatabaseError, ErrorKind};
    use std::borrow::Cow;
    use std::error::Error as StdError;
    use std::fmt;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // Minimal mock `DatabaseError` so we can build a real
    // `sqlx::Error::Database` with a chosen SQLite code in tests.
    // sqlx's `SqliteError::new` is `pub(crate)`, so we cannot reuse
    // it directly — the `DatabaseError` trait surface is public and
    // is the correct seam.
    #[derive(Debug)]
    struct MockSqliteError {
        code: String,
    }

    impl fmt::Display for MockSqliteError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "(mock code: {})", self.code)
        }
    }

    impl StdError for MockSqliteError {}

    impl DatabaseError for MockSqliteError {
        fn message(&self) -> &str {
            "mock"
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code.as_str()))
        }

        fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    fn db_err(code: &str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(MockSqliteError {
            code: code.to_string(),
        }))
    }

    // ── Classifier tests ─────────────────────────────────────────

    #[test]
    fn classifier_matches_busy() {
        // Primary codes.
        assert!(db_err("5").is_retryable_busy(), "SQLITE_BUSY (5)");
        assert!(db_err("6").is_retryable_busy(), "SQLITE_LOCKED (6)");
        // BUSY extended codes: low 8 bits == 5.
        assert!(
            db_err("261").is_retryable_busy(),
            "SQLITE_BUSY_RECOVERY (261 = 5 | 1<<8)"
        );
        assert!(
            db_err("517").is_retryable_busy(),
            "SQLITE_BUSY_SNAPSHOT (517 = 5 | 2<<8)"
        );
        assert!(
            db_err("773").is_retryable_busy(),
            "SQLITE_BUSY_TIMEOUT (773 = 5 | 3<<8)"
        );
        // LOCKED extended codes: low 8 bits == 6.
        assert!(
            db_err("262").is_retryable_busy(),
            "SQLITE_LOCKED_SHAREDCACHE (262 = 6 | 1<<8)"
        );
        assert!(
            db_err("518").is_retryable_busy(),
            "SQLITE_LOCKED_VTAB (518 = 6 | 2<<8)"
        );
    }

    #[test]
    fn classifier_rejects_corrupt() {
        // SQLITE_CORRUPT = 11
        assert!(!db_err("11").is_retryable_busy());
    }

    #[test]
    fn classifier_rejects_full() {
        // SQLITE_FULL = 13
        assert!(!db_err("13").is_retryable_busy());
    }

    #[test]
    fn classifier_rejects_misuse() {
        // SQLITE_MISUSE = 21
        assert!(!db_err("21").is_retryable_busy());
    }

    #[test]
    fn classifier_rejects_non_db_error() {
        assert!(!sqlx::Error::RowNotFound.is_retryable_busy());
    }

    #[test]
    fn classifier_rejects_extended_non_busy_family() {
        // SQLITE_IOERR = 10; SQLITE_IOERR_READ = 10 | 1<<8 = 266.
        // High byte collides nothing with BUSY/LOCKED — low 8 bits
        // must be 5 or 6 for a match. Guard against future mask bugs.
        assert!(!db_err("266").is_retryable_busy());
        // SQLITE_CONSTRAINT = 19; SQLITE_CONSTRAINT_UNIQUE = 19 | 8<<8 = 2067.
        assert!(!db_err("2067").is_retryable_busy());
    }

    // ── retry_serializable tests ─────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn retry_succeeds_on_first_try() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let result: Result<u32, sqlx::Error> = retry_serializable(|| {
            let calls = calls_c.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(42)
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_exhausts_after_max_attempts() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let result: Result<(), sqlx::Error> = retry_serializable(|| {
            let calls = calls_c.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(db_err("5")) // SQLITE_BUSY
            }
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_retryable_busy());
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_returns_non_retryable_immediately() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let result: Result<(), sqlx::Error> = retry_serializable(|| {
            let calls = calls_c.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(db_err("11")) // SQLITE_CORRUPT
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_succeeds_on_second_attempt() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let result: Result<u32, sqlx::Error> = retry_serializable(|| {
            let calls = calls_c.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(db_err("5"))
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_backoff_matches_pg_shape() {
        // With start_paused, any sleep advances virtual time only when
        // the runtime auto-advances (which happens when all tasks are
        // parked). We observe total elapsed virtual time after an
        // exhausted retry: sleeps are 5ms (after attempt 0) + 10ms
        // (after attempt 1) = 15ms. No sleep after the final attempt.
        let start = tokio::time::Instant::now();
        let _: Result<(), sqlx::Error> = retry_serializable(|| async {
            Err::<(), _>(db_err("5"))
        })
        .await;
        let elapsed = start.elapsed();
        assert_eq!(
            elapsed,
            Duration::from_millis(15),
            "expected 5ms + 10ms between three attempts"
        );
    }
}
