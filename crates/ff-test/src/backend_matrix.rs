//! Backend matrix fixture harness — **RFC-v0.7 Wave 7a**.
//!
//! Lets the same integration-test body run against **both** the
//! Valkey-backed and Postgres-backed [`EngineBackend`] impls. Each
//! test that opts into the matrix writes its body once, takes a
//! [`BackendFixture`] parameter, and the harness loops over the
//! requested backends.
//!
//! # Environment
//!
//! * `FF_TEST_BACKEND` — selects which backends the matrix exercises.
//!   Accepts `valkey` (default), `postgres`, or `both`.
//! * `FF_PG_TEST_URL` — Postgres connection URL. If unset, any
//!   `postgres` slot is skipped with a log message (matching the
//!   existing `#[ignore]`-by-default Postgres test pattern in
//!   `ff-test/tests/pg_*.rs`).
//! * Valkey connection config is read by
//!   [`crate::fixtures::TestCluster::connect`].
//!
//! # Usage
//!
//! ```rust,ignore
//! use ff_test::backend_matrix::{run_matrix, BackendFixture};
//!
//! #[tokio::test]
//! async fn list_lanes_sorted() {
//!     run_matrix(|fx| async move {
//!         fx.seed_lanes(&["alpha", "bravo"]).await;
//!         let page = fx.backend().list_lanes(None, 10).await.unwrap();
//!         assert_eq!(page.lanes.len(), 2);
//!     }).await;
//! }
//! ```
//!
//! Tests that exercise Valkey-specific seeding (direct FCALL) should
//! stay non-matrix for now and live in their existing files.

use std::future::Future;
use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::CreateExecutionArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::ExecutionId;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::OnceCell;

use crate::fixtures::TestCluster;

/// Which backend an integration test body is running against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Valkey,
    Postgres,
}

impl BackendKind {
    /// Human-readable label for assertion messages + log lines.
    pub fn label(self) -> &'static str {
        match self {
            BackendKind::Valkey => "valkey",
            BackendKind::Postgres => "postgres",
        }
    }
}

/// Backend-agnostic integration-test fixture. One instance is built
/// per backend per test invocation by [`run_matrix`].
///
/// The Valkey arm holds a [`TestCluster`]; the Postgres arm holds a
/// [`PgPool`]. Both arms expose the same surface:
/// [`Self::backend`] (an `Arc<dyn EngineBackend>`), plus backend-
/// agnostic seed helpers ([`Self::seed_lanes`],
/// [`Self::seed_execution`]) so tests don't reach for FCALL / raw SQL
/// directly.
pub struct BackendFixture {
    kind: BackendKind,
    partition_config: PartitionConfig,
    inner: Inner,
}

enum Inner {
    Valkey {
        tc: TestCluster,
        backend: Arc<dyn EngineBackend>,
    },
    Postgres {
        pool: PgPool,
        backend: Arc<PostgresBackend>,
    },
}

impl BackendFixture {
    /// Backend tag for this fixture instance.
    pub fn kind(&self) -> BackendKind {
        self.kind
    }

    /// Partition config the fixture is wired to. Tests should use
    /// this value (not the compile-time constant) when minting ids.
    pub fn partition_config(&self) -> &PartitionConfig {
        &self.partition_config
    }

    /// Trait-object backend handle.
    pub fn backend(&self) -> Arc<dyn EngineBackend> {
        match &self.inner {
            Inner::Valkey { backend, .. } => backend.clone(),
            Inner::Postgres { backend, .. } => backend.clone() as Arc<dyn EngineBackend>,
        }
    }

    /// Valkey-only accessor. Panics on the Postgres arm; use sparingly
    /// from tests that need direct cluster access (e.g. FCALL setup
    /// that hasn't yet been lifted into the fixture surface).
    pub fn as_valkey(&self) -> &TestCluster {
        match &self.inner {
            Inner::Valkey { tc, .. } => tc,
            Inner::Postgres { .. } => panic!("BackendFixture::as_valkey called on Postgres arm"),
        }
    }

