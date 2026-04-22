use ff_core::partition::PartitionConfig;
use ff_core::types::LaneId;
use ff_engine::EngineConfig;
use std::time::Duration;

/// Server configuration, loaded from environment variables.
pub struct ServerConfig {
    /// Valkey host. Default: `"localhost"`.
    pub host: String,
    /// Valkey port. Default: `6379`.
    pub port: u16,
    /// Enable TLS for Valkey connections.
    pub tls: bool,
    /// Enable Valkey cluster mode.
    pub cluster: bool,
    /// Partition counts (execution/flow/budget/quota).
    pub partition_config: PartitionConfig,
    /// Lanes to manage. Default: `["default"]`.
    pub lanes: Vec<LaneId>,
    /// Listen address for the API surface. Default: `"0.0.0.0:9090"`.
    pub listen_addr: String,
    /// Scanner intervals and engine config.
    pub engine_config: EngineConfig,
    /// Skip library loading (for tests where TestCluster already loaded it).
    pub skip_library_load: bool,
    /// Allowed CORS origins. `["*"]` means permissive (all origins).
    pub cors_origins: Vec<String>,
    /// Shared-secret API token. If set, all requests except GET /healthz must
    /// include `Authorization: Bearer <token>`. If unset, auth is disabled.
    pub api_token: Option<String>,
    /// Hex-encoded secret used to sign waitpoint HMAC tokens (RFC-004
    /// §Waitpoint Security). Required on boot; the server refuses to start
    /// without it so multi-tenant signal authentication is never silently
    /// disabled. Recommended length: 64 hex chars (32 bytes).
    pub waitpoint_hmac_secret: String,
    /// Grace window during which tokens signed by the previous kid remain
    /// accepted after rotation. Tokens already in flight survive operator
    /// rotation; operators tighten this for sensitive tenants. Default 24h.
    pub waitpoint_hmac_grace_ms: u64,
    /// Maximum concurrent stream-op callers (`read_attempt_stream` +
    /// `tail_attempt_stream` combined). Each caller holds one semaphore
    /// permit for the duration of its Valkey round-trip(s); contention
    /// surfaces as HTTP 429 at the REST boundary.
    ///
    /// Shared bound for both read and tail because both run on the same
    /// dedicated `tail_client` (see `Server.tail_client`) — a big
    /// 10_000-frame XRANGE reply can head-of-line the mux just as badly
    /// as a long `XREAD BLOCK`, so they should share fairness accounting.
    ///
    /// Default `64`. Set below the server's request-concurrency budget
    /// so stream ops cannot starve other routes. Env var:
    /// `FF_MAX_CONCURRENT_STREAM_OPS` (preferred) or legacy
    /// `FF_MAX_CONCURRENT_TAIL` (accepted during the R4 rename; both
    /// valid for at least one release).
    pub max_concurrent_stream_ops: u32,
}

