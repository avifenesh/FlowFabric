// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

pub mod types;

use crate::cluster::ClusterConnection;
use crate::cluster::routing::{
    MultipleNodeRoutingInfo, ResponsePolicy, Routable, RoutingInfo, SingleNodeRoutingInfo,
};
use crate::cluster::scan::{ClusterScanArgs, ScanStateRC};
use crate::cluster::slotmap::ReadFromReplicaStrategy;
use crate::cluster_scan_container::insert_cluster_scan_cursor;
use crate::cmd::Cmd;
use crate::compression::CompressionBackendType;
use crate::compression::lz4_backend::Lz4Backend;
use crate::compression::zstd_backend::ZstdBackend;
use crate::compression::{CompressionConfig, CompressionManager};
use crate::connection::ConnectionLike;
use crate::connection::info::{ConnectionAddr, ConnectionInfo, ValkeyConnectionInfo};
use crate::connection::tls::{TlsCertificates, TlsConnParams, retrieve_tls_certificates};
use crate::pipeline::PipelineRetryStrategy;
use crate::pubsub::push_manager::PushInfo;
use crate::retry_strategies::RetryStrategy;
use crate::scripts_container::get_script;
use crate::value::{ErrorKind, Error, FromValue, Result, Value};
use futures_util::future::BoxFuture;
use futures::FutureExt;
pub use standalone_client::StandaloneClient;
use std::io;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;
pub use types::*;

use self::value_conversion::{convert_to_expected_type, expected_type_for_cmd, get_value_type};
mod reconnecting_connection;
#[cfg(feature = "iam")]
pub use reconnecting_connection::IAMTokenHandle;
mod standalone_client;
mod value_conversion;
use crate::pubsub::{PubSubSynchronizer, create_pubsub_synchronizer};
use crate::request_type::RequestType;
use crate::value::InfoDict;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::{RwLock, mpsc};
use versions::Versioning;

pub const HEARTBEAT_SLEEP_DURATION: Duration = Duration::from_secs(1);
pub(crate) const DEFAULT_RETRIES: u32 = 3;
/// Note: If you change the default value, make sure to change the documentation in *all* wrappers.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);
pub(crate) const DEFAULT_PERIODIC_TOPOLOGY_CHECKS_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const FINISHED_SCAN_CURSOR: &str = "finished";

/// The value of 1000 for the maximum number of inflight requests is determined based on Little's Law in queuing theory:
///
/// Expected maximum request rate: 50,000 requests/second
/// Expected response time: 1 millisecond
///
/// According to Little's Law, the maximum number of inflight requests required to fully utilize the maximum request rate is:
///   (50,000 requests/second) × (1 millisecond / 1000 milliseconds) = 50 requests
///
/// The value of 1000 provides a buffer for bursts while still allowing full utilization of the maximum request rate.
pub(crate) const DEFAULT_MAX_INFLIGHT_REQUESTS: u32 = 1000;

/// The connection check interval is currently not exposed to the user via ConnectionRequest,
/// as improper configuration could negatively impact performance or pub/sub resiliency.
/// A 3-second interval provides a reasonable balance between connection validation
/// and performance overhead.
pub const CONNECTION_CHECKS_INTERVAL: Duration = Duration::from_secs(3);

/// Extract RequestType from a Valkey command for decompression processing
fn extract_request_type_from_cmd(cmd: &Cmd) -> Option<RequestType> {
    // Get the command name (first argument)
    let command_name = cmd.command()?;
    let command_str = String::from_utf8_lossy(&command_name).to_uppercase();

    // Map command names to RequestType for decompression
    // Only read commands that return values needing decompression are included
    match command_str.as_str() {
        "GET" => Some(RequestType::Get),
        "MGET" => Some(RequestType::MGet),
        "GETEX" => Some(RequestType::GetEx),
        "GETDEL" => Some(RequestType::GetDel),
        "GETSET" => Some(RequestType::GetSet),
        // SET key value ... GET is the modern replacement for GETSET.
        // The GET flag makes the server return the old value, which needs decompression.
        "SET" if cmd.position(b"GET").is_some() => Some(RequestType::GetSet),
        _ => None, // Unknown command or write command, no decompression needed
    }
}

pub(super) fn get_port(address: &NodeAddress) -> u16 {
    const DEFAULT_PORT: u16 = 6379;
    if address.port == 0 {
        DEFAULT_PORT
    } else {
        address.port
    }
}

/// Get Valkey connection info with IAM token integration (when built
/// with `feature = "iam"`).
///
/// If IAM config + token manager exist, use the IAM token as the
/// password; otherwise use the provided password. The
/// `iam_token_manager` parameter is cfg-gated so non-IAM builds have
/// a smaller signature with no dependency on the IAM module.
///
/// With `feature = "iam"` off, this function only ever uses the
/// static password path — the match on `iam_config` vanishes at
/// compile time.
pub async fn get_valkey_connection_info(
    connection_request: &ConnectionRequest,
    #[cfg(feature = "iam")] iam_token_manager: Option<&Arc<crate::iam::IAMTokenManager>>,
) -> crate::connection::info::ValkeyConnectionInfo {
    let protocol = connection_request.protocol.unwrap_or_default();
    let db = connection_request.database_id;
    let client_name = connection_request.client_name.clone();
    let lib_name = connection_request.lib_name.clone();

    match &connection_request.authentication_info {
        Some(info) => {
            // If we have IAM configuration and a token manager, use the IAM token as password.
            // The entire branch is cfg-gated so non-IAM builds compile it out.
            #[cfg(feature = "iam")]
            if let (Some(_), Some(manager)) = (&info.iam_config, iam_token_manager) {
                let token = manager.get_token().await;

                return crate::connection::info::ValkeyConnectionInfo {
                    db,
                    username: info.username.clone(),
                    password: Some(token),
                    protocol,
                    client_name,
                    lib_name,
                };
            }

            // Regular password-based authentication (always taken when
            // feature = "iam" is off; taken when no iam_config present
            // or no manager provided when the feature is on).
            crate::connection::info::ValkeyConnectionInfo {
                db,
                username: info.username.clone(),
                password: info.password.clone(),
                protocol,
                client_name,
                lib_name,
            }
        }
        None => crate::connection::info::ValkeyConnectionInfo {
            db,
            protocol,
            client_name,
            lib_name,
            ..Default::default()
        },
    }
}

// tls_params should be only set if tls_mode is SecureTls
// this should be validated before calling this function
pub(super) fn get_connection_info(
    address: &NodeAddress,
    tls_mode: TlsMode,
    valkey_connection_info: ValkeyConnectionInfo,
    tls_params: Option<TlsConnParams>,
) -> ConnectionInfo {
    let addr = if tls_mode != TlsMode::NoTls {
        ConnectionAddr::TcpTls {
            host: address.host.to_string(),
            port: get_port(address),
            insecure: tls_mode == TlsMode::InsecureTls,
            tls_params,
        }
    } else {
        ConnectionAddr::Tcp(address.host.to_string(), get_port(address))
    };
    ConnectionInfo {
        addr,
        valkey: valkey_connection_info,
    }
}

/// The underlying connected client held by a [`Client`].
///
/// By construction this is ALWAYS connected — [`Client::new`] and
/// [`ClientBuilder::build`] only return after
/// [`StandaloneClient::create_client`] or [`create_cluster_client`]
/// succeeds. There is no transient "not yet connected" state; code
/// that needs connection-on-first-use constructs a
/// [`crate::LazyClient`] via [`crate::ClientBuilder::build_lazy`]
/// instead.
#[derive(Clone)]
pub enum ClientWrapper {
    Standalone(StandaloneClient),
    Cluster { client: ClusterConnection },
}

#[derive(Clone)]
pub struct Client {
    internal_client: Arc<RwLock<ClientWrapper>>,
    request_timeout: Duration,
    /// Extra time added to a blocking command's server-side timeout
    /// when computing the client-side request deadline. Defaults to
    /// [`DEFAULT_DEFAULT_EXT_SECS`] (500ms).
    blocking_cmd_timeout_extension: Duration,
    inflight_requests_allowed: Arc<AtomicIsize>,
    inflight_requests_limit: isize,
    inflight_log_interval: isize,
    // IAM token manager for automatic credential refresh.
    // Only present when built with `feature = "iam"`; non-IAM builds
    // don't carry the field and don't pay the `Option<Arc<_>>` clone
    // cost on the per-command Client::clone hot path.
    #[cfg(feature = "iam")]
    iam_token_manager: Option<Arc<crate::iam::IAMTokenManager>>,
    // Optional compression manager for automatic compression/decompression
    compression_manager: Option<Arc<CompressionManager>>,
    pubsub_synchronizer: Arc<dyn PubSubSynchronizer>,
    otel_metadata: types::OTelMetadata,
}

async fn run_with_timeout<T>(
    timeout: Option<Duration>,
    future: impl futures::Future<Output = Result<T>> + Send,
) -> crate::value::Result<T> {
    match timeout {
        Some(duration) => match tokio::time::timeout(duration, future).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    target: "ferriskey",
                    event = "timeout",
                    duration_ms = duration.as_millis() as u64,
                    "ferriskey: request timed out"
                );
                Err(io::Error::from(io::ErrorKind::TimedOut).into())
            }
        },
        None => future.await,
    }
}

/// Default extension to the request timeout for blocking commands.
///
/// When a blocking command (BLPOP, BLMPOP, XREAD BLOCK, etc.) is dispatched,
/// the client-side request deadline is set to `server_timeout + extension`.
/// The extension is a safety margin so the client doesn't fail a request
/// that the server is legitimately about to answer over a slow link.
///
/// Override per-client via [`crate::ClientBuilder::blocking_cmd_timeout_extension`].
pub const DEFAULT_DEFAULT_EXT_SECS: Duration = Duration::from_millis(500);

enum TimeUnit {
    Milliseconds = 1000,
    Seconds = 1,
}

/// Enumeration representing different request timeout options.
#[derive(Default, PartialEq, Debug)]
enum RequestTimeoutOption {
    // Indicates no timeout should be set for the request.
    NoTimeout,
    // Indicates the request timeout should be based on the client's configured timeout.
    #[default]
    ClientConfig,
    // Indicates the request timeout should be based on the timeout specified in the blocking command.
    BlockingCommand(Duration),
}

/// Helper function for parsing a timeout argument to f64.
/// Attempts to parse the argument found at `timeout_idx` from bytes into an f64.
fn parse_timeout_to_f64(cmd: &Cmd, timeout_idx: usize) -> Result<f64> {
    let create_err = |err_msg| {
        Error::from((
            ErrorKind::ResponseError,
            err_msg,
            format!(
                "Expected to find timeout value at index {:?} for command {:?}.",
                timeout_idx,
                std::str::from_utf8(&cmd.command().unwrap_or_default()),
            ),
        ))
    };
    let timeout_bytes = cmd
        .arg_idx(timeout_idx)
        .ok_or(create_err("Couldn't find timeout index"))?;
    let timeout_str = std::str::from_utf8(timeout_bytes)
        .map_err(|_| create_err("Failed to parse the timeout argument to string"))?;
    timeout_str
        .parse::<f64>()
        .map_err(|_| create_err("Failed to parse the timeout argument to f64"))
}

