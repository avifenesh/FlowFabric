//! Adds async IO support to redis.
pub mod factory;

pub use factory::{Client, FerrisKeyConnectionOptions, IAMTokenProvider};

use crate::cmd::{Cmd, cmd};
use crate::connection::info::{ValkeyConnectionInfo, get_resp3_hello_command_error};
use crate::pipeline::PipelineRetryStrategy;
use crate::value::{
    ErrorKind, FromValue, InfoDict, ProtocolVersion, Error, Result, Value,
};
use ::tokio::io::{AsyncRead, AsyncWrite};
use async_trait::async_trait;
use futures_util::Future;
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use crate::connection::tls::TlsConnParams;

/// Enables the tokio compatibility
pub mod tokio;

/// Represents the ability of connecting via TCP or via Unix socket
#[async_trait]
pub(crate) trait ValkeyRuntime: AsyncStream + Send + Sync + Sized + 'static {
    /// Performs a TCP connection
    async fn connect_tcp(socket_addr: SocketAddr, tcp_nodelay: bool) -> Result<Self>;

    // Performs a TCP TLS connection
    async fn connect_tcp_tls(
        hostname: &str,
        socket_addr: SocketAddr,
        insecure: bool,
        tls_params: &Option<TlsConnParams>,
        tcp_nodelay: bool,
    ) -> Result<Self>;

    /// Performs a UNIX connection
    #[cfg(unix)]
    async fn connect_unix(path: &Path) -> Result<Self>;

    fn spawn(f: impl Future<Output = ()> + Send + 'static);

    fn boxed(self) -> Pin<Box<dyn AsyncStream + Send + Sync>> {
        Box::pin(self)
    }
}

/// Trait for objects that implements `AsyncRead` and `AsyncWrite`
pub trait AsyncStream: AsyncRead + AsyncWrite {}
impl<S> AsyncStream for S where S: AsyncRead + AsyncWrite {}

/// An async abstraction over connections.
pub trait ConnectionLike: Send {
    /// Sends an already encoded (packed) command into the TCP socket and
    /// reads the single response from it.
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a Cmd,
    ) -> impl Future<Output = Result<Value>> + Send + 'a;

    /// Sends multiple already encoded (packed) command into the TCP socket
    /// and reads `count` responses from it.  This is used to implement
    /// pipelining.
    /// Important - this function is meant for internal usage, since it's
    /// easy to pass incorrect `offset` & `count` parameters, which might
    /// cause the connection to enter an erroneous state. Users shouldn't
    /// call it, instead using the Pipeline::query_async function.
    #[doc(hidden)]
    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a crate::pipeline::Pipeline,
        offset: usize,
        count: usize,
        pipeline_retry_strategy: Option<PipelineRetryStrategy>,
    ) -> impl Future<Output = Result<Vec<Result<Value>>>> + Send + 'a;

    /// Sends pre-packed RESP bytes directly, skipping command serialization.
    /// Only meaningful on per-node connections (MultiplexedConnection).
    /// Default returns an error — cluster-level connections should use req_packed_command.
    fn send_packed_bytes<'a>(
        &'a mut self,
        _packed: bytes::Bytes,
        _is_fenced: bool,
    ) -> impl Future<Output = Result<Value>> + Send + 'a {
        async {
            Err(Error::from((
                ErrorKind::ClientError,
                "send_packed_bytes not supported — use req_packed_command",
            )))
        }
    }

    /// Returns the database this connection is bound to.  Note that this
    /// information might be unreliable because it's initially cached and
    /// also might be incorrect if the connection like object is not
    /// actually connected.
    fn get_db(&self) -> i64;

    /// Returns the state of the connection
    fn is_closed(&self) -> bool;

    /// Get the connection availability zone
    fn get_az(&self) -> Option<String> {
        None
    }

    /// Set the connection availability zone
    fn set_az(&mut self, _az: Option<String>) {}

    /// Update the node address used for PubSub tracking.
    /// Default implementation does nothing - only MultiplexedConnection implements this.
    fn update_push_manager_node_address(&mut self, _address: String) {
        // Default: no-op
    }
}

