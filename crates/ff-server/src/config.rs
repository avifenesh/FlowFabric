use ff_core::partition::PartitionConfig;
use ff_core::types::LaneId;
use ff_engine::EngineConfig;
use std::time::Duration;

/// RFC-017 Stage A: backend family selector. Default `Valkey`. At
/// Stage E4 (v0.8.0) both `Valkey` and `Postgres` are first-class and
/// boot without a dev-override; `BACKEND_STAGE_READY` remains in
/// `ff-server::server` as defence-in-depth for future backend
/// additions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum BackendKind {
    /// Valkey / FCALL backend (production path through v0.7.x).
    #[default]
    Valkey,
    /// Postgres backend. First-class since v0.8.0 (RFC-017 Stage E4).
    Postgres,
    /// SQLite dev-only backend (RFC-023 v0.12.0). Activation requires
    /// `FF_DEV_MODE=1`; the backend refuses to construct otherwise
    /// regardless of entry point (embedded `SqliteBackend::new` or
    /// HTTP `start_sqlite_branch`).
    Sqlite,
}

impl BackendKind {
    /// Stable `&'static str` label matching the backend's
    /// `backend_label()` for metrics dimensioning.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Valkey => "valkey",
            Self::Postgres => "postgres",
            Self::Sqlite => "sqlite",
        }
    }
}

/// RFC-017 Wave 8 Stage E1: Postgres connection parameters carried
/// on [`ServerConfig`] when `backend == BackendKind::Postgres`.
///
/// Unlike the flat Valkey fields (`host` / `port` / `tls` / `cluster`),
/// the Postgres surface is gathered into its own struct because Stage
/// E4 will retire the flat Valkey fields in favour of a sum-typed
/// `BackendConfig` on `ServerConfig`. Keeping the Postgres carrier
/// pre-sum-typed avoids churn when E4 lands.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PostgresServerConfig {
    /// Connection URL (libpq / sqlx shape:
    /// `postgres://user:pass@host:port/db`). Read from `FF_POSTGRES_URL`.
    pub url: String,
    /// Max pool connections. Read from `FF_POSTGRES_POOL_SIZE`,
    /// default `10` (matches sqlx's out-of-box default +
    /// [`ff_core::backend::PostgresConnection`] default).
    pub pool_size: u32,
}

impl PostgresServerConfig {
    /// Construct a new config with the given URL and the default
    /// `pool_size = 10` (matches sqlx's out-of-box default +
    /// [`ff_core::backend::PostgresConnection`] default).
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            pool_size: 10,
        }
    }
}

impl Default for PostgresServerConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            pool_size: 10,
        }
    }
}

/// RFC-017 Wave 8 Stage E4 (v0.8.0): Valkey connection parameters
/// carried on [`ServerConfig`] when `backend == BackendKind::Valkey`.
///
/// Mirrors the pre-existing [`PostgresServerConfig`] shape. Replaces
/// the flat `host` / `port` / `tls` / `cluster` / `skip_library_load`
/// fields removed at v0.8.0 in favour of sum-typed nesting.
///
/// **Not `#[non_exhaustive]`** so downstream tests and consumers can
/// construct the struct literal directly. Adding fields here is a
/// v0.y.0 breaking bump; call sites that want to remain insulated can
/// use `..ValkeyServerConfig::default()` in their literal.
#[derive(Debug, Clone)]
pub struct ValkeyServerConfig {
    /// Valkey host. Default: `"localhost"`.
    pub host: String,
    /// Valkey port. Default: `6379`.
    pub port: u16,
    /// Enable TLS for Valkey connections.
    pub tls: bool,
    /// Enable Valkey cluster mode.
    pub cluster: bool,
    /// Skip library loading (for tests where TestCluster already loaded it).
    pub skip_library_load: bool,
}

impl Default for ValkeyServerConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 6379,
            tls: false,
            cluster: false,
            skip_library_load: false,
        }
    }
}

/// RFC-023 (v0.12.0): SQLite dev-only connection parameters carried
/// on [`ServerConfig`] when `backend == BackendKind::Sqlite`.
///
/// `#[non_exhaustive]` — consumers build via [`Self::new`] + the
/// builder methods; adding future fields is additive.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SqliteServerConfig {
    /// File path or in-memory URI — `:memory:`, `file::memory:...`,
    /// or an ordinary filesystem path. Read from `FF_SQLITE_PATH`.
    pub path: String,
    /// Connection-pool size. Default `4` (1 writer + 3 readers under
    /// WAL). Read from `FF_SQLITE_POOL_SIZE`.
    pub pool_size: u32,
    /// Enable WAL journaling. Default `true`. Ignored for
    /// `:memory:` databases where WAL is a no-op.
    pub wal_mode: bool,
}