/// Attempts to get the timeout duration from the command argument at `timeout_idx`.
/// If the argument can be parsed into a duration, it returns the duration in seconds with BlockingCmdTimeout.
/// If the timeout argument value is zero, NoTimeout will be returned. Otherwise, ClientConfigTimeout is returned.
///
/// `extension` is the per-client safety margin added to the server-side timeout.
fn get_timeout_from_cmd_arg(
    cmd: &Cmd,
    timeout_idx: usize,
    time_unit: TimeUnit,
    extension: Duration,
) -> Result<RequestTimeoutOption> {
    let timeout_secs = parse_timeout_to_f64(cmd, timeout_idx)? / ((time_unit as i32) as f64);
    if timeout_secs < 0.0 {
        // Timeout cannot be negative, return the client's configured request timeout
        Err(Error::from((
            ErrorKind::ResponseError,
            "Timeout cannot be negative",
            format!("Received timeout = {timeout_secs:?}."),
        )))
    } else if timeout_secs == 0.0 {
        // `0` means we should set no timeout
        Ok(RequestTimeoutOption::NoTimeout)
    } else {
        // We limit the maximum timeout due to restrictions imposed by Valkey and the Duration crate
        if timeout_secs > u32::MAX as f64 {
            Err(Error::from((
                ErrorKind::ResponseError,
                "Timeout is out of range, max timeout is 2^32 - 1 (u32::MAX)",
                format!("Received timeout = {timeout_secs:?}."),
            )))
        } else {
            // Extend the request timeout to ensure we don't timeout before receiving a response from the server.
            Ok(RequestTimeoutOption::BlockingCommand(
                Duration::from_secs_f64(
                    (timeout_secs + extension.as_secs_f64()).min(u32::MAX as f64),
                ),
            ))
        }
    }
}

fn get_request_timeout(
    cmd: &Cmd,
    default_timeout: Duration,
    extension: Duration,
) -> Result<Option<Duration>> {
    let command = cmd.command().unwrap_or_default();
    let timeout = match command.as_slice() {
        b"BLPOP" | b"BRPOP" | b"BLMOVE" | b"BZPOPMAX" | b"BZPOPMIN" | b"BRPOPLPUSH" => {
            get_timeout_from_cmd_arg(cmd, cmd.args_iter().len() - 1, TimeUnit::Seconds, extension)
        }
        b"BLMPOP" | b"BZMPOP" => get_timeout_from_cmd_arg(cmd, 1, TimeUnit::Seconds, extension),
        b"XREAD" | b"XREADGROUP" => cmd
            .position(b"BLOCK")
            .map(|idx| get_timeout_from_cmd_arg(cmd, idx + 1, TimeUnit::Milliseconds, extension))
            .unwrap_or(Ok(RequestTimeoutOption::ClientConfig)),
        b"WAIT" | b"WAITAOF" => {
            let idx = if command.as_slice() == b"WAITAOF" {
                3
            } else {
                2
            };
            get_timeout_from_cmd_arg(cmd, idx, TimeUnit::Milliseconds, extension)
        }
        _ => Ok(RequestTimeoutOption::ClientConfig),
    }?;

    match timeout {
        RequestTimeoutOption::NoTimeout => Ok(None),
        RequestTimeoutOption::ClientConfig => Ok(Some(default_timeout)),
        RequestTimeoutOption::BlockingCommand(blocking_cmd_duration) => {
            Ok(Some(blocking_cmd_duration))
        }
    }
}

impl Client {
    // Command-shape inspectors below take no `&self` — they're pure
    // functions over a `&Cmd`. Associated-fn syntax lets the unit
    // tests call them without constructing a `Client` (which now
    // requires a connected wrapper and can't be stubbed without a
    // real TCP connection).

    fn is_select_command(cmd: &Cmd) -> bool {
        cmd.command().is_some_and(|bytes| bytes == b"SELECT")
    }

    /// Extract the database ID from a SELECT command's first argument.
    fn extract_database_id_from_select(cmd: &Cmd) -> Result<i64> {
        // For both crate::cmd::cmd("SELECT").arg("5") and crate::cmd::Cmd::new().arg("SELECT").arg("5")
        // the database ID is at arg_idx(1)
        cmd.arg_idx(1)
            .ok_or_else(|| {
                Error::from((
                    ErrorKind::ResponseError,
                    "SELECT command missing database argument",
                ))
            })
            .and_then(|db_bytes| {
                std::str::from_utf8(db_bytes)
                    .map_err(|_| {
                        Error::from((ErrorKind::ResponseError, "Invalid database ID format"))
                    })
                    .and_then(|db_str| {
                        db_str.parse::<i64>().map_err(|_| {
                            Error::from((
                                ErrorKind::ResponseError,
                                "Database ID must be a valid integer",
                            ))
                        })
                    })
            })
    }

    /// Handle SELECT: update stored database ID and OTel metadata.
    ///
    /// Note: `db_namespace` is updated on `&mut self`, but `Client` is cloned
    /// into each request handler. If concurrent tasks issue SELECT, a cloned
    /// Client may report a stale `db_namespace` in OTel spans. This is an
    /// acceptable trade-off since concurrent SELECTs are rare in practice.
    async fn handle_select_command(&mut self, cmd: &Cmd) -> Result<()> {
        let database_id = Self::extract_database_id_from_select(cmd)?;

        self.update_stored_database_id(database_id).await?;
        // Keep OTel db.namespace in sync
        self.otel_metadata.db_namespace = database_id.to_string();
        Ok(())
    }

    async fn update_stored_database_id(&self, database_id: i64) -> Result<()> {
        let mut guard = self.internal_client.write().await;
        match &mut *guard {
            ClientWrapper::Standalone(client) => {
                client.update_connection_database(database_id).await?;
                Ok(())
            }
            ClientWrapper::Cluster { client } => {
                // Update cluster connection database configuration
                client.update_connection_database(database_id).await?;
                Ok(())
            }
        }
    }

    fn is_client_set_name_command(cmd: &Cmd) -> bool {
        cmd.command()
            .is_some_and(|bytes| bytes == b"CLIENT SETNAME")
    }

    fn extract_client_name_from_client_set_name(cmd: &Cmd) -> Option<String> {
        // For crate::cmd::cmd("CLIENT").arg("SETNAME").arg("name")
        // the client name is at arg_idx(2) (after "SETNAME")
        cmd.arg_idx(2).and_then(|name_bytes| {
            std::str::from_utf8(name_bytes)
                .ok()
                .map(|name_str| name_str.to_string())
        })
    }

    async fn handle_client_set_name_command(&mut self, cmd: &Cmd) -> Result<()> {
        let client_name = Self::extract_client_name_from_client_set_name(cmd);
        self.update_stored_client_name(client_name).await?;
        Ok(())
    }

    async fn update_stored_client_name(&self, client_name: Option<String>) -> Result<()> {
        let mut guard = self.internal_client.write().await;
        match &mut *guard {
            ClientWrapper::Standalone(client) => {
                client.update_connection_client_name(client_name).await?;
                Ok(())
            }
            ClientWrapper::Cluster { client } => {
                // Update cluster connection database configuration
                client.update_connection_client_name(client_name).await?;
                Ok(())
            }
        }
    }

    fn is_auth_command(cmd: &Cmd) -> bool {
        cmd.command().is_some_and(|bytes| bytes == b"AUTH")
    }

    /// Extract (username, password) from an AUTH command.
    fn extract_auth_info(cmd: &Cmd) -> (Option<String>, Option<String>) {
        // Get the first argument
        let first_arg = cmd
            .arg_idx(1)
            .and_then(|bytes| std::str::from_utf8(bytes).ok().map(|s| s.to_string()));

        // Get the second argument
        let second_arg = cmd
            .arg_idx(2)
            .and_then(|bytes| std::str::from_utf8(bytes).ok().map(|s| s.to_string()));

        match (first_arg, second_arg) {
            // AUTH username password
            (Some(username), Some(password)) => (Some(username), Some(password)),
            // AUTH password
            (Some(password), None) => (None, Some(password)),
            // Invalid AUTH command
            _ => (None, None),
        }
    }

    async fn handle_auth_command(&mut self, cmd: &Cmd) -> Result<()> {
        let (username, password) = Self::extract_auth_info(cmd);
        if username.is_some() {
            self.update_stored_username(username).await?;
        }
        if password.is_some() {
            self.update_stored_password(password).await?;
        }

        Ok(())
    }

    async fn update_stored_username(&self, username: Option<String>) -> Result<()> {
        let mut guard = self.internal_client.write().await;
        match &mut *guard {
            ClientWrapper::Standalone(client) => {
                client.update_connection_username(username).await?;
                Ok(())
            }
            ClientWrapper::Cluster { client } => {
                client.update_connection_username(username).await?;
                Ok(())
            }
        }
    }

    async fn update_stored_password(&self, password: Option<String>) -> Result<()> {
        let mut guard = self.internal_client.write().await;
        match &mut *guard {
            ClientWrapper::Standalone(client) => {
                client.update_connection_password(password).await?;
                Ok(())
            }
            ClientWrapper::Cluster { client } => {
                client.update_connection_password(password).await?;
                Ok(())
            }
        }
    }

    fn is_hello_command(cmd: &Cmd) -> bool {
        cmd.command().is_some_and(|bytes| bytes == b"HELLO")
    }

    /// Extract (protocol, username, password, client_name) from a HELLO command.
    ///
    /// HELLO command formats:
    /// - HELLO 3
    /// - HELLO 3 AUTH username password
    /// - HELLO 3 SETNAME clientname
    /// - HELLO 3 AUTH username password SETNAME clientname
    fn extract_hello_info(
        cmd: &Cmd,
    ) -> (
        Option<crate::value::ProtocolVersion>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        // Get protocol version (first argument)
        let protocol = cmd.arg_idx(1).and_then(|bytes| {
            std::str::from_utf8(bytes).ok().and_then(|s| match s {
                "2" => Some(crate::value::ProtocolVersion::RESP2),
                "3" => Some(crate::value::ProtocolVersion::RESP3),
                _ => None,
            })
        });

        let mut username = None;
        let mut password = None;
        let mut client_name = None;

        // Parse optional arguments (AUTH username password, SETNAME name)
        let mut idx = 2;
        while let Some(arg) = cmd.arg_idx(idx) {
            if let Ok(arg_str) = std::str::from_utf8(arg) {
                match arg_str.to_uppercase().as_str() {
                    "AUTH" => {
                        // Next two args are username and password
                        username = cmd.arg_idx(idx + 1).and_then(|bytes| {
                            std::str::from_utf8(bytes).ok().map(|s| s.to_string())
                        });
                        password = cmd.arg_idx(idx + 2).and_then(|bytes| {
                            std::str::from_utf8(bytes).ok().map(|s| s.to_string())
                        });
                        idx += 3;
                    }
                    "SETNAME" => {
                        // Next arg is client name
                        client_name = cmd.arg_idx(idx + 1).and_then(|bytes| {
                            std::str::from_utf8(bytes).ok().map(|s| s.to_string())
                        });
                        idx += 2;
                    }
                    _ => {
                        idx += 1;
                    }
                }
            } else {
                break;
            }
        }

        (protocol, username, password, client_name)
    }

    async fn handle_hello_command(&mut self, cmd: &Cmd) -> Result<()> {
        let (protocol, username, password, client_name) = Self::extract_hello_info(cmd);

        // Update protocol version if provided
        if let Some(protocol) = protocol {
            self.update_stored_protocol(protocol).await?;
        }

        // Update username if provided
        if username.is_some() {
            self.update_stored_username(username).await?;
        }

        // Update password if provided
        if password.is_some() {
            self.update_stored_password(password).await?;
        }

        // Update client name if provided
        if client_name.is_some() {
            self.update_stored_client_name(client_name).await?;
        }

        Ok(())
    }

