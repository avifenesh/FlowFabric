//! PgPool construction helper.
//!
//! **RFC-v0.7 Wave 0.** Reads `BackendConnection::Postgres { url,
//! max_connections, min_connections, acquire_timeout }` and builds a
//! `sqlx::PgPool`. Future waves add TLS root-store plumbing, LISTEN-
//! connection setup (Q1 + Q2), transaction-pooler detection (Q2
//! footnote on PgBouncer session-mode requirement), etc. Today this
//! is a thin pass-through to `sqlx::postgres::PgPoolOptions` so the
//! backend `connect` constructor has a single call site for pool
//! setup.

use ff_core::backend::{BackendConfig, BackendConnection, PostgresConnection};
use ff_core::engine_error::EngineError;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::error::map_sqlx_error;

/// Build a `sqlx::PgPool` from a [`BackendConfig`] whose connection
/// arm is [`BackendConnection::Postgres`].
///
/// Returns `EngineError::Unavailable { op: "pg.pool.build" }` when
/// the config's connection arm is not Postgres — the Valkey backend
/// has its own dial path, and a mis-routed config is a programmer
/// error the backend surfaces as a typed error rather than a panic.
pub async fn build_pool(config: &BackendConfig) -> Result<PgPool, EngineError> {
    let BackendConnection::Postgres(pg) = &config.connection else {
        return Err(EngineError::Unavailable {
            op: "pg.pool.build (non-Postgres BackendConnection)",
        });
    };
    build_pool_from_connection(pg).await
}

/// Lower-level helper: build a `PgPool` directly from a
/// [`PostgresConnection`]. Separated from [`build_pool`] so tests +
/// future migration-CLI tooling can feed a bare connection shape
/// without constructing a full `BackendConfig`.
pub async fn build_pool_from_connection(pg: &PostgresConnection) -> Result<PgPool, EngineError> {
    PgPoolOptions::new()
        .max_connections(pg.max_connections)
        .min_connections(pg.min_connections)
        .acquire_timeout(pg.acquire_timeout)
        .connect(&pg.url)
        .await
        .map_err(map_sqlx_error)
}