impl SqliteServerConfig {
    /// Construct a new config with the given path and defaults
    /// (`pool_size = 4`, `wal_mode = true`).
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            pool_size: 4,
            wal_mode: true,
        }
    }

    /// Builder: override pool size (writer + readers combined).
    pub fn with_pool_size(mut self, n: u32) -> Self {
        self.pool_size = n;
        self
    }

    /// Builder: toggle WAL journaling.
    pub fn with_wal(mut self, on: bool) -> Self {
        self.wal_mode = on;
        self
    }
}

impl Default for SqliteServerConfig {
    fn default() -> Self {
        Self::new(":memory:")
    }
}

/// Server configuration, loaded from environment variables.
///
/// **RFC-017 Stage E4 (v0.8.0):** the flat Valkey fields (`host`,
/// `port`, `tls`, `cluster`, `skip_library_load`) were removed. Use
/// [`ValkeyServerConfig`] on the `valkey` field instead.
///
/// **RFC-023 (v0.12.0):** gained `#[non_exhaustive]` + a `sqlite`
/// field. External consumers cannot use struct-literal construction
/// (including `..Default::default()` spread syntax — Rust rejects
/// the struct-update form on `#[non_exhaustive]` types from other
/// crates). Build via one of:
///
/// * [`ServerConfig::from_env`] — production-ready, validates all
///   env-driven axes.
/// * [`ServerConfig::sqlite_dev`] — zero-config dev harness.
/// * `ServerConfig::default()` followed by field-by-field mutation
///   — for tests that need to override one or two axes.
#[non_exhaustive]
pub struct ServerConfig {
    /// Partition counts (execution/flow/budget/quota).
    pub partition_config: PartitionConfig,
    /// Lanes to manage. Default: `["default"]`.
    pub lanes: Vec<LaneId>,
    /// Listen address for the API surface. Default: `"0.0.0.0:9090"`.
    pub listen_addr: String,
    /// Scanner intervals and engine config.
    pub engine_config: EngineConfig,
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
    /// RFC-017 Stage A: which backend family to boot. Default
    /// [`BackendKind::Valkey`]. `BackendKind::Postgres` is rejected
    /// at startup through Stage D per RFC-017 §9.0.
    pub backend: BackendKind,
    /// RFC-017 Stage E4 (v0.8.0): Valkey connection parameters.
    /// Meaningful only when `backend == BackendKind::Valkey`; the
    /// Postgres path ignores these fields.
    pub valkey: ValkeyServerConfig,
    /// RFC-017 Wave 8 Stage E1: Postgres connection parameters.
    /// Meaningful only when `backend == BackendKind::Postgres`; the
    /// Valkey path ignores these fields.
    pub postgres: PostgresServerConfig,
    /// RFC-023 (v0.12.0): SQLite dev-only connection parameters.
    /// Meaningful only when `backend == BackendKind::Sqlite`; the
    /// Valkey/Postgres paths ignore these fields.
    pub sqlite: SqliteServerConfig,
}

impl ServerConfig {
    /// Construct a [`ServerConfig`] with the same defaults as
    /// [`Self::default`]. Provided so external consumers have an
    /// explicit constructor against this `#[non_exhaustive]` struct
    /// (struct-literal + `..Default::default()` spread are both
    /// forbidden cross-crate). Override fields field-by-field after
    /// calling; for env-driven boot use [`Self::from_env`].
    pub fn new() -> Self {
        Self::default()
    }
}

impl ServerConfig {
    /// RFC-017 Wave 8 Stage E1: build the
    /// [`ff_core::backend::BackendConfig`] the Postgres backend's
    /// `connect_with_metrics` expects, from the flat `postgres.url`
    /// + `postgres.pool_size` fields on this struct.
    pub fn postgres_config(&self) -> ff_core::backend::BackendConfig {
        let mut cfg = ff_core::backend::BackendConfig::postgres(&self.postgres.url);
        if let ff_core::backend::BackendConnection::Postgres(ref mut conn) = cfg.connection {
            conn.max_connections = self.postgres.pool_size;
        }
        cfg
    }
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
    /// | `FF_BACKEND` | `valkey` | Backend family — `valkey`, `postgres`, or `sqlite` (RFC-023 dev-only). |
    /// | `FF_POSTGRES_URL` | *(empty)* | Postgres connection URL (libpq/sqlx shape, e.g. `postgres://user:pass@host:port/db`). Required when `FF_BACKEND=postgres`; ignored otherwise. |
    /// | `FF_POSTGRES_POOL_SIZE` | `10` | Max Postgres pool connections; ignored on the Valkey path. |
    /// | `FF_SQLITE_PATH` | `:memory:` | SQLite database path or URI. Used when `FF_BACKEND=sqlite`. |
    /// | `FF_SQLITE_POOL_SIZE` | `4` | Max SQLite pool connections (1 writer + (pool_size - 1) readers). |
    /// | `FF_DEV_MODE` | *(unset)* | Must be `1` to activate `FF_BACKEND=sqlite`. Orthogonal to `FF_ENV=development` / `FF_BACKEND_ACCEPT_UNREADY=1`. |
    pub fn from_env() -> Result<Self, ConfigError> {
        let valkey = ValkeyServerConfig {
            host: env_or("FF_HOST", "localhost"),
            port: env_u16("FF_PORT", 6379)?,
            tls: env_bool("FF_TLS"),
            cluster: env_bool("FF_CLUSTER"),
            skip_library_load: false,
        };
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
            edge_cancel_dispatcher_interval: Duration::from_secs(
                env_u64("FF_EDGE_CANCEL_DISPATCHER_INTERVAL_S", 1)?
            ),
            edge_cancel_reconciler_interval: Duration::from_secs(
                env_u64("FF_EDGE_CANCEL_RECONCILER_INTERVAL_S", 10)?
            ),
            // Issue #122: default is no-op. Multi-tenant deployments
            // override this after ServerConfig construction.
            scanner_filter: Default::default(),
        };

