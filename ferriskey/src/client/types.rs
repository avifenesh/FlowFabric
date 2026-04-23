// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

use std::time::Duration;

use crate::compression::CompressionConfig;
#[cfg(feature = "iam")]
pub use crate::iam::ServiceType;

#[derive(Default, Clone, Debug)]
pub struct ConnectionRequest {
    pub read_from: Option<ReadFrom>,
    pub client_name: Option<String>,
    pub lib_name: Option<String>,
    pub authentication_info: Option<AuthenticationInfo>,
    pub database_id: i64,
    pub protocol: Option<crate::value::ProtocolVersion>,
    pub tls_mode: Option<TlsMode>,
    pub addresses: Vec<NodeAddress>,
    pub cluster_mode_enabled: bool,
    pub request_timeout: Option<u32>,
    pub connection_timeout: Option<u32>,
    pub connection_retry_strategy: Option<ConnectionRetryStrategy>,
    pub periodic_checks: Option<PeriodicCheck>,
    pub pubsub_subscriptions: Option<crate::connection::info::PubSubSubscriptionInfo>,
    pub inflight_requests_limit: Option<u32>,
    /// Deprecated glide-era flag. The connect-on-first-use path now
    /// lives on [`crate::LazyClient`] constructed via
    /// [`crate::ClientBuilder::build_lazy`]. If this flag is set on
    /// a request handed to [`crate::Client::new`], the call returns
    /// `InvalidClientConfig` with an actionable migration message —
    /// see that error for guidance.
    pub lazy_connect: bool,
    pub refresh_topology_from_initial_nodes: bool,
    pub root_certs: Vec<Vec<u8>>,
    pub client_cert: Vec<u8>,
    pub client_key: Vec<u8>,
    pub compression_config: Option<CompressionConfig>,
    pub tcp_nodelay: bool,
    pub pubsub_reconciliation_interval_ms: Option<u32>,
    pub read_only: bool,
    /// Extra time added to a blocking command's server-side timeout when
    /// setting the client-side request deadline. The client waits for
    /// `server_timeout + extension` before treating the command as
    /// timed out — the extension is a safety margin so the client
    /// doesn't fail a request that the server is legitimately about to
    /// answer. `None` uses [`DEFAULT_BLOCKING_CMD_TIMEOUT_EXTENSION`].
    pub blocking_cmd_timeout_extension: Option<Duration>,
}

/// Default connection timeout used when not specified in the request.
/// Note: If you change this value, make sure to change the documentation in *all* wrappers.
pub const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_millis(2000);

impl ConnectionRequest {
    /// Returns the connection timeout from the request, or the default if not specified.
    /// This centralizes the timeout logic to ensure consistency across all client types.
    pub(crate) fn get_connection_timeout(&self) -> Duration {
        self.connection_timeout
            .map(|val| Duration::from_millis(val as u64))
            .unwrap_or(DEFAULT_CONNECTION_TIMEOUT)
    }
}

/// Authentication information for connecting to Valkey servers
///
/// Supports traditional username/password authentication and (when built
/// with `feature = "iam"`) AWS IAM authentication. IAM authentication
/// takes priority when both are configured; the `iam_config` field is
/// only present on builds that enable the `iam` feature.
#[derive(PartialEq, Eq, Clone, Default, Debug)]
pub struct AuthenticationInfo {
    /// Username for authentication (required for IAM)
    pub username: Option<String>,

    /// Password for traditional authentication (fallback when IAM unavailable)
    pub password: Option<String>,

    /// IAM authentication configuration (takes precedence over password).
    /// Only available when built with `feature = "iam"`.
    #[cfg(feature = "iam")]
    pub iam_config: Option<IamAuthenticationConfig>,
}

/// AWS IAM authentication configuration for ElastiCache and MemoryDB.
///
/// Handles AWS credential resolution, SigV4 token signing, and automatic
/// token refresh. Tokens are valid for 15 minutes and refreshed every
/// 14 minutes by default. Only available when built with
/// `feature = "iam"`.
#[cfg(feature = "iam")]
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct IamAuthenticationConfig {
    /// AWS ElastiCache or MemoryDB cluster name
    pub cluster_name: String,

    /// AWS region where the cluster is located
    pub region: String,

    /// AWS service type (ElastiCache or MemoryDB)
    pub service_type: ServiceType,

    /// Token refresh interval in seconds (1 second to 12 hours, default 14 minutes)
    pub refresh_interval_seconds: Option<u32>,
}

#[derive(Default, Clone, Copy, Debug)]
pub enum PeriodicCheck {
    #[default]
    Enabled,
    Disabled,
    ManualInterval(Duration),
}

#[derive(Clone, Debug)]
pub struct NodeAddress {
    pub host: String,
    pub port: u16,
}

impl ::std::fmt::Display for NodeAddress {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        write!(f, "Host: `{}`, Port: {}", self.host, self.port)
    }
}

/// Initial connection metadata used as default OTel span attributes.
#[derive(Clone, Debug)]
pub struct OTelMetadata {
    pub address: NodeAddress,
    pub db_namespace: String,
}

#[derive(PartialEq, Eq, Clone, Default, Debug)]
pub enum ReadFrom {
    #[default]
    Primary,
    PreferReplica,
    AZAffinity(String),
    AZAffinityReplicasAndPrimary(String),
}

#[derive(PartialEq, Eq, Clone, Copy, Default, Debug)]
pub enum TlsMode {
    #[default]
    NoTls,
    InsecureTls,
    SecureTls,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug, Default)]
pub struct ConnectionRetryStrategy {
    pub exponent_base: u32,
    pub factor: u32,
    pub number_of_retries: u32,
    pub jitter_percent: Option<u32>,
}
