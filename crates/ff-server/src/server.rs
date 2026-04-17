use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ferriskey::{Client, ClientBuilder, Value};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinSet;
use ff_core::contracts::{
    AddExecutionToFlowArgs, AddExecutionToFlowResult, BudgetStatus, CancelExecutionArgs,
    CancelExecutionResult, CancelFlowArgs, CancelFlowResult, ChangePriorityResult,
    CreateBudgetArgs, CreateBudgetResult, CreateExecutionArgs, CreateExecutionResult,
    CreateFlowArgs, CreateFlowResult, CreateQuotaPolicyArgs, CreateQuotaPolicyResult,
    ApplyDependencyToChildArgs, ApplyDependencyToChildResult,
    DeliverSignalArgs, DeliverSignalResult, ExecutionInfo, ExecutionSummary,
    ListExecutionsResult, ReplayExecutionResult,
    ReportUsageArgs, ReportUsageResult, ResetBudgetResult,
    RevokeLeaseResult,
    StageDependencyEdgeArgs, StageDependencyEdgeResult,
};
use ff_core::keys::{
    self, BudgetKeyContext, ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys,
    QuotaKeyContext,
};
use ff_core::partition::{
    budget_partition, execution_partition, flow_partition, quota_partition, PartitionConfig,
};
use ff_core::state::{PublicState, StateVector};
use ff_core::types::*;
use ff_engine::Engine;
use ff_script::retry::is_retryable_kind;

use crate::config::ServerConfig;

/// Upper bound on `member_execution_ids` returned in the
/// [`CancelFlowResult::Cancelled`] response when the flow was already in a
/// terminal state (idempotent retry). The first (non-idempotent) cancel call
/// returns the full list; retries only need a sample.
const ALREADY_TERMINAL_MEMBER_CAP: usize = 1000;

/// FlowFabric server — connects everything together.
///
/// Manages the Valkey connection, Lua library loading, background scanners,
/// and provides a minimal API for Phase 1.
pub struct Server {
    client: Client,
    engine: Engine,
    config: ServerConfig,
    /// Background tasks spawned by async handlers (e.g. cancel_flow member
    /// dispatch). Drained on shutdown with a bounded timeout.
    background_tasks: Arc<AsyncMutex<JoinSet<()>>>,
}

/// Server error type.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Valkey connection or command error (preserves ErrorKind for caller inspection).
    #[error("valkey: {0}")]
    Valkey(#[from] ferriskey::Error),
    /// Valkey error with additional context (preserves ErrorKind via #[source]).
    #[error("valkey ({context}): {source}")]
    ValkeyContext {
        #[source]
        source: ferriskey::Error,
        context: String,
    },
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("library load: {0}")]
    LibraryLoad(#[from] ff_script::loader::LoadError),
    #[error("partition mismatch: {0}")]
    PartitionMismatch(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("operation failed: {0}")]
    OperationFailed(String),
    #[error("script: {0}")]
    Script(String),
}

impl ServerError {
    /// Returns the underlying ferriskey ErrorKind, if this error carries one.
    /// Covers direct Valkey variants and library-load failures that bubble a
    /// `ferriskey::Error` through `LoadError::Valkey`.
    pub fn valkey_kind(&self) -> Option<ferriskey::ErrorKind> {
        match self {
            Self::Valkey(e) | Self::ValkeyContext { source: e, .. } => Some(e.kind()),
            Self::LibraryLoad(e) => e.valkey_kind(),
            _ => None,
        }
    }

    /// Whether this error is safely retryable by a caller. Semantics match
    /// `ScriptError::class() == Retryable` for Lua errors plus a kind-aware
    /// check for transport/library-load failures. Business-logic rejections
    /// (NotFound, InvalidInput, OperationFailed, Script, Config, PartitionMismatch)
    /// return false — those won't change on retry.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Valkey(e) | Self::ValkeyContext { source: e, .. } => {
                is_retryable_kind(e.kind())
            }
            Self::LibraryLoad(load_err) => load_err
                .valkey_kind()
                .map(is_retryable_kind)
                .unwrap_or(false),
            Self::Config(_)
            | Self::PartitionMismatch(_)
            | Self::NotFound(_)
            | Self::InvalidInput(_)
            | Self::OperationFailed(_)
            | Self::Script(_) => false,
        }
    }
}