    async fn update_stored_protocol(
        &self,
        protocol: crate::value::ProtocolVersion,
    ) -> Result<()> {
        let mut guard = self.internal_client.write().await;
        match &mut *guard {
            ClientWrapper::Standalone(client) => {
                client.update_connection_protocol(protocol).await?;
                Ok(())
            }
            ClientWrapper::Cluster { client } => {
                client.update_connection_protocol(protocol).await?;
                Ok(())
            }
        }
    }

    /// Return a clone of the underlying [`ClientWrapper`].
    ///
    /// Post-construction the wrapper is guaranteed to be
    /// [`ClientWrapper::Standalone`] or [`ClientWrapper::Cluster`] —
    /// there is no lazy state here anymore. The lazy connect-on-first-
    /// use path lives on [`crate::LazyClient`] instead. This method
    /// keeps its historical name so internal callers don't churn,
    /// but it is now a simple read-clone.
    async fn get_or_initialize_client(&self) -> Result<ClientWrapper> {
        let guard = self.internal_client.read().await;
        Ok(guard.clone())
    }

    pub fn send_command<'a>(
        &'a mut self,
        cmd: &'a mut Cmd,
        routing: Option<RoutingInfo>,
    ) -> BoxFuture<'a, Result<Value>> {
        // The per-command span is the primary public observation point.
        // Stable name = "ferriskey.send_command", target = "ferriskey".
        // Zero-cost when no subscriber is installed (tracing short-
        // circuits via the global dispatcher's `would_enabled` check).
        use tracing::Instrument;
        let command_name = cmd
            .command()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_else(|| "<unknown>".to_owned());
        let span = tracing::debug_span!(
            target: "ferriskey",
            "ferriskey.send_command",
            command = %command_name,
            has_routing = routing.is_some(),
        );
        Box::pin(async move {
            // Check for IAM token changes and update the password without
            // authentication if needed (pull model). Entirely cfg-gated:
            // non-IAM builds do not carry the `iam_token_manager` field
            // and do not compile this check.
            #[cfg(feature = "iam")]
            if let Some(iam_manager) = &self.iam_token_manager
                && iam_manager.token_changed()
            {
                let current_token = iam_manager.get_token().await;
                if current_token.is_empty() {
                    return Err(Error::from((
                        ErrorKind::ClientError,
                        "IAM token not available",
                    )));
                }
                iam_manager.clear_token_changed();
                tracing::debug!("update_connection_password - Updating connection password with IAM token");
                self.update_connection_password(Some(current_token), false)
                    .await?;
            }

            let client = self.get_or_initialize_client().await?;

            if let Some(result) = self.pubsub_synchronizer.intercept_pubsub_command(cmd).await {
                return result;
            }

            let request_timeout = get_request_timeout(
                cmd,
                self.request_timeout,
                self.blocking_cmd_timeout_extension,
            )?;

            // Reserve an inflight slot. The tracker holds the slot until the
            // last clone of the Cmd is dropped (i.e. all sub-commands in the
            // cluster event loop finish). This decouples user-facing timeout
            // from internal pipeline cleanup.
            let tracker = match self.reserve_inflight_request() {
                Some(t) => t,
                None => {
                    return Err(Error::from((
                        ErrorKind::ClientError,
                        "Reached maximum inflight requests",
                    )));
                }
            };

            // Log at debug level when inflight usage crosses a 10% threshold.
            // Only one log per threshold crossing — zero noise when stable.
            {
                static LAST_BUCKET: AtomicIsize = AtomicIsize::new(0);
                let remaining = self.inflight_requests_allowed.load(Ordering::Relaxed);
                let used = self.inflight_requests_limit - remaining;
                let bucket = used / self.inflight_log_interval;
                let prev = LAST_BUCKET.load(Ordering::Relaxed);
                if bucket != prev {
                    LAST_BUCKET.store(bucket, Ordering::Relaxed);
                    let limit = self.inflight_requests_limit;
                    tracing::debug!("inflight - Inflight: {used}/{limit} slots used");
                }
            }

            cmd.set_inflight_tracker(tracker);

            // Clone compression_manager reference only if compression is enabled
            let compression_manager = if self.is_compression_enabled() {
                self.compression_manager.clone()
            } else {
                None
            };

            // Inlined dispatch + post-processing. The execute future borrows
            // `&*cmd` and owns `client`/`routing`/`compression_manager`; `self`
            // is reborrowed after the await for handle_* post-calls. Holding
            // the future as an async block lets the timeout branch still drop
            // it on elapse without changing user-facing semantics.
            //
            // Lifetime shift: pre-collapse, `Self::execute_command_owned(...)`
            // was deliberately `Send + 'static` so the outer `tokio::select!`
            // could pin it across the sleep, paying for `self.clone()` +
            // `Arc::new(cmd.clone())` per call. Post-collapse, `execute` is
            // `Send + 'a` (borrows `&mut cmd`). That is sufficient because the
            // surrounding `Box::pin(async move { ... })` is already `'a`-bound,
            // so the select runs inside a single `'a`-scoped future. No caller
            // requires the inner future to be `'static`; reintroducing that
            // bound would also reintroduce the two allocations this commit
            // removed.
            let execute = async {
                let raw_value = match client {
                    ClientWrapper::Standalone(mut client) => client.send_command(cmd).await,
                    ClientWrapper::Cluster { mut client } => {
                        let final_routing = if let Some(RoutingInfo::SingleNode(
                            SingleNodeRoutingInfo::Random,
                        )) = routing
                        {
                            let cmd_name = cmd.command().unwrap_or_default();
                            let cmd_name = String::from_utf8_lossy(&cmd_name);
                            if crate::cluster::routing::is_readonly_cmd(cmd_name.as_bytes()) {
                                RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random)
                            } else {
                                tracing::warn!("send_command - User provided 'Random' routing which is not suitable for the writable command '{cmd_name}'. Changing it to 'RandomPrimary'");
                                RoutingInfo::SingleNode(SingleNodeRoutingInfo::RandomPrimary)
                            }
                        } else {
                            routing
                                .or_else(|| RoutingInfo::for_routable(cmd))
                                .unwrap_or(RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random))
                        };
                        client.route_command(cmd, final_routing).await
                    }
                }?;

                let processed_value = if let Some(ref compression_manager) = compression_manager {
                    if let Some(request_type) = extract_request_type_from_cmd(cmd) {
                        match crate::compression::process_response_for_decompression(
                            raw_value.clone(),
                            request_type,
                            Some(compression_manager.as_ref()),
                        ) {
                            Ok(decompressed_value) => decompressed_value,
                            Err(e) => {
                                tracing::warn!("send_command_decompression - Failed to decompress response: {e}");
                                raw_value
                            }
                        }
                    } else {
                        raw_value
                    }
                } else {
                    raw_value
                };

                let expected_type = expected_type_for_cmd(cmd);
                let value = convert_to_expected_type(processed_value, expected_type)?;

                // Post-dispatch handlers run INSIDE the timeout scope to match
                // pre-collapse semantics: in the old execute_command_owned these
                // ran after the ClientWrapper dispatch but before the function
                // returned, so the outer select!'s timeout bounded them too.
                if Self::is_client_set_name_command(cmd) {
                    self.handle_client_set_name_command(cmd).await?;
                }
                if Self::is_select_command(cmd) {
                    self.handle_select_command(cmd).await?;
                }
                if Self::is_auth_command(cmd) {
                    self.handle_auth_command(cmd).await?;
                }
                if Self::is_hello_command(cmd) {
                    self.handle_hello_command(cmd).await?;
                }
                Ok::<_, Error>(value)
            };

            let value = match request_timeout {
                Some(duration) => {
                    tokio::pin!(execute);
                    tokio::select! {
                        result = &mut execute => result?,
                        _ = tokio::time::sleep(duration) => {
                            // User timeout — execute future is dropped. A clone
                            // of the InflightRequestTracker was already attached
                            // to cmd via set_inflight_tracker, so the tracker
                            // (and its slot guard) lives until the caller drops
                            // their Cmd — independent of this future's drop.
                            tracing::warn!(
                                target: "ferriskey",
                                event = "timeout",
                                duration_ms = duration.as_millis() as u64,
                                "ferriskey: command timed out"
                            );
                            return Err(io::Error::from(io::ErrorKind::TimedOut).into());
                        }
                    }
                }
                None => execute.await?,
            };

            Ok(value)
        }
        .instrument(span))
    }

    // Cluster scan is not passed to ferriskey as a regular command, so we need to handle it separately.
    // We send the command to a specific function in the ferriskey cluster client, which internally handles the
    // the complication of a command scan, and generate the command base on the logic in the ferriskey library.
    //
    // The function returns a tuple with the cursor and the keys found in the scan.
    // The cursor is not a regular cursor, but an ARC to a struct that contains the cursor and the data needed
    // to continue the scan called ScanState.
    // In order to avoid passing Rust GC to clean the ScanState when the cursor (ref) is passed to the wrapper,
    // which means that Rust layer is not aware of the cursor anymore, we need to keep the ScanState alive.
    // We do that by storing the ScanState in a global container, and return a cursor-id of the cursor to the wrapper.
    //
    // The wrapper create an object contain the cursor-id with a drop function that will remove the cursor from the container.
    // When the ref is removed from the hash-map, there's no more references to the ScanState, and the GC will clean it.
    /// Returns `true` when the underlying connection is in
    /// cluster mode. Cheap: acquires the read-lock on the
    /// already-initialized [`ClientWrapper`] and matches the
    /// variant.
    pub async fn is_cluster(&self) -> bool {
        let guard = self.internal_client.read().await;
        matches!(&*guard, ClientWrapper::Cluster { .. })
    }

    /// Force an immediate refresh of the cluster slot map.
    ///
    /// No-op in standalone mode; callers typically dispatch this
    /// after observing a stale-topology symptom (e.g. a `READONLY`
    /// error from a write that was routed to a former-primary that
    /// has been demoted to a replica) before retrying the command.
    pub async fn force_cluster_slot_refresh(&self) -> Result<()> {
        // Clone the `ClusterConnection` under the lock so we don't
        // hold the `internal_client` read guard across the network
        // refresh: a writer (e.g. `update_connection_password`) that
        // queues during the refresh would otherwise starve every
        // subsequent reader until topology resolution completes.
        // `ClusterConnection` is `Clone` and the clone is cheap (mpsc
        // sender + `Arc<InnerCore>`).
        let cluster = {
            let guard = self.internal_client.read().await;
            match &*guard {
                ClientWrapper::Standalone(_) => return Ok(()),
                ClientWrapper::Cluster { client } => client.clone(),
            }
        };
        cluster.force_slot_refresh().await
    }

    /// Typed-tuple variant of [`Self::cluster_scan`] for Rust
    /// callers that want direct access to the next scan state and
    /// key list without the language-binding cursor-id container
    /// shim. Errors on standalone.
    pub async fn cluster_scan_typed<'a>(
        &'a mut self,
        scan_state_cursor: &'a ScanStateRC,
        cluster_scan_args: ClusterScanArgs,
    ) -> Result<(ScanStateRC, Vec<Value>)> {
        let scan_state_cursor_clone = scan_state_cursor.clone();
        let cluster_scan_args_clone = cluster_scan_args.clone();
        let client = self.get_or_initialize_client().await?;
        match client {
            ClientWrapper::Standalone(_) => Err(Error::from((
                ErrorKind::InvalidClientConfig,
                "Cluster scan is not supported in standalone mode",
            ))),
            ClientWrapper::Cluster { mut client } => {
                client
                    .cluster_scan(scan_state_cursor_clone, cluster_scan_args_clone)
                    .await
            }
        }
    }

    pub async fn cluster_scan<'a>(
        &'a mut self,
        scan_state_cursor: &'a ScanStateRC,
        cluster_scan_args: ClusterScanArgs,
    ) -> Result<Value> {
        // Clone arguments before the async block (ScanStateRC is Arc, clone is cheap)
        let scan_state_cursor_clone = scan_state_cursor.clone();
        let cluster_scan_args_clone = cluster_scan_args.clone(); // Assuming ClusterScanArgs is Clone

        // Check and initialize if lazy *inside* the async block
        let client = self.get_or_initialize_client().await?;

        match client {
            ClientWrapper::Standalone(_) => {
                Err(crate::value::Error::from((
                    crate::value::ErrorKind::InvalidClientConfig,
                    "Cluster scan is not supported in standalone mode",
                )))
            }
            ClientWrapper::Cluster { mut client } => {
                let (cursor, keys) = client
                    .cluster_scan(scan_state_cursor_clone, cluster_scan_args_clone) // Use clones
                    .await?;
                let cluster_cursor_id = if cursor.is_finished() {
                    Value::BulkString(FINISHED_SCAN_CURSOR.into()) // Use constant
                } else {
                    Value::BulkString(insert_cluster_scan_cursor(cursor).into())
                };
                Ok(Value::Array(vec![Ok(cluster_cursor_id), Ok(Value::Array(keys.into_iter().map(Ok).collect()))]))
            }
            // Lazy case is now handled by the initial check
        }
    }

    fn get_transaction_values(
        pipeline: &crate::pipeline::Pipeline,
        mut values: Vec<Result<Value>>,
        command_count: usize,
        offset: usize,
        raise_on_error: bool,
    ) -> Result<Value> {
        if values.len() != 1 {
            return Err((
                ErrorKind::ResponseError,
                "Expected single transaction result",
            )
                .into());
        }
        let value = values.pop();
        let values = match value {
            Some(Ok(Value::Array(values))) => values,
            Some(Ok(Value::Nil)) => {
                return Ok(Value::Nil);
            }
            Some(Err(e)) => return Err(e),
            Some(Ok(value)) => {
                if offset == 2 {
                    vec![Ok(value)]
                } else {
                    return Err((
                        ErrorKind::ResponseError,
                        "Received non-array response for transaction",
                        format!("(response was {:?})", get_value_type(&value)),
                    )
                        .into());
                }
            }
            _ => {
                return Err((
                    ErrorKind::ResponseError,
                    "Received empty response for transaction",
                )
                    .into());
            }
        };
        Self::convert_pipeline_values_to_expected_types(
            pipeline,
            values,
            command_count,
            raise_on_error,
        )
    }

    fn convert_pipeline_values_to_expected_types(
        pipeline: &crate::pipeline::Pipeline,
        values: Vec<Result<Value>>,
        command_count: usize,
        raise_on_error: bool,
    ) -> Result<Value> {
        let mut results: Vec<Result<Value>> = Vec::with_capacity(command_count);
        for (value, expected_type) in values.into_iter().zip(
            pipeline
                .cmd_iter()
                .map(|cmd| expected_type_for_cmd(cmd.as_ref())),
        ) {
            match value {
                Ok(val) => {
                    let val = if raise_on_error {
                        val.extract_error()?
                    } else {
                        val
                    };
                    results.push(Ok(convert_to_expected_type(val, expected_type)?));
                }
                Err(e) => {
                    if raise_on_error {
                        return Err(e);
                    }
                    results.push(Err(e));
                }
            }
        }
        Ok(Value::Array(results))
    }

    /// Send a pipeline to the server.
    /// Transaction is a batch of commands that are sent in a single request.
    /// Unlike a pipelines, transactions are atomic, and in cluster mode, the key-based commands must route to the same slot.
    pub fn send_transaction<'a>(
        &'a mut self,
        pipeline: &'a crate::pipeline::Pipeline,
        routing: Option<RoutingInfo>,
        transaction_timeout: Option<u32>,
        raise_on_error: bool,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let client = self.get_or_initialize_client().await?;

            let command_count = pipeline.cmd_iter().count();
            // The offset is set to command_count + 1 to account for:
            // 1. The first command, which is the "MULTI" command, that returns "OK"
            // 2. The "QUEUED" responses for each of the commands in the pipeline (before EXEC)
            // After these initial responses (OK and QUEUED), we expect a single response,
            // which is an array containing the results of all the commands in the pipeline.
            let offset = command_count + 1;

            run_with_timeout(
                Some(to_duration(transaction_timeout, self.request_timeout)),
                async move {
                    match client {
                        ClientWrapper::Standalone(mut client) => {
                            let values = client.send_pipeline(pipeline, offset, 1).await?;
                            Client::get_transaction_values(
                                pipeline,
                                values,
                                command_count,
                                offset,
                                raise_on_error,
                            )
                        }
                        ClientWrapper::Cluster { mut client } => {
                            let values = match routing {
                                Some(RoutingInfo::SingleNode(route)) => {
                                    client
                                        .route_pipeline(pipeline, offset, 1, Some(route), None)
                                        .await?
                                }
                                _ => {
                                    client
                                        .req_packed_commands(pipeline, offset, 1, None)
                                        .await?
                                }
                            };
                            Client::get_transaction_values(
                                pipeline,
                                values,
                                command_count,
                                offset,
                                raise_on_error,
                            )
                        }
                    }
                },
            )
            .await
        })
    }

    /// Send a pipeline to the server.
    /// Pipeline is a batch of commands that are sent in a single request.
    /// Unlike a transaction, the commands are not executed atomically, and in cluster mode, the commands can be sent to different nodes.
    ///
    /// The `raise_on_error` parameter determines whether the pipeline should raise an error if any of the commands in the pipeline fail, or return the error as part of the response.
    /// - `pipeline_retry_strategy`: Configures the retry behavior for pipeline commands.
    ///   - If `retry_server_error` is `true`, failed commands with a retriable `RetryMethod` will be retried,
    ///     potentially causing reordering within the same slot.
    ///     ⚠️ **Caution**: This may lead to commands being executed in a different order than originally sent,
    ///     which could affect operations that rely on strict execution sequence.
    ///   - If `retry_connection_error` is `true`, sub-pipeline requests will be retried on connection errors.
    ///     ⚠️ **Caution**: Retrying after a connection error may result in duplicate executions, since the server might have already received and processed the request before the error occurred.
    pub fn send_pipeline<'a>(
        &'a mut self,
        pipeline: &'a crate::pipeline::Pipeline,
        routing: Option<RoutingInfo>,
        raise_on_error: bool,
        pipeline_timeout: Option<u32>,
        pipeline_retry_strategy: PipelineRetryStrategy,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let client = self.get_or_initialize_client().await?;

            let command_count = pipeline.cmd_iter().count();
            if pipeline.is_empty() {
                return Err(Error::from((
                    ErrorKind::ResponseError,
                    "Received empty pipeline",
                )));
            }

            run_with_timeout(
                Some(to_duration(pipeline_timeout, self.request_timeout)),
                async move {
                    let values = match client {
                        ClientWrapper::Standalone(mut client) => {
                            client.send_pipeline(pipeline, 0, command_count).await
                        }

                        ClientWrapper::Cluster { mut client } => match routing {
                            Some(RoutingInfo::SingleNode(route)) => {
                                client
                                    .route_pipeline(
                                        pipeline,
                                        0,
                                        command_count,
                                        Some(route),
                                        Some(pipeline_retry_strategy),
                                    )
                                    .await
                            }
                            _ => {
                                client
                                    .route_pipeline(
                                        pipeline,
                                        0,
                                        command_count,
                                        None,
                                        Some(pipeline_retry_strategy),
                                    )
                                    .await
                            }
                        },
                    }?;

                    Client::convert_pipeline_values_to_expected_types(
                        pipeline,
                        values,
                        command_count,
                        raise_on_error,
                    )
                },
            )
            .await
        })
    }

    pub async fn invoke_script<'a>(
        &'a mut self,
        hash: &'a str,
        keys: &[&[u8]],
        args: &[&[u8]],
        routing: Option<RoutingInfo>,
    ) -> crate::value::Result<Value> {
        let _ = self.get_or_initialize_client().await?;

        let mut eval = eval_cmd(hash, keys, args);
        let result = self.send_command(&mut eval, routing.clone()).await;
        let Err(err) = result else {
            return result;
        };
        if err.kind() == ErrorKind::NoScriptError {
            let Some(code) = get_script(hash) else {
                return Err(err);
            };
            let mut load = load_cmd(&code);
            self.send_command(&mut load, None).await?;
            self.send_command(&mut eval, routing).await
        } else {
            Err(err)
        }
    }

    /// Reserve an inflight slot, returning a tracker whose Drop releases it.
    /// Returns `None` if no slots available.
    pub fn reserve_inflight_request(&self) -> Option<crate::value::InflightRequestTracker> {
        crate::value::InflightRequestTracker::try_new(self.inflight_requests_allowed.clone())
    }

    /// Returns the current number of available inflight slots.
    /// For testing/observability — the inflight limit minus this value equals
    /// the number of commands currently held by the internal pipeline.
    pub fn available_inflight_count(&self) -> isize {
        self.inflight_requests_allowed.load(Ordering::Relaxed)
    }

    /// Update the password used to authenticate with the servers.
    /// If None is passed, the password will be removed.
    /// If `immediate_auth` is true, the password will be used to authenticate with the servers immediately using the `AUTH` command.
    /// The default behavior is to update the password without authenticating immediately.
    /// If the password is empty or None, and `immediate_auth` is true, the password will be updated and an error will be returned.
    pub async fn update_connection_password(
        &mut self,
        password: Option<String>,
        immediate_auth: bool,
    ) -> Result<Value> {
        let timeout = self.request_timeout;
        // The password update operation is wrapped in a timeout to prevent it from blocking indefinitely.
        // If the operation times out, an error is returned.
        // Since the password update operation is not a command that go through the regular command pipeline,
        // it is not have the regular timeout handling, as such we need to handle it separately.
        match tokio::time::timeout(timeout, async {
            let mut client = self.get_or_initialize_client().await?;
            match client {
                ClientWrapper::Standalone(ref mut client) => {
                    client.update_connection_password(password.clone()).await
                }
                ClientWrapper::Cluster { ref mut client } => {
                    client.update_connection_password(password.clone()).await
                }
            }
        })
        .await
        {
            Ok(result) => {
                if immediate_auth {
                    self.send_immediate_auth(password).await
                } else {
                    result
                }
            }
            Err(_elapsed) => Err(Error::from((
                ErrorKind::IoError,
                "Password update operation timed out, please check the connection",
            ))),
        }
    }

    /// Send AUTH command using IAM token (preferred) or the provided password
    async fn send_immediate_auth(&mut self, password: Option<String>) -> Result<Value> {
        // Determine the password to use for authentication
        let pass = if let Some(ref password) = password {
            if password.is_empty() {
                return Err(Error::from((
                    ErrorKind::UserOperationError,
                    "Empty password provided for authentication",
                )));
            }
            tracing::debug!("send_immediate_auth - Using password for authentication");
            password.to_string()
        } else {
            return Err(Error::from((
                ErrorKind::UserOperationError,
                "No password provided for authentication",
            )));
        };

        let routing = RoutingInfo::MultiNode((
            MultipleNodeRoutingInfo::AllNodes,
            Some(ResponsePolicy::AllSucceeded),
        ));

        let username = self.get_username().await.ok().flatten();

        let mut cmd = crate::cmd::cmd("AUTH");
        if let Some(username) = username {
            cmd.arg(&username);
        }
        cmd.arg(pass);
        self.send_command(&mut cmd, Some(routing)).await
    }

    /// Returns the username if one was configured during client creation. Otherwise, returns None.
    pub async fn get_username(&mut self) -> Result<Option<String>> {
        let client = self.get_or_initialize_client().await?;

        match client {
            ClientWrapper::Cluster { mut client } => match client.get_username().await {
                Ok(Value::SimpleString(username)) => Ok(Some(username)),
                Ok(Value::Nil) => Ok(None),
                Ok(other) => Err(Error::from((
                    ErrorKind::ClientError,
                    "Unexpected type",
                    format!("Expected SimpleString or Nil, got: {other:?}"),
                ))),
                Err(e) => Err(Error::from((
                    ErrorKind::ResponseError,
                    "Error getting username",
                    format!("Received error - {e:?}."),
                ))),
            },
            ClientWrapper::Standalone(client) => Ok(client.get_username()),
        }
    }

    /// Create an `IAMTokenManager` when IAM auth is configured.
    ///
    /// Client retrieves tokens on-demand during command execution.
    /// Only compiled when built with `feature = "iam"`.
    #[cfg(feature = "iam")]
    async fn create_iam_token_manager(
        auth_info: &crate::client::types::AuthenticationInfo,
    ) -> Option<std::sync::Arc<crate::iam::IAMTokenManager>> {
        if let Some(iam_config) = &auth_info.iam_config {
            if let Some(username) = &auth_info.username {
                match crate::iam::IAMTokenManager::new(
                    iam_config.cluster_name.clone(),
                    username.clone(),
                    iam_config.region.clone(),
                    iam_config.service_type,
                    iam_config.refresh_interval_seconds,
                )
                .await
                {
                    Ok(mut token_manager) => {
                        token_manager.start_refresh_task();
                        Some(std::sync::Arc::new(token_manager))
                    }
                    Err(e) => {
                        tracing::error!("IAM - Failed to create IAM token manager: {e}");
                        None
                    }
                }
            } else {
                tracing::error!("IAM - IAM authentication requires a username");
                None
            }
        } else {
            None
        }
    }

    /// Manually refresh the IAM token and update connection authentication.
    ///
    /// This method generates a new IAM token using the configured IAM
    /// token manager and immediately authenticates all connections
    /// with the new token. Only available when built with
    /// `feature = "iam"`.
    ///
    /// # Returns
    /// - `Ok(())` if the token was successfully refreshed and authentication succeeded
    /// - `Err(Error)` if no IAM token manager is configured, token generation fails,
    ///   or authentication with the new token fails.
    #[cfg(feature = "iam")]
    pub async fn refresh_iam_token(&mut self) -> Result<()> {
        // Check if IAM token manager is available
        let iam_manager = self.iam_token_manager.as_ref().ok_or_else(|| {
            Error::from((
                ErrorKind::ClientError,
                "No IAM token manager configured - IAM token refresh requires IAM authentication to be enabled during client creation",
            ))
        })?;

        // Refresh the token using the IAM token manager
        iam_manager.refresh_token().await.map_err(|e| {
            Error::from((
                ErrorKind::ClientError,
                "IAM token refresh failed",
                e.to_string(),
            ))
        })?;
        Ok(())
    }
}
/// Trait for executing PubSub commands on the internal client wrapper
pub trait PubSubCommandApplier: Send + Sync {
    /// Send a subscription command (SUBSCRIBE, UNSUBSCRIBE, etc.)
    /// If routing is provided, use it; otherwise use default routing logic
    fn apply_pubsub_command<'a>(
        &'a mut self,
        cmd: &'a mut Cmd,
        routing: Option<SingleNodeRoutingInfo>,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;
}

