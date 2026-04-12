use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::connection::info::{ConnectionInfo, IntoConnectionInfo};
use crate::connection::runtime;
use crate::connection::tls::{TlsCertificates, inner_build_with_tls};
use crate::connection::{DisconnectNotifier, MultiplexedConnection, RedisRuntime, connect_simple};
use crate::pubsub::push_manager::PushInfo;
use crate::pubsub::synchronizer_trait::PubSubSynchronizer;
use crate::retry_strategies::RetryStrategy;
use crate::value::{ProtocolVersion, ValkeyResult};

/// The client type.
#[derive(Debug, Clone)]
pub struct Client {
    pub(crate) connection_info: ConnectionInfo,
}

/// The client acts as connector to the redis server.  By itself it does not
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
/// let client = redis::Client::open("redis://127.0.0.1/").unwrap();
/// let con = client.get_connection(None).unwrap();
/// ```
impl Client {
    /// Connects to a valkey server and returns a client.  This does not
    /// actually open a connection yet but it does perform some basic
    /// checks on the URL that might make the operation fail.
    pub fn open<T: IntoConnectionInfo>(params: T) -> ValkeyResult<Client> {
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

impl Client {
    /// Returns an async multiplexed connection from the client.
    pub async fn get_multiplexed_async_connection(
        &self,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> ValkeyResult<MultiplexedConnection> {
        self.get_multiplexed_async_connection_with_timeouts(
            std::time::Duration::MAX,
            std::time::Duration::MAX,
            ferriskey_connection_options,
        )
        .await
    }

    /// Returns an async connection from the client.
    pub async fn get_multiplexed_async_connection_with_timeouts(
        &self,
        response_timeout: std::time::Duration,
        connection_timeout: std::time::Duration,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> ValkeyResult<MultiplexedConnection> {
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

    /// For TCP connections: returns (async connection, Some(the direct IP address))
    /// For Unix connections, returns (async connection, None)
    pub async fn get_multiplexed_async_connection_ip(
        &self,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> ValkeyResult<(MultiplexedConnection, Option<IpAddr>)> {
        self.get_multiplexed_async_connection_inner::<crate::connection::tokio::Tokio>(
            Duration::MAX,
            None,
            ferriskey_connection_options,
        )
        .await
    }

    pub(crate) async fn get_multiplexed_async_connection_inner<T>(
        &self,
        response_timeout: std::time::Duration,
        socket_addr: Option<SocketAddr>,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> ValkeyResult<(MultiplexedConnection, Option<IpAddr>)>
    where
        T: RedisRuntime,
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

    /// Constructs a new `Client` with parameters necessary to create a TLS connection.
    ///
    /// - `conn_info` - URL using the `rediss://` scheme.
    /// - `tls_certs` - `TlsCertificates` structure containing:
    ///   -- `client_tls` - Optional `ClientTlsConfig` containing byte streams for
    ///   -- `client_cert` - client's byte stream containing client certificate in PEM format
    ///   -- `client_key` - client's byte stream containing private key in PEM format
    ///   -- `root_cert` - Optional byte stream yielding PEM formatted file for root certificates.
    ///
    /// If `ClientTlsConfig` ( cert+key pair ) is not provided, then client-side authentication is not enabled.
    /// If `root_cert` is not provided, then system root certificates are used instead.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use std::{fs::File, io::{BufReader, Read}};
    ///
    /// use redis::{Client, AsyncCommands as _, TlsCertificates, ClientTlsConfig};
    ///
    /// async fn do_redis_code(
    ///     url: &str,
    ///     root_cert_file: &str,
    ///     cert_file: &str,
    ///     key_file: &str
    /// ) -> redis::ValkeyResult<()> {
    ///     let root_cert_file = File::open(root_cert_file).expect("cannot open private cert file");
    ///     let mut root_cert_vec = Vec::new();
    ///     BufReader::new(root_cert_file)
    ///         .read_to_end(&mut root_cert_vec)
    ///         .expect("Unable to read ROOT cert file");
    ///
    ///     let cert_file = File::open(cert_file).expect("cannot open private cert file");
    ///     let mut client_cert_vec = Vec::new();
    ///     BufReader::new(cert_file)
    ///         .read_to_end(&mut client_cert_vec)
    ///         .expect("Unable to read client cert file");
    ///
    ///     let key_file = File::open(key_file).expect("cannot open private key file");
    ///     let mut client_key_vec = Vec::new();
    ///     BufReader::new(key_file)
    ///         .read_to_end(&mut client_key_vec)
    ///         .expect("Unable to read client key file");
    ///
    ///     let client = Client::build_with_tls(
    ///         url,
    ///         TlsCertificates {
    ///             client_tls: Some(ClientTlsConfig{
    ///                 client_cert: client_cert_vec,
    ///                 client_key: client_key_vec,
    ///             }),
    ///             root_cert: Some(root_cert_vec),
    ///         }
    ///     )
    ///     .expect("Unable to build client");
    ///
    ///     let connection_info = client.get_connection_info();
    ///
    ///     println!(">>> connection info: {connection_info:?}");
    ///
    ///     let mut con = client.get_async_connection(None).await?;
    ///
    ///     con.set::<_, _, ()>("key1", b"foo").await?;
    ///
    ///     redis::cmd("SET")
    ///         .arg(&["key2", "bar"])
    ///         .query_async::<_, ()>(&mut con)
    ///         .await?;
    ///
    ///     let result = redis::cmd("MGET")
    ///         .arg(&["key1", "key2"])
    ///         .query_async(&mut con)
    ///         .await;
    ///     assert_eq!(result, Ok(("foo".to_string(), b"bar".to_vec())));
    ///     println!("Result from MGET: {result:?}");
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn build_with_tls<C: IntoConnectionInfo>(
        conn_info: C,
        tls_certs: TlsCertificates,
    ) -> ValkeyResult<Client> {
        let connection_info = conn_info.into_connection_info()?;

        inner_build_with_tls(connection_info, tls_certs)
    }

    /// Updates the password in connection_info.
    pub fn update_password(&mut self, password: Option<String>) {
        self.connection_info.valkey.password = password;
    }

    /// Updates the database ID in connection_info.
    ///
    /// This method updates the database field in the connection information,
    /// which will be used for subsequent connections and reconnections.
    ///
    /// # Arguments
    ///
    /// * `database_id` - The database ID to use for connections (typically 0-15)
    ///
    /// ```
    pub fn update_database(&mut self, database_id: i64) {
        self.connection_info.valkey.db = database_id;
    }

    /// Updates the client_name in connection_info.
    pub fn update_client_name(&mut self, client_name: Option<String>) {
        self.connection_info.valkey.client_name = client_name;
    }

    /// Updates the username in connection_info.
    ///
    /// This method updates the username field in the connection information,
    /// which will be used for subsequent connections and reconnections.
    /// Typically updated when AUTH command is used with a username.
    ///
    /// # Arguments
    ///
    /// * `username` - The username to use for authentication (None to clear)
    ///
    pub fn update_username(&mut self, username: Option<String>) {
        self.connection_info.valkey.username = username;
    }

    /// Updates the protocol version in connection_info.
    ///
    /// This method updates the protocol field in the connection information,
    /// which will be used for subsequent connections and reconnections.
    /// Typically updated when HELLO command is used to change protocol version.
    ///
    /// # Arguments
    ///
    /// * `protocol` - The protocol version to use (RESP2 or RESP3)
    ///
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