impl ServerConfig {
    /// Load configuration from environment variables.
    ///
    /// The table below enumerates every variable this function reads. It is
    /// the canonical rustdoc mirror of the identical table in the top-level
    /// `README.md`. `docs/DEPLOYMENT.md` references these names.
    ///
    /// **Maintenance contract:** every env var key this function consumes —
    /// whether via a direct `std::env::var(...)` call or through the
    /// `env_or` / `env_bool` / `env_u16` / `env_u16_positive` / `env_u64` /
    /// `env_u32_positive` helpers — MUST have a row here. When you add,
    /// rename, or remove an env var, update this table in the same commit.
    /// There is no compile-time check — reviewers enforce it. Legacy
    /// aliases accepted during a rename window (e.g. `FF_MAX_CONCURRENT_TAIL`)
    /// should be listed alongside their preferred name.
    ///
    /// | Variable | Default | Description |
    /// |----------|---------|-------------|
    /// | `FF_WAITPOINT_HMAC_SECRET` | *required* | Hex-encoded HMAC signing secret for waitpoint tokens (RFC-004 §Waitpoint Security). Even-length hex; 64 chars (32 bytes) recommended. Boot fails without it. |
    /// | `FF_HOST` | `localhost` | Valkey host |
    /// | `FF_PORT` | `6379` | Valkey port |
    /// | `FF_TLS` | `false` | Enable TLS for Valkey (`1` or `true`) |
    /// | `FF_CLUSTER` | `false` | Enable Valkey cluster mode (`1` or `true`) |
    /// | `FF_LISTEN_ADDR` | `0.0.0.0:9090` | API listen address |
    /// | `FF_LANES` | `default` | Comma-separated lane names; at least one non-empty lane required |
    /// | `FF_FLOW_PARTITIONS` | `256` | Flow partition count — authoritative; under RFC-011 hash-tag co-location, exec keys also route here |
    /// | `FF_BUDGET_PARTITIONS` | `32` | Budget partition count |
    /// | `FF_QUOTA_PARTITIONS` | `32` | Quota partition count |
    /// | `FF_CORS_ORIGINS` | `*` | Comma-separated CORS origins (`*` = permissive). Empty string is rejected; unset the var to get the default. |
    /// | `FF_API_TOKEN` | *(none)* | Shared-secret Bearer token. If set, all non-`/healthz` requests require it. |
    /// | `FF_WAITPOINT_HMAC_GRACE_MS` | `86400000` | Grace window (ms) during which tokens signed by the previous kid remain accepted after rotation. Default 24h. |
    /// | `FF_MAX_CONCURRENT_STREAM_OPS` | `64` | Shared semaphore bound for `read_attempt_stream` + `tail_attempt_stream`. Legacy `FF_MAX_CONCURRENT_TAIL` is accepted as a fallback; if both are set, the new name wins. |
    /// | `FF_MAX_CONCURRENT_TAIL` | *(legacy)* | Deprecated alias for `FF_MAX_CONCURRENT_STREAM_OPS`; accepted during the R4 rename window. |
    /// | `FF_LEASE_EXPIRY_INTERVAL_MS` | `1500` | Lease-expiry scanner interval |
    /// | `FF_DELAYED_PROMOTER_INTERVAL_MS` | `750` | Delayed-promoter scanner interval |
    /// | `FF_INDEX_RECONCILER_INTERVAL_S` | `45` | Index reconciler interval |
    /// | `FF_ATTEMPT_TIMEOUT_INTERVAL_S` | `2` | Attempt-timeout scanner interval |
    /// | `FF_SUSPENSION_TIMEOUT_INTERVAL_S` | `2` | Suspension-timeout scanner interval |
    /// | `FF_PENDING_WP_EXPIRY_INTERVAL_S` | `5` | Pending-waitpoint expiry scanner interval |
    /// | `FF_RETENTION_TRIMMER_INTERVAL_S` | `60` | Retention-trimmer scanner interval |
    /// | `FF_BUDGET_RESET_INTERVAL_S` | `15` | Budget-reset scanner interval |
    /// | `FF_BUDGET_RECONCILER_INTERVAL_S` | `30` | Budget reconciler interval |
    /// | `FF_QUOTA_RECONCILER_INTERVAL_S` | `30` | Quota reconciler interval |
    /// | `FF_UNBLOCK_INTERVAL_S` | `5` | Unblock scanner interval |
    /// | `FF_DEPENDENCY_RECONCILER_INTERVAL_S` | `15` | DAG dependency reconciler interval (safety net behind push-based promotion) |
    /// | `FF_FLOW_PROJECTOR_INTERVAL_S` | `15` | Flow projector scanner interval |
    /// | `FF_EXECUTION_DEADLINE_INTERVAL_S` | `5` | Execution-deadline scanner interval |
    /// | `FF_CANCEL_RECONCILER_INTERVAL_S` | `15` | Cancel reconciler scanner interval |
    pub fn from_env() -> Result<Self, ConfigError> {
        let host = env_or("FF_HOST", "localhost");
        let port = env_u16("FF_PORT", 6379)?;
        let tls = env_bool("FF_TLS");
        let cluster = env_bool("FF_CLUSTER");
        let listen_addr = env_or("FF_LISTEN_ADDR", "0.0.0.0:9090");
        // FF_CORS_ORIGINS contract:
        //   unset      → default "*" (permissive)
        //   "*"        → permissive
        //   "a,b,c"    → explicit allowlist
        //   ""         → hard error. An empty explicit value almost always
        //                means "I tried to unset it" which a blank env var
        //                does not do. We refuse to guess and make the
        //                operator's intent explicit.
        let cors_raw = std::env::var("FF_CORS_ORIGINS");
        let cors_source = match &cors_raw {
            Ok(s) if s.is_empty() => {
                return Err(ConfigError::InvalidValue {
                    var: "FF_CORS_ORIGINS".to_owned(),
                    message: "FF_CORS_ORIGINS is set but empty; \
                              unset it to default to \"*\", or pass \"*\" explicitly, \
                              or pass a non-empty comma-separated origin list"
                        .to_owned(),
                });
            }
            Ok(s) => s.clone(),
            Err(_) => "*".to_owned(),
        };
        let cors_origins: Vec<String> = cors_source
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();

        let api_token = std::env::var("FF_API_TOKEN").ok().filter(|s| !s.is_empty());

        // Waitpoint HMAC secret. Required on boot — refuse to start without
        // it so multi-tenant signal authentication can never be silently
        // disabled. Validate hex shape eagerly; empty strings and bad hex
        // produce a configuration error, not a runtime crash later.
        let waitpoint_hmac_secret = std::env::var("FF_WAITPOINT_HMAC_SECRET")
            .map_err(|_| ConfigError::InvalidValue {
                var: "FF_WAITPOINT_HMAC_SECRET".to_owned(),
                message:
                    "required: hex-encoded HMAC signing secret for waitpoint tokens \
                     (RFC-004 §Waitpoint Security); suggested 64 hex chars (32 bytes)"
                        .to_owned(),
            })?;
        if waitpoint_hmac_secret.is_empty() {
            return Err(ConfigError::InvalidValue {
                var: "FF_WAITPOINT_HMAC_SECRET".to_owned(),
                message: "must not be empty".to_owned(),
            });
        }
        if waitpoint_hmac_secret.len() % 2 != 0
            || !waitpoint_hmac_secret.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(ConfigError::InvalidValue {
                var: "FF_WAITPOINT_HMAC_SECRET".to_owned(),
                message: "must be an even-length hex string (0-9a-fA-F)".to_owned(),
            });
        }
        let waitpoint_hmac_grace_ms = env_u64("FF_WAITPOINT_HMAC_GRACE_MS", 86_400_000)?;
        // Preferred env var: FF_MAX_CONCURRENT_STREAM_OPS. Legacy
        // FF_MAX_CONCURRENT_TAIL is accepted for one release to avoid
        // breaking existing deployments mid-rename (R4 unified the two
        // stream-op clients on one permit pool). If both are set, the
        // new name wins.
        let max_concurrent_stream_ops = match std::env::var("FF_MAX_CONCURRENT_STREAM_OPS") {
            Ok(_) => env_u32_positive("FF_MAX_CONCURRENT_STREAM_OPS", 64)?,
            Err(_) => env_u32_positive("FF_MAX_CONCURRENT_TAIL", 64)?,
        };

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
            // RFC-011: num_execution_partitions retired; exec keys co-locate on
            // {fp:N}. FF_FLOW_PARTITIONS is the canonical env var.
            num_flow_partitions: env_u16_positive("FF_FLOW_PARTITIONS", 256)?,
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
        // Raised from 1s (pre-Batch-C) to 15s now that push-based DAG
        // promotion is primary. The reconciler is a safety net post-
        // completion-listener; see ff-engine docs on
        // `dependency_reconciler_interval`.
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
            cancel_reconciler_interval: Duration::from_secs(
                env_u64("FF_CANCEL_RECONCILER_INTERVAL_S", 15)?
            ),
        };

        Ok(Self {
            host,
            port,
            tls,
            cluster,
            partition_config,
            lanes,
            listen_addr,
            engine_config,
            skip_library_load: false,
            cors_origins,
            api_token,
            waitpoint_hmac_secret,
            waitpoint_hmac_grace_ms,
            max_concurrent_stream_ops,
        })
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        let lanes = vec![LaneId::new("default")];
        let partition_config = PartitionConfig::default();
        Self {
            host: "localhost".into(),
            port: 6379,
            tls: false,
            cluster: false,
            partition_config,
            lanes: lanes.clone(),
            listen_addr: "0.0.0.0:9090".into(),
            engine_config: EngineConfig {
                partition_config,
                lanes,
                ..Default::default()
            },
            skip_library_load: false,
            cors_origins: vec!["*".to_owned()],
            api_token: None,
            // Deterministic dev/test secret. Production deployments MUST
            // override via FF_WAITPOINT_HMAC_SECRET (ServerConfig::from_env
            // requires it), so this default only applies to unit tests and
            // TestCluster fixtures that skip env validation.
            waitpoint_hmac_secret:
                "0000000000000000000000000000000000000000000000000000000000000000"
                    .to_owned(),
            waitpoint_hmac_grace_ms: 86_400_000,
            max_concurrent_stream_ops: 64,
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

fn env_u32_positive(key: &str, default: u32) -> Result<u32, ConfigError> {
    let val = match std::env::var(key) {
        Ok(v) => v.parse::<u32>().map_err(|_| ConfigError::InvalidValue {
            var: key.to_owned(),
            message: format!("expected u32, got '{v}'"),
        })?,
        Err(_) => default,
    };
    if val == 0 {
        return Err(ConfigError::InvalidValue {
            var: key.to_owned(),
            message: "must be > 0 (semaphore size)".to_owned(),
        });
    }
    Ok(val)
}
