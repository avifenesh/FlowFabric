use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::connection::info::{ConnectionInfo, IntoConnectionInfo};
use crate::connection::runtime;
use crate::connection::{DisconnectNotifier, MultiplexedConnection, ValkeyRuntime, connect_simple};
use crate::pubsub::push_manager::PushInfo;
use crate::pubsub::synchronizer_trait::PubSubSynchronizer;
use crate::retry_strategies::RetryStrategy;
use crate::value::{ProtocolVersion, Result};

/// The client type.
#[derive(Debug, Clone)]
pub struct Client {
    pub(crate) connection_info: ConnectionInfo,
}

/// The client acts as connector to the Valkey server.  By itself it does not
/// do much other than providing a convenient way to fetch a connection from
/// it.  In the future the plan is to provide a connection pool in the client.
///
/// When opening a client a URL in the following format should be used:
///
/// ```plain
/// redis://host:port/db
/// ```
///
/// Example usage::
///
/// ```rust,no_run,ignore
/// let client = ferriskey::Client::open("redis://127.0.0.1/").unwrap();
/// let con = client.get_connection(None).unwrap();
/// ```
impl Client {
    /// Connects to a valkey server and returns a client.  This does not
    /// actually open a connection yet but it does perform some basic
    /// checks on the URL that might make the operation fail.
    pub fn open<T: IntoConnectionInfo>(params: T) -> Result<Client> {
        Ok(Client {
            connection_info: params.into_connection_info()?,
        })
    }

    /// Returns a reference of client connection info object.
    pub fn get_connection_info(&self) -> &ConnectionInfo {
        &self.connection_info
    }
}

/// Ferriskey-specific connection options
#[derive(Clone, Default)]
pub struct FerrisKeyConnectionOptions {
    /// Queue for RESP3 notifications
    pub push_sender: Option<mpsc::UnboundedSender<PushInfo>>,
    /// Passive disconnect notifier
    pub disconnect_notifier: Option<Box<dyn DisconnectNotifier>>,
    /// If ReadFromReplica strategy is set to AZAffinity or AZAffinityReplicasAndPrimary, this parameter will be set to 'true'.
    /// In this case, an INFO command will be triggered in the connection's setup to update the connection's 'availability_zone' property.
    pub discover_az: bool,
    /// Connection timeout duration.
    ///
    /// This optional field sets the maximum duration to wait when attempting to establish
    /// a connection. If `None`, the connection will use `DEFAULT_CONNECTION_TIMEOUT`.
    pub connection_timeout: Option<Duration>,
    /// Retry strategy configuration for reconnect attempts.
    pub connection_retry_strategy: Option<RetryStrategy>,
    /// TCP_NODELAY socket option. When true, disables Nagle's algorithm for lower latency.
    /// When false, enables Nagle's algorithm to reduce network overhead.
    pub tcp_nodelay: bool,
    /// Optional PubSub synchronizer for managing subscription state
    pub pubsub_synchronizer: Option<Arc<dyn PubSubSynchronizer>>,
    /// Optional async callback that returns a valid IAM token for authentication.
    /// When set, the cluster reconnection loop will invoke this callback before each
    /// connection attempt to ensure the password uses fresh credentials.
    /// The callback should return `Some(token)` if a valid token is available,
    /// or `None` if token retrieval failed.
    pub iam_token_provider: Option<Arc<dyn IAMTokenProvider>>,
}

/// Trait for providing IAM tokens to the reconnection path.
/// Implemented by the IAM token handle in ferriskey.
#[async_trait::async_trait]
pub trait IAMTokenProvider: Send + Sync {
    /// Returns a valid token, refreshing it if expired.
    async fn get_valid_token(&self) -> Option<String>;
}

/// Sentinel value meaning "no timeout": effectively waits forever.
/// Used where a `Duration` is required but the caller does not want to impose a deadline.
pub(crate) const NO_TIMEOUT: std::time::Duration = std::time::Duration::MAX;

impl Client {
    /// Returns an async multiplexed connection from the client.
    pub async fn get_multiplexed_async_connection(
        &self,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> Result<MultiplexedConnection> {
        self.get_multiplexed_async_connection_with_timeouts(
            NO_TIMEOUT,
            NO_TIMEOUT,
            ferriskey_connection_options,
        )
        .await
    }

    /// Returns an async connection from the client.
    pub(crate) async fn get_multiplexed_async_connection_with_timeouts(
        &self,
        response_timeout: std::time::Duration,
        connection_timeout: std::time::Duration,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> Result<MultiplexedConnection> {
        let result = runtime::timeout(
            connection_timeout,
            self.get_multiplexed_async_connection_inner::<crate::connection::tokio::Tokio>(
                response_timeout,
                None,
                ferriskey_connection_options,
            ),
        )
        .await;

        match result {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(e)) => Err(e),
            Err(elapsed) => Err(elapsed.into()),
        }
        .map(|(conn, _ip)| conn)
    }

    pub(crate) async fn get_multiplexed_async_connection_inner<T>(
        &self,
        response_timeout: std::time::Duration,
        socket_addr: Option<SocketAddr>,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> Result<(MultiplexedConnection, Option<IpAddr>)>
    where
        T: ValkeyRuntime,
    {
        let conn_info = self.connection_info.clone();
        let (con, ip) = connect_simple::<T>(
            &conn_info,
            socket_addr,
            ferriskey_connection_options.tcp_nodelay,
        )
        .await?;
        let (connection, driver) = MultiplexedConnection::new_with_response_timeout(
            conn_info,
            con.boxed(),
            response_timeout,
            ferriskey_connection_options,
        )
        .await?;
        T::spawn(driver);
        Ok((connection, ip))
    }

    /// Updates the password in connection_info.
    pub fn update_password(&mut self, password: Option<String>) {
        self.connection_info.valkey.password = password;
    }

    /// Updates the database ID in connection_info.
    pub fn update_database(&mut self, database_id: i64) {
        self.connection_info.valkey.db = database_id;
    }

    /// Updates the client_name in connection_info.
    pub fn update_client_name(&mut self, client_name: Option<String>) {
        self.connection_info.valkey.client_name = client_name;
    }

    /// Updates the username in connection_info.
    pub fn update_username(&mut self, username: Option<String>) {
        self.connection_info.valkey.username = username;
    }

    /// Updates the protocol version in connection_info.
    pub fn update_protocol(&mut self, protocol: ProtocolVersion) {
        self.connection_info.valkey.protocol = protocol;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn regression_293_parse_ipv6_with_interface() {
        assert!(Client::open(("fe80::cafe:beef%eno1", 6379)).is_ok());
    }

    #[test]
    fn test_update_database() {
        // Test database ID updates with valid value (1)
        let mut client = Client::open("redis://127.0.0.1/0").unwrap();

        // Verify initial database is 0
        assert_eq!(client.connection_info.valkey.db, 0);

        // Update to database 1
        client.update_database(1);

        // Verify database was updated
        assert_eq!(client.connection_info.valkey.db, 1);
    }
}