/// Implement the trait for ClientWrapper
impl PubSubCommandApplier for ClientWrapper {
    fn apply_pubsub_command<'a>(
        &'a mut self,
        cmd: &'a mut Cmd,
        routing: Option<SingleNodeRoutingInfo>,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            match self {
                ClientWrapper::Standalone(client) => {
                    // For standalone mode, send unsubscribe commands to all nodes.
                    // This handles ElastiCache scenarios where DNS address could change
                    // So we can't know which node we subscribed to
                    if let Some(command) = cmd.command() {
                        let cmd_upper = command.to_ascii_uppercase();
                        if cmd_upper == b"UNSUBSCRIBE" || cmd_upper == b"PUNSUBSCRIBE" {
                            return client
                                .send_request_to_all_nodes(cmd, Some(ResponsePolicy::AllSucceeded))
                                .await;
                        }
                    }
                    client.send_command(cmd).await
                }
                ClientWrapper::Cluster { client } => {
                    let final_routing = routing
                        .map(RoutingInfo::SingleNode)
                        .or_else(|| RoutingInfo::for_routable(cmd))
                        .unwrap_or(RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random));
                    client.route_command(cmd, final_routing).await
                }
            }
        })
    }
}

fn load_cmd(code: &[u8]) -> Cmd {
    let mut cmd = crate::cmd::cmd("SCRIPT");
    cmd.arg("LOAD").arg(code);
    cmd
}