/// Implements ability to notify about disconnection events
#[async_trait]
pub trait DisconnectNotifier: Send + Sync {
    /// Notify about disconnect event
    fn notify_disconnect(&mut self);

    /// Wait for disconnect event with timeout
    async fn wait_for_disconnect_with_timeout(&self, max_wait: &Duration);

    /// Intended to be used with Box
    fn clone_box(&self) -> Box<dyn DisconnectNotifier>;
}

impl Clone for Box<dyn DisconnectNotifier> {
    fn clone(&self) -> Box<dyn DisconnectNotifier> {
        self.clone_box()
    }
}

// Helper function to extract and update availability zone from INFO command
async fn update_az_from_info<C>(con: &mut C) -> Result<()>
where
    C: ConnectionLike,
{
    let info_res = con.req_packed_command(&cmd("INFO")).await;

    match info_res {
        Ok(value) => {
            let info_dict: InfoDict = FromValue::from_value(&value)?;
            if let Some(node_az) = info_dict.get::<String>("availability_zone") {
                con.set_az(Some(node_az));
            }
            Ok(())
        }
        Err(e) => {
            // Handle the error case for the INFO command
            Err(Error::from((
                ErrorKind::ResponseError,
                "Failed to execute INFO command. ",
                format!("{e:?}"),
            )))
        }
    }
}

// Initial setup for every connection.
async fn setup_connection<C>(
    connection_info: &ValkeyConnectionInfo,
    con: &mut C,
    // This parameter is set to 'true' if ReadFromReplica strategy is set to AZAffinity or AZAffinityReplicasAndPrimary.
    // An INFO command will be triggered in the connection's setup to update the 'availability_zone' property.
    discover_az: bool,
) -> Result<()>
where
    C: ConnectionLike,
{
    if connection_info.protocol != ProtocolVersion::RESP2 {
        let hello_cmd = resp3_hello(connection_info);
        let val: Result<Value> = hello_cmd.query_async(con).await;
        if let Err(err) = val {
            return Err(get_resp3_hello_command_error(err));
        }
    } else if let Some(password) = &connection_info.password {
        let has_username = connection_info.username.is_some();
        let mut command = cmd("AUTH");
        if let Some(username) = &connection_info.username {
            command.arg(username);
        }
        match command.arg(password).query_async(con).await {
            Ok(Value::Okay) => (),
            Err(e) if has_username && e.kind() == ErrorKind::ResponseError => {
                // AUTH with username failed with an ERR response -- this
                // typically means the server doesn't support the two-argument
                // AUTH form (e.g. older Redis/Valkey without ACL support),
                // returning "ERR wrong number of arguments for 'auth' command".
                // Retry with password-only AUTH before giving up.
                //
                // Other error kinds (WRONGPASS, NOPERM, etc.) indicate the
                // server understood the command but rejected the credentials;
                // falling back would silently authenticate as the default user.
                let mut command = cmd("AUTH");
                match command.arg(password).query_async(con).await {
                    Ok(Value::Okay) => (),
                    _ => {
                        // Both attempts failed; report the original error
                        return Err(Error::from((
                            ErrorKind::AuthenticationFailed,
                            "Password authentication failed",
                            format!("Initial AUTH (with username) error: {e}"),
                        )));
                    }
                }
            }
            Err(e) if has_username => {
                // The server understood the two-argument AUTH but rejected
                // the credentials (WRONGPASS, NOPERM, etc.). Preserve the
                // original error rather than masking it with a fallback.
                return Err(Error::from((
                    ErrorKind::AuthenticationFailed,
                    "Password authentication failed",
                    format!("AUTH (with username) error: {e}"),
                )));
            }
            Err(_) | Ok(_) => {
                fail!((
                    ErrorKind::AuthenticationFailed,
                    "Password authentication failed"
                ));
            }
        }
    }

    if connection_info.db != 0 {
        match cmd("SELECT").arg(connection_info.db).query_async(con).await {
            Ok(Value::Okay) => (),
            _ => fail!((
                ErrorKind::ResponseError,
                "Valkey server refused to switch database"
            )),
        }
    }

    if let Some(client_name) = &connection_info.client_name {
        match cmd("CLIENT")
            .arg("SETNAME")
            .arg(client_name)
            .query_async(con)
            .await
        {
            Ok(Value::Okay) => {}
            _ => fail!((
                ErrorKind::ResponseError,
                "Valkey server refused to set client name"
            )),
        }
    }

    if discover_az {
        update_az_from_info(con).await?;
    }

    // result is ignored, as per the command's instructions.
    // https://redis.io/commands/client-setinfo/
    let _: Result<()> =
        crate::connection::info::client_set_info_pipeline(connection_info.lib_name.as_deref())
            .query_async(con)
            .await;
    Ok(())
}

