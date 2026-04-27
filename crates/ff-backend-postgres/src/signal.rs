//! Signal delivery + HMAC secret rotation for the Postgres backend.
//!
//! **Wave 4 Agent D (RFC-v0.7 v0.7 migration-master).**
//!
//! **Scope note.** The Wave-4 brief asks for an 8-method
//! suspend+signal family. The actual `EngineBackend` trait today
//! exposes 6 of those (`suspend`, `observe_signals`,
//! `list_suspended`, `claim_resumed_execution`, `deliver_signal`,
//! `rotate_waitpoint_hmac_secret_all`); `try_suspend` and the
//! single-partition `rotate_waitpoint_hmac_secret` are not on the
//! trait surface. Extending the trait touches every parallel Wave-4
//! agent's compile graph and needs owner adjudication, so this
//! tranche ships the in-trait method that is fully standalone
//! (`rotate_waitpoint_hmac_secret_all`) plus the primitives the
//! other 5 methods will consume in a follow-up:
//!
//! * [`hmac_sign`] / [`hmac_verify`] — first Rust-side HMAC code in
//!   the workspace. The Valkey backend signs inside Lua; this module
//!   owns server-side signing on the Postgres path.
//! * [`SERIALIZABLE_RETRY_BUDGET`] — the Q11 retry cap used by the
//!   suspend / deliver_signal SERIALIZABLE sites when those land.
//! * [`current_active_kid`] / [`fetch_kid`] — keystore helpers.
//! * [`rotate_waitpoint_hmac_secret_all_impl`] — Q4 single-global-
//!   row write + active-flag flip. Wired into `EngineBackend` in
//!   [`crate::lib.rs`].
//!
//! HMAC sign/verify primitives hoisted to `ff_core::crypto::hmac` in
//! RFC-023 Phase 2b.2.1 so the Postgres + SQLite backends share one
//! Rust implementation. The Valkey backend still signs inside Lua, so
//! no third consumer is pending. This module re-exports for backward
//! compatibility of internal call sites.

use ff_core::engine_error::EngineError;
use sqlx::PgPool;

use crate::error::map_sqlx_error;

pub use ff_core::crypto::hmac::{hmac_sign, hmac_verify, HmacVerifyError};

/// Q11 retry budget for SERIALIZABLE transactions. On retry exhaustion
/// the suspend / deliver_signal call sites are expected to return
/// `EngineError::Contention(LeaseConflict)` so the reconciler (or
/// calling worker) reconstructs intent rather than spinning in-process.
pub const SERIALIZABLE_RETRY_BUDGET: usize = 3;

/// True iff `err` is a retryable serialization/deadlock fault.
/// Exposed for callers that run their own SERIALIZABLE-tx retry loop
/// and need to tell retryable from fatal on sqlx errors.
pub fn is_retryable_serialization(err: &sqlx::Error) -> bool {
    if let Some(db) = err.as_database_error()
        && let Some(code) = db.code()
    {
        matches!(code.as_ref(), "40001" | "40P01")
    } else {
        false
    }
}

/// Resolve the currently-active HMAC secret (kid + bytes) from
/// `ff_waitpoint_hmac`. Returns `Ok(None)` when the keystore is empty
/// (bootstrap race); callers treat that as a state error.
pub async fn current_active_kid(
    pool: &PgPool,
) -> Result<Option<(String, Vec<u8>)>, EngineError> {
    let row: Option<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT kid, secret FROM ff_waitpoint_hmac \
         WHERE active = TRUE \
         ORDER BY rotated_at_ms DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;
    Ok(row)
}

/// Fetch a specific kid's secret. Returns `Ok(None)` when the kid is
/// unknown. Includes inactive kids (inside the rotation grace window).
pub async fn fetch_kid(pool: &PgPool, kid: &str) -> Result<Option<Vec<u8>>, EngineError> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT secret FROM ff_waitpoint_hmac WHERE kid = $1",
    )
    .bind(kid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;
    Ok(row.map(|(s,)| s))
}