fn eval_cmd(hash: &str, keys: &[&[u8]], args: &[&[u8]]) -> Cmd {
    let mut cmd = crate::cmd::cmd("EVALSHA");
    cmd.arg(hash).arg(keys.len());
    for key in keys {
        cmd.arg(key);
    }
    for arg in args {
        cmd.arg(arg);
    }
    cmd
}

pub(crate) fn to_duration(time_in_millis: Option<u32>, default: Duration) -> Duration {
    time_in_millis
        .map(|val| Duration::from_millis(val as u64))
        .unwrap_or(default)
}

async fn create_cluster_client(
    request: ConnectionRequest,
    push_sender: Option<mpsc::UnboundedSender<PushInfo>>,
    #[cfg(feature = "iam")] iam_token_manager: Option<&Arc<crate::iam::IAMTokenManager>>,
    pubsub_synchronizer: Arc<dyn crate::pubsub::PubSubSynchronizer>,
) -> Result<crate::cluster::ClusterConnection> {
    let tls_mode = request.tls_mode.unwrap_or_default();

    #[cfg(feature = "iam")]
    let valkey_connection_info = get_valkey_connection_info(&request, iam_token_manager).await;
    #[cfg(not(feature = "iam"))]
    let valkey_connection_info = get_valkey_connection_info(&request).await;

    let has_root_certs = !request.root_certs.is_empty();
    let has_client_cert = !request.client_cert.is_empty();
    let has_client_key = !request.client_key.is_empty();
    if has_client_cert != has_client_key {
        return Err(Error::from((
            ErrorKind::InvalidClientConfig,
            "client_cert and client_key must both be provided or both be empty",
        )));
    }

    let (tls_params, tls_certificates) = if has_root_certs || has_client_cert || has_client_key {
        if tls_mode == TlsMode::NoTls {
            return Err(Error::from((
                ErrorKind::InvalidClientConfig,
                "TLS certificates provided but TLS is disabled",
            )));
        }

        let root_cert = if has_root_certs {
            let mut combined_certs = Vec::new();
            for cert in &request.root_certs {
                if cert.is_empty() {
                    return Err(Error::from((
                        ErrorKind::InvalidClientConfig,
                        "Root certificate cannot be empty byte string",
                    )));
                }
                combined_certs.extend_from_slice(cert);
            }
            Some(combined_certs)
        } else {
            None
        };

        let client_tls = if has_client_cert && has_client_key {
            Some(crate::connection::tls::ClientTlsConfig {
                client_cert: request.client_cert.clone(),
                client_key: request.client_key.clone(),
            })
        } else {
            None
        };

        let tls_certs = TlsCertificates {
            client_tls,
            root_cert,
        };
        let params = retrieve_tls_certificates(tls_certs.clone())?;
        (Some(params), Some(tls_certs))
    } else {
        (None, None)
    };
    let periodic_topology_checks = match request.periodic_checks {
        Some(PeriodicCheck::Disabled) => None,
        Some(PeriodicCheck::Enabled) => Some(DEFAULT_PERIODIC_TOPOLOGY_CHECKS_INTERVAL),
        Some(PeriodicCheck::ManualInterval(interval)) => Some(interval),
        None => Some(DEFAULT_PERIODIC_TOPOLOGY_CHECKS_INTERVAL),
    };
    let connection_timeout = request.get_connection_timeout();
    let initial_nodes: Vec<_> = request
        .addresses
        .into_iter()
        .map(|address| {
            get_connection_info(
                &address,
                tls_mode,
                valkey_connection_info.clone(),
                tls_params.clone(),
            )
        })
        .collect();

    let mut builder = crate::cluster::compat::ClusterClientBuilder::new(initial_nodes)
        .connection_timeout(connection_timeout)
        .retries(DEFAULT_RETRIES);
    let read_from_strategy = request.read_from.unwrap_or_default();
    builder = builder.read_from(match read_from_strategy {
        ReadFrom::AZAffinity(az) => ReadFromReplicaStrategy::AZAffinity(az),
        ReadFrom::AZAffinityReplicasAndPrimary(az) => {
            ReadFromReplicaStrategy::AZAffinityReplicasAndPrimary(az)
        }
        ReadFrom::PreferReplica => ReadFromReplicaStrategy::RoundRobin,
        ReadFrom::Primary => ReadFromReplicaStrategy::AlwaysFromPrimary,
    });
    if let Some(interval_duration) = periodic_topology_checks {
        builder = builder.periodic_topology_checks(interval_duration);
    }
    builder = builder.use_protocol(request.protocol.unwrap_or_default());
    builder = builder.database_id(valkey_connection_info.db);
    if let Some(client_name) = valkey_connection_info.client_name {
        builder = builder.client_name(client_name);
    }
    if let Some(lib_name) = valkey_connection_info.lib_name {
        builder = builder.lib_name(lib_name);
    }
    if tls_mode != TlsMode::NoTls {
        let tls = if tls_mode == TlsMode::SecureTls {
            crate::connection::info::TlsMode::Secure
        } else {
            crate::connection::info::TlsMode::Insecure
        };
        builder = builder.tls(tls);
        if let Some(certs) = tls_certificates {
            builder = builder.certs(certs);
        }
    }

    let retry_strategy = match request.connection_retry_strategy {
        Some(strategy) => RetryStrategy::new(
            strategy.exponent_base,
            strategy.factor,
            strategy.number_of_retries,
            strategy.jitter_percent,
        ),
        None => RetryStrategy::default(),
    };
    builder = builder.reconnect_retry_strategy(retry_strategy);

    builder =
        builder.refresh_topology_from_initial_nodes(request.refresh_topology_from_initial_nodes);

    builder = builder.tcp_nodelay(request.tcp_nodelay);

    // Always use with Ferriskey
    builder = builder.periodic_connections_checks(Some(CONNECTION_CHECKS_INTERVAL));

    let client = builder.build()?;
    #[cfg(feature = "iam")]
    let iam_token_provider: Option<Arc<dyn crate::connection::factory::IAMTokenProvider>> =
        iam_token_manager.map(|manager| {
            Arc::new(manager.get_token_handle())
                as Arc<dyn crate::connection::factory::IAMTokenProvider>
        });
    #[cfg(not(feature = "iam"))]
    let iam_token_provider: Option<Arc<dyn crate::connection::factory::IAMTokenProvider>> = None;

    let mut con = client
        .get_async_connection(push_sender, Some(pubsub_synchronizer), iam_token_provider)
        .await?;

    // This validation ensures that sharded subscriptions are not applied to Valkey engines older than version 7.0,
    // preventing scenarios where the client becomes inoperable or, worse, unaware that sharded pubsub messages are not being received.
    // The issue arises because `client.get_async_connection()` might succeed even if the engine does not support sharded pubsub.
    // For example, initial connections may exclude the target node for sharded subscriptions, allowing the creation to succeed,
    // but subsequent resubscription tasks will fail when `setup_connection()` cannot establish a connection to the node.
    //
    // One approach to handle this would be to check the engine version inside `setup_connection()` and skip applying sharded subscriptions.
    // However, this approach would leave the application unaware that the subscriptions were not applied, requiring the user to analyze logs to identify the issue.
    // Instead, we explicitly check the engine version here and fail the connection creation if it is incompatible with sharded subscriptions.

    if let Some(pubsub_subscriptions) = &request.pubsub_subscriptions
        && pubsub_subscriptions
            .contains_key(&crate::connection::info::PubSubSubscriptionKind::Sharded)
    {
        let info_res = con
            .route_command(
                crate::cmd::cmd("INFO").arg("SERVER"),
                RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random),
            )
            .await?;
        let info_dict: InfoDict = FromValue::from_value(&info_res)?;
        match info_dict.get::<String>("valkey_version") {
            Some(version) => match (Versioning::new(version), Versioning::new("7.0")) {
                (Some(server_ver), Some(min_ver)) => {
                    if server_ver < min_ver {
                        return Err(Error::from((
                            ErrorKind::InvalidClientConfig,
                            "Sharded subscriptions provided, but the engine version is < 7.0",
                        )));
                    }
                }
                _ => {
                    return Err(Error::from((
                        ErrorKind::ResponseError,
                        "Failed to parse engine version",
                    )));
                }
            },
            _ => {
                return Err(Error::from((
                    ErrorKind::ResponseError,
                    "Could not determine engine version from INFO result",
                )));
            }
        }
    }
    Ok(con)
}