impl Server {
    /// Start the FlowFabric server.
    ///
    /// Boot sequence:
    /// 1. Connect to Valkey
    /// 2. Validate or create partition config key
    /// 3. Load the FlowFabric Lua library
    /// 4. Start engine (14 background scanners)
    pub async fn start(config: ServerConfig) -> Result<Self, ServerError> {
        // Step 1: Connect to Valkey via ClientBuilder
        tracing::info!(
            host = %config.host, port = config.port,
            tls = config.tls, cluster = config.cluster,
            "connecting to Valkey"
        );
        let mut builder = ClientBuilder::new()
            .host(&config.host, config.port)
            .connect_timeout(Duration::from_secs(10))
            .request_timeout(Duration::from_millis(5000));
        if config.tls {
            builder = builder.tls();
        }
        if config.cluster {
            builder = builder.cluster();
        }
        let client = builder
            .build()
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "connect".into() })?;

        // Verify connectivity
        let pong: String = client
            .cmd("PING")
            .execute()
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "PING".into() })?;
        if pong != "PONG" {
            return Err(ServerError::OperationFailed(format!(
                "unexpected PING response: {pong}"
            )));
        }
        tracing::info!("Valkey connection established");

        // Step 2: Validate or create partition config
        validate_or_create_partition_config(&client, &config.partition_config).await?;

        // Step 3: Load Lua library (skippable for tests where fixture already loaded)
        if !config.skip_library_load {
            tracing::info!("loading flowfabric Lua library");
            ff_script::loader::ensure_library(&client)
                .await
                .map_err(ServerError::LibraryLoad)?;
        } else {
            tracing::info!("skipping library load (skip_library_load=true)");
        }

        // Step 4: Start engine with scanners
        // Build a fresh EngineConfig rather than cloning (EngineConfig doesn't derive Clone).
        let engine_cfg = ff_engine::EngineConfig {
            partition_config: config.partition_config,
            lanes: config.lanes.clone(),
            lease_expiry_interval: config.engine_config.lease_expiry_interval,
            delayed_promoter_interval: config.engine_config.delayed_promoter_interval,
            index_reconciler_interval: config.engine_config.index_reconciler_interval,
            attempt_timeout_interval: config.engine_config.attempt_timeout_interval,
            suspension_timeout_interval: config.engine_config.suspension_timeout_interval,
            pending_wp_expiry_interval: config.engine_config.pending_wp_expiry_interval,
            retention_trimmer_interval: config.engine_config.retention_trimmer_interval,
            budget_reset_interval: config.engine_config.budget_reset_interval,
            budget_reconciler_interval: config.engine_config.budget_reconciler_interval,
            quota_reconciler_interval: config.engine_config.quota_reconciler_interval,
            unblock_interval: config.engine_config.unblock_interval,
            dependency_reconciler_interval: config.engine_config.dependency_reconciler_interval,
            flow_projector_interval: config.engine_config.flow_projector_interval,
            execution_deadline_interval: config.engine_config.execution_deadline_interval,
        };
        let engine = Engine::start(engine_cfg, client.clone());

        tracing::info!(
            exec_partitions = config.partition_config.num_execution_partitions,
            flow_partitions = config.partition_config.num_flow_partitions,
            budget_partitions = config.partition_config.num_budget_partitions,
            quota_partitions = config.partition_config.num_quota_partitions,
            lanes = ?config.lanes.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            listen_addr = %config.listen_addr,
            "FlowFabric server started. Partitions: {}/{}/{}/{}. Scanners: 14 active.",
            config.partition_config.num_execution_partitions,
            config.partition_config.num_flow_partitions,
            config.partition_config.num_budget_partitions,
            config.partition_config.num_quota_partitions,
        );

        Ok(Self {
            client,
            engine,
            config,
            background_tasks: Arc::new(AsyncMutex::new(JoinSet::new())),
        })
    }

    /// Get a reference to the ferriskey client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Execute an FCALL with automatic Lua library reload on "function not loaded".
    ///
    /// After a Valkey failover the new primary may not have the Lua library
    /// loaded (replication lag or cold replica). This wrapper detects that
    /// condition, reloads the library via `ff_script::loader::ensure_library`,
    /// and retries the FCALL once.
    async fn fcall_with_reload(
        &self,
        function: &str,
        keys: &[&str],
        args: &[&str],
    ) -> Result<Value, ServerError> {
        fcall_with_reload_on_client(&self.client, function, keys, args).await
    }

    /// Get the server config.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Get the partition config.
    pub fn partition_config(&self) -> &PartitionConfig {
        &self.config.partition_config
    }

    // ── Minimal Phase 1 API ──

    /// Create a new execution.
    ///
    /// Uses raw FCALL — will migrate to typed ff-script wrappers in Step 1.2.
    pub async fn create_execution(
        &self,
        args: &CreateExecutionArgs,
    ) -> Result<CreateExecutionResult, ServerError> {
        let partition = execution_partition(&args.execution_id, &self.config.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);

        let lane = &args.lane_id;
        let tag = partition.hash_tag();
        let idem_key = match &args.idempotency_key {
            Some(k) if !k.is_empty() => {
                keys::idempotency_key(&tag, args.namespace.as_str(), k)
            }
            _ => ctx.noop(),
        };

        let delay_str = args
            .delay_until
            .map(|d| d.0.to_string())
            .unwrap_or_default();
        let is_delayed = !delay_str.is_empty();

        // KEYS (8) must match lua/execution.lua ff_create_execution positional order:
        //   [1] exec_core, [2] payload, [3] policy, [4] tags,
        //   [5] scheduling_zset (eligible OR delayed — ONE key),
        //   [6] idem_key, [7] execution_deadline, [8] all_executions
        let scheduling_zset = if is_delayed {
            idx.lane_delayed(lane)
        } else {
            idx.lane_eligible(lane)
        };

        let fcall_keys: Vec<String> = vec![
            ctx.core(),                  // 1
            ctx.payload(),               // 2
            ctx.policy(),                // 3
            ctx.tags(),                  // 4
            scheduling_zset,             // 5
            idem_key,                    // 6
            idx.execution_deadline(),    // 7
            idx.all_executions(),        // 8
        ];

        let tags_json = serde_json::to_string(&args.tags).unwrap_or_else(|_| "{}".to_owned());

        // ARGV (13) must match lua/execution.lua ff_create_execution positional order:
        //   [1] execution_id, [2] namespace, [3] lane_id, [4] execution_kind,
        //   [5] priority, [6] creator_identity, [7] policy_json,
        //   [8] input_payload, [9] delay_until, [10] dedup_ttl_ms,
        //   [11] tags_json, [12] execution_deadline_at, [13] partition_id
        let fcall_args: Vec<String> = vec![
            args.execution_id.to_string(),           // 1
            args.namespace.to_string(),              // 2
            args.lane_id.to_string(),                // 3
            args.execution_kind.clone(),             // 4
            args.priority.to_string(),               // 5
            args.creator_identity.clone(),           // 6
            args.policy.as_ref()
                .map(|p| serde_json::to_string(p).unwrap_or_else(|_| "{}".to_owned()))
                .unwrap_or_else(|| "{}".to_owned()), // 7
            String::from_utf8_lossy(&args.input_payload).into_owned(), // 8
            delay_str,                               // 9
            args.idempotency_key.as_ref()
                .map(|_| "86400000".to_string())
                .unwrap_or_default(),                // 10 dedup_ttl_ms
            tags_json,                               // 11
            args.execution_deadline_at
                .map(|d| d.to_string())
                .unwrap_or_default(),                // 12 execution_deadline_at
            args.partition_id.to_string(),           // 13
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_create_execution", &key_refs, &arg_refs)
            .await?;

        parse_create_result(&raw, &args.execution_id)
    }

    /// Cancel an execution.
    pub async fn cancel_execution(
        &self,
        args: &CancelExecutionArgs,
    ) -> Result<CancelExecutionResult, ServerError> {
        let raw = self
            .fcall_cancel_execution_with_reload(args)
            .await?;
        parse_cancel_result(&raw, &args.execution_id)
    }

    /// Build KEYS/ARGV for `ff_cancel_execution` and invoke via the server's
    /// reload-capable FCALL. Shared by the inline method and background
    /// cancel_flow dispatch via [`Self::fcall_cancel_execution_with_reload`].
    async fn fcall_cancel_execution_with_reload(
        &self,
        args: &CancelExecutionArgs,
    ) -> Result<Value, ServerError> {
        let (keys, argv) = build_cancel_execution_fcall(
            &self.client,
            &self.config.partition_config,
            args,
        )
        .await?;
        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        self.fcall_with_reload("ff_cancel_execution", &key_refs, &arg_refs).await
    }

    /// Get the public state of an execution.
    ///
    /// Reads `public_state` from the exec_core hash. Returns the parsed
    /// PublicState enum. If the execution is not found, returns an error.
    pub async fn get_execution_state(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<PublicState, ServerError> {
        let partition = execution_partition(execution_id, &self.config.partition_config);
        let ctx = ExecKeyContext::new(&partition, execution_id);

        let state_str: Option<String> = self
            .client
            .hget(&ctx.core(), "public_state")
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGET public_state".into() })?;

        match state_str {
            Some(s) => {
                let quoted = format!("\"{s}\"");
                serde_json::from_str(&quoted).map_err(|e| {
                    ServerError::Script(format!(
                        "invalid public_state '{s}' for {execution_id}: {e}"
                    ))
                })
            }
            None => Err(ServerError::NotFound(format!(
                "execution not found: {execution_id}"
            ))),
        }
    }

    // ── Budget / Quota API ──

    /// Create a new budget policy.
    pub async fn create_budget(
        &self,
        args: &CreateBudgetArgs,
    ) -> Result<CreateBudgetResult, ServerError> {
        let partition = budget_partition(&args.budget_id, &self.config.partition_config);
        let bctx = BudgetKeyContext::new(&partition, &args.budget_id);
        let resets_key = keys::budget_resets_key(bctx.hash_tag());
        let policies_index = keys::budget_policies_index(bctx.hash_tag());

        // KEYS (5): budget_def, budget_limits, budget_usage, budget_resets_zset,
        //           budget_policies_index
        let fcall_keys: Vec<String> = vec![
            bctx.definition(),
            bctx.limits(),
            bctx.usage(),
            resets_key,
            policies_index,
        ];

        // ARGV (variable): budget_id, scope_type, scope_id, enforcement_mode,
        //   on_hard_limit, on_soft_limit, reset_interval_ms, now_ms,
        //   dimension_count, dim_1..dim_N, hard_1..hard_N, soft_1..soft_N
        let dim_count = args.dimensions.len();
        let mut fcall_args: Vec<String> = Vec::with_capacity(9 + dim_count * 3);
        fcall_args.push(args.budget_id.to_string());
        fcall_args.push(args.scope_type.clone());
        fcall_args.push(args.scope_id.clone());
        fcall_args.push(args.enforcement_mode.clone());
        fcall_args.push(args.on_hard_limit.clone());
        fcall_args.push(args.on_soft_limit.clone());
        fcall_args.push(args.reset_interval_ms.to_string());
        fcall_args.push(args.now.to_string());
        fcall_args.push(dim_count.to_string());
        for dim in &args.dimensions {
            fcall_args.push(dim.clone());
        }
        for hard in &args.hard_limits {
            fcall_args.push(hard.to_string());
        }
        for soft in &args.soft_limits {
            fcall_args.push(soft.to_string());
        }

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_create_budget", &key_refs, &arg_refs)
            .await?;

        parse_budget_create_result(&raw, &args.budget_id)
    }

    /// Create a new quota/rate-limit policy.
    pub async fn create_quota_policy(
        &self,
        args: &CreateQuotaPolicyArgs,
    ) -> Result<CreateQuotaPolicyResult, ServerError> {
        let partition = quota_partition(&args.quota_policy_id, &self.config.partition_config);
        let qctx = QuotaKeyContext::new(&partition, &args.quota_policy_id);

        // KEYS (5): quota_def, quota_window_zset, quota_concurrency_counter,
        //           admitted_set, quota_policies_index
        let fcall_keys: Vec<String> = vec![
            qctx.definition(),
            qctx.window("requests_per_window"),
            qctx.concurrency(),
            qctx.admitted_set(),
            keys::quota_policies_index(qctx.hash_tag()),
        ];

        // ARGV (5): quota_policy_id, window_seconds, max_requests_per_window,
        //           max_concurrent, now_ms
        let fcall_args: Vec<String> = vec![
            args.quota_policy_id.to_string(),
            args.window_seconds.to_string(),
            args.max_requests_per_window.to_string(),
            args.max_concurrent.to_string(),
            args.now.to_string(),
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_create_quota_policy", &key_refs, &arg_refs)
            .await?;

        parse_quota_create_result(&raw, &args.quota_policy_id)
    }

    /// Read-only budget status for operator visibility.
    pub async fn get_budget_status(
        &self,
        budget_id: &BudgetId,
    ) -> Result<BudgetStatus, ServerError> {
        let partition = budget_partition(budget_id, &self.config.partition_config);
        let bctx = BudgetKeyContext::new(&partition, budget_id);

        // Read budget definition
        let def: HashMap<String, String> = self
            .client
            .hgetall(&bctx.definition())
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGETALL budget_def".into() })?;

        if def.is_empty() {
            return Err(ServerError::NotFound(format!(
                "budget not found: {budget_id}"
            )));
        }

        // Read usage
        let usage_raw: HashMap<String, String> = self
            .client
            .hgetall(&bctx.usage())
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGETALL budget_usage".into() })?;
        let usage: HashMap<String, u64> = usage_raw
            .into_iter()
            .filter(|(k, _)| k != "_init")
            .map(|(k, v)| (k, v.parse().unwrap_or(0)))
            .collect();

        // Read limits
        let limits_raw: HashMap<String, String> = self
            .client
            .hgetall(&bctx.limits())
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGETALL budget_limits".into() })?;
        let mut hard_limits = HashMap::new();
        let mut soft_limits = HashMap::new();
        for (k, v) in &limits_raw {
            if let Some(dim) = k.strip_prefix("hard:") {
                hard_limits.insert(dim.to_string(), v.parse().unwrap_or(0));
            } else if let Some(dim) = k.strip_prefix("soft:") {
                soft_limits.insert(dim.to_string(), v.parse().unwrap_or(0));
            }
        }

        let non_empty = |s: Option<&String>| -> Option<String> {
            s.filter(|v| !v.is_empty()).cloned()
        };

        Ok(BudgetStatus {
            budget_id: budget_id.to_string(),
            scope_type: def.get("scope_type").cloned().unwrap_or_default(),
            scope_id: def.get("scope_id").cloned().unwrap_or_default(),
            enforcement_mode: def.get("enforcement_mode").cloned().unwrap_or_default(),
            usage,
            hard_limits,
            soft_limits,
            breach_count: def
                .get("breach_count")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            soft_breach_count: def
                .get("soft_breach_count")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            last_breach_at: non_empty(def.get("last_breach_at")),
            last_breach_dim: non_empty(def.get("last_breach_dim")),
            next_reset_at: non_empty(def.get("next_reset_at")),
            created_at: non_empty(def.get("created_at")),
        })
    }

    /// Report usage against a budget and check limits.
    pub async fn report_usage(
        &self,
        budget_id: &BudgetId,
        args: &ReportUsageArgs,
    ) -> Result<ReportUsageResult, ServerError> {
        let partition = budget_partition(budget_id, &self.config.partition_config);
        let bctx = BudgetKeyContext::new(&partition, budget_id);

        // KEYS (3): budget_usage, budget_limits, budget_def
        let fcall_keys: Vec<String> = vec![bctx.usage(), bctx.limits(), bctx.definition()];

        // ARGV: dim_count, dim_1..dim_N, delta_1..delta_N, now_ms, [dedup_key]
        let dim_count = args.dimensions.len();
        let mut fcall_args: Vec<String> = Vec::with_capacity(3 + dim_count * 2);
        fcall_args.push(dim_count.to_string());
        for dim in &args.dimensions {
            fcall_args.push(dim.clone());
        }
        for delta in &args.deltas {
            fcall_args.push(delta.to_string());
        }
        fcall_args.push(args.now.to_string());
        let dedup_key_val = args
            .dedup_key
            .as_ref()
            .filter(|k| !k.is_empty())
            .map(|k| format!("ff:usagededup:{}:{}", bctx.hash_tag(), k))
            .unwrap_or_default();
        fcall_args.push(dedup_key_val);

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_report_usage_and_check", &key_refs, &arg_refs)
            .await?;

        parse_report_usage_result(&raw)
    }

    /// Reset a budget's usage counters and schedule the next reset.
    pub async fn reset_budget(
        &self,
        budget_id: &BudgetId,
    ) -> Result<ResetBudgetResult, ServerError> {
        let partition = budget_partition(budget_id, &self.config.partition_config);
        let bctx = BudgetKeyContext::new(&partition, budget_id);
        let resets_key = keys::budget_resets_key(bctx.hash_tag());

        // KEYS (3): budget_def, budget_usage, budget_resets_zset
        let fcall_keys: Vec<String> = vec![bctx.definition(), bctx.usage(), resets_key];

        // ARGV (2): budget_id, now_ms
        let now = TimestampMs::now();
        let fcall_args: Vec<String> = vec![budget_id.to_string(), now.to_string()];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_reset_budget", &key_refs, &arg_refs)
            .await?;

        parse_reset_budget_result(&raw)
    }

    // ── Flow API ──

    /// Create a new flow container.
    pub async fn create_flow(
        &self,
        args: &CreateFlowArgs,
    ) -> Result<CreateFlowResult, ServerError> {
        let partition = flow_partition(&args.flow_id, &self.config.partition_config);
        let fctx = FlowKeyContext::new(&partition, &args.flow_id);
        let fidx = FlowIndexKeys::new(&partition);

        // KEYS (3): flow_core, members_set, flow_index
        let fcall_keys: Vec<String> = vec![fctx.core(), fctx.members(), fidx.flow_index()];

        // ARGV (4): flow_id, flow_kind, namespace, now_ms
        let fcall_args: Vec<String> = vec![
            args.flow_id.to_string(),
            args.flow_kind.clone(),
            args.namespace.to_string(),
            args.now.to_string(),
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_create_flow", &key_refs, &arg_refs)
            .await?;

        parse_create_flow_result(&raw, &args.flow_id)
    }

    /// Add an execution to a flow.
    ///
    /// Two-phase cross-partition operation:
    /// 1. FCALL ff_add_execution_to_flow on {fp:N} — add to membership set
    /// 2. HSET flow_id on exec_core on {p:N} — link execution to flow
    pub async fn add_execution_to_flow(
        &self,
        args: &AddExecutionToFlowArgs,
    ) -> Result<AddExecutionToFlowResult, ServerError> {
        let partition = flow_partition(&args.flow_id, &self.config.partition_config);
        let fctx = FlowKeyContext::new(&partition, &args.flow_id);
        let fidx = FlowIndexKeys::new(&partition);

        // Phase 1: FCALL on {fp:N}
        // KEYS (3): flow_core, members_set, flow_index
        let fcall_keys: Vec<String> = vec![fctx.core(), fctx.members(), fidx.flow_index()];

        // ARGV (3): flow_id, execution_id, now_ms
        let fcall_args: Vec<String> = vec![
            args.flow_id.to_string(),
            args.execution_id.to_string(),
            args.now.to_string(),
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_add_execution_to_flow", &key_refs, &arg_refs)
            .await?;

        let result = parse_add_execution_to_flow_result(&raw)?;

        // Phase 2: HSET flow_id on exec_core on {p:N}
        // This is idempotent — safe to retry if phase 1 succeeded but phase 2 failed.
        let exec_partition =
            execution_partition(&args.execution_id, &self.config.partition_config);
        let ectx = ExecKeyContext::new(&exec_partition, &args.execution_id);
        self.client
            .hset(&ectx.core(), "flow_id", &args.flow_id.to_string())
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HSET flow_id on exec_core".into() })?;

        Ok(result)
    }

    /// Cancel a flow.
    ///
    /// Flips `public_flow_state` to `cancelled` atomically via
    /// `ff_cancel_flow` on `{fp:N}`. For `cancel_all` policy, member
    /// executions must be cancelled cross-partition; this dispatch runs in
    /// the background and the call returns [`CancelFlowResult::CancellationScheduled`]
    /// immediately. For all other policies (or flows with no members), or
    /// when the flow was already in a terminal state (idempotent retry),
    /// the call returns [`CancelFlowResult::Cancelled`].
    ///
    /// Clients that need synchronous completion can call [`Self::cancel_flow_wait`].
    ///
    /// # Backpressure
    ///
    /// Each call that hits the async dispatch path spawns a new task into
    /// the shared background `JoinSet`. Rapid repeated calls against the
    /// same flow will spawn *multiple* overlapping dispatch tasks. This is
    /// not a correctness issue — each member cancel is idempotent and
    /// terminal flows short-circuit via [`ParsedCancelFlow::AlreadyTerminal`]
    /// — but heavy burst callers should either use `?wait=true` (serialises
    /// the dispatch on the HTTP thread, giving natural backpressure) or
    /// implement client-side deduplication on `flow_id`. The `JoinSet` is
    /// drained with a 15s timeout on [`Self::shutdown`], so very long
    /// dispatch tails may be aborted during graceful shutdown.
    ///
    /// # Orphan-member semantics on shutdown abort
    ///
    /// If shutdown fires `JoinSet::abort_all()` after its drain timeout
    /// while a dispatch loop is mid-iteration, the already-issued
    /// `ff_cancel_execution` FCALLs (atomic Lua) complete cleanly with
    /// `terminal_outcome = cancelled` and the caller-supplied reason. The
    /// members not yet visited are abandoned mid-loop. They remain in
    /// whichever state they were in (active/eligible/suspended) until the
    /// natural lifecycle scanners reach them: active leases expire
    /// (`lease_expiry`) and attempt-timeout them to `expired`, suspended
    /// members time out to `skipped`, eligible ones sit until retention
    /// trim. So no orphan state — but the terminal_outcome for the
    /// abandoned members will be `expired`/`skipped` rather than
    /// `cancelled`, and the operator-supplied `reason` is lost for them.
    /// Audit tooling that requires reason fidelity across shutdowns should
    /// use `?wait=true`.
    pub async fn cancel_flow(
        &self,
        args: &CancelFlowArgs,
    ) -> Result<CancelFlowResult, ServerError> {
        self.cancel_flow_inner(args, false).await
    }

    /// Cancel a flow and wait for all member cancellations to complete
    /// inline. Slower than [`Self::cancel_flow`] for large flows, but
    /// guarantees every member is in a terminal state on return.
    pub async fn cancel_flow_wait(
        &self,
        args: &CancelFlowArgs,
    ) -> Result<CancelFlowResult, ServerError> {
        self.cancel_flow_inner(args, true).await
    }

    async fn cancel_flow_inner(
        &self,
        args: &CancelFlowArgs,
        wait: bool,
    ) -> Result<CancelFlowResult, ServerError> {
        let partition = flow_partition(&args.flow_id, &self.config.partition_config);
        let fctx = FlowKeyContext::new(&partition, &args.flow_id);
        let fidx = FlowIndexKeys::new(&partition);

        // KEYS (3): flow_core, members_set, flow_index
        let fcall_keys: Vec<String> = vec![fctx.core(), fctx.members(), fidx.flow_index()];

        // ARGV (4): flow_id, reason, cancellation_policy, now_ms
        let fcall_args: Vec<String> = vec![
            args.flow_id.to_string(),
            args.reason.clone(),
            args.cancellation_policy.clone(),
            args.now.to_string(),
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_cancel_flow", &key_refs, &arg_refs)
            .await?;

        let (policy, members) = match parse_cancel_flow_raw(&raw)? {
            ParsedCancelFlow::Cancelled { policy, member_execution_ids } => {
                (policy, member_execution_ids)
            }
            // Idempotent retry: flow was already cancelled/completed/failed.
            // Return Cancelled with the *stored* policy and member list so
            // observability tooling gets the real historical state rather
            // than echoing the caller's retry intent. One HMGET + SMEMBERS
            // on the idempotent path — both on {fp:N}, same slot.
            ParsedCancelFlow::AlreadyTerminal => {
                let flow_meta: Vec<Option<String>> = self
                    .client
                    .cmd("HMGET")
                    .arg(fctx.core())
                    .arg("cancellation_policy")
                    .arg("cancel_reason")
                    .execute()
                    .await
                    .map_err(|e| ServerError::ValkeyContext {
                        source: e,
                        context: "HMGET flow_core cancellation_policy,cancel_reason".into(),
                    })?;
                let stored_policy = flow_meta
                    .first()
                    .and_then(|v| v.as_ref())
                    .filter(|s| !s.is_empty())
                    .cloned();
                let stored_reason = flow_meta
                    .get(1)
                    .and_then(|v| v.as_ref())
                    .filter(|s| !s.is_empty())
                    .cloned();
                let all_members: Vec<String> = self
                    .client
                    .cmd("SMEMBERS")
                    .arg(fctx.members())
                    .execute()
                    .await
                    .map_err(|e| ServerError::ValkeyContext {
                        source: e,
                        context: "SMEMBERS flow members (already terminal)".into(),
                    })?;
                // Cap the returned list to avoid pathological bandwidth on
                // idempotent retries for flows with 10k+ members. Clients
                // already received the authoritative member list on the
                // first (non-idempotent) call; subsequent retries just need
                // enough to confirm the operation and trigger per-member
                // polling if desired.
                let total_members = all_members.len();
                let stored_members: Vec<String> = all_members
                    .into_iter()
                    .take(ALREADY_TERMINAL_MEMBER_CAP)
                    .collect();
                tracing::debug!(
                    flow_id = %args.flow_id,
                    stored_policy = stored_policy.as_deref().unwrap_or(""),
                    stored_reason = stored_reason.as_deref().unwrap_or(""),
                    total_members,
                    returned_members = stored_members.len(),
                    "cancel_flow: flow already terminal, returning idempotent Cancelled"
                );
                return Ok(CancelFlowResult::Cancelled {
                    // Fall back to caller's policy only if the stored field
                    // is missing (flows cancelled by older Lua that did not
                    // persist cancellation_policy).
                    cancellation_policy: stored_policy
                        .unwrap_or_else(|| args.cancellation_policy.clone()),
                    member_execution_ids: stored_members,
                });
            }
        };
        let needs_dispatch = policy == "cancel_all" && !members.is_empty();

        if !needs_dispatch {
            return Ok(CancelFlowResult::Cancelled {
                cancellation_policy: policy,
                member_execution_ids: members,
            });
        }

        if wait {
            // Synchronous dispatch — cancel every member inline before returning.
            for eid_str in &members {
                if let Err(e) = cancel_member_execution(
                    &self.client,
                    &self.config.partition_config,
                    eid_str,
                    &args.reason,
                    args.now,
                )
                .await
                {
                    tracing::warn!(
                        execution_id = %eid_str,
                        error = %e,
                        "cancel_flow(wait): individual execution cancel failed (may be terminal)"
                    );
                }
            }
            return Ok(CancelFlowResult::Cancelled {
                cancellation_policy: policy,
                member_execution_ids: members,
            });
        }

        // Asynchronous dispatch — spawn into the shared JoinSet so Server::shutdown
        // can wait for pending cancellations (bounded by a shutdown timeout).
        let client = self.client.clone();
        let partition_config = self.config.partition_config;
        let reason = args.reason.clone();
        let now = args.now;
        let dispatch_members = members.clone();
        let flow_id = args.flow_id.clone();
        // Every async cancel_flow contends on this lock, but the critical
        // section is tiny: try_join_next drain + spawn. Drain is amortized
        // O(1) — each completed task is reaped exactly once across all
        // callers, and spawn is synchronous. At realistic cancel rates the
        // lock hold time is microseconds and does not bottleneck handlers.
        let mut guard = self.background_tasks.lock().await;

        // Reap completed background dispatches before spawning the next.
        // Without this sweep, JoinSet accumulates Ok(()) results for every
        // async cancel ever issued — a memory leak in long-running servers
        // that would otherwise only drain on Server::shutdown. Surface any
        // panicked/aborted dispatches via tracing so silent failures in
        // cancel_member_execution are visible in logs.
        while let Some(joined) = guard.try_join_next() {
            if let Err(e) = joined {
                tracing::warn!(
                    error = %e,
                    "cancel_flow: background dispatch task panicked or was aborted"
                );
            }
        }

        guard.spawn(async move {
            for eid_str in &dispatch_members {
                if let Err(e) = cancel_member_execution(
                    &client,
                    &partition_config,
                    eid_str,
                    &reason,
                    now,
                )
                .await
                {
                    tracing::warn!(
                        flow_id = %flow_id,
                        execution_id = %eid_str,
                        error = %e,
                        "cancel_flow(async): individual execution cancel failed (may be terminal)"
                    );
                }
            }
            tracing::debug!(
                flow_id = %flow_id,
                member_count = dispatch_members.len(),
                "cancel_flow: background member dispatch complete"
            );
        });
        drop(guard);

        let member_count = u32::try_from(members.len()).unwrap_or(u32::MAX);
        Ok(CancelFlowResult::CancellationScheduled {
            cancellation_policy: policy,
            member_count,
            member_execution_ids: members,
        })
    }

    /// Stage a dependency edge between two executions in a flow.
    ///
    /// Runs on the flow partition {fp:N}.
    /// KEYS (6), ARGV (8) — matches lua/flow.lua ff_stage_dependency_edge.
    pub async fn stage_dependency_edge(
        &self,
        args: &StageDependencyEdgeArgs,
    ) -> Result<StageDependencyEdgeResult, ServerError> {
        let partition = flow_partition(&args.flow_id, &self.config.partition_config);
        let fctx = FlowKeyContext::new(&partition, &args.flow_id);

        // KEYS (6): flow_core, members_set, edge_hash, out_adj_set, in_adj_set, grant_hash
        let fcall_keys: Vec<String> = vec![
            fctx.core(),
            fctx.members(),
            fctx.edge(&args.edge_id),
            fctx.outgoing(&args.upstream_execution_id),
            fctx.incoming(&args.downstream_execution_id),
            fctx.grant(&args.edge_id.to_string()),
        ];

        // ARGV (8): flow_id, edge_id, upstream_eid, downstream_eid,
        //           dependency_kind, data_passing_ref, expected_graph_revision, now_ms
        let fcall_args: Vec<String> = vec![
            args.flow_id.to_string(),
            args.edge_id.to_string(),
            args.upstream_execution_id.to_string(),
            args.downstream_execution_id.to_string(),
            args.dependency_kind.clone(),
            args.data_passing_ref.clone().unwrap_or_default(),
            args.expected_graph_revision.to_string(),
            args.now.to_string(),
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_stage_dependency_edge", &key_refs, &arg_refs)
            .await?;

        parse_stage_dependency_edge_result(&raw)
    }

    /// Apply a staged dependency edge to the child execution.
    ///
    /// Runs on the child execution partition {p:N}.
    /// KEYS (7), ARGV (7) — matches lua/flow.lua ff_apply_dependency_to_child.
    pub async fn apply_dependency_to_child(
        &self,
        args: &ApplyDependencyToChildArgs,
    ) -> Result<ApplyDependencyToChildResult, ServerError> {
        let partition = execution_partition(
            &args.downstream_execution_id,
            &self.config.partition_config,
        );
        let ctx = ExecKeyContext::new(&partition, &args.downstream_execution_id);
        let idx = IndexKeys::new(&partition);

        // Pre-read lane_id for index keys
        let lane_str: Option<String> = self
            .client
            .hget(&ctx.core(), "lane_id")
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGET lane_id".into() })?;
        let lane = LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()));

        // KEYS (7): exec_core, deps_meta, unresolved_set, dep_hash,
        //           eligible_zset, blocked_deps_zset, deps_all_edges
        let fcall_keys: Vec<String> = vec![
            ctx.core(),
            ctx.deps_meta(),
            ctx.deps_unresolved(),
            ctx.dep_edge(&args.edge_id),
            idx.lane_eligible(&lane),
            idx.lane_blocked_dependencies(&lane),
            ctx.deps_all_edges(),
        ];

        // ARGV (7): flow_id, edge_id, upstream_eid, graph_revision,
        //           dependency_kind, data_passing_ref, now_ms
        let fcall_args: Vec<String> = vec![
            args.flow_id.to_string(),
            args.edge_id.to_string(),
            args.upstream_execution_id.to_string(),
            args.graph_revision.to_string(),
            args.dependency_kind.clone(),
            args.data_passing_ref.clone().unwrap_or_default(),
            args.now.to_string(),
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_apply_dependency_to_child", &key_refs, &arg_refs)
            .await?;

        parse_apply_dependency_result(&raw)
    }

    // ── Execution operations API ──

    /// Deliver a signal to a suspended (or pending-waitpoint) execution.
    ///
    /// Pre-reads exec_core for waitpoint/suspension fields needed for KEYS.
    /// KEYS (13), ARGV (17) — matches lua/signal.lua ff_deliver_signal.
    pub async fn deliver_signal(
        &self,
        args: &DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, ServerError> {
        let partition = execution_partition(&args.execution_id, &self.config.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);

        // Pre-read lane_id for index keys
        let lane_str: Option<String> = self
            .client
            .hget(&ctx.core(), "lane_id")
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGET lane_id".into() })?;
        let lane = LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()));

        let wp_id = &args.waitpoint_id;
        let sig_id = &args.signal_id;
        let idem_key = args
            .idempotency_key
            .as_ref()
            .filter(|k| !k.is_empty())
            .map(|k| ctx.signal_dedup(wp_id, k))
            .unwrap_or_else(|| ctx.noop());

        // KEYS (13): exec_core, wp_condition, wp_signals_stream,
        //            exec_signals_zset, signal_hash, signal_payload,
        //            idem_key, waitpoint_hash, suspension_current,
        //            eligible_zset, suspended_zset, delayed_zset,
        //            suspension_timeout_zset
        let fcall_keys: Vec<String> = vec![
            ctx.core(),                       // 1
            ctx.waitpoint_condition(wp_id),    // 2
            ctx.waitpoint_signals(wp_id),      // 3
            ctx.exec_signals(),                // 4
            ctx.signal(sig_id),                // 5
            ctx.signal_payload(sig_id),        // 6
            idem_key,                          // 7
            ctx.waitpoint(wp_id),              // 8
            ctx.suspension_current(),          // 9
            idx.lane_eligible(&lane),          // 10
            idx.lane_suspended(&lane),         // 11
            idx.lane_delayed(&lane),           // 12
            idx.suspension_timeout(),          // 13
        ];

        // ARGV (17): signal_id, execution_id, waitpoint_id, signal_name,
        //            signal_category, source_type, source_identity,
        //            payload, payload_encoding, idempotency_key,
        //            correlation_id, target_scope, created_at,
        //            dedup_ttl_ms, resume_delay_ms, signal_maxlen,
        //            max_signals_per_execution
        let fcall_args: Vec<String> = vec![
            args.signal_id.to_string(),                          // 1
            args.execution_id.to_string(),                       // 2
            args.waitpoint_id.to_string(),                       // 3
            args.signal_name.clone(),                            // 4
            args.signal_category.clone(),                        // 5
            args.source_type.clone(),                            // 6
            args.source_identity.clone(),                        // 7
            args.payload.as_ref()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default(),                            // 8
            args.payload_encoding
                .clone()
                .unwrap_or_else(|| "json".to_owned()),           // 9
            args.idempotency_key
                .clone()
                .unwrap_or_default(),                            // 10
            args.correlation_id
                .clone()
                .unwrap_or_default(),                            // 11
            args.target_scope.clone(),                           // 12
            args.created_at
                .map(|ts| ts.to_string())
                .unwrap_or_else(|| args.now.to_string()),        // 13
            args.dedup_ttl_ms.unwrap_or(86_400_000).to_string(), // 14
            args.resume_delay_ms.unwrap_or(0).to_string(),       // 15
            args.signal_maxlen.unwrap_or(1000).to_string(),      // 16
            args.max_signals_per_execution
                .unwrap_or(10_000)
                .to_string(),                                    // 17
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_deliver_signal", &key_refs, &arg_refs)
            .await?;

        parse_deliver_signal_result(&raw, &args.signal_id)
    }

    /// Change an execution's priority.
    ///
    /// KEYS (2), ARGV (2) — matches lua/scheduling.lua ff_change_priority.
    pub async fn change_priority(
        &self,
        execution_id: &ExecutionId,
        new_priority: i32,
    ) -> Result<ChangePriorityResult, ServerError> {
        let partition = execution_partition(execution_id, &self.config.partition_config);
        let ctx = ExecKeyContext::new(&partition, execution_id);
        let idx = IndexKeys::new(&partition);

        // Read lane_id for eligible_zset key
        let lane_str: Option<String> = self
            .client
            .hget(&ctx.core(), "lane_id")
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGET lane_id".into() })?;
        let lane = LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()));

        // KEYS (2): exec_core, eligible_zset
        let fcall_keys: Vec<String> = vec![ctx.core(), idx.lane_eligible(&lane)];

        // ARGV (2): execution_id, new_priority
        let fcall_args: Vec<String> = vec![
            execution_id.to_string(),
            new_priority.to_string(),
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_change_priority", &key_refs, &arg_refs)
            .await?;

        parse_change_priority_result(&raw, execution_id)
    }

    /// Revoke an active lease (operator-initiated).
    pub async fn revoke_lease(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<RevokeLeaseResult, ServerError> {
        let partition = execution_partition(execution_id, &self.config.partition_config);
        let ctx = ExecKeyContext::new(&partition, execution_id);
        let idx = IndexKeys::new(&partition);

        // Pre-read worker_instance_id for worker_leases key
        let wiid_str: Option<String> = self
            .client
            .hget(&ctx.core(), "current_worker_instance_id")
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGET worker_instance_id".into() })?;
        let wiid = match wiid_str {
            Some(ref s) if !s.is_empty() => WorkerInstanceId::new(s),
            _ => {
                return Err(ServerError::NotFound(format!(
                    "no active lease for execution {execution_id} (no current_worker_instance_id)"
                )));
            }
        };

        // KEYS (5): exec_core, lease_current, lease_history, lease_expiry_zset, worker_leases
        let fcall_keys: Vec<String> = vec![
            ctx.core(),
            ctx.lease_current(),
            ctx.lease_history(),
            idx.lease_expiry(),
            idx.worker_leases(&wiid),
        ];

        // ARGV (3): execution_id, expected_lease_id (empty = skip check), revoke_reason
        let fcall_args: Vec<String> = vec![
            execution_id.to_string(),
            String::new(), // no expected_lease_id check
            "operator_revoke".to_owned(),
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_revoke_lease", &key_refs, &arg_refs)
            .await?;

        parse_revoke_lease_result(&raw)
    }

    /// Get full execution info via HGETALL on exec_core.
    pub async fn get_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<ExecutionInfo, ServerError> {
        let partition = execution_partition(execution_id, &self.config.partition_config);
        let ctx = ExecKeyContext::new(&partition, execution_id);

        let fields: HashMap<String, String> = self
            .client
            .hgetall(&ctx.core())
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGETALL exec_core".into() })?;

        if fields.is_empty() {
            return Err(ServerError::NotFound(format!(
                "execution not found: {execution_id}"
            )));
        }

        let parse_enum = |field: &str| -> String {
            fields.get(field).cloned().unwrap_or_default()
        };
        fn deserialize<T: serde::de::DeserializeOwned>(field: &str, raw: &str) -> Result<T, ServerError> {
            let quoted = format!("\"{raw}\"");
            serde_json::from_str(&quoted).map_err(|e| {
                ServerError::Script(format!("invalid {field} '{raw}': {e}"))
            })
        }

        let lp_str = parse_enum("lifecycle_phase");
        let os_str = parse_enum("ownership_state");
        let es_str = parse_enum("eligibility_state");
        let br_str = parse_enum("blocking_reason");
        let to_str = parse_enum("terminal_outcome");
        let as_str = parse_enum("attempt_state");
        let ps_str = parse_enum("public_state");

        let state_vector = StateVector {
            lifecycle_phase: deserialize("lifecycle_phase", &lp_str)?,
            ownership_state: deserialize("ownership_state", &os_str)?,
            eligibility_state: deserialize("eligibility_state", &es_str)?,
            blocking_reason: deserialize("blocking_reason", &br_str)?,
            terminal_outcome: deserialize("terminal_outcome", &to_str)?,
            attempt_state: deserialize("attempt_state", &as_str)?,
            public_state: deserialize("public_state", &ps_str)?,
        };

        let flow_id_val = fields.get("flow_id").filter(|s| !s.is_empty()).cloned();

        Ok(ExecutionInfo {
            execution_id: execution_id.clone(),
            namespace: parse_enum("namespace"),
            lane_id: parse_enum("lane_id"),
            priority: fields
                .get("priority")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            execution_kind: parse_enum("execution_kind"),
            state_vector,
            public_state: deserialize("public_state", &ps_str)?,
            created_at: parse_enum("created_at"),
            current_attempt_index: fields
                .get("current_attempt_index")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            flow_id: flow_id_val,
            blocking_detail: parse_enum("blocking_detail"),
        })
    }

    /// List executions from a partition's index ZSET.
    ///
    /// No FCALL — direct ZRANGE + pipelined HMGET reads.
    pub async fn list_executions(
        &self,
        partition_id: u16,
        lane: &LaneId,
        state_filter: &str,
        offset: u64,
        limit: u64,
    ) -> Result<ListExecutionsResult, ServerError> {
        let partition = ff_core::partition::Partition {
            family: ff_core::partition::PartitionFamily::Execution,
            index: partition_id,
        };
        let idx = IndexKeys::new(&partition);

        let zset_key = match state_filter {
            "eligible" => idx.lane_eligible(lane),
            "delayed" => idx.lane_delayed(lane),
            "terminal" => idx.lane_terminal(lane),
            "suspended" => idx.lane_suspended(lane),
            "active" => idx.lane_active(lane),
            other => {
                return Err(ServerError::InvalidInput(format!(
                    "invalid state_filter: {other}. Use: eligible, delayed, terminal, suspended, active"
                )));
            }
        };

        // ZRANGE key -inf +inf BYSCORE LIMIT offset count
        let eids: Vec<String> = self
            .client
            .cmd("ZRANGE")
            .arg(&zset_key)
            .arg("-inf")
            .arg("+inf")
            .arg("BYSCORE")
            .arg("LIMIT")
            .arg(offset)
            .arg(limit)
            .execute()
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: format!("ZRANGE {zset_key}") })?;

        if eids.is_empty() {
            return Ok(ListExecutionsResult {
                executions: vec![],
                total_returned: 0,
            });
        }

        // Parse execution IDs, warning on corrupt ZSET members
        let mut parsed = Vec::with_capacity(eids.len());
        for eid_str in &eids {
            match ExecutionId::parse(eid_str) {
                Ok(id) => parsed.push(id),
                Err(e) => {
                    tracing::warn!(
                        raw_id = %eid_str,
                        error = %e,
                        zset = %zset_key,
                        "list_executions: ZSET member failed to parse as ExecutionId (data corruption?)"
                    );
                }
            }
        }

        if parsed.is_empty() {
            return Ok(ListExecutionsResult {
                executions: vec![],
                total_returned: 0,
            });
        }

        // Pipeline all HMGETs into a single round-trip
        let mut pipe = self.client.pipeline();
        let mut slots = Vec::with_capacity(parsed.len());
        for eid in &parsed {
            let ep = execution_partition(eid, &self.config.partition_config);
            let ctx = ExecKeyContext::new(&ep, eid);
            let slot = pipe
                .cmd::<Vec<Option<String>>>("HMGET")
                .arg(ctx.core())
                .arg("namespace")
                .arg("lane_id")
                .arg("execution_kind")
                .arg("public_state")
                .arg("priority")
                .arg("created_at")
                .finish();
            slots.push(slot);
        }

        pipe.execute()
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "pipeline HMGET".into() })?;

        let mut summaries = Vec::with_capacity(parsed.len());
        for (eid, slot) in parsed.into_iter().zip(slots) {
            let fields: Vec<Option<String>> = slot.value()
                .map_err(|e| ServerError::ValkeyContext { source: e, context: "pipeline slot".into() })?;

            let field = |i: usize| -> String {
                fields
                    .get(i)
                    .and_then(|v| v.as_ref())
                    .cloned()
                    .unwrap_or_default()
            };

            summaries.push(ExecutionSummary {
                execution_id: eid,
                namespace: field(0),
                lane_id: field(1),
                execution_kind: field(2),
                public_state: field(3),
                priority: field(4).parse().unwrap_or(0),
                created_at: field(5),
            });
        }

        let total = summaries.len();
        Ok(ListExecutionsResult {
            executions: summaries,
            total_returned: total,
        })
    }

    /// Replay a terminal execution.
    ///
    /// Pre-reads exec_core for flow_id and dep edges (variable KEYS).
    /// KEYS (4+N), ARGV (2+N) — matches lua/flow.lua ff_replay_execution.
    pub async fn replay_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<ReplayExecutionResult, ServerError> {
        let partition = execution_partition(execution_id, &self.config.partition_config);
        let ctx = ExecKeyContext::new(&partition, execution_id);
        let idx = IndexKeys::new(&partition);

        // Pre-read lane_id, flow_id, terminal_outcome
        let dyn_fields: Vec<Option<String>> = self
            .client
            .cmd("HMGET")
            .arg(ctx.core())
            .arg("lane_id")
            .arg("flow_id")
            .arg("terminal_outcome")
            .execute()
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HMGET replay pre-read".into() })?;
        let lane = LaneId::new(
            dyn_fields
                .first()
                .and_then(|v| v.as_ref())
                .cloned()
                .unwrap_or_else(|| "default".to_owned()),
        );
        let flow_id_str = dyn_fields
            .get(1)
            .and_then(|v| v.as_ref())
            .cloned()
            .unwrap_or_default();
        let terminal_outcome = dyn_fields
            .get(2)
            .and_then(|v| v.as_ref())
            .cloned()
            .unwrap_or_default();

        let is_skipped_flow_member = terminal_outcome == "skipped" && !flow_id_str.is_empty();

        // Base KEYS (4): exec_core, terminal_zset, eligible_zset, lease_history
        let mut fcall_keys: Vec<String> = vec![
            ctx.core(),
            idx.lane_terminal(&lane),
            idx.lane_eligible(&lane),
            ctx.lease_history(),
        ];

        // Base ARGV (2): execution_id, now_ms
        let now = TimestampMs::now();
        let mut fcall_args: Vec<String> = vec![execution_id.to_string(), now.to_string()];

        if is_skipped_flow_member {
            // Read ALL inbound edge IDs from the flow partition's adjacency set.
            // Cannot use deps:unresolved because impossible edges were SREM'd
            // by ff_resolve_dependency. The flow's in:<eid> set has all edges.
            let flow_id = FlowId::parse(&flow_id_str)
                .map_err(|e| ServerError::Script(format!("bad flow_id: {e}")))?;
            let flow_part =
                flow_partition(&flow_id, &self.config.partition_config);
            let flow_ctx = FlowKeyContext::new(&flow_part, &flow_id);
            let edge_ids: Vec<String> = self
                .client
                .cmd("SMEMBERS")
                .arg(flow_ctx.incoming(execution_id))
                .execute()
                .await
                .map_err(|e| ServerError::ValkeyContext { source: e, context: "SMEMBERS replay edges".into() })?;

            // Extended KEYS: blocked_deps_zset, deps_meta, deps_unresolved, dep_edge_0..N
            fcall_keys.push(idx.lane_blocked_dependencies(&lane)); // 5
            fcall_keys.push(ctx.deps_meta()); // 6
            fcall_keys.push(ctx.deps_unresolved()); // 7
            for eid_str in &edge_ids {
                let edge_id = EdgeId::parse(eid_str)
                    .unwrap_or_else(|_| EdgeId::new());
                fcall_keys.push(ctx.dep_edge(&edge_id)); // 8..8+N
                fcall_args.push(eid_str.clone()); // 3..3+N
            }
        }

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .fcall_with_reload("ff_replay_execution", &key_refs, &arg_refs)
            .await?;

        parse_replay_result(&raw)
    }

    /// Graceful shutdown — stops scanners, drains background handler tasks
    /// (e.g. cancel_flow member dispatch) with a bounded timeout, then waits
    /// for scanners to finish.
    pub async fn shutdown(self) {
        tracing::info!("shutting down FlowFabric server");

        // Drain handler-spawned background tasks with the same ceiling as
        // Engine::shutdown. If dispatch is still running at the deadline,
        // drop the JoinSet to abort remaining tasks.
        let drain_timeout = Duration::from_secs(15);
        let background = self.background_tasks.clone();
        let drain = async move {
            let mut guard = background.lock().await;
            while guard.join_next().await.is_some() {}
        };
        match tokio::time::timeout(drain_timeout, drain).await {
            Ok(()) => {}
            Err(_) => {
                tracing::warn!(
                    timeout_s = drain_timeout.as_secs(),
                    "shutdown: background tasks did not finish in time, aborting"
                );
                self.background_tasks.lock().await.abort_all();
            }
        }

        self.engine.shutdown().await;
        tracing::info!("FlowFabric server shutdown complete");
    }
}

// ── Partition config validation ──

/// Validate or create the `ff:config:partitions` key on first boot.
///
/// If the key exists, its values must match the server's config.
/// If it doesn't exist, create it (first boot).
async fn validate_or_create_partition_config(
    client: &Client,
    config: &PartitionConfig,
) -> Result<(), ServerError> {
    let key = keys::global_config_partitions();

    let existing: HashMap<String, String> = client
        .hgetall(&key)
        .await
        .map_err(|e| ServerError::ValkeyContext { source: e, context: format!("HGETALL {key}") })?;

    if existing.is_empty() {
        // First boot — create the config
        tracing::info!("first boot: creating {key}");
        client
            .hset(&key, "num_execution_partitions", &config.num_execution_partitions.to_string())
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HSET num_execution_partitions".into() })?;
        client
            .hset(&key, "num_flow_partitions", &config.num_flow_partitions.to_string())
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HSET num_flow_partitions".into() })?;
        client
            .hset(&key, "num_budget_partitions", &config.num_budget_partitions.to_string())
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HSET num_budget_partitions".into() })?;
        client
            .hset(&key, "num_quota_partitions", &config.num_quota_partitions.to_string())
            .await
            .map_err(|e| ServerError::ValkeyContext { source: e, context: "HSET num_quota_partitions".into() })?;
        return Ok(());
    }

    // Validate existing config matches
    let check = |field: &str, expected: u16| -> Result<(), ServerError> {
        let stored: u16 = existing
            .get(field)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if stored != expected {
            return Err(ServerError::PartitionMismatch(format!(
                "{field}: stored={stored}, config={expected}. \
                 Partition counts are fixed at deployment time. \
                 Either fix your config or migrate the data."
            )));
        }
        Ok(())
    };

    check("num_execution_partitions", config.num_execution_partitions)?;
    check("num_flow_partitions", config.num_flow_partitions)?;
    check("num_budget_partitions", config.num_budget_partitions)?;
    check("num_quota_partitions", config.num_quota_partitions)?;

    tracing::info!("partition config validated against stored {key}");
    Ok(())
}

// ── FCALL result parsing ──

fn parse_create_result(
    raw: &Value,
    execution_id: &ExecutionId,
) -> Result<CreateExecutionResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_create_execution: expected Array".into())),
    };

    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_create_execution: bad status code".into())),
    };

    if status == 1 {
        // Check sub-status: OK or DUPLICATE
        let sub = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();

        if sub == "DUPLICATE" {
            Ok(CreateExecutionResult::Duplicate {
                execution_id: execution_id.clone(),
            })
        } else {
            Ok(CreateExecutionResult::Created {
                execution_id: execution_id.clone(),
                public_state: PublicState::Waiting,
            })
        }
    } else {
        let error_code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned());
        Err(ServerError::OperationFailed(format!(
            "ff_create_execution failed: {error_code}"
        )))
    }
}

