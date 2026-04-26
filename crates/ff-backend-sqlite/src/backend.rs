//! `SqliteBackend` — RFC-023 dev-only SQLite [`EngineBackend`] impl.
//!
//! Phase 1a lands the scaffolding: construction guard, registry
//! dedup, pool setup, WARN banner, and Unavailable stubs for every
//! required trait method. Phase 2+ progressively replaces the stubs
//! with real bodies paralleling `ff-backend-postgres`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

use ff_core::backend::{
    AppendFrameOutcome, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy,
    FailOutcome, FailureClass, FailureReason, Frame, Handle, LeaseRenewal, PendingWaitpoint,
    ReclaimToken, ResumeSignal, UsageDimensions,
};
use ff_core::capability::{BackendIdentity, Capabilities, Supports, Version};
#[cfg(feature = "core")]
use ff_core::contracts::{
    ClaimResumedExecutionArgs, ClaimResumedExecutionResult, DeliverSignalArgs,
    DeliverSignalResult, EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot, ListExecutionsPage,
    ListFlowsPage, ListLanesPage, ListSuspendedPage, SetEdgeGroupPolicyResult,
};
use ff_core::contracts::{
    CancelFlowResult, ExecutionSnapshot, FlowSnapshot, ReportUsageResult, SuspendArgs,
    SuspendOutcome,
};
#[cfg(feature = "streaming")]
use ff_core::contracts::{StreamCursor, StreamFrames};
use ff_core::backend::PrepareOutcome;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{BackendError, EngineError};
#[cfg(feature = "core")]
use ff_core::partition::PartitionKey;
#[cfg(feature = "streaming")]
use ff_core::types::AttemptIndex;
#[cfg(feature = "core")]
use ff_core::types::EdgeId;
use ff_core::types::{BudgetId, ExecutionId, FlowId, LaneId, TimestampMs};

use crate::pubsub::PubSub;
use crate::registry;

/// Phase-1a-wide `Unavailable` helper. Each stubbed method names
/// itself here so call-site errors carry a stable identifier.
#[inline]
fn unavailable<T>(op: &'static str) -> Result<T, EngineError> {
    Err(EngineError::Unavailable { op })
}

/// Internal shared state. `Arc<SqliteBackendInner>` is what the
/// registry stores weak references to and what `SqliteBackend`
/// wraps.
pub(crate) struct SqliteBackendInner {
    /// Connection pool. Held live even when the trait-object surface
    /// isn't exercising it so Phase 2+ can migrate bodies without
    /// re-plumbing construction.
    #[allow(dead_code)]
    pub(crate) pool: SqlitePool,
    /// Per-backend wakeup channels (Phase 3 wiring).
    #[allow(dead_code)]
    pub(crate) pubsub: PubSub,
    /// Registry key (canonical path or verbatim `:memory:` URI).
    /// Held for Drop-time cleanup if we need it in a future phase;
    /// today the `Weak` entries decay naturally.
    #[allow(dead_code)]
    pub(crate) key: PathBuf,
    /// Sentinel connection for shared-cache `:memory:` databases.
    /// SQLite drops a shared-cache in-memory DB the moment the last
    /// connection referencing it closes; the pool recycles idle
    /// connections, so without a pinned sentinel the schema + data
    /// would silently reset between pool checkouts. `None` for
    /// filesystem-backed databases where the file itself is the
    /// durable backing store.
    ///
    /// Held in a `Mutex` so `Drop` can take ownership — sqlx's
    /// `SqliteConnection::close` is async + consumes `self`, but we
    /// don't need graceful close here: process exit drops the
    /// in-memory DB regardless. Presence alone is what keeps the
    /// shared cache alive.
    #[allow(dead_code)]
    pub(crate) memory_sentinel: Option<std::sync::Mutex<Option<sqlx::SqliteConnection>>>,
}

/// RFC-023 SQLite dev-only backend.
///
/// Construction demands `FF_DEV_MODE=1` (§4.5). Identical paths
/// within a process return the same handle via the §4.2 B6
/// registry.
#[derive(Clone)]
pub struct SqliteBackend {
    inner: Arc<SqliteBackendInner>,
}