fn format_optional_value<T>(name: &'static str, value: Option<T>) -> String
where
    T: std::fmt::Display,
{
    if let Some(value) = value {
        format!("\n{name}: {value}")
    } else {
        String::new()
    }
}

fn sanitized_request_string(request: &ConnectionRequest) -> String {
    let addresses = request
        .addresses
        .iter()
        .map(|address| format!("{}:{}", address.host, address.port))
        .collect::<Vec<_>>()
        .join(", ");
    let tls_mode = request
        .tls_mode
        .map(|tls_mode| {
            format!(
                "\nTLS mode: {}",
                match tls_mode {
                    TlsMode::NoTls => "No TLS",
                    TlsMode::SecureTls => "Secure",
                    TlsMode::InsecureTls => "Insecure",
                }
            )
        })
        .unwrap_or_default();
    let cluster_mode = if request.cluster_mode_enabled {
        "\nCluster mode"
    } else {
        "\nStandalone mode"
    };
    let request_timeout = format!(
        "\nRequest timeout: {}",
        request
            .request_timeout
            .unwrap_or(DEFAULT_RESPONSE_TIMEOUT.as_millis() as u32)
    );
    let connection_timeout = format!(
        "\nConnection timeout: {}",
        request.get_connection_timeout().as_millis()
    );
    let database_id = format!("\ndatabase ID: {}", request.database_id);
    let rfr_strategy = request
        .read_from
        .clone()
        .map(|rfr| {
            format!(
                "\nRead from Replica mode: {}",
                match rfr {
                    ReadFrom::Primary => "Only primary",
                    ReadFrom::PreferReplica => "Prefer replica",
                    ReadFrom::AZAffinity(_) => "Prefer replica in user's availability zone",
                    ReadFrom::AZAffinityReplicasAndPrimary(_) =>
                        "Prefer replica and primary in user's availability zone",
                }
            )
        })
        .unwrap_or_default();
    let connection_retry_strategy = request.connection_retry_strategy.as_ref().map(|strategy|
            format!("\nreconnect backoff strategy: number of increasing duration retries: {}, base: {}, factor: {}, jitter: {:?}",
        strategy.number_of_retries, strategy.exponent_base, strategy.factor, strategy.jitter_percent)).unwrap_or_default();
    let protocol = request
        .protocol
        .map(|protocol| format!("\nProtocol: {protocol:?}"))
        .unwrap_or_default();
    let client_name = request
        .client_name
        .as_ref()
        .map(|client_name| format!("\nClient name: {client_name}"))
        .unwrap_or_default();
    let periodic_checks = if request.cluster_mode_enabled {
        match request.periodic_checks {
            Some(PeriodicCheck::Disabled) => "\nPeriodic Checks: Disabled".to_string(),
            Some(PeriodicCheck::Enabled) => format!(
                "\nPeriodic Checks: Enabled with default interval of {DEFAULT_PERIODIC_TOPOLOGY_CHECKS_INTERVAL:?}"
            ),
            Some(PeriodicCheck::ManualInterval(interval)) => format!(
                "\nPeriodic Checks: Enabled with manual interval of {:?}s",
                interval.as_secs()
            ),
            None => String::new(),
        }
    } else {
        String::new()
    };

    let pubsub_subscriptions = request
        .pubsub_subscriptions
        .as_ref()
        .map(|pubsub_subscriptions| format!("\nPubsub subscriptions: {pubsub_subscriptions:?}"))
        .unwrap_or_default();

    let inflight_requests_limit =
        format_optional_value("Inflight requests limit", request.inflight_requests_limit);

    format!(
        "\nAddresses: {addresses}{tls_mode}{cluster_mode}{request_timeout}{connection_timeout}{rfr_strategy}{connection_retry_strategy}{database_id}{protocol}{client_name}{periodic_checks}{pubsub_subscriptions}{inflight_requests_limit}",
    )
}

/// Create a compression manager from the given configuration
/// Returns None if compression is disabled or not configured
fn create_compression_manager(
    compression_config: Option<CompressionConfig>,
) -> std::result::Result<Option<Arc<CompressionManager>>, Error> {
    let Some(config) = compression_config else {
        return Ok(None);
    };

    if !config.enabled {
        return Ok(None);
    }

    let backend: Box<dyn crate::compression::CompressionBackend> = match config.backend {
        CompressionBackendType::Zstd => Box::new(ZstdBackend::new()),
        CompressionBackendType::Lz4 => Box::new(Lz4Backend::new()),
    };

    let manager = CompressionManager::new(backend, config).map_err(|e| {
        Error::from((
            ErrorKind::InvalidClientConfig,
            "Failed to create compression manager",
            e.to_string(),
        ))
    })?;

    Ok(Some(Arc::new(manager)))
}

impl Client {
    pub async fn new(
        request: ConnectionRequest,
        push_sender: Option<mpsc::UnboundedSender<PushInfo>>,
    ) -> std::result::Result<Self, Error> {
        // Add buffer to connection_timeout to allow inner connection logic to fully execute before the outer timeout triggers
        let client_creation_timeout = request.get_connection_timeout() + Duration::from_millis(500);

        tracing::info!("Connection configuration - {}", sanitized_request_string(&request));
        let request_timeout = to_duration(request.request_timeout, DEFAULT_RESPONSE_TIMEOUT);
        let blocking_cmd_timeout_extension = request
            .blocking_cmd_timeout_extension
            .unwrap_or(DEFAULT_DEFAULT_EXT_SECS);
        let inflight_requests_limit = request
            .inflight_requests_limit
            .unwrap_or(DEFAULT_MAX_INFLIGHT_REQUESTS);
        let inflight_requests_allowed = Arc::new(AtomicIsize::new(
            inflight_requests_limit
                .try_into()
                .expect("inflight limit exceeds isize::MAX"),
        ));

        // Create compression manager from configuration
        let compression_manager = create_compression_manager(request.compression_config.clone())?;

        let reconciliation_interval = match request.pubsub_reconciliation_interval_ms {
            Some(ms) if ms > 0 => Some(Duration::from_millis(ms as u64)),
            _ => None,
        };

        // Lazy-connect is no longer supported through `Client::new`.
        // The dedicated `LazyClient` type (built via
        // `ClientBuilder::build_lazy`) owns the connect-on-first-use
        // path. Failing loud here ensures callers who still set the
        // flag migrate explicitly rather than picking up a silent
        // behaviour change.
        if request.lazy_connect {
            return Err(Error::from((
                ErrorKind::InvalidClientConfig,
                "lazy_connect is no longer supported on Client::new",
                "Use ClientBuilder::build_lazy() -> LazyClient to defer connection.".to_string(),
            )));
        }

        tokio::time::timeout(client_creation_timeout, async move {
            let initial_subscriptions = request.pubsub_subscriptions.clone();

            // Build the synchronizer up front with an EMPTY weak
            // handle — we'll wire it to the real `internal_client_arc`
            // once the wrapper has been constructed. The placeholder
            // is safe because synchronizer code only `upgrade()`s the
            // weak when reconciling; startup callers don't.
            let pubsub_synchronizer = create_pubsub_synchronizer(
                push_sender.clone(),
                initial_subscriptions,
                request.cluster_mode_enabled,
                Weak::new(),
                reconciliation_interval,
                request_timeout,
            )
            .await;

            // Extract connection metadata for OTel span attributes.
            // Port 0 is normalized to the default (6379) for OTel reporting.
            let otel_metadata = types::OTelMetadata {
                address: request
                    .addresses
                    .first()
                    .map(|addr| types::NodeAddress {
                        host: addr.host.clone(),
                        port: get_port(addr),
                    })
                    .unwrap_or_else(|| types::NodeAddress {
                        host: "unknown".to_string(),
                        port: 6379,
                    }),
                db_namespace: request.database_id.to_string(),
            };

            // Create IAM token manager if needed — must happen
            // before `create_cluster_client` / `create_client` so
            // the token is available to the connect handshake.
            // Only compiled when built with `feature = "iam"`.
            #[cfg(feature = "iam")]
            let iam_token_manager = if let Some(auth_info) = &request.authentication_info {
                Self::create_iam_token_manager(auth_info).await
            } else {
                None
            };

            // Build the real wrapper — NO transient Lazy state.
            // This is the only place the wrapper is constructed
            // now; `Client::new` never returns a disconnected
            // handle.
            let internal_client = if request.cluster_mode_enabled {
                let client = create_cluster_client(
                    request,
                    push_sender,
                    #[cfg(feature = "iam")]
                    iam_token_manager.as_ref(),
                    pubsub_synchronizer.clone(),
                )
                .await?;
                ClientWrapper::Cluster { client }
            } else {
                ClientWrapper::Standalone(
                    StandaloneClient::create_client(
                        request,
                        push_sender,
                        #[cfg(feature = "iam")]
                        iam_token_manager.as_ref(),
                        Some(pubsub_synchronizer.clone()),
                    )
                    .await?,
                )
            };

            let internal_client_arc = Arc::new(RwLock::new(internal_client));

            // Now that the Arc exists and holds a connected wrapper,
            // wire the synchronizer's Weak handle so reconciliation
            // can reach the client.
            crate::pubsub::attach_internal_client(
                &pubsub_synchronizer,
                Arc::downgrade(&internal_client_arc),
            );

            let inflight_limit: isize = inflight_requests_limit
                .try_into()
                .expect("inflight limit exceeds isize::MAX");
            let inflight_log_interval = (inflight_limit / 10).max(1);

            let client = Self {
                internal_client: internal_client_arc,
                request_timeout,
                blocking_cmd_timeout_extension,
                inflight_requests_allowed,
                inflight_requests_limit: inflight_limit,
                inflight_log_interval,
                compression_manager,
                #[cfg(feature = "iam")]
                iam_token_manager,
                pubsub_synchronizer: pubsub_synchronizer.clone(),
                otel_metadata,
            };

            pubsub_synchronizer.trigger_reconciliation();
            if let Err(e) = pubsub_synchronizer.wait_for_sync(0, None, None, None).await {
                tracing::error!(
                    "Client::new - Failed to establish initial subscriptions within timeout: {e:?}"
                );
            }

            Ok(client)
        })
        .await
        .map_err(|_| Error::from(std::io::Error::from(std::io::ErrorKind::TimedOut)))?
    }

    /// Check if compression is enabled for this client
    ///
    /// # Returns
    /// * `true` if compression is enabled and configured
    /// * `false` if compression is disabled or not configured
    pub(crate) fn is_compression_enabled(&self) -> bool {
        self.compression_manager
            .as_ref()
            .map(|manager| manager.is_enabled())
            .unwrap_or(false)
    }

    /// Returns the initial connection address, used as the default
    /// OTel `server.address` span attribute.
    pub fn server_address(&self) -> &str {
        &self.otel_metadata.address.host
    }

    /// Returns the initial connection port, used as the default
    /// OTel `server.port` span attribute.
    pub fn server_port(&self) -> u16 {
        self.otel_metadata.address.port
    }

    pub fn db_namespace(&self) -> &str {
        &self.otel_metadata.db_namespace
    }
}

pub trait ValkeyClientForTests {
    fn send_command<'a>(
        &'a mut self,
        cmd: &'a mut Cmd,
        routing: Option<RoutingInfo>,
    ) -> BoxFuture<'a, Result<Value>>;
}