fn parse_cancel_result(
    raw: &Value,
    execution_id: &ExecutionId,
) -> Result<CancelExecutionResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_cancel_execution: expected Array".into())),
    };

    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_cancel_execution: bad status code".into())),
    };

    if status == 1 {
        Ok(CancelExecutionResult::Cancelled {
            execution_id: execution_id.clone(),
            public_state: PublicState::Cancelled,
        })
    } else {
        let error_code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned());
        Err(ServerError::OperationFailed(format!(
            "ff_cancel_execution failed: {error_code}"
        )))
    }
}

fn parse_budget_create_result(
    raw: &Value,
    budget_id: &BudgetId,
) -> Result<CreateBudgetResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_create_budget: expected Array".into())),
    };

    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_create_budget: bad status code".into())),
    };

    if status == 1 {
        let sub = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();

        if sub == "ALREADY_SATISFIED" {
            Ok(CreateBudgetResult::AlreadySatisfied {
                budget_id: budget_id.clone(),
            })
        } else {
            Ok(CreateBudgetResult::Created {
                budget_id: budget_id.clone(),
            })
        }
    } else {
        let error_code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned());
        Err(ServerError::OperationFailed(format!(
            "ff_create_budget failed: {error_code}"
        )))
    }
}

fn parse_quota_create_result(
    raw: &Value,
    quota_policy_id: &QuotaPolicyId,
) -> Result<CreateQuotaPolicyResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_create_quota_policy: expected Array".into())),
    };

    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_create_quota_policy: bad status code".into())),
    };

    if status == 1 {
        let sub = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();

        if sub == "ALREADY_SATISFIED" {
            Ok(CreateQuotaPolicyResult::AlreadySatisfied {
                quota_policy_id: quota_policy_id.clone(),
            })
        } else {
            Ok(CreateQuotaPolicyResult::Created {
                quota_policy_id: quota_policy_id.clone(),
            })
        }
    } else {
        let error_code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned());
        Err(ServerError::OperationFailed(format!(
            "ff_create_quota_policy failed: {error_code}"
        )))
    }
}

