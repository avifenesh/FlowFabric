//! Schema-migration application.
//!
//! **RFC-v0.7 Wave 0, Q12.** Migrations are applied **out-of-band by
//! the operator** via a future `sqlx migrate run` CLI entry (or this
//! `apply_migrations` function in tests). The ff-server binary does
//! NOT auto-apply on backend connect; at boot it runs a
//! schema-version check ([`crate::version::check_schema_version`])
//! and refuses to start on mismatch per Q12's branch rules. The
//! rationale lives in the adjudication block in
//! `rfcs/drafts/v0.7-migration-master.md §Q12`:
//!
//! 1. Auto-apply across N replicas races on the sqlx advisory lock
//!    and stalls rolling deploys.
//! 2. Auto-apply fights peer-team-boundaries — cairn's SRE owns DDL
//!    timing.

use sqlx::PgPool;

/// Apply all pending migrations bundled in this crate's
/// `migrations/` directory.
///
/// **Out-of-band only.** Wave 0 ships a placeholder `0001_*.sql`
/// (just `SELECT 1;`) so the machinery is exercised; wave 3
/// replaces it with the real schema. Callers: tests + a future
/// `ff-migrate` CLI. Must NOT be called from
/// `PostgresBackend::connect`.
pub async fn apply_migrations(pool: &PgPool) -> Result<(), MigrationError> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// Migration application failure. Thin newtype over
/// `sqlx::migrate::MigrateError` so we can attach crate-local context
/// in later waves (e.g. DDL-lock-timeout hint, destructive-migration
/// guard) without breaking the public fn signature.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// Wrap sqlx's typed migration error (version mismatch, checksum
    /// failure, connection error, etc.).
    #[error(transparent)]
    Sqlx(#[from] sqlx::migrate::MigrateError),
}