impl std::fmt::Debug for SqliteBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteBackend")
            .field("key", &self.inner.key)
            .finish()
    }
}

impl SqliteBackend {
    /// RFC-023 Phase 1a entry point. `path` accepts a filesystem
    /// path, `:memory:`, or a `file:...?mode=memory&cache=shared`
    /// URI.
    ///
    /// Uses the [`SqliteServerConfig`] defaults (pool size 4, WAL on
    /// for file paths). For operator-tuned pool/WAL settings, call
    /// [`SqliteBackend::new_with_config`].
    ///
    /// [`SqliteServerConfig`]: ff_server::config::SqliteServerConfig
    ///
    /// # Errors
    ///
    /// * [`BackendError::RequiresDevMode`] when `FF_DEV_MODE` is
    ///   unset or not `"1"`.
    /// * [`BackendError::Valkey`] (historical name — the classifier
    ///   is backend-agnostic despite the variant name) when the
    ///   pool cannot be constructed.
    pub async fn new(path: &str) -> Result<Arc<Self>, BackendError> {
        Self::new_with_tuning(path, 4, true).await
    }

    /// Operator-tuned entry point. `pool_size` sets the pool's max
    /// connections; `wal_mode` enables `PRAGMA journal_mode=WAL` for
    /// filesystem-backed databases (ignored for `:memory:` variants
    /// per RFC-023 §4.6).
    pub async fn new_with_tuning(
        path: &str,
        pool_size: u32,
        wal_mode: bool,
    ) -> Result<Arc<Self>, BackendError> {
        // §4.5 production guard — TYPE-level emission point (§3.3 A3).
        if std::env::var("FF_DEV_MODE").as_deref() != Ok("1") {
            return Err(BackendError::RequiresDevMode);
        }

        // §4.2 B6: canonicalize the key. `:memory:` and
        // `file::memory:...` pass through verbatim (distinct per-URI
        // entries via embedded UUIDs). Filesystem paths resolve via
        // `fs::canonicalize` when the file exists; absent files fall
        // back to the raw path so two concurrent constructions before
        // file creation still dedup.
        //
        // F1: bare `:memory:` is rewritten to
        // `file::memory:?cache=shared&uri=true` so a multi-connection
        // pool shares ONE in-memory database. Without this rewrite,
        // each pool connection opens its own private DB and tests see
        // schema mismatches silently.
        let is_memory =
            path == ":memory:" || path.starts_with("file::memory:");
        let effective_path: std::borrow::Cow<'_, str> = if path == ":memory:" {
            std::borrow::Cow::Borrowed("file::memory:?cache=shared")
        } else {
            std::borrow::Cow::Borrowed(path)
        };

        let key = if is_memory {
            PathBuf::from(effective_path.as_ref())
        } else {
            std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
        };

        if let Some(existing) = registry::lookup(&key) {
            // F6: emit WARN only on first-time construction. Registry
            // hits are dedup clones; operators already saw the banner
            // when the original handle was built.
            return Ok(Arc::new(Self { inner: existing }));
        }