// ── Flow FCALL result parsing ──

fn parse_create_flow_result(
    raw: &Value,
    flow_id: &FlowId,
) -> Result<CreateFlowResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_create_flow: expected Array".into())),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_create_flow: bad status code".into())),
    };
    if status == 1 {
        let sub = fcall_field_str(arr, 1);
        if sub == "ALREADY_SATISFIED" {
            Ok(CreateFlowResult::AlreadySatisfied {
                flow_id: flow_id.clone(),
            })
        } else {
            Ok(CreateFlowResult::Created {
                flow_id: flow_id.clone(),
            })
        }
    } else {
        let error_code = fcall_field_str(arr, 1);
        Err(ServerError::OperationFailed(format!(
            "ff_create_flow failed: {error_code}"
        )))
    }
}

fn parse_add_execution_to_flow_result(
    raw: &Value,
) -> Result<AddExecutionToFlowResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(ServerError::Script(
                "ff_add_execution_to_flow: expected Array".into(),
            ))
        }
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(ServerError::Script(
                "ff_add_execution_to_flow: bad status code".into(),
            ))
        }
    };
    if status == 1 {
        let sub = fcall_field_str(arr, 1);
        let eid_str = fcall_field_str(arr, 2);
        let nc_str = fcall_field_str(arr, 3);
        let eid = ExecutionId::parse(&eid_str)
            .map_err(|e| ServerError::Script(format!("bad execution_id: {e}")))?;
        let nc: u32 = nc_str.parse().unwrap_or(0);
        if sub == "ALREADY_SATISFIED" {
            Ok(AddExecutionToFlowResult::AlreadyMember {
                execution_id: eid,
                node_count: nc,
            })
        } else {
            Ok(AddExecutionToFlowResult::Added {
                execution_id: eid,
                new_node_count: nc,
            })
        }
    } else {
        let error_code = fcall_field_str(arr, 1);
        Err(ServerError::OperationFailed(format!(
            "ff_add_execution_to_flow failed: {error_code}"
        )))
    }
}

