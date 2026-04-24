//! Boot-time schema-version check.
//!
//! **RFC-v0.7 Wave 0, Q12.** Mirrors the Valkey boot-time
//! partition-count check (Q5). Reads the `_sqlx_migrations` ledger
//! sqlx writes as part of `sqlx::migrate!` application and compares
//! the latest applied version to `required`:
//!
//! * `ledger < required` → refuse to start
//!   ([`EngineError::Unavailable { op: "schema_version" }`]).
//! * `ledger == required` → pass.
//! * `ledger > required` → pass + log-warn iff every intervening
//!   migration is annotated `backward_compatible=true`. Wave 3
//!   adds the annotation column to `_sqlx_migrations` (or a
//!   sibling table); Wave 0 skips the annotation read and
//!   unconditionally log-warns on a ledger-ahead match so the
//!   boot-diagnostic channel is exercised. The destructive-migration
//!   refuse-to-start branch also lands in Wave 3.

use sqlx::PgPool;
use sqlx::Row;
use tracing::warn;

use ff_core::engine_error::EngineError;

use crate::error::map_sqlx_error;

/// Compare the Postgres migration ledger against the binary's
/// required schema version and enforce Q12's boot-time contract.
///
/// `required` is the migration version the caller's binary was
/// built against (typically a `const REQUIRED_SCHEMA_VERSION: u32`
/// ff-server embeds).
///
/// Wave 0 scope: implements the `<` (refuse) + `==` (pass) +
/// `>` (warn) branches against sqlx's `_sqlx_migrations` table.
/// Wave 3 extends the `>` branch to read the per-migration
/// `backward_compatible` annotation and refuse-to-start on a
/// destructive-ahead ledger.
pub async fn check_schema_version(pool: &PgPool, required: u32) -> Result<(), EngineError> {
    // `_sqlx_migrations(version BIGINT, ...)` is sqlx's own ledger.
    // When the migrator has never run the table doesn't exist —
    // treat that as "ledger = 0" (below any `required >= 1`).
    let ledger: i64 = match sqlx::query(
        "SELECT MAX(version) FROM _sqlx_migrations WHERE success = TRUE",
    )
    .fetch_optional(pool)
    .await
    {
        Ok(Some(row)) => row.try_get::<Option<i64>, _>(0).ok().flatten().unwrap_or(0),
        Ok(None) => 0,
        // Missing-table (undefined_table): fresh database → ledger = 0.
        Err(sqlx::Error::Database(ref db)) if db.code().as_deref() == Some("42P01") => 0,
        Err(other) => return Err(map_sqlx_error(other)),
    };
    let required_i = required as i64;

    if ledger < required_i {
        return Err(EngineError::Unavailable {
            op: "schema_version",
        });
    }
    if ledger > required_i {
        // Wave 3 will gate this on a per-migration
        // `backward_compatible` annotation and refuse-to-start on
        // a destructive-ahead ledger (Q12 branch 3). For Wave 0
        // we unconditionally warn so the boot-diagnostic channel
        // is exercised.
        warn!(
            ledger,
            required = required_i,
            "postgres schema ledger is ahead of binary — Wave 3 will gate this on the backward_compatible annotation"
        );
    }
    Ok(())
}