        // Build the pool. sqlx's SqliteConnectOptions parses the full
        // URI form as well as plain paths. `create_if_missing` is
        // what embedded-test consumers expect.
        let opts: SqliteConnectOptions = effective_path
            .parse::<SqliteConnectOptions>()
            .map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Protocol,
                message: format!("sqlite connect-opts parse for {path:?}: {e}"),
            })?
            .create_if_missing(true);

        // F2: apply WAL for filesystem-backed DBs only. SQLite's WAL
        // is a no-op (and warns) for `:memory:` variants per RFC §4.6.
        let opts = if wal_mode && !is_memory {
            opts.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        } else {
            opts
        };

        // F2: pool size from config, default 4. Minimum 1 — sqlx
        // rejects 0 at pool-build time anyway.
        let pool_max = pool_size.max(1);
        let pool = SqlitePoolOptions::new()
            .max_connections(pool_max)
            .connect_with(opts.clone())
            .await
            .map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Transport,
                message: format!("sqlite pool connect for {path:?}: {e}"),
            })?;

        // F1: for shared-cache `:memory:` DBs, open a standalone
        // sentinel connection and hold it for the `Arc`'s lifetime.
        // The shared cache is torn down the moment the last connection
        // closes; without the sentinel, a pool-idle cycle (all 4
        // connections temporarily returned) would drop the DB between
        // test assertions.
        let memory_sentinel = if is_memory {
            use sqlx::ConnectOptions;
            let conn = opts.connect().await.map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Transport,
                message: format!(
                    "sqlite sentinel connect for {path:?}: {e}"
                ),
            })?;
            Some(std::sync::Mutex::new(Some(conn)))
        } else {
            None
        };

        // F6: §3.3 WARN banner — now emitted AFTER registry-miss is
        // confirmed so dedup clones don't spam the log.
        tracing::warn!(
            "FlowFabric SQLite backend active (FF_DEV_MODE=1). \
             This backend is dev-only; single-writer, single-process, \
             not supported in production. See RFC-023."
        );

        // RFC-023 Phase 1b: apply the 14 hand-ported SQLite-dialect
        // migrations against the freshly-constructed pool. `sqlx::migrate!`
        // embeds the files at compile time and records applied versions
        // in `_sqlx_migrations` so reruns are idempotent.
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Protocol,
                message: format!("sqlite migrate for {path:?}: {e}"),
            })?;

        let inner = Arc::new(SqliteBackendInner {
            pool,
            pubsub: PubSub::new(),
            key: key.clone(),
            memory_sentinel,
        });
        let inner = registry::insert(key, inner);
        Ok(Arc::new(Self { inner }))
    }

    /// Accessor for Phase 2+ code that needs direct pool access
    /// without re-routing through the trait surface.
    #[allow(dead_code)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.inner.pool
    }

    /// Test-only pool accessor. Hidden from rustdoc; not a stable
    /// API. Exists so the in-crate integration tests can verify
    /// pool-level behaviour (F1 shared-cache sentinel) without
    /// waiting for Phase 2 data-plane methods to land.
    #[doc(hidden)]
    pub fn pool_for_test(&self) -> &SqlitePool {
        &self.inner.pool
    }
}

#[async_trait]
impl EngineBackend for SqliteBackend {
    // ── Claim + lifecycle ──

    async fn claim(
        &self,
        _lane: &LaneId,
        _capabilities: &CapabilitySet,
        _policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        unavailable("sqlite.claim")
    }

    async fn renew(&self, _handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        unavailable("sqlite.renew")
    }

    async fn progress(
        &self,
        _handle: &Handle,
        _percent: Option<u8>,
        _message: Option<String>,
    ) -> Result<(), EngineError> {
        unavailable("sqlite.progress")
    }

    async fn append_frame(
        &self,
        _handle: &Handle,
        _frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        unavailable("sqlite.append_frame")
    }

    async fn complete(
        &self,
        _handle: &Handle,
        _payload: Option<Vec<u8>>,
    ) -> Result<(), EngineError> {
        unavailable("sqlite.complete")
    }

    async fn fail(
        &self,
        _handle: &Handle,
        _reason: FailureReason,
        _classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        unavailable("sqlite.fail")
    }