mod multiplexed;
pub use multiplexed::*;
pub(crate) mod info;
pub(crate) mod runtime;
pub(crate) mod tls;

use crate::connection::info::ConnectionAddr;
use futures_util::future::select_ok;

pub(crate) async fn get_socket_addrs(
    host: &str,
    port: u16,
) -> Result<impl Iterator<Item = SocketAddr> + Send + '_> {
    let socket_addrs = ::tokio::net::lookup_host((host, port)).await?;
    let mut socket_addrs = socket_addrs.peekable();
    match socket_addrs.peek() {
        Some(_) => Ok(socket_addrs),
        None => Err(Error::from((
            ErrorKind::InvalidClientConfig,
            "No address found for host",
        ))),
    }
}

pub(crate) async fn connect_simple<T: ValkeyRuntime>(
    connection_info: &crate::connection::info::ConnectionInfo,
    _socket_addr: Option<SocketAddr>,
    tcp_nodelay: bool,
) -> Result<(T, Option<std::net::IpAddr>)> {
    Ok(match connection_info.addr {
        ConnectionAddr::Tcp(ref host, port) => {
            if let Some(socket_addr) = _socket_addr {
                return Ok::<_, Error>((
                    <T>::connect_tcp(socket_addr, tcp_nodelay).await?,
                    Some(socket_addr.ip()),
                ));
            }
            let socket_addrs = get_socket_addrs(host, port).await?;
            select_ok(socket_addrs.map(|socket_addr| {
                Box::pin(async move {
                    Ok::<_, Error>((
                        <T>::connect_tcp(socket_addr, tcp_nodelay).await?,
                        Some(socket_addr.ip()),
                    ))
                })
            }))
            .await?
            .0
        }
        ConnectionAddr::TcpTls {
            ref host,
            port,
            insecure,
            ref tls_params,
        } => {
            if let Some(socket_addr) = _socket_addr {
                return Ok::<_, Error>((
                    <T>::connect_tcp_tls(host, socket_addr, insecure, tls_params, tcp_nodelay)
                        .await?,
                    Some(socket_addr.ip()),
                ));
            }
            let socket_addrs = get_socket_addrs(host, port).await?;
            select_ok(socket_addrs.map(|socket_addr| {
                Box::pin(async move {
                    Ok::<_, Error>((
                        <T>::connect_tcp_tls(host, socket_addr, insecure, tls_params, tcp_nodelay)
                            .await?,
                        Some(socket_addr.ip()),
                    ))
                })
            }))
            .await?
            .0
        }
        #[cfg(unix)]
        ConnectionAddr::Unix(ref path) => (<T>::connect_unix(path).await?, None),
        #[cfg(not(unix))]
        ConnectionAddr::Unix(_) => {
            return Err(Error::from((
                ErrorKind::InvalidClientConfig,
                "Cannot connect to unix sockets on this platform",
            )));
        }
    })
}

pub fn resp3_hello(connection_info: &ValkeyConnectionInfo) -> Cmd {
    let mut hello_cmd = cmd("HELLO");
    hello_cmd.arg("3");
    if let Some(password) = &connection_info.password {
        let username: &str = match connection_info.username.as_ref() {
            None => "default",
            Some(username) => username,
        };
        hello_cmd.arg("AUTH").arg(username).arg(password);
    }
    hello_cmd
}