/// Outcome of parsing a raw `ff_cancel_flow` FCALL response.
///
/// Keeps `AlreadyTerminal` distinct from other script errors so the caller
/// can treat cancel on an already-cancelled/completed/failed flow as
/// idempotent success instead of surfacing a 400 to the client.
enum ParsedCancelFlow {
    Cancelled {
        policy: String,
        member_execution_ids: Vec<String>,
    },
    AlreadyTerminal,
}

/// Parse the raw `ff_cancel_flow` FCALL response.
///
/// Returns [`ParsedCancelFlow::Cancelled`] on success, [`ParsedCancelFlow::AlreadyTerminal`]
/// when the flow was already in a terminal state (idempotent retry), or a
/// [`ServerError`] for any other failure.
fn parse_cancel_flow_raw(raw: &Value) -> Result<ParsedCancelFlow, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_cancel_flow: expected Array".into())),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_cancel_flow: bad status code".into())),
    };
    if status != 1 {
        let error_code = fcall_field_str(arr, 1);
        if error_code == "flow_already_terminal" {
            return Ok(ParsedCancelFlow::AlreadyTerminal);
        }
        return Err(ServerError::OperationFailed(format!(
            "ff_cancel_flow failed: {error_code}"
        )));
    }
    // {1, "OK", cancellation_policy, member1, member2, ...}
    let policy = fcall_field_str(arr, 2);
    // Iterate to arr.len() rather than breaking on the first empty string —
    // safer against malformed Lua responses and clearer than a sentinel loop.
    let mut members = Vec::with_capacity(arr.len().saturating_sub(3));
    for i in 3..arr.len() {
        members.push(fcall_field_str(arr, i));
    }
    Ok(ParsedCancelFlow::Cancelled { policy, member_execution_ids: members })
}