    /// Postgres-only accessor. Panics on the Valkey arm.
    pub fn as_postgres(&self) -> &PgPool {
        match &self.inner {
            Inner::Postgres { pool, .. } => pool,
            Inner::Valkey { .. } => panic!("BackendFixture::as_postgres called on Valkey arm"),
        }
    }

    /// Seed the lane registry with the given ids. Backend-agnostic.
    ///
    /// * Valkey: `SADD ff:idx:lanes <lanes...>` — matches the shape
    ///   tests were already driving via ad-hoc `SADD` calls.
    /// * Postgres: bulk `INSERT ... ON CONFLICT DO NOTHING` into
    ///   `ff_lane_registry`.
    pub async fn seed_lanes(&self, lanes: &[&str]) {
        if lanes.is_empty() {
            return;
        }
        match &self.inner {
            Inner::Valkey { tc, .. } => {
                let members: Vec<String> =
                    lanes.iter().map(|s| (*s).to_owned()).collect();
                let _: i64 = tc
                    .client()
                    .cmd("SADD")
                    .arg("ff:idx:lanes")
                    .arg(members)
                    .execute()
                    .await
                    .expect("SADD ff:idx:lanes");
            }
            Inner::Postgres { pool, .. } => {
                let now_ms: i64 = ff_core::types::TimestampMs::now().0;
                for lane in lanes {
                    sqlx::query(
                        "INSERT INTO ff_lane_registry \
                         (lane_id, registered_at_ms, registered_by) \
                         VALUES ($1, $2, $3) \
                         ON CONFLICT (lane_id) DO NOTHING",
                    )
                    .bind(*lane)
                    .bind(now_ms)
                    .bind("ff-test-matrix")
                    .execute(pool)
                    .await
                    .expect("INSERT ff_lane_registry");
                }
            }
        }
    }

    /// Seed a single execution. Backend-agnostic: lands the row via
    /// the Valkey `ff_create_execution` FCALL or the Postgres
    /// [`PostgresBackend::create_execution`] inherent.
    pub async fn seed_execution(&self, args: CreateExecutionArgs) -> ExecutionId {
        match &self.inner {
            Inner::Valkey { tc, .. } => seed_execution_valkey(tc, args).await,
            Inner::Postgres { backend, .. } => backend
                .create_execution(args)
                .await
                .expect("PostgresBackend::create_execution"),
        }
    }

    /// Drop every row / key the fixture can reach. Called implicitly
    /// at fixture construction so each `run_matrix` invocation starts
    /// from a clean slate.
    pub async fn cleanup(&self) {
        match &self.inner {
            Inner::Valkey { tc, .. } => tc.cleanup().await,
            Inner::Postgres { pool, .. } => truncate_postgres(pool).await,
        }
    }
}

/// Build a fixture for the given backend. Panics on connection
/// failure — matches [`TestCluster::connect`]'s existing contract.
pub async fn fixture(kind: BackendKind) -> BackendFixture {
    match kind {
        BackendKind::Valkey => {
            let tc = TestCluster::connect().await;
            tc.cleanup().await;
            let partition_config = *tc.partition_config();
            let backend = tc.backend();
            BackendFixture {
                kind,
                partition_config,
                inner: Inner::Valkey { tc, backend },
            }
        }
        BackendKind::Postgres => {
            let url = std::env::var("FF_PG_TEST_URL").unwrap_or_else(|_| {
                panic!(
                    "FF_PG_TEST_URL must be set to build a Postgres BackendFixture; \
                     `requested_backends()` should have filtered this out"
                )
            });
            let pool = pg_pool().await.unwrap_or_else(|| {
                panic!("failed to build Postgres pool at {url}")
            });
            truncate_postgres(&pool).await;
            // Match the Valkey arm's partition config so execution ids
            // route to the same `partition_key` values on both backends.
            let partition_config = crate::fixtures::TEST_PARTITION_CONFIG;
            let backend = PostgresBackend::from_pool(pool.clone(), partition_config);
            BackendFixture {
                kind,
                partition_config,
                inner: Inner::Postgres { pool, backend },
            }
        }
    }
}