/// Implementation of `EngineBackend::rotate_waitpoint_hmac_secret_all`.
///
/// Q4 (v0.7 migration-master, adjudicated 2026-04-24): the Postgres
/// backend stores one global row per kid (no `partition_id` column).
/// The "all" fan-out collapses to
///
/// ```sql
/// UPDATE ff_waitpoint_hmac SET active = FALSE WHERE active = TRUE;
/// INSERT INTO ff_waitpoint_hmac(kid, secret, rotated_at_ms, active)
///     VALUES ($1, $2, $3, TRUE);
/// ```
///
/// Result shape: `entries.len() == 1`, `partition = 0`. This is the
/// wire parity the adjudication pinned — consumers that walk
/// `entries` work against both Valkey and Postgres backends.
///
/// **Replay contract.** If `new_kid` is already installed with the
/// same secret bytes, return `Noop { kid }` — operator retries are
/// safe. Same kid with *different* bytes is a `RotationConflict`:
/// callers must pick a fresh kid instead of silently overwriting.
pub async fn rotate_waitpoint_hmac_secret_all_impl(
    pool: &PgPool,
    args: ff_core::contracts::RotateWaitpointHmacSecretAllArgs,
    now_ms: i64,
) -> Result<ff_core::contracts::RotateWaitpointHmacSecretAllResult, EngineError> {
    use ff_core::contracts::{
        RotateWaitpointHmacSecretAllEntry, RotateWaitpointHmacSecretAllResult,
        RotateWaitpointHmacSecretOutcome,
    };

    // Decode the hex secret up front so a malformed input fails
    // Validation rather than smuggling through as Transport.
    let secret_bytes = hex::decode(&args.new_secret_hex).map_err(|_| {
        EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::InvalidInput,
            detail: "new_secret_hex is not valid hex".into(),
        }
    })?;

    let outcome_res: Result<RotateWaitpointHmacSecretOutcome, EngineError> = async {
        let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

        // Exact replay check: same kid + same bytes ⇒ Noop.
        let existing: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT secret FROM ff_waitpoint_hmac WHERE kid = $1",
        )
        .bind(&args.new_kid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        if let Some((prior,)) = existing {
            if prior == secret_bytes {
                tx.commit().await.map_err(map_sqlx_error)?;
                return Ok(RotateWaitpointHmacSecretOutcome::Noop {
                    kid: args.new_kid.clone(),
                });
            }
            tx.rollback().await.ok();
            return Err(EngineError::Conflict(
                ff_core::engine_error::ConflictKind::RotationConflict(format!(
                    "kid {} already installed with a different secret",
                    args.new_kid
                )),
            ));
        }

        // Capture the prior current-kid for the outcome envelope.
        let prior_active: Option<(String,)> = sqlx::query_as(
            "SELECT kid FROM ff_waitpoint_hmac \
             WHERE active = TRUE \
             ORDER BY rotated_at_ms DESC LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // `grace_ms` is not stored per-row in the Wave-3 schema; the
        // grace window is enforced by retaining prior `active=false`
        // rows in the table so [`fetch_kid`] still returns their
        // secret during verification within the window.
        let _ = args.grace_ms;

        sqlx::query("UPDATE ff_waitpoint_hmac SET active = FALSE WHERE active = TRUE")
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;

        sqlx::query(
            "INSERT INTO ff_waitpoint_hmac (kid, secret, rotated_at_ms, active) \
             VALUES ($1, $2, $3, TRUE)",
        )
        .bind(&args.new_kid)
        .bind(&secret_bytes)
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        tx.commit().await.map_err(map_sqlx_error)?;

        Ok(RotateWaitpointHmacSecretOutcome::Rotated {
            previous_kid: prior_active.map(|(k,)| k),
            new_kid: args.new_kid.clone(),
            gc_count: 0,
        })
    }
    .await;

    Ok(RotateWaitpointHmacSecretAllResult::new(vec![
        RotateWaitpointHmacSecretAllEntry::new(0, outcome_res),
    ]))
}

/// Implementation of `EngineBackend::seed_waitpoint_hmac_secret` (issue #280).
///
/// Semantics against the global `ff_waitpoint_hmac` table:
///
/// * No active row exists and no row for the supplied kid → INSERT
///   the new kid as the active signing kid. Returns
///   `SeedOutcome::Seeded { kid }`.
/// * Row for the supplied kid exists (regardless of `active`) →
///   `SeedOutcome::AlreadySeeded { kid, same_secret }` where
///   `same_secret` reports whether the stored bytes match.
/// * Different kid is currently `active=TRUE` → `Validation(InvalidInput)`;
///   operators must rotate rather than re-seed under a conflicting
///   kid. Mirrors the Valkey backend's behaviour when the per-partition
///   `current_kid` differs from the supplied one.
pub async fn seed_waitpoint_hmac_secret_impl(
    pool: &PgPool,
    args: ff_core::contracts::SeedWaitpointHmacSecretArgs,
    now_ms: i64,
) -> Result<ff_core::contracts::SeedOutcome, EngineError> {
    use ff_core::contracts::SeedOutcome;

    if args.secret_hex.len() != 64 || !args.secret_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::InvalidInput,
            detail: "secret_hex must be 64 hex characters (256-bit secret)".into(),
        });
    }
    if args.kid.is_empty() {
        return Err(EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::InvalidInput,
            detail: "kid must be non-empty".into(),
        });
    }
    let secret_bytes = hex::decode(&args.secret_hex).map_err(|_| EngineError::Validation {
        kind: ff_core::engine_error::ValidationKind::InvalidInput,
        detail: "secret_hex is not valid hex".into(),
    })?;

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let existing: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT secret FROM ff_waitpoint_hmac WHERE kid = $1")
            .bind(&args.kid)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
    if let Some((prior,)) = existing {
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(SeedOutcome::AlreadySeeded {
            kid: args.kid,
            same_secret: prior == secret_bytes,
        });
    }

    let active: Option<(String,)> = sqlx::query_as(
        "SELECT kid FROM ff_waitpoint_hmac WHERE active = TRUE \
         ORDER BY rotated_at_ms DESC LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    if let Some((active_kid,)) = active {
        tx.rollback().await.ok();
        return Err(EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::InvalidInput,
            detail: format!(
                "seed_waitpoint_hmac_secret: a different kid {active_kid:?} is already active; \
                 use rotate_waitpoint_hmac_secret_all to change kid"
            ),
        });
    }

    sqlx::query(
        "INSERT INTO ff_waitpoint_hmac (kid, secret, rotated_at_ms, active) \
         VALUES ($1, $2, $3, TRUE)",
    )
    .bind(&args.kid)
    .bind(&secret_bytes)
    .bind(now_ms)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(SeedOutcome::Seeded { kid: args.kid })
}

// HMAC round-trip + tamper tests live in `ff_core::crypto::hmac`
// alongside the extracted primitive.