fn parse_stage_dependency_edge_result(
    raw: &Value,
) -> Result<StageDependencyEdgeResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_stage_dependency_edge: expected Array".into())),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_stage_dependency_edge: bad status code".into())),
    };
    if status == 1 {
        let edge_id_str = fcall_field_str(arr, 2);
        let rev_str = fcall_field_str(arr, 3);
        let edge_id = EdgeId::parse(&edge_id_str)
            .map_err(|e| ServerError::Script(format!("bad edge_id: {e}")))?;
        let rev: u64 = rev_str.parse().unwrap_or(0);
        Ok(StageDependencyEdgeResult::Staged {
            edge_id,
            new_graph_revision: rev,
        })
    } else {
        let error_code = fcall_field_str(arr, 1);
        Err(ServerError::OperationFailed(format!(
            "ff_stage_dependency_edge failed: {error_code}"
        )))
    }
}

fn parse_apply_dependency_result(
    raw: &Value,
) -> Result<ApplyDependencyToChildResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_apply_dependency_to_child: expected Array".into())),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_apply_dependency_to_child: bad status code".into())),
    };
    if status == 1 {
        let sub = fcall_field_str(arr, 1);
        if sub == "ALREADY_APPLIED" || sub == "already_applied" {
            Ok(ApplyDependencyToChildResult::AlreadyApplied)
        } else {
            // OK status — field at index 2 is unsatisfied count
            let count_str = fcall_field_str(arr, 2);
            let count: u32 = count_str.parse().unwrap_or(0);
            Ok(ApplyDependencyToChildResult::Applied {
                unsatisfied_count: count,
            })
        }
    } else {
        let error_code = fcall_field_str(arr, 1);
        Err(ServerError::OperationFailed(format!(
            "ff_apply_dependency_to_child failed: {error_code}"
        )))
    }
}