    async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
        unavailable("sqlite.cancel")
    }

    async fn suspend(
        &self,
        _handle: &Handle,
        _args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        unavailable("sqlite.suspend")
    }

    async fn create_waitpoint(
        &self,
        _handle: &Handle,
        _waitpoint_key: &str,
        _expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        unavailable("sqlite.create_waitpoint")
    }

    async fn observe_signals(&self, _handle: &Handle) -> Result<Vec<ResumeSignal>, EngineError> {
        unavailable("sqlite.observe_signals")
    }

    async fn claim_from_reclaim(
        &self,
        _token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError> {
        unavailable("sqlite.claim_from_reclaim")
    }

    async fn delay(
        &self,
        _handle: &Handle,
        _delay_until: TimestampMs,
    ) -> Result<(), EngineError> {
        unavailable("sqlite.delay")
    }

    async fn wait_children(&self, _handle: &Handle) -> Result<(), EngineError> {
        unavailable("sqlite.wait_children")
    }

    // ── Read / admin ──

    async fn describe_execution(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        unavailable("sqlite.describe_execution")
    }

    async fn describe_flow(
        &self,
        _id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError> {
        unavailable("sqlite.describe_flow")
    }

    #[cfg(feature = "core")]
    async fn list_edges(
        &self,
        _flow_id: &FlowId,
        _direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        unavailable("sqlite.list_edges")
    }

    #[cfg(feature = "core")]
    async fn describe_edge(
        &self,
        _flow_id: &FlowId,
        _edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        unavailable("sqlite.describe_edge")
    }

    #[cfg(feature = "core")]
    async fn resolve_execution_flow_id(
        &self,
        _eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        unavailable("sqlite.resolve_execution_flow_id")
    }

    #[cfg(feature = "core")]
    async fn list_flows(
        &self,
        _partition: PartitionKey,
        _cursor: Option<FlowId>,
        _limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        unavailable("sqlite.list_flows")
    }

    #[cfg(feature = "core")]
    async fn list_lanes(
        &self,
        _cursor: Option<LaneId>,
        _limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        unavailable("sqlite.list_lanes")
    }

    #[cfg(feature = "core")]
    async fn list_suspended(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        unavailable("sqlite.list_suspended")
    }

    #[cfg(feature = "core")]
    async fn list_executions(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        unavailable("sqlite.list_executions")
    }

    #[cfg(feature = "core")]
    async fn deliver_signal(
        &self,
        _args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        unavailable("sqlite.deliver_signal")
    }

    #[cfg(feature = "core")]
    async fn claim_resumed_execution(
        &self,
        _args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        unavailable("sqlite.claim_resumed_execution")
    }

    async fn cancel_flow(
        &self,
        _id: &FlowId,
        _policy: CancelFlowPolicy,
        _wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        unavailable("sqlite.cancel_flow")
    }

    #[cfg(feature = "core")]
    async fn set_edge_group_policy(
        &self,
        _flow_id: &FlowId,
        _downstream_execution_id: &ExecutionId,
        _policy: EdgeDependencyPolicy,
    ) -> Result<SetEdgeGroupPolicyResult, EngineError> {
        unavailable("sqlite.set_edge_group_policy")
    }

    async fn report_usage(
        &self,
        _handle: &Handle,
        _budget: &BudgetId,
        _dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        unavailable("sqlite.report_usage")
    }

    #[cfg(feature = "streaming")]
    async fn read_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _from: StreamCursor,
        _to: StreamCursor,
        _count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        unavailable("sqlite.read_stream")
    }

    #[cfg(feature = "streaming")]
    async fn tail_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _after: StreamCursor,
        _block_ms: u64,
        _count_limit: u64,
        _visibility: ff_core::backend::TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        unavailable("sqlite.tail_stream")
    }

    #[cfg(feature = "streaming")]
    async fn read_summary(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
    ) -> Result<Option<ff_core::backend::SummaryDocument>, EngineError> {
        unavailable("sqlite.read_summary")
    }

    // ── RFC-018 capability discovery ──

    fn backend_label(&self) -> &'static str {
        "sqlite"
    }

    fn capabilities(&self) -> Capabilities {
        // RFC-023 §4.3: Phase 1a exposes only the identity tuple; real
        // `Supports::*` flags flip at the Phase 4 release PR when trait
        // bodies ship. Consumers seeing `supports.*` all-false under
        // "sqlite" know to expect `Unavailable` on every data-plane
        // call until Phase 2+ lands.
        Capabilities::new(
            BackendIdentity::new(
                "sqlite",
                Version::new(
                    env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
                    env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
                    env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
                ),
                "Phase-1a",
            ),
            Supports::none(),
        )
    }

    async fn prepare(&self) -> Result<PrepareOutcome, EngineError> {
        // Phase 1a: no boot-time prep (no migrations yet). Phase 1b
        // applies the migrations inside `SqliteBackend::new` itself
        // rather than here, matching the PG posture.
        Ok(PrepareOutcome::NoOp)
    }
}