impl ValkeyClientForTests for Client {
    fn send_command<'a>(
        &'a mut self,
        cmd: &'a mut Cmd,
        routing: Option<RoutingInfo>,
    ) -> BoxFuture<'a, Result<Value>> {
        self.send_command(cmd, routing)
    }
}

impl ValkeyClientForTests for StandaloneClient {
    fn send_command<'a>(
        &'a mut self,
        cmd: &'a mut Cmd,
        _routing: Option<RoutingInfo>,
    ) -> BoxFuture<'a, Result<Value>> {
        self.send_command(cmd).boxed()
    }
}

// This is used for pubsub tests
impl ValkeyClientForTests for ClusterConnection {
    fn send_command<'a>(
        &'a mut self,
        cmd: &'a mut crate::cmd::Cmd,
        routing: Option<RoutingInfo>,
    ) -> BoxFuture<'a, Result<Value>> {
        let final_routing =
            routing.unwrap_or(RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random));

        async move { self.route_command(cmd, final_routing).await }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::cmd::Cmd;

    use crate::client::{
        DEFAULT_DEFAULT_EXT_SECS, RequestTimeoutOption, TimeUnit, get_request_timeout,
    };

    use super::{Client, get_timeout_from_cmd_arg};

    /// Default extension as seconds (f64) — tests compare against
    /// `Duration::from_secs_f64` expressions.
    const DEFAULT_EXT_SECS: f64 = 0.5;
    const DEFAULT_EXT: Duration = DEFAULT_DEFAULT_EXT_SECS;

    #[test]
    fn test_get_timeout_from_cmd_returns_correct_duration_int() {
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP").arg("key1").arg("key2").arg("5");
        let result = get_timeout_from_cmd_arg(
            &cmd,
            cmd.args_iter().len() - 1,
            TimeUnit::Seconds,
            DEFAULT_EXT,
        );
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            RequestTimeoutOption::BlockingCommand(Duration::from_secs_f64(
                5.0 + DEFAULT_EXT_SECS
            ))
        );
    }

    #[test]
    fn test_get_timeout_from_cmd_returns_correct_duration_float() {
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP").arg("key1").arg("key2").arg(0.5);
        let result = get_timeout_from_cmd_arg(
            &cmd,
            cmd.args_iter().len() - 1,
            TimeUnit::Seconds,
            DEFAULT_EXT,
        );
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            RequestTimeoutOption::BlockingCommand(Duration::from_secs_f64(
                0.5 + DEFAULT_EXT_SECS
            ))
        );
    }

    #[test]
    fn test_get_timeout_from_cmd_returns_correct_duration_milliseconds() {
        let mut cmd = Cmd::new();
        cmd.arg("XREAD").arg("BLOCK").arg("500").arg("key");
        let result = get_timeout_from_cmd_arg(&cmd, 2, TimeUnit::Milliseconds, DEFAULT_EXT);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            RequestTimeoutOption::BlockingCommand(Duration::from_secs_f64(
                0.5 + DEFAULT_EXT_SECS
            ))
        );
    }

    #[test]
    fn test_get_timeout_from_cmd_returns_err_when_timeout_isnt_passed() {
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP").arg("key1").arg("key2").arg("key3");
        let result = get_timeout_from_cmd_arg(
            &cmd,
            cmd.args_iter().len() - 1,
            TimeUnit::Seconds,
            DEFAULT_EXT,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        println!("{err:?}");
        assert!(err.to_string().to_lowercase().contains("index"), "{err}");
    }

    #[test]
    fn test_get_timeout_from_cmd_returns_err_when_timeout_is_larger_than_u32_max() {
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP")
            .arg("key1")
            .arg("key2")
            .arg(u32::MAX as u64 + 1);
        let result = get_timeout_from_cmd_arg(
            &cmd,
            cmd.args_iter().len() - 1,
            TimeUnit::Seconds,
            DEFAULT_EXT,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        println!("{err:?}");
        assert!(err.to_string().to_lowercase().contains("u32"), "{err}");
    }

    #[test]
    fn test_get_timeout_from_cmd_returns_err_when_timeout_is_negative() {
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP").arg("key1").arg("key2").arg(-1);
        let result = get_timeout_from_cmd_arg(
            &cmd,
            cmd.args_iter().len() - 1,
            TimeUnit::Seconds,
            DEFAULT_EXT,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().to_lowercase().contains("negative"), "{err}");
    }

    #[test]
    fn test_get_timeout_from_cmd_returns_no_timeout_when_zero_is_passed() {
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP").arg("key1").arg("key2").arg(0);
        let result = get_timeout_from_cmd_arg(
            &cmd,
            cmd.args_iter().len() - 1,
            TimeUnit::Seconds,
            DEFAULT_EXT,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), RequestTimeoutOption::NoTimeout,);
    }

    #[test]
    fn test_get_request_timeout_with_blocking_command_returns_cmd_arg_timeout() {
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP").arg("key1").arg("key2").arg("500");
        let result = get_request_timeout(&cmd, Duration::from_millis(100), DEFAULT_EXT).unwrap();
        assert_eq!(
            result,
            Some(Duration::from_secs_f64(
                500.0 + DEFAULT_EXT_SECS
            ))
        );

        let mut cmd = Cmd::new();
        cmd.arg("XREADGROUP").arg("BLOCK").arg("500").arg("key");
        let result = get_request_timeout(&cmd, Duration::from_millis(100), DEFAULT_EXT).unwrap();
        assert_eq!(
            result,
            Some(Duration::from_secs_f64(
                0.5 + DEFAULT_EXT_SECS
            ))
        );

        let mut cmd = Cmd::new();
        cmd.arg("BLMPOP").arg("0.857").arg("key");
        let result = get_request_timeout(&cmd, Duration::from_millis(100), DEFAULT_EXT).unwrap();
        assert_eq!(
            result,
            Some(Duration::from_secs_f64(
                0.857 + DEFAULT_EXT_SECS
            ))
        );

        let mut cmd = Cmd::new();
        cmd.arg("WAIT").arg(1).arg("500");
        let result = get_request_timeout(&cmd, Duration::from_millis(500), DEFAULT_EXT).unwrap();
        assert_eq!(
            result,
            Some(Duration::from_secs_f64(
                0.5 + DEFAULT_EXT_SECS
            ))
        );

        // WAITAOF
        let mut cmd = Cmd::new();
        cmd.arg("WAITAOF").arg(1).arg(1).arg("500");
        let result = get_request_timeout(&cmd, Duration::from_millis(500), DEFAULT_EXT).unwrap();
        assert_eq!(
            result,
            Some(Duration::from_secs_f64(
                0.5 + DEFAULT_EXT_SECS
            ))
        );

        // Infinite block (0) — returns None (no client timeout)
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP").arg("key").arg("0");
        let result = get_request_timeout(&cmd, Duration::from_millis(100), DEFAULT_EXT).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_get_request_timeout_non_blocking_command_returns_default_timeout() {
        let mut cmd = Cmd::new();
        cmd.arg("SET").arg("key").arg("value").arg("PX").arg("500");
        let result = get_request_timeout(&cmd, Duration::from_millis(100), DEFAULT_EXT).unwrap();
        assert_eq!(result, Some(Duration::from_millis(100)));

        let mut cmd = Cmd::new();
        cmd.arg("XREADGROUP").arg("key");
        let result = get_request_timeout(&cmd, Duration::from_millis(100), DEFAULT_EXT).unwrap();
        assert_eq!(result, Some(Duration::from_millis(100)));
    }

    #[test]
    fn test_is_select_command_detects_valid_select_commands() {
        // Test detection of valid SELECT commands
        // Test uppercase SELECT command
        let mut cmd = Cmd::new();
        cmd.arg("SELECT").arg("1");
        assert!(Client::is_select_command(&cmd));

        // Test SELECT with different database IDs
        let mut cmd = Cmd::new();
        cmd.arg("SELECT").arg("0");
        assert!(Client::is_select_command(&cmd));
    }

    #[test]
    fn test_extract_database_id_from_select() {
        // Test detection of valid SELECT commands
        // Test uppercase SELECT command
        let mut cmd = Cmd::new();
        cmd.arg("SELECT").arg("1");
        assert_eq!(Client::extract_database_id_from_select(&cmd), Ok(1));

        // Test SELECT with different database IDs
        let mut cmd = Cmd::new();
        cmd.arg("SELECT").arg("0");
        assert_eq!(Client::extract_database_id_from_select(&cmd), Ok(0));
    }

    #[test]
    fn test_is_select_command_rejects_non_select_commands() {
        // Test rejection of non-SELECT commands
        // Test common Valkey commands
        let mut cmd = Cmd::new();
        cmd.arg("GET").arg("key");
        assert!(!Client::is_select_command(&cmd));

        let mut cmd = Cmd::new();
        cmd.arg("SET").arg("key").arg("value");
        assert!(!Client::is_select_command(&cmd));
    }

    #[test]
    fn test_is_select_command_case_normalization() {
        // Test that ferriskey normalizes commands to uppercase
        // Test lowercase select (ferriskey normalizes to uppercase, so this works too)
        let mut cmd = Cmd::new();
        cmd.arg("select").arg("1");
        assert!(Client::is_select_command(&cmd));
    }

    #[test]
    fn test_is_select_command_handles_empty_command() {
        // Test handling of empty or malformed commands
        // Test empty command
        let cmd = Cmd::new();
        assert!(!Client::is_select_command(&cmd));
    }

    #[test]
    fn test_is_client_set_name_command() {
        // Create a mock client for testing
        // Test valid CLIENT SETNAME command
        let mut cmd = Cmd::new();
        cmd.arg("CLIENT").arg("SETNAME").arg("test_client");
        assert!(Client::is_client_set_name_command(&cmd));

        // Test CLIENT SETNAME with different case (should work due to case-insensitive comparison)
        let mut cmd = Cmd::new();
        cmd.arg("client").arg("setname").arg("test_client");
        assert!(Client::is_client_set_name_command(&cmd));

        // Test CLIENT command without SETNAME
        let mut cmd = Cmd::new();
        cmd.arg("CLIENT").arg("INFO");
        assert!(!Client::is_client_set_name_command(&cmd));

        // Test non-CLIENT command
        let mut cmd = Cmd::new();
        cmd.arg("SET").arg("key").arg("value");
        assert!(!Client::is_client_set_name_command(&cmd));

        // Test CLIENT SETNAME without client name argument
        let mut cmd = Cmd::new();
        cmd.arg("CLIENT").arg("SETNAME");
        assert!(Client::is_client_set_name_command(&cmd));

        // Test CLIENT only
        let mut cmd = Cmd::new();
        cmd.arg("CLIENT");
        assert!(!Client::is_client_set_name_command(&cmd));
    }

    #[test]
    fn test_extract_client_name_from_client_set_name() {
        // Test detection of valid CLIENT SETNAME commands
        // Test uppercase CLIENT SETNAME command
        let mut cmd = Cmd::new();
        cmd.arg("CLIENT").arg("SETNAME").arg("test_name");
        assert_eq!(
            Client::extract_client_name_from_client_set_name(&cmd),
            Some("test_name".to_string())
        );
    }

    #[test]
    fn test_is_auth_command() {
        // Test valid AUTH command with password
        let mut cmd = Cmd::new();
        cmd.arg("AUTH").arg("password123");
        assert!(Client::is_auth_command(&cmd));

        // Test AUTH command with username and password
        let mut cmd = Cmd::new();
        cmd.arg("AUTH").arg("myuser").arg("password123");
        assert!(Client::is_auth_command(&cmd));

        // Test non-AUTH command
        let mut cmd = Cmd::new();
        cmd.arg("SET").arg("key").arg("value");
        assert!(!Client::is_auth_command(&cmd));
    }

    #[test]
    fn test_extract_auth_info() {
        // Test AUTH with password only
        let mut cmd = Cmd::new();
        cmd.arg("AUTH").arg("password123");
        let (username, password) = Client::extract_auth_info(&cmd);
        assert_eq!(username, None);
        assert_eq!(password, Some("password123".to_string()));

        // Test AUTH with username and password
        let mut cmd = Cmd::new();
        cmd.arg("AUTH").arg("myuser").arg("password123");
        let (username, password) = Client::extract_auth_info(&cmd);
        assert_eq!(username, Some("myuser".to_string()));
        assert_eq!(password, Some("password123".to_string()));

        // Test AUTH with no arguments (invalid)
        let mut cmd = Cmd::new();
        cmd.arg("AUTH");
        let (username, password) = Client::extract_auth_info(&cmd);
        assert_eq!(username, None);
        assert_eq!(password, None);
    }

    #[test]
    fn test_is_hello_command() {
        // Test valid HELLO command
        let mut cmd = Cmd::new();
        cmd.arg("HELLO").arg("3");
        assert!(Client::is_hello_command(&cmd));

        // Test HELLO with AUTH
        let mut cmd = Cmd::new();
        cmd.arg("HELLO")
            .arg("3")
            .arg("AUTH")
            .arg("user")
            .arg("pass");
        assert!(Client::is_hello_command(&cmd));

        // Test non-HELLO command
        let mut cmd = Cmd::new();
        cmd.arg("PING");
        assert!(!Client::is_hello_command(&cmd));
    }

    #[test]
    fn test_extract_hello_info() {
        // Test HELLO 3
        let mut cmd = Cmd::new();
        cmd.arg("HELLO").arg("3");
        let (protocol, username, password, client_name) = Client::extract_hello_info(&cmd);
        assert_eq!(protocol, Some(crate::value::ProtocolVersion::RESP3));
        assert_eq!(username, None);
        assert_eq!(password, None);
        assert_eq!(client_name, None);

        // Test HELLO 2
        let mut cmd = Cmd::new();
        cmd.arg("HELLO").arg("2");
        let (protocol, username, password, client_name) = Client::extract_hello_info(&cmd);
        assert_eq!(protocol, Some(crate::value::ProtocolVersion::RESP2));
        assert_eq!(username, None);
        assert_eq!(password, None);
        assert_eq!(client_name, None);

        // Test HELLO 3 AUTH username password
        let mut cmd = Cmd::new();
        cmd.arg("HELLO")
            .arg("3")
            .arg("AUTH")
            .arg("myuser")
            .arg("mypass");
        let (protocol, username, password, client_name) = Client::extract_hello_info(&cmd);
        assert_eq!(protocol, Some(crate::value::ProtocolVersion::RESP3));
        assert_eq!(username, Some("myuser".to_string()));
        assert_eq!(password, Some("mypass".to_string()));
        assert_eq!(client_name, None);

        // Test HELLO 3 SETNAME myclient
        let mut cmd = Cmd::new();
        cmd.arg("HELLO").arg("3").arg("SETNAME").arg("myclient");
        let (protocol, username, password, client_name) = Client::extract_hello_info(&cmd);
        assert_eq!(protocol, Some(crate::value::ProtocolVersion::RESP3));
        assert_eq!(username, None);
        assert_eq!(password, None);
        assert_eq!(client_name, Some("myclient".to_string()));

        // Test HELLO 3 AUTH user pass SETNAME myclient
        let mut cmd = Cmd::new();
        cmd.arg("HELLO")
            .arg("3")
            .arg("AUTH")
            .arg("myuser")
            .arg("mypass")
            .arg("SETNAME")
            .arg("myclient");
        let (protocol, username, password, client_name) = Client::extract_hello_info(&cmd);
        assert_eq!(protocol, Some(crate::value::ProtocolVersion::RESP3));
        assert_eq!(username, Some("myuser".to_string()));
        assert_eq!(password, Some("mypass".to_string()));
        assert_eq!(client_name, Some("myclient".to_string()));

        // Test HELLO with invalid protocol version
        let mut cmd = Cmd::new();
        cmd.arg("HELLO").arg("99");
        let (protocol, username, password, client_name) = Client::extract_hello_info(&cmd);
        assert_eq!(protocol, None);
        assert_eq!(username, None);
        assert_eq!(password, None);
        assert_eq!(client_name, None);
    }

    // ===== Edge case tests for blocking command timeout detection =====

    #[test]
    fn test_blocking_command_infinite_block_returns_none() {
        // BLPOP key 0 — infinite block → no client timeout
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP").arg("key").arg("0");
        assert_eq!(
            get_request_timeout(&cmd, Duration::from_millis(1000), DEFAULT_EXT).unwrap(),
            None
        );

        // XREAD BLOCK 0 — infinite block
        let mut cmd = Cmd::new();
        cmd.arg("XREAD")
            .arg("BLOCK")
            .arg("0")
            .arg("STREAMS")
            .arg("s1")
            .arg("$");
        assert_eq!(
            get_request_timeout(&cmd, Duration::from_millis(1000), DEFAULT_EXT).unwrap(),
            None
        );
    }

    #[test]
    fn test_blocking_timeout_extends_beyond_block_duration() {
        // BLPOP key 5 — blocks 5s, timeout should be 5s + extension
        let mut cmd = Cmd::new();
        cmd.arg("BLPOP").arg("key").arg("5");
        let result = get_request_timeout(&cmd, Duration::from_millis(1000), DEFAULT_EXT).unwrap();
        let expected = Duration::from_secs_f64(5.0 + DEFAULT_EXT_SECS);
        assert_eq!(result, Some(expected));
        assert!(expected > Duration::from_secs(5));
    }

    #[test]
    fn test_non_blocking_command_uses_default_timeout() {
        for cmd_name in &["SET", "GET", "DEL", "HGET", "LPUSH", "SADD", "PING"] {
            let mut cmd = Cmd::new();
            cmd.arg(*cmd_name).arg("key");
            let result = get_request_timeout(&cmd, Duration::from_millis(1000), DEFAULT_EXT).unwrap();
            assert_eq!(
                result,
                Some(Duration::from_millis(1000)),
                "{cmd_name} should use default timeout"
            );
        }
    }

    #[test]
    fn test_waitaof_detected_as_blocking() {
        let mut cmd = Cmd::new();
        cmd.arg("WAITAOF").arg(1).arg(1).arg("3000");
        let expected = Duration::from_secs_f64(3.0 + DEFAULT_EXT_SECS);
        assert_eq!(
            get_request_timeout(&cmd, Duration::from_millis(1000), DEFAULT_EXT).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn test_wait_detected_as_blocking() {
        let mut cmd = Cmd::new();
        cmd.arg("WAIT").arg(1).arg("5000");
        let expected = Duration::from_secs_f64(5.0 + DEFAULT_EXT_SECS);
        assert_eq!(
            get_request_timeout(&cmd, Duration::from_millis(1000), DEFAULT_EXT).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn test_xread_without_block_is_not_blocking() {
        let mut cmd = Cmd::new();
        cmd.arg("XREAD")
            .arg("COUNT")
            .arg("10")
            .arg("STREAMS")
            .arg("s1")
            .arg("$");
        assert_eq!(
            get_request_timeout(&cmd, Duration::from_millis(1000), DEFAULT_EXT).unwrap(),
            Some(Duration::from_millis(1000))
        );
    }

    #[test]
    fn test_blocking_fractional_seconds() {
        let mut cmd = Cmd::new();
        cmd.arg("BLMPOP").arg("0.857").arg("key");
        let expected = Duration::from_secs_f64(0.857 + DEFAULT_EXT_SECS);
        assert_eq!(
            get_request_timeout(&cmd, Duration::from_millis(100), DEFAULT_EXT).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn test_all_blocking_commands_detected() {
        let blocking_cmds: Vec<(&str, Vec<&str>)> = vec![
            ("BLPOP", vec!["key", "5"]),
            ("BRPOP", vec!["key", "5"]),
            ("BLMOVE", vec!["src", "dst", "LEFT", "RIGHT", "5"]),
            ("BZPOPMAX", vec!["key", "5"]),
            ("BZPOPMIN", vec!["key", "5"]),
            ("BRPOPLPUSH", vec!["src", "dst", "5"]),
            ("BLMPOP", vec!["5", "1", "key"]),
            ("BZMPOP", vec!["5", "1", "key", "MIN"]),
            ("WAIT", vec!["1", "5000"]),
            ("WAITAOF", vec!["1", "1", "5000"]),
        ];

        for (cmd_name, args) in blocking_cmds {
            let mut cmd = Cmd::new();
            cmd.arg(cmd_name);
            for a in &args {
                cmd.arg(*a);
            }
            let result = get_request_timeout(&cmd, Duration::from_millis(100), DEFAULT_EXT).unwrap();
            assert!(
                result.is_some(),
                "{cmd_name} should be detected as blocking"
            );
        }
    }

    #[test]
    fn test_blocking_extension_configurable() {
        // Same BLMPOP 5s command, three different extensions — the
        // client-side deadline tracks `server_timeout + extension`.
        let mut cmd = Cmd::new();
        cmd.arg("BLMPOP").arg("5").arg("1").arg("key");

        for ext_ms in [500u64, 1_000, 2_000] {
            let ext = Duration::from_millis(ext_ms);
            let result = get_request_timeout(&cmd, Duration::from_millis(100), ext).unwrap();
            let expected = Duration::from_secs_f64(5.0 + ext.as_secs_f64());
            assert_eq!(result, Some(expected), "ext={ext_ms}ms");
        }
    }
}