fn parse_deliver_signal_result(
    raw: &Value,
    signal_id: &SignalId,
) -> Result<DeliverSignalResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_deliver_signal: expected Array".into())),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_deliver_signal: bad status code".into())),
    };
    if status == 1 {
        let sub = fcall_field_str(arr, 1);
        if sub == "DUPLICATE" {
            // ok_duplicate(existing_signal_id) → {1, "DUPLICATE", existing_signal_id}
            let existing_str = fcall_field_str(arr, 2);
            let existing_id = SignalId::parse(&existing_str).unwrap_or_else(|_| signal_id.clone());
            Ok(DeliverSignalResult::Duplicate {
                existing_signal_id: existing_id,
            })
        } else {
            // ok(signal_id, effect) → {1, "OK", signal_id, effect}
            let effect = fcall_field_str(arr, 3);
            Ok(DeliverSignalResult::Accepted {
                signal_id: signal_id.clone(),
                effect,
            })
        }
    } else {
        let error_code = fcall_field_str(arr, 1);
        Err(ServerError::OperationFailed(format!(
            "ff_deliver_signal failed: {error_code}"
        )))
    }
}

fn parse_change_priority_result(
    raw: &Value,
    execution_id: &ExecutionId,
) -> Result<ChangePriorityResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_change_priority: expected Array".into())),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_change_priority: bad status code".into())),
    };
    if status == 1 {
        Ok(ChangePriorityResult::Changed {
            execution_id: execution_id.clone(),
        })
    } else {
        let error_code = fcall_field_str(arr, 1);
        Err(ServerError::OperationFailed(format!(
            "ff_change_priority failed: {error_code}"
        )))
    }
}

fn parse_replay_result(raw: &Value) -> Result<ReplayExecutionResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_replay_execution: expected Array".into())),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_replay_execution: bad status code".into())),
    };
    if status == 1 {
        // ok("0") for normal replay, ok(N) for skipped flow member
        let unsatisfied = fcall_field_str(arr, 2);
        let ps = if unsatisfied == "0" {
            PublicState::Waiting
        } else {
            PublicState::WaitingChildren
        };
        Ok(ReplayExecutionResult::Replayed { public_state: ps })
    } else {
        let error_code = fcall_field_str(arr, 1);
        Err(ServerError::OperationFailed(format!(
            "ff_replay_execution failed: {error_code}"
        )))
    }
}

/// Extract a string from an FCALL result array at the given index.
fn fcall_field_str(arr: &[Result<Value, ferriskey::Error>], index: usize) -> String {
    match arr.get(index) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        Some(Ok(Value::Int(n))) => n.to_string(),
        _ => String::new(),
    }
}

/// Parse ff_report_usage_and_check result.
/// Standard format: {1, "OK"}, {1, "SOFT_BREACH", dim, current, limit},
///                  {1, "HARD_BREACH", dim, current, limit}, {1, "ALREADY_APPLIED"}
fn parse_report_usage_result(raw: &Value) -> Result<ReportUsageResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_report_usage_and_check: expected Array".into())),
    };
    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(ServerError::Script(
                "ff_report_usage_and_check: expected Int status code".into(),
            ))
        }
    };
    if status_code != 1 {
        let error_code = fcall_field_str(arr, 1);
        return Err(ServerError::OperationFailed(format!(
            "ff_report_usage_and_check failed: {error_code}"
        )));
    }
    let sub_status = fcall_field_str(arr, 1);
    match sub_status.as_str() {
        "OK" => Ok(ReportUsageResult::Ok),
        "ALREADY_APPLIED" => Ok(ReportUsageResult::AlreadyApplied),
        "SOFT_BREACH" => {
            let dim = fcall_field_str(arr, 2);
            let current: u64 = fcall_field_str(arr, 3).parse().unwrap_or(0);
            let limit: u64 = fcall_field_str(arr, 4).parse().unwrap_or(0);
            Ok(ReportUsageResult::SoftBreach { dimension: dim, current_usage: current, soft_limit: limit })
        }
        "HARD_BREACH" => {
            let dim = fcall_field_str(arr, 2);
            let current: u64 = fcall_field_str(arr, 3).parse().unwrap_or(0);
            let limit: u64 = fcall_field_str(arr, 4).parse().unwrap_or(0);
            Ok(ReportUsageResult::HardBreach {
                dimension: dim,
                current_usage: current,
                hard_limit: limit,
            })
        }
        _ => Err(ServerError::OperationFailed(format!(
            "ff_report_usage_and_check: unknown sub-status: {sub_status}"
        ))),
    }
}

