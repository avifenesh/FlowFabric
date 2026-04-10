// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

use std::time::Duration;

use crate::compression::CompressionConfig;
use crate::iam::ServiceType;

#[derive(Default, Clone, Debug)]
pub struct ConnectionRequest {
    pub read_from: Option<ReadFrom>,
    pub client_name: Option<String>,
    pub lib_name: Option<String>,
    pub authentication_info: Option<AuthenticationInfo>,
    pub database_id: i64,
    pub protocol: Option<crate::valkey::ProtocolVersion>,
    pub tls_mode: Option<TlsMode>,
    pub addresses: Vec<NodeAddress>,
    pub cluster_mode_enabled: bool,
    pub request_timeout: Option<u32>,
    pub connection_timeout: Option<u32>,
    pub connection_retry_strategy: Option<ConnectionRetryStrategy>,
    pub periodic_checks: Option<PeriodicCheck>,
    pub pubsub_subscriptions: Option<crate::valkey::PubSubSubscriptionInfo>,
    pub inflight_requests_limit: Option<u32>,
    pub lazy_connect: bool,
    pub refresh_topology_from_initial_nodes: bool,
    pub root_certs: Vec<Vec<u8>>,
    pub client_cert: Vec<u8>,
    pub client_key: Vec<u8>,
    pub compression_config: Option<CompressionConfig>,
    pub tcp_nodelay: bool,
    pub pubsub_reconciliation_interval_ms: Option<u32>,
    pub read_only: bool,
}

/// Default connection timeout used when not specified in the request.
/// Note: If you change this value, make sure to change the documentation in *all* wrappers.
pub const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_millis(2000);

impl ConnectionRequest {
    /// Returns the connection timeout from the request, or the default if not specified.
    /// This centralizes the timeout logic to ensure consistency across all client types.
    pub fn get_connection_timeout(&self) -> Duration {
        self.connection_timeout
            .map(|val| Duration::from_millis(val as u64))
            .unwrap_or(DEFAULT_CONNECTION_TIMEOUT)
    }
}

/// Authentication information for connecting to Redis/Valkey servers
///
/// Supports traditional username/password authentication and AWS IAM authentication.
/// IAM authentication takes priority when both are configured.
#[derive(PartialEq, Eq, Clone, Default, Debug)]
pub struct AuthenticationInfo {
    /// Username for authentication (required for IAM)
    pub username: Option<String>,

    /// Password for traditional authentication (fallback when IAM unavailable)
    pub password: Option<String>,

    /// IAM authentication configuration (takes precedence over password)
    pub iam_config: Option<IamAuthenticationConfig>,
}

/// AWS IAM authentication configuration for ElastiCache and MemoryDB
///
/// Handles AWS credential resolution, SigV4 token signing, and automatic token refresh.
/// Tokens are valid for 15 minutes and refreshed every 14 minutes by default.
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