        // RFC-017 Stage E4 (v0.8.0): `FF_BACKEND` selects the backend
        // family at boot. Default `valkey`; both `valkey` and `postgres`
        // are first-class. Unknown values are rejected eagerly so typos
        // don't silently fall through to the default. FF_POSTGRES_URL +
        // FF_POSTGRES_POOL_SIZE populate `postgres` when FF_BACKEND=postgres.
        // Read regardless of backend selector so operators can preset the
        // values; the Valkey path ignores them.
        let postgres = PostgresServerConfig {
            url: std::env::var("FF_POSTGRES_URL").unwrap_or_default(),
            pool_size: env_u32_positive("FF_POSTGRES_POOL_SIZE", 10)?,
        };

        // RFC-023 (v0.12.0): read SQLite fields unconditionally so
        // operators can preset values; non-sqlite backends ignore.
        let sqlite = SqliteServerConfig {
            path: env_or("FF_SQLITE_PATH", ":memory:"),
            pool_size: env_u32_positive("FF_SQLITE_POOL_SIZE", 4)?,
            wal_mode: true,
        };

        let backend = match std::env::var("FF_BACKEND") {
            Ok(v) => match v.to_ascii_lowercase().as_str() {
                "" | "valkey" => BackendKind::Valkey,
                "postgres" => BackendKind::Postgres,
                "sqlite" => BackendKind::Sqlite,
                other => {
                    return Err(ConfigError::InvalidValue {
                        var: "FF_BACKEND".to_owned(),
                        message: format!(
                            "unknown backend '{other}': expected 'valkey', 'postgres', or 'sqlite'"
                        ),
                    });
                }
            },
            Err(_) => BackendKind::default(),
        };

        Ok(Self {
            partition_config,
            lanes,
            listen_addr,
            engine_config,
            cors_origins,
            api_token,
            waitpoint_hmac_secret,
            waitpoint_hmac_grace_ms,
            max_concurrent_stream_ops,
            backend,
            valkey,
            postgres,
            sqlite,
        })
    }

    /// RFC-023 §4.4 item 4 (v0.12.0): zero-config SQLite dev harness
    /// builder. Returns a `ServerConfig` pre-wired with
    /// `backend = Sqlite`, `sqlite.path = ":memory:"`,
    /// `listen_addr = "127.0.0.1:0"` (OS-picked port), and ALL auth
    /// axes disabled: `api_token = None`, admin auth off,
    /// `waitpoint_hmac_secret` set to a deterministic dev placeholder.
    ///
    /// **Dev/test only.** The returned config is unsafe for
    /// production — every auth path is open and the SQLite backend
    /// itself demands `FF_DEV_MODE=1` at construction time per
    /// §4.5.
    pub fn sqlite_dev() -> Self {
        Self {
            sqlite: SqliteServerConfig::new(":memory:"),
            backend: BackendKind::Sqlite,
            listen_addr: "127.0.0.1:0".into(),
            api_token: None,
            ..Default::default()
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        let lanes = vec![LaneId::new("default")];
        let partition_config = PartitionConfig::default();
        Self {
            partition_config,
            lanes: lanes.clone(),
            listen_addr: "0.0.0.0:9090".into(),
            engine_config: EngineConfig {
                partition_config,
                lanes,
                ..Default::default()
            },
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
            backend: BackendKind::default(),
            valkey: ValkeyServerConfig::default(),
            postgres: PostgresServerConfig::default(),
            sqlite: SqliteServerConfig::default(),
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