fn parse_revoke_lease_result(raw: &Value) -> Result<RevokeLeaseResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_revoke_lease: expected Array".into())),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_revoke_lease: bad status code".into())),
    };
    if status == 1 {
        let sub = fcall_field_str(arr, 1);
        if sub == "ALREADY_SATISFIED" {
            let reason = fcall_field_str(arr, 2);
            Ok(RevokeLeaseResult::AlreadySatisfied { reason })
        } else {
            let lid = fcall_field_str(arr, 2);
            let epoch = fcall_field_str(arr, 3);
            Ok(RevokeLeaseResult::Revoked {
                lease_id: lid,
                lease_epoch: epoch,
            })
        }
    } else {
        let error_code = fcall_field_str(arr, 1);
        Err(ServerError::OperationFailed(format!(
            "ff_revoke_lease failed: {error_code}"
        )))
    }
}

/// Detect Valkey errors indicating the Lua function library is not loaded.
///
/// After a failover, the new primary may not have the library if replication
/// was incomplete. Valkey returns `ERR Function not loaded` for FCALL calls
/// targeting missing functions.
fn is_function_not_loaded(e: &ferriskey::Error) -> bool {
    if matches!(e.kind(), ferriskey::ErrorKind::NoScriptError) {
        return true;
    }
    e.detail()
        .map(|d| {
            d.contains("Function not loaded")
                || d.contains("No matching function")
                || d.contains("function not found")
        })
        .unwrap_or(false)
        || e.to_string().contains("Function not loaded")
}

/// Free-function form of [`Server::fcall_with_reload`] — callable from
/// background tasks that own a cloned `Client` but no `&Server`.
async fn fcall_with_reload_on_client(
    client: &Client,
    function: &str,
    keys: &[&str],
    args: &[&str],
) -> Result<Value, ServerError> {
    match client.fcall(function, keys, args).await {
        Ok(v) => Ok(v),
        Err(e) if is_function_not_loaded(&e) => {
            tracing::warn!(function, "Lua library not found on server, reloading");
            ff_script::loader::ensure_library(client)
                .await
                .map_err(ServerError::LibraryLoad)?;
            client
                .fcall(function, keys, args)
                .await
                .map_err(ServerError::Valkey)
        }
        Err(e) => Err(ServerError::Valkey(e)),
    }
}

/// Build the `ff_cancel_execution` KEYS (21) and ARGV (5) by pre-reading
/// dynamic fields from `exec_core`. Shared by [`Server::cancel_execution`]
/// and the async cancel_flow member-dispatch path.
async fn build_cancel_execution_fcall(
    client: &Client,
    partition_config: &PartitionConfig,
    args: &CancelExecutionArgs,
) -> Result<(Vec<String>, Vec<String>), ServerError> {
    let partition = execution_partition(&args.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &args.execution_id);
    let idx = IndexKeys::new(&partition);

    let lane_str: Option<String> = client
        .hget(&ctx.core(), "lane_id")
        .await
        .map_err(|e| ServerError::ValkeyContext { source: e, context: "HGET lane_id".into() })?;
    let lane = LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()));

    let dyn_fields: Vec<Option<String>> = client
        .cmd("HMGET")
        .arg(ctx.core())
        .arg("current_attempt_index")
        .arg("current_waitpoint_id")
        .arg("current_worker_instance_id")
        .execute()
        .await
        .map_err(|e| ServerError::ValkeyContext { source: e, context: "HMGET cancel pre-read".into() })?;

    let att_idx_val = dyn_fields.first()
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let att_idx = AttemptIndex::new(att_idx_val);
    let wp_id_str = dyn_fields.get(1).and_then(|v| v.as_ref()).cloned().unwrap_or_default();
    let wp_id = if wp_id_str.is_empty() {
        WaitpointId::new()
    } else {
        WaitpointId::parse(&wp_id_str).unwrap_or_else(|_| WaitpointId::new())
    };
    let wiid_str = dyn_fields.get(2).and_then(|v| v.as_ref()).cloned().unwrap_or_default();
    let wiid = WorkerInstanceId::new(&wiid_str);

    let keys: Vec<String> = vec![
        ctx.core(),                              // 1
        ctx.attempt_hash(att_idx),               // 2
        ctx.stream_meta(att_idx),                // 3
        ctx.lease_current(),                     // 4
        ctx.lease_history(),                     // 5
        idx.lease_expiry(),                      // 6
        idx.worker_leases(&wiid),                // 7
        ctx.suspension_current(),                // 8
        ctx.waitpoint(&wp_id),                   // 9
        ctx.waitpoint_condition(&wp_id),         // 10
        idx.suspension_timeout(),                // 11
        idx.lane_terminal(&lane),                // 12
        idx.attempt_timeout(),                   // 13
        idx.execution_deadline(),                // 14
        idx.lane_eligible(&lane),                // 15
        idx.lane_delayed(&lane),                 // 16
        idx.lane_blocked_dependencies(&lane),    // 17
        idx.lane_blocked_budget(&lane),          // 18
        idx.lane_blocked_quota(&lane),           // 19
        idx.lane_blocked_route(&lane),           // 20
        idx.lane_blocked_operator(&lane),        // 21
    ];
    let argv: Vec<String> = vec![
        args.execution_id.to_string(),
        args.reason.clone(),
        args.source.to_string(),
        args.lease_id.as_ref().map(|l| l.to_string()).unwrap_or_default(),
        args.lease_epoch.as_ref().map(|e| e.to_string()).unwrap_or_default(),
    ];
    Ok((keys, argv))
}

/// Cancel a single member execution from a cancel_flow dispatch context.
/// Parses the flow-member EID string, builds the FCALL via the shared helper,
/// and executes with the same reload-on-failover semantics as the inline path.
async fn cancel_member_execution(
    client: &Client,
    partition_config: &PartitionConfig,
    eid_str: &str,
    reason: &str,
    now: TimestampMs,
) -> Result<(), ServerError> {
    let execution_id = ExecutionId::parse(eid_str)
        .map_err(|e| ServerError::InvalidInput(format!("bad execution_id '{eid_str}': {e}")))?;
    let args = CancelExecutionArgs {
        execution_id: execution_id.clone(),
        reason: reason.to_owned(),
        source: CancelSource::OperatorOverride,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now,
    };
    let (keys, argv) = build_cancel_execution_fcall(client, partition_config, &args).await?;
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let raw = fcall_with_reload_on_client(client, "ff_cancel_execution", &key_refs, &arg_refs).await?;
    parse_cancel_result(&raw, &execution_id).map(|_| ())
}

fn parse_reset_budget_result(raw: &Value) -> Result<ResetBudgetResult, ServerError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => return Err(ServerError::Script("ff_reset_budget: expected Array".into())),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => return Err(ServerError::Script("ff_reset_budget: bad status code".into())),
    };
    if status == 1 {
        let next_str = fcall_field_str(arr, 2);
        let next_ms: i64 = next_str.parse().unwrap_or(0);
        Ok(ResetBudgetResult::Reset {
            next_reset_at: TimestampMs::from_millis(next_ms),
        })
    } else {
        let error_code = fcall_field_str(arr, 1);
        Err(ServerError::OperationFailed(format!(
            "ff_reset_budget failed: {error_code}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferriskey::ErrorKind;

    fn mk_fk_err(kind: ErrorKind) -> ferriskey::Error {
        ferriskey::Error::from((kind, "synthetic"))
    }

    #[test]
    fn is_retryable_valkey_variant_uses_kind_table() {
        assert!(ServerError::Valkey(mk_fk_err(ErrorKind::IoError)).is_retryable());
        assert!(ServerError::Valkey(mk_fk_err(ErrorKind::FatalSendError)).is_retryable());
        assert!(ServerError::Valkey(mk_fk_err(ErrorKind::TryAgain)).is_retryable());
        assert!(ServerError::Valkey(mk_fk_err(ErrorKind::BusyLoadingError)).is_retryable());
        assert!(ServerError::Valkey(mk_fk_err(ErrorKind::ClusterDown)).is_retryable());

        assert!(!ServerError::Valkey(mk_fk_err(ErrorKind::FatalReceiveError)).is_retryable());
        assert!(!ServerError::Valkey(mk_fk_err(ErrorKind::AuthenticationFailed)).is_retryable());
        assert!(!ServerError::Valkey(mk_fk_err(ErrorKind::NoScriptError)).is_retryable());
        assert!(!ServerError::Valkey(mk_fk_err(ErrorKind::Moved)).is_retryable());
        assert!(!ServerError::Valkey(mk_fk_err(ErrorKind::Ask)).is_retryable());
        assert!(!ServerError::Valkey(mk_fk_err(ErrorKind::ReadOnly)).is_retryable());
    }

    #[test]
    fn is_retryable_valkey_context_uses_kind_table() {
        let err = ServerError::ValkeyContext {
            source: mk_fk_err(ErrorKind::IoError),
            context: "HGET test".into(),
        };
        assert!(err.is_retryable());

        let err = ServerError::ValkeyContext {
            source: mk_fk_err(ErrorKind::AuthenticationFailed),
            context: "auth".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn is_retryable_library_load_delegates_to_inner_kind() {
        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::Valkey(
            mk_fk_err(ErrorKind::IoError),
        ));
        assert!(err.is_retryable());

        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::Valkey(
            mk_fk_err(ErrorKind::AuthenticationFailed),
        ));
        assert!(!err.is_retryable());

        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::VersionMismatch {
            expected: "1".into(),
            got: "2".into(),
        });
        assert!(!err.is_retryable());
    }

    #[test]
    fn is_retryable_business_logic_variants_are_false() {
        assert!(!ServerError::NotFound("x".into()).is_retryable());
        assert!(!ServerError::InvalidInput("x".into()).is_retryable());
        assert!(!ServerError::OperationFailed("x".into()).is_retryable());
        assert!(!ServerError::Script("x".into()).is_retryable());
        assert!(!ServerError::PartitionMismatch("x".into()).is_retryable());
    }

    #[test]
    fn valkey_kind_delegates_through_library_load() {
        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::Valkey(
            mk_fk_err(ErrorKind::ClusterDown),
        ));
        assert_eq!(err.valkey_kind(), Some(ErrorKind::ClusterDown));

        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::VersionMismatch {
            expected: "1".into(),
            got: "2".into(),
        });
        assert_eq!(err.valkey_kind(), None);
    }
}