/// Parse the `FF_TEST_BACKEND` env var + filter by availability.
///
/// * `valkey` (or unset) — `[Valkey]`
/// * `postgres` — `[Postgres]`
/// * `both` — `[Valkey, Postgres]`
/// * anything else — panics with a readable error.
///
/// When the list would include `Postgres` but `FF_PG_TEST_URL` is
/// unset, the Postgres entry is dropped with a log message. If that
/// leaves the list empty, returns `[Valkey]` so CI that sets
/// `FF_TEST_BACKEND=postgres` without a URL still exercises
/// *something* (matches the soft-skip behaviour of the existing
/// `pg_*.rs` tests, which return early when `FF_PG_TEST_URL` is unset
/// rather than hard-failing).
pub fn requested_backends() -> Vec<BackendKind> {
    let raw = std::env::var("FF_TEST_BACKEND").unwrap_or_else(|_| "valkey".to_owned());
    let mut requested: Vec<BackendKind> = match raw.as_str() {
        "valkey" => vec![BackendKind::Valkey],
        "postgres" => vec![BackendKind::Postgres],
        "both" => vec![BackendKind::Valkey, BackendKind::Postgres],
        other => panic!(
            "FF_TEST_BACKEND must be one of valkey|postgres|both (got {other:?})"
        ),
    };
    if requested.contains(&BackendKind::Postgres) && std::env::var("FF_PG_TEST_URL").is_err() {
        eprintln!(
            "ff-test backend-matrix: FF_PG_TEST_URL unset — skipping Postgres slot"
        );
        requested.retain(|k| *k != BackendKind::Postgres);
    }
    if requested.is_empty() {
        requested.push(BackendKind::Valkey);
    }
    requested
}

/// Run the async test body once per requested backend.
///
/// Each iteration builds a fresh [`BackendFixture`] (with cleanup on
/// construction), runs the body, and drops the fixture. Panics in
/// the body propagate with a backend label attached so failures say
/// which backend tripped.
pub async fn run_matrix<F, Fut>(body: F)
where
    F: Fn(BackendFixture) -> Fut,
    Fut: Future<Output = ()>,
{
    for kind in requested_backends() {
        eprintln!("ff-test backend-matrix: running against {}", kind.label());
        let fx = fixture(kind).await;
        body(fx).await;
    }
}

// ─── Internal helpers ───

/// Shared Postgres pool guarded by `OnceCell` so all matrix-based
/// tests reuse one set of connections per process (matches the
/// `LIBRARY_LOADED` pattern in `fixtures.rs`).
static PG_POOL: OnceCell<PgPool> = OnceCell::const_new();

async fn pg_pool() -> Option<PgPool> {
    let cell = PG_POOL
        .get_or_try_init(|| async {
            let url = std::env::var("FF_PG_TEST_URL")
                .map_err(|_| "FF_PG_TEST_URL not set".to_owned())?;
            // Bump max_connections: each `PostgresBackend::from_pool`
            // call spawns a `StreamNotifier` task that holds a
            // dedicated LISTEN connection for that backend's lifetime.
            // Matrix tests rebuild the backend per test, and tokio's
            // per-test runtime teardown doesn't always reclaim the
            // connection promptly — a higher cap avoids the pool
            // running dry across a test file's worth of fixtures.
            let pool = PgPoolOptions::new()
                .max_connections(32)
                .connect(&url)
                .await
                .map_err(|e| format!("connect {url}: {e}"))?;
            ff_backend_postgres::apply_migrations(&pool)
                .await
                .map_err(|e| format!("apply_migrations: {e}"))?;
            Ok::<PgPool, String>(pool)
        })
        .await
        .ok()?;
    Some(cell.clone())
}

