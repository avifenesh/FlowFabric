use ff_core::partition::PartitionConfig;
use ff_core::types::LaneId;
use ff_engine::EngineConfig;
use std::time::Duration;

/// Server configuration, loaded from environment variables.
pub struct ServerConfig {
    /// Valkey connection URL.
    pub valkey_url: String,
    /// Whether to use TLS for Valkey connections.
    pub tls: bool,
    /// Partition counts (execution/flow/budget/quota).
    pub partition_config: PartitionConfig,
    /// Lanes to manage. Default: `["default"]`.
    pub lanes: Vec<LaneId>,
    /// Listen address for the API surface. Default: `"0.0.0.0:9090"`.
    pub listen_addr: String,
    /// Scanner intervals and engine config.
    pub engine_config: EngineConfig,
}

impl ServerConfig {
    /// Load configuration from environment variables.
    ///
    /// | Variable | Default | Description |
    /// |----------|---------|-------------|
    /// | `FF_VALKEY_URL` | `valkey://localhost:6379` | Valkey connection URL |
    /// | `FF_TLS` | `false` | Enable TLS (`1` or `true`) |
    /// | `FF_LISTEN_ADDR` | `0.0.0.0:9090` | API listen address |
    /// | `FF_LANES` | `default` | Comma-separated lane names |
    /// | `FF_EXEC_PARTITIONS` | `256` | Execution partition count |
    /// | `FF_FLOW_PARTITIONS` | `64` | Flow partition count |
    /// | `FF_BUDGET_PARTITIONS` | `32` | Budget partition count |
    /// | `FF_QUOTA_PARTITIONS` | `32` | Quota partition count |
    /// | `FF_LEASE_EXPIRY_INTERVAL_MS` | `1500` | Lease expiry scanner interval |
    /// | `FF_DELAYED_PROMOTER_INTERVAL_MS` | `750` | Delayed promoter interval |
    /// | `FF_INDEX_RECONCILER_INTERVAL_S` | `45` | Index reconciler interval |
    pub fn from_env() -> Result<Self, ConfigError> {
        let valkey_url = env_or("FF_VALKEY_URL", "valkey://localhost:6379");
        let tls = env_bool("FF_TLS");
        let listen_addr = env_or("FF_LISTEN_ADDR", "0.0.0.0:9090");

        let lanes: Vec<LaneId> = env_or("FF_LANES", "default")
            .split(',')
            .map(|s| LaneId::new(s.trim()))
            .filter(|l| !l.as_str().is_empty())
            .collect();
        if lanes.is_empty() {
            return Err(ConfigError::InvalidValue {
                var: "FF_LANES".to_owned(),
                message: "at least one non-empty lane name is required".to_owned(),
            });
        }

        let partition_config = PartitionConfig {
            num_execution_partitions: env_u16_positive("FF_EXEC_PARTITIONS", 256)?,
            num_flow_partitions: env_u16_positive("FF_FLOW_PARTITIONS", 64)?,
            num_budget_partitions: env_u16_positive("FF_BUDGET_PARTITIONS", 32)?,
            num_quota_partitions: env_u16_positive("FF_QUOTA_PARTITIONS", 32)?,
        };

        let lease_expiry_interval =
            Duration::from_millis(env_u64("FF_LEASE_EXPIRY_INTERVAL_MS", 1500)?);
        let delayed_promoter_interval =
            Duration::from_millis(env_u64("FF_DELAYED_PROMOTER_INTERVAL_MS", 750)?);
        let index_reconciler_interval =
            Duration::from_secs(env_u64("FF_INDEX_RECONCILER_INTERVAL_S", 45)?);
        let attempt_timeout_interval =
            Duration::from_secs(env_u64("FF_ATTEMPT_TIMEOUT_INTERVAL_S", 2)?);
        let suspension_timeout_interval =
            Duration::from_secs(env_u64("FF_SUSPENSION_TIMEOUT_INTERVAL_S", 2)?);
        let pending_wp_expiry_interval =
            Duration::from_secs(env_u64("FF_PENDING_WP_EXPIRY_INTERVAL_S", 5)?);
        let retention_trimmer_interval =
            Duration::from_secs(env_u64("FF_RETENTION_TRIMMER_INTERVAL_S", 60)?);
        let budget_reset_interval =
            Duration::from_secs(env_u64("FF_BUDGET_RESET_INTERVAL_S", 15)?);
        let budget_reconciler_interval =
            Duration::from_secs(env_u64("FF_BUDGET_RECONCILER_INTERVAL_S", 30)?);
        let quota_reconciler_interval =
            Duration::from_secs(env_u64("FF_QUOTA_RECONCILER_INTERVAL_S", 30)?);
        let unblock_interval =
            Duration::from_secs(env_u64("FF_UNBLOCK_INTERVAL_S", 5)?);
        let dependency_reconciler_interval =
            Duration::from_secs(env_u64("FF_DEPENDENCY_RECONCILER_INTERVAL_S", 15)?);

        let engine_config = EngineConfig {
            partition_config,
            lanes: lanes.clone(),
            lease_expiry_interval,
            delayed_promoter_interval,
            index_reconciler_interval,
            attempt_timeout_interval,
            suspension_timeout_interval,
            pending_wp_expiry_interval,
            retention_trimmer_interval,
            budget_reset_interval,
            budget_reconciler_interval,
            quota_reconciler_interval,
            unblock_interval,
            dependency_reconciler_interval,
            flow_projector_interval: Duration::from_secs(
                env_u64("FF_FLOW_PROJECTOR_INTERVAL_S", 15)?
            ),
            execution_deadline_interval: Duration::from_secs(
                env_u64("FF_EXECUTION_DEADLINE_INTERVAL_S", 5)?
            ),
        };

        Ok(Self {
            valkey_url,
            tls,
            partition_config,
            lanes,
            listen_addr,
            engine_config,
        })
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        let lanes = vec![LaneId::new("default")];
        let partition_config = PartitionConfig::default();
        Self {
            valkey_url: "valkey://localhost:6379".into(),
            tls: false,
            partition_config,
            lanes: lanes.clone(),
            listen_addr: "0.0.0.0:9090".into(),
            engine_config: EngineConfig {
                partition_config,
                lanes,
                ..Default::default()
            },
        }
    }
}

/// Configuration error.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid value for {var}: {message}")]
    InvalidValue { var: String, message: String },
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn env_bool(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_u16(key: &str, default: u16) -> Result<u16, ConfigError> {
    match std::env::var(key) {
        Ok(v) => v.parse().map_err(|_| ConfigError::InvalidValue {
            var: key.to_owned(),
            message: format!("expected u16, got '{v}'"),
        }),
        Err(_) => Ok(default),
    }
}

/// Like env_u16 but rejects 0 (for partition counts that are used as divisors).
fn env_u16_positive(key: &str, default: u16) -> Result<u16, ConfigError> {
    let val = env_u16(key, default)?;
    if val == 0 {
        return Err(ConfigError::InvalidValue {
            var: key.to_owned(),
            message: "must be > 0 (used as divisor in partition math)".to_owned(),
        });
    }
    Ok(val)
}

fn env_u64(key: &str, default: u64) -> Result<u64, ConfigError> {
    match std::env::var(key) {
        Ok(v) => v.parse().map_err(|_| ConfigError::InvalidValue {
            var: key.to_owned(),
            message: format!("expected u64, got '{v}'"),
        }),
        Err(_) => Ok(default),
    }
}
