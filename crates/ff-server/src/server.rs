use std::collections::HashMap;

use ferriskey::{Client, Value};
use ff_core::contracts::{
    CancelExecutionArgs, CancelExecutionResult, CreateExecutionArgs, CreateExecutionResult,
};
use ff_core::keys::{self, ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_engine::Engine;

use crate::config::ServerConfig;

/// FlowFabric server — connects everything together.
///
/// Manages the Valkey connection, Lua library loading, background scanners,
/// and provides a minimal API for Phase 1.
pub struct Server {
    client: Client,
    engine: Engine,
    config: ServerConfig,
}

/// Server error type.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("valkey: {0}")]
    Valkey(String),
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("library load: {0}")]
    LibraryLoad(String),
    #[error("partition mismatch: {0}")]
    PartitionMismatch(String),
    #[error("script: {0}")]
    Script(String),
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
        // Step 1: Connect to Valkey
        tracing::info!(url = %config.valkey_url, "connecting to Valkey");
        let client = Client::connect(&config.valkey_url)
            .await
            .map_err(|e| ServerError::Valkey(format!("connect failed: {e}")))?;

        // Verify connectivity
        let pong: String = client
            .cmd("PING")
            .execute()
            .await
            .map_err(|e| ServerError::Valkey(format!("PING failed: {e}")))?;
        if pong != "PONG" {
            return Err(ServerError::Valkey(format!(
                "unexpected PING response: {pong}"
            )));
        }
        tracing::info!("Valkey connection established");

        // Step 2: Validate or create partition config
        validate_or_create_partition_config(&client, &config.partition_config).await?;

        // Step 3: Load Lua library
        tracing::info!("loading flowfabric Lua library");
        ff_script::loader::ensure_library(&client)
            .await
            .map_err(|e| ServerError::LibraryLoad(e.to_string()))?;

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
        })
    }

    /// Get a reference to the ferriskey client.
    pub fn client(&self) -> &Client {
        &self.client
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
        let idem_key = match &args.idempotency_key {
            Some(k) if !k.is_empty() => {
                keys::idempotency_key(args.namespace.as_str(), k)
            }
            _ => String::new(),
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
            args.policy_json.clone(),                // 7
            String::from_utf8_lossy(&args.input_payload).into_owned(), // 8
            delay_str,                               // 9
            args.idempotency_key.as_ref()
                .map(|_| "86400000".to_string())
                .unwrap_or_default(),                // 10 dedup_ttl_ms
            tags_json,                               // 11
            String::new(),                           // 12 execution_deadline_at
            args.partition_id.to_string(),           // 13
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_create_execution", &key_refs, &arg_refs)
            .await
            .map_err(|e| ServerError::Valkey(e.to_string()))?;

        parse_create_result(&raw, &args.execution_id)
    }

    /// Cancel an execution.
    pub async fn cancel_execution(
        &self,
        args: &CancelExecutionArgs,
    ) -> Result<CancelExecutionResult, ServerError> {
        let partition = execution_partition(&args.execution_id, &self.config.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);

        // Determine lane from exec_core for index key construction
        let lane_str: Option<String> = self
            .client
            .hget(&ctx.core(), "lane_id")
            .await
            .map_err(|e| ServerError::Valkey(format!("HGET lane_id: {e}")))?;
        let lane = LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()));

        // Read dynamic fields from exec_core for correct key construction
        let dyn_fields: Vec<Option<String>> = self
            .client
            .cmd("HMGET")
            .arg(ctx.core())
            .arg("current_attempt_index")
            .arg("current_waitpoint_id")
            .arg("current_worker_instance_id")
            .execute()
            .await
            .unwrap_or_default();
        let att_idx_val = dyn_fields.first()
            .and_then(|v| v.as_ref())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let att_idx = AttemptIndex::new(att_idx_val);
        let wp_id_str = dyn_fields.get(1)
            .and_then(|v| v.as_ref())
            .cloned()
            .unwrap_or_default();
        let wp_id = if wp_id_str.is_empty() {
            WaitpointId::new() // placeholder
        } else {
            WaitpointId::parse(&wp_id_str).unwrap_or_else(|_| WaitpointId::new())
        };
        let wiid_str = dyn_fields.get(2)
            .and_then(|v| v.as_ref())
            .cloned()
            .unwrap_or_default();
        let wiid = WorkerInstanceId::new(&wiid_str);

        // KEYS (21): must match lua/execution.lua ff_cancel_execution positional order
        let fcall_keys: Vec<String> = vec![
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

        // ARGV (5): execution_id, reason, source, lease_id, lease_epoch
        let fcall_args: Vec<String> = vec![
            args.execution_id.to_string(),
            args.reason.clone(),
            args.source.clone().unwrap_or_else(|| "operator_override".to_owned()),
            args.lease_id
                .as_ref()
                .map(|l| l.to_string())
                .unwrap_or_default(),
            args.lease_epoch
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_default(),
        ];

        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_cancel_execution", &key_refs, &arg_refs)
            .await
            .map_err(|e| ServerError::Valkey(e.to_string()))?;

        parse_cancel_result(&raw, &args.execution_id)
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
            .map_err(|e| ServerError::Valkey(format!("HGET public_state: {e}")))?;

        match state_str {
            Some(s) => {
                let quoted = format!("\"{s}\"");
                serde_json::from_str(&quoted).map_err(|e| {
                    ServerError::Script(format!(
                        "invalid public_state '{s}' for {execution_id}: {e}"
                    ))
                })
            }
            None => Err(ServerError::Script(format!(
                "execution not found: {execution_id}"
            ))),
        }
    }

    /// Graceful shutdown — stops scanners and waits for them to finish.
    pub async fn shutdown(self) {
        tracing::info!("shutting down FlowFabric server");
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
        .map_err(|e| ServerError::Valkey(format!("HGETALL {key}: {e}")))?;

    if existing.is_empty() {
        // First boot — create the config
        tracing::info!("first boot: creating {key}");
        client
            .hset(&key, "num_execution_partitions", &config.num_execution_partitions.to_string())
            .await
            .map_err(|e| ServerError::Valkey(format!("HSET: {e}")))?;
        client
            .hset(&key, "num_flow_partitions", &config.num_flow_partitions.to_string())
            .await
            .map_err(|e| ServerError::Valkey(format!("HSET: {e}")))?;
        client
            .hset(&key, "num_budget_partitions", &config.num_budget_partitions.to_string())
            .await
            .map_err(|e| ServerError::Valkey(format!("HSET: {e}")))?;
        client
            .hset(&key, "num_quota_partitions", &config.num_quota_partitions.to_string())
            .await
            .map_err(|e| ServerError::Valkey(format!("HSET: {e}")))?;
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
        Err(ServerError::Script(format!(
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
        Err(ServerError::Script(format!(
            "ff_cancel_execution failed: {error_code}"
        )))
    }
}