/// Truncate every Wave-4+ table the matrix fixture can write. Run
/// between fixture instances so tests don't see each other's rows.
///
/// Uses `TRUNCATE ... RESTART IDENTITY CASCADE` so foreign-key
/// dependents are cleared automatically. Limited to tables the
/// current matrix tests seed or assert against — new Wave-n tables
/// can be appended here as they come online without breaking older
/// callers.
async fn truncate_postgres(pool: &PgPool) {
    let tables = [
        "ff_exec_core",
        "ff_attempt_current",
        "ff_attempt_history",
        "ff_suspension_current",
        "ff_flow_core",
        "ff_flow_members",
        "ff_flow_edges",
        "ff_lane_registry",
        "ff_budget_current",
        "ff_stream_frames",
        "ff_pending_cancel_groups",
        "ff_completion_outbox",
    ];
    // Per-table truncate gated by `to_regclass` so a table a future
    // migration hasn't landed yet is skipped rather than aborting the
    // reset. A bulk `TRUNCATE a, b, c` would be faster but fails
    // atomically on the first missing relation.
    for t in &tables {
        let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(*t)
            .fetch_one(pool)
            .await
            .unwrap_or(None);
        if exists.is_some() {
            let _ = sqlx::query(&format!("TRUNCATE {t} RESTART IDENTITY CASCADE"))
                .execute(pool)
                .await;
        }
    }
}

/// Valkey arm of `seed_execution`: drive the same `ff_create_execution`
/// FCALL that production traffic (ff-server → ValkeyBackend) uses.
async fn seed_execution_valkey(
    tc: &TestCluster,
    args: CreateExecutionArgs,
) -> ExecutionId {
    use ff_core::keys::{ExecKeyContext, IndexKeys};
    use ff_core::partition::execution_partition;

    let config = *tc.partition_config();
    let partition = execution_partition(&args.execution_id, &config);
    let ctx = ExecKeyContext::new(&partition, &args.execution_id);
    let idx = IndexKeys::new(&partition);

    let tags_json = serde_json::to_string(&args.tags).unwrap_or_else(|_| "{}".to_owned());
    let policy_json = args
        .policy
        .as_ref()
        .map(|p| serde_json::to_string(p).unwrap_or_else(|_| "{}".to_owned()))
        .unwrap_or_else(|| "{}".to_owned());
    let delay_str = args
        .delay_until
        .map(|d| d.0.to_string())
        .unwrap_or_default();
    let is_delayed = !delay_str.is_empty();
    let scheduling_zset = if is_delayed {
        idx.lane_delayed(&args.lane_id)
    } else {
        idx.lane_eligible(&args.lane_id)
    };

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        scheduling_zset,
        ctx.noop(),
        idx.execution_deadline(),
        idx.all_executions(),
    ];
    let argv: Vec<String> = vec![
        args.execution_id.to_string(),
        args.namespace.to_string(),
        args.lane_id.to_string(),
        args.execution_kind,
        args.priority.to_string(),
        args.creator_identity,
        policy_json,
        String::from_utf8_lossy(&args.input_payload).into_owned(),
        delay_str,
        String::new(),
        tags_json,
        args.execution_deadline_at
            .map(|d| d.to_string())
            .unwrap_or_default(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = argv.iter().map(String::as_str).collect();
    let _: ferriskey::Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution (matrix seed, Valkey arm)");
    args.execution_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_labels_are_stable() {
        assert_eq!(BackendKind::Valkey.label(), "valkey");
        assert_eq!(BackendKind::Postgres.label(), "postgres");
    }

    #[test]
    fn requested_backends_default_is_valkey() {
        // Don't mutate env inside concurrent tests — only read-path.
        if std::env::var("FF_TEST_BACKEND").is_err() {
            assert_eq!(requested_backends(), vec![BackendKind::Valkey]);
        }
    }
}
