// TODO: High-level API (Client, ClientBuilder, TypedPipeline) has zero integration
// test coverage. A dedicated test session should add tests for each public method,
// including edge cases like empty inputs and error paths.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::client::{
    AuthenticationInfo, Client as ClientInner, ConnectionRequest, ConnectionRetryStrategy,
    NodeAddress, TlsMode as ClientTlsMode,
};
use crate::cmd::{Cmd, cmd};
use crate::connection::info::{ConnectionAddr, ConnectionInfo, IntoConnectionInfo};
use crate::value::{Error, ErrorKind, ProtocolVersion, from_owned_value};

pub use crate::client::ReadFrom;
pub use crate::value::FromValue;
pub use crate::value::ToArgs;

pub type Result<T> = crate::value::Result<T>;

/// High-level Valkey client with convenience methods for common operations.
#[derive(Clone)]
pub struct Client(Arc<ClientInner>);

/// Builder for constructing a [`Client`] with custom connection options.
pub struct ClientBuilder {
    request: ConnectionRequest,
}

/// Builder for executing an arbitrary command through a [`Client`].
pub struct CommandBuilder {
    client: Arc<ClientInner>,
    cmd: Cmd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SharedConnectionOptions {
    tls_mode: ClientTlsMode,
    username: Option<String>,
    password: Option<String>,
    database_id: i64,
    protocol: ProtocolVersion,
    client_name: Option<String>,
    lib_name: Option<String>,
}

impl Client {
    /// Connect to a standalone Valkey server by URL.
    pub async fn connect(url: &str) -> Result<Client> {
        let request = connection_request_from_url(url, false)?;
        let inner = ClientInner::new(request, None).await?;
        Ok(Self(Arc::new(inner)))
    }

    /// Connect to a Valkey cluster using one or more seed node URLs.
    pub async fn connect_cluster(urls: &[&str]) -> Result<Client> {
        if urls.is_empty() {
            return Err(Error::from((
                ErrorKind::InvalidClientConfig,
                "Cluster URLs cannot be empty",
            )));
        }

        let infos = urls
            .iter()
            .map(|url| (*url).into_connection_info())
            .collect::<crate::value::Result<Vec<_>>>()?;

        let mut iter = infos.into_iter();
        let first = iter.next().expect("checked non-empty cluster URLs");
        let baseline = shared_connection_options(&first)?;
        let mut request = connection_request_from_info(first, true)?;

        for info in iter {
            let options = shared_connection_options(&info)?;
            if options != baseline {
                return Err(Error::from((
                    ErrorKind::InvalidClientConfig,
                    "All cluster URLs must share TLS, auth, database, protocol, and client metadata",
                )));
            }
            request.addresses.push(node_address_from_addr(info.addr)?);
        }

        let inner = ClientInner::new(request, None).await?;
        Ok(Self(Arc::new(inner)))
    }

    /// Get the value of a key, returning `None` if the key does not exist.
    pub async fn get<T: FromValue>(&self, key: impl ToArgs) -> Result<Option<T>> {
        let mut cmd = cmd("GET");
        cmd.arg(key);
        self.execute(cmd).await
    }

    /// Increment the integer value of a key by one, returning the new value.
    pub async fn incr(&self, key: impl ToArgs) -> Result<i64> {
        let mut cmd = cmd("INCR");
        cmd.arg(key);
        self.execute(cmd).await
    }

    /// Set a key to a value.
    pub async fn set(&self, key: impl ToArgs, value: impl ToArgs) -> Result<()> {
        let mut cmd = cmd("SET");
        cmd.arg(key).arg(value);
        self.execute(cmd).await
    }

    /// Set a key to a value and return the old value.
    ///
    /// Uses `SET key value GET` (the GETSET command is deprecated since Valkey/Redis 6.2).
    pub async fn get_set<T: FromValue>(
        &self,
        key: impl ToArgs,
        value: impl ToArgs,
    ) -> Result<Option<T>> {
        let mut cmd = cmd("SET");
        cmd.arg(key).arg(value).arg("GET");
        self.execute(cmd).await
    }

    /// Set a key to a value with an expiration time.
    ///
    /// Note: PSETEX wire format is `PSETEX key milliseconds value`, but the public
    /// API takes `(key, value, ttl)`. The arguments are reordered here to match
    /// the protocol: key first, then milliseconds, then value.
    pub async fn set_ex(&self, key: impl ToArgs, value: impl ToArgs, ttl: Duration) -> Result<()> {
        let mut cmd = cmd("PSETEX");
        cmd.arg(key).arg(duration_to_millis(ttl)?).arg(value);
        self.execute(cmd).await
    }

    /// Delete one or more keys, returning the number of keys removed.
    ///
    /// Returns `Ok(0)` immediately if `keys` is empty, avoiding a
    /// round-trip for a no-op DEL command.
    pub async fn del(&self, keys: &[impl ToArgs]) -> Result<i64> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut cmd = cmd("DEL");
        cmd.arg(keys);
        self.execute(cmd).await
    }

    /// Set a timeout on a key, returning `true` if the key exists.
    pub async fn expire(&self, key: impl ToArgs, ttl: Duration) -> Result<bool> {
        let mut cmd = cmd("PEXPIRE");
        cmd.arg(key).arg(duration_to_millis(ttl)?);
        self.execute(cmd).await
    }

    /// Check if a key exists.
    pub async fn exists(&self, key: impl ToArgs) -> Result<bool> {
        let mut cmd = cmd("EXISTS");
        cmd.arg(key);
        let exists: i64 = self.execute(cmd).await?;
        Ok(exists != 0)
    }

    /// Set a hash field to a value, returning the number of new fields added.
    pub async fn hset(
        &self,
        key: impl ToArgs,
        field: impl ToArgs,
        value: impl ToArgs,
    ) -> Result<i64> {
        let mut cmd = cmd("HSET");
        cmd.arg(key).arg(field).arg(value);
        self.execute(cmd).await
    }

    /// Get the value of a hash field, returning `None` if the field does not exist.
    pub async fn hget<T: FromValue>(
        &self,
        key: impl ToArgs,
        field: impl ToArgs,
    ) -> Result<Option<T>> {
        let mut cmd = cmd("HGET");
        cmd.arg(key).arg(field);
        self.execute(cmd).await
    }

    /// Get all fields and values of a hash.
    pub async fn hgetall(&self, key: impl ToArgs) -> Result<HashMap<String, String>> {
        let mut cmd = cmd("HGETALL");
        cmd.arg(key);
        self.execute(cmd).await
    }

    /// Prepend one or more elements to a list, returning the new length.
    pub async fn lpush(&self, key: impl ToArgs, elements: &[impl ToArgs]) -> Result<i64> {
        if elements.is_empty() {
            return Err(Error::from((
                ErrorKind::ClientError,
                "LPUSH requires at least one element",
            )));
        }
        let mut cmd = cmd("LPUSH");
        cmd.arg(key).arg(elements);
        self.execute(cmd).await
    }

    /// Remove and return the last element of a list.
    pub async fn rpop(&self, key: impl ToArgs, count: Option<usize>) -> Result<Option<String>> {
        match count {
            None => {
                let mut cmd = cmd("RPOP");
                cmd.arg(key);
                self.execute(cmd).await
            }
            Some(0) => Ok(None),
            Some(1) => {
                let mut cmd = cmd("RPOP");
                cmd.arg(key).arg(1usize);
                let values: Option<Vec<String>> = self.execute(cmd).await?;
                Ok(values.and_then(|items| items.into_iter().next()))
            }
            Some(_) => Err(Error::from((
                ErrorKind::InvalidClientConfig,
                "rpop(count > 1) is not representable as Option<String>; use cmd(\"RPOP\") instead",
            ))),
        }
    }

    /// Get the values of multiple keys at once.
    ///
    /// Returns a `Vec` with one entry per key; each entry is `None` if the key
    /// does not exist.
    pub async fn mget<T: FromValue>(&self, keys: &[impl ToArgs]) -> Result<Vec<Option<T>>> {
        if keys.is_empty() {
            return Ok(vec![]);
        }
        let mut cmd = cmd("MGET");
        cmd.arg(keys);
        self.execute(cmd).await
    }

    /// Set multiple key-value pairs at once.
    pub async fn mset(&self, pairs: &[(impl ToArgs, impl ToArgs)]) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mut cmd = cmd("MSET");
        for (k, v) in pairs {
            cmd.arg(k).arg(v);
        }
        self.execute(cmd).await
    }

    /// Get the remaining time-to-live of a key in seconds.
    ///
    /// Returns `-2` if the key does not exist, `-1` if it has no expiry.
    pub async fn ttl(&self, key: impl ToArgs) -> Result<i64> {
        let mut cmd = cmd("TTL");
        cmd.arg(key);
        self.execute(cmd).await
    }

    /// Append one or more elements to the tail of a list, returning the new length.
    pub async fn rpush(&self, key: impl ToArgs, elements: &[impl ToArgs]) -> Result<i64> {
        if elements.is_empty() {
            return Err(Error::from((
                ErrorKind::ClientError,
                "RPUSH requires at least one element",
            )));
        }
        let mut cmd = cmd("RPUSH");
        cmd.arg(key).arg(elements);
        self.execute(cmd).await
    }

    /// Call a Valkey Function (server-side Lua registered via FUNCTION LOAD).
    ///
    /// Wire format: `FCALL function numkeys key [key ...] arg [arg ...]`
    pub async fn fcall<T: FromValue>(
        &self,
        function: &str,
        keys: &[impl ToArgs],
        args: &[impl ToArgs],
    ) -> Result<T> {
        let mut c = cmd("FCALL");
        c.arg(function).arg(keys.len()).arg(keys).arg(args);
        self.execute(c).await
    }

    /// Call a Valkey Function in read-only mode (safe for replicas).
    ///
    /// Wire format: `FCALL_RO function numkeys key [key ...] arg [arg ...]`
    pub async fn fcall_readonly<T: FromValue>(
        &self,
        function: &str,
        keys: &[impl ToArgs],
        args: &[impl ToArgs],
    ) -> Result<T> {
        let mut c = cmd("FCALL_RO");
        c.arg(function).arg(keys.len()).arg(keys).arg(args);
        self.execute(c).await
    }

    /// Load (or replace) a Valkey Functions library.
    ///
    /// Wire format: `FUNCTION LOAD REPLACE <library_code>`
    /// Returns the library name on success.
    pub async fn function_load_replace(&self, library_code: &str) -> Result<String> {
        let mut c = cmd("FUNCTION");
        c.arg("LOAD").arg("REPLACE").arg(library_code);
        self.execute(c).await
    }

    /// List loaded function libraries matching the given library name.
    ///
    /// Wire format: `FUNCTION LIST LIBRARYNAME <library_name>`
    /// Returns the raw Value for caller to inspect.
    pub async fn function_list(&self, library_name: &str) -> Result<crate::value::Value> {
        let mut c = cmd("FUNCTION");
        c.arg("LIST").arg("LIBRARYNAME").arg(library_name);
        self.execute(c).await
    }

    /// Delete a loaded function library by name.
    ///
    /// Wire format: `FUNCTION DELETE <library_name>`
    pub async fn function_delete(&self, library_name: &str) -> Result<()> {
        let mut c = cmd("FUNCTION");
        c.arg("DELETE").arg(library_name);
        self.execute(c).await
    }

    /// Start building an arbitrary command by name.
    pub fn cmd(&self, name: &str) -> CommandBuilder {
        CommandBuilder {
            client: self.0.clone(),
            cmd: cmd(name),
        }
    }

    /// Create a new typed pipeline for batching multiple commands.
    pub fn pipeline(&self) -> TypedPipeline {
        TypedPipeline {
            inner: crate::pipeline::Pipeline::new(),
            client: self.0.clone(),
            results: Arc::new(std::sync::OnceLock::new()),
            next_index: 0,
        }
    }

    /// Create a new atomic pipeline (MULTI/EXEC transaction).
    pub fn transaction(&self) -> TypedPipeline {
        let mut pipe = self.pipeline();
        pipe.atomic();
        pipe
    }

    async fn execute<T: FromValue>(&self, mut cmd: Cmd) -> Result<T> {
        // Clone is required: ClientInner::send_command takes &mut self because it
        // may update the connection password (IAM token rotation) internally.
        // Refactoring send_command to &self would require changing
        // update_connection_password and the ValkeyClientForTests trait across the
        // codebase. The clone is cheap (Arc fields + atomics).
        let mut inner = (*self.0).clone();
        let value = inner.send_command(&mut cmd, None).await?;
        from_owned_value(value)
    }
}

impl ClientBuilder {
    /// Create a new builder with default options.
    pub fn new() -> Self {
        Self {
            request: ConnectionRequest::default(),
        }
    }

    /// Add a server address (host and port).
    pub fn host(mut self, host: &str, port: u16) -> Self {
        self.request.addresses.push(NodeAddress {
            host: host.to_string(),
            port,
        });
        self
    }

    /// Add a server by URL, extracting host, port, TLS, and auth settings.
    pub fn url(mut self, url: &str) -> Result<Self> {
        let info = url.into_connection_info()?;
        apply_connection_info(&mut self.request, info)?;
        Ok(self)
    }

    /// Enable cluster mode.
    pub fn cluster(mut self) -> Self {
        self.request.cluster_mode_enabled = true;
        self
    }

    /// Enable TLS with certificate verification.
    pub fn tls(mut self) -> Self {
        self.request.tls_mode = Some(ClientTlsMode::SecureTls);
        self
    }

    /// Enable TLS without certificate verification.
    pub fn tls_insecure(mut self) -> Self {
        self.request.tls_mode = Some(ClientTlsMode::InsecureTls);
        self
    }

    /// Set the authentication password.
    pub fn password(mut self, pw: impl Into<String>) -> Self {
        authentication_info_mut(&mut self.request).password = Some(pw.into());
        self
    }

    /// Set the authentication username (ACL).
    pub fn username(mut self, username: impl Into<String>) -> Self {
        authentication_info_mut(&mut self.request).username = Some(username.into());
        self
    }

    /// Select the database number.
    pub fn database(mut self, db: i64) -> Self {
        self.request.database_id = db;
        self
    }

    /// Set the read-from strategy (primary, replica, etc.).
    pub fn read_from(mut self, strategy: ReadFrom) -> Self {
        self.request.read_from = Some(strategy);
        self
    }

    /// Set the connection timeout.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.request.connection_timeout = Some(duration_to_request_millis(timeout));
        self
    }

    /// Set the per-request timeout.
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request.request_timeout = Some(duration_to_request_millis(timeout));
        self
    }

    /// Set the maximum number of in-flight requests.
    pub fn max_inflight(mut self, max_inflight: u32) -> Self {
        self.request.inflight_requests_limit = Some(max_inflight);
        self
    }

    /// Set the connection retry strategy.
    pub fn retry_strategy(mut self, strategy: ConnectionRetryStrategy) -> Self {
        self.request.connection_retry_strategy = Some(strategy);
        self
    }

    /// Set the client name sent to the server.
    pub fn client_name(mut self, name: impl Into<String>) -> Self {
        self.request.client_name = Some(name.into());
        self
    }

    /// Set the RESP protocol version.
    pub fn protocol(mut self, proto: crate::value::ProtocolVersion) -> Self {
        self.request.protocol = Some(proto);
        self
    }

    /// Enable lazy connection (connect on first command, not on build).
    pub fn lazy_connect(mut self) -> Self {
        self.request.lazy_connect = true;
        self
    }

    /// Enable TCP_NODELAY.
    pub fn tcp_nodelay(mut self) -> Self {
        self.request.tcp_nodelay = true;
        self
    }

    /// Set PubSub subscriptions for the connection.
    pub fn pubsub_subscriptions(mut self, subs: crate::connection::info::PubSubSubscriptionInfo) -> Self {
        self.request.pubsub_subscriptions = Some(subs);
        self
    }

    /// Build and connect the client.
    pub async fn build(self) -> Result<Client> {
        if self.request.addresses.is_empty() {
            return Err(Error::from((
                ErrorKind::InvalidClientConfig,
                "ClientBuilder requires at least one address",
            )));
        }

        let inner = ClientInner::new(self.request, None).await?;
        Ok(Client(Arc::new(inner)))
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandBuilder {
    /// Append an argument to the command.
    pub fn arg(mut self, arg: impl ToArgs) -> Self {
        self.cmd.arg(arg);
        self
    }

    /// Execute the command and return the typed result.
    pub async fn execute<T: FromValue>(self) -> Result<T> {
        let mut inner = (*self.client).clone();
        let mut cmd = self.cmd;
        let value = inner.send_command(&mut cmd, None).await?;
        from_owned_value(value)
    }
}

/// A typed pipeline that returns [`PipeSlot`] handles for each command.
///
/// After calling [`execute()`](TypedPipeline::execute), each slot can be
/// resolved to its typed value via [`PipeSlot::value()`].
///
/// ```rust,ignore
/// let mut pipe = client.pipeline();
/// let name  = pipe.get::<String>("user:1:name");
/// let score = pipe.get::<f64>("user:1:score");
/// let _     = pipe.set("user:1:name", "alice");
/// pipe.execute().await?;
/// let name: Option<String> = name.value()?;
/// ```
pub struct TypedPipeline {
    inner: crate::pipeline::Pipeline,
    client: Arc<ClientInner>,
    results: Arc<std::sync::OnceLock<Vec<crate::value::Result<crate::value::Value>>>>,
    next_index: usize,
}

/// A handle to a single result within a [`TypedPipeline`].
///
/// Created by pipeline methods like [`TypedPipeline::get()`] and
/// [`TypedPipeline::set()`]. Call [`.value()`](PipeSlot::value) after
/// `pipeline.execute()` to extract the typed result.
pub struct PipeSlot<T> {
    index: usize,
    results: Arc<std::sync::OnceLock<Vec<crate::value::Result<crate::value::Value>>>>,
    _marker: std::marker::PhantomData<T>,
}

impl TypedPipeline {
    /// Enable atomic (MULTI/EXEC) mode for this pipeline.
    pub fn atomic(&mut self) -> &mut Self {
        self.inner.atomic();
        self
    }

    fn push_cmd(&mut self, c: Cmd) -> usize {
        self.inner.add_command(c);
        let idx = self.next_index;
        self.next_index += 1;
        idx
    }

    fn slot<T: FromValue>(&self, index: usize) -> PipeSlot<T> {
        PipeSlot {
            index,
            results: self.results.clone(),
            _marker: std::marker::PhantomData,
        }
    }

    /// Queue a GET command, returning a slot for the result.
    pub fn get<T: FromValue>(&mut self, key: impl ToArgs) -> PipeSlot<Option<T>> {
        let mut c = cmd("GET");
        c.arg(key);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Queue a SET command.
    pub fn set(&mut self, key: impl ToArgs, value: impl ToArgs) -> PipeSlot<()> {
        let mut c = cmd("SET");
        c.arg(key).arg(value);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Queue a DEL command, returning the number of keys removed.
    pub fn del(&mut self, key: impl ToArgs) -> PipeSlot<i64> {
        let mut c = cmd("DEL");
        c.arg(key);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Queue an INCR command, returning the new value.
    pub fn incr(&mut self, key: impl ToArgs) -> PipeSlot<i64> {
        let mut c = cmd("INCR");
        c.arg(key);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Queue an HSET command, returning the number of new fields added.
    pub fn hset(
        &mut self,
        key: impl ToArgs,
        field: impl ToArgs,
        value: impl ToArgs,
    ) -> PipeSlot<i64> {
        let mut c = cmd("HSET");
        c.arg(key).arg(field).arg(value);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Queue an HGET command, returning the field value or `None`.
    pub fn hget<T: FromValue>(
        &mut self,
        key: impl ToArgs,
        field: impl ToArgs,
    ) -> PipeSlot<Option<T>> {
        let mut c = cmd("HGET");
        c.arg(key).arg(field);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Queue an LPUSH command, returning the new list length.
    pub fn lpush(&mut self, key: impl ToArgs, elements: &[impl ToArgs]) -> PipeSlot<i64> {
        let mut c = cmd("LPUSH");
        c.arg(key).arg(elements);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Queue an RPOP command, returning the removed element or `None`.
    pub fn rpop(&mut self, key: impl ToArgs) -> PipeSlot<Option<String>> {
        let mut c = cmd("RPOP");
        c.arg(key);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Queue a PEXPIRE command, returning `true` if the timeout was set.
    ///
    /// Returns an error slot if the duration overflows `u64` milliseconds.
    pub fn expire(&mut self, key: impl ToArgs, ttl: Duration) -> Result<PipeSlot<bool>> {
        let mut c = cmd("PEXPIRE");
        c.arg(key).arg(duration_to_millis(ttl)?);
        let idx = self.push_cmd(c);
        Ok(self.slot(idx))
    }

    /// Queue an EXISTS command.
    pub fn exists(&mut self, key: impl ToArgs) -> PipeSlot<bool> {
        let mut c = cmd("EXISTS");
        c.arg(key);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Queue an FCALL command, returning a typed slot for the result.
    pub fn fcall<T: FromValue>(
        &mut self,
        function: &str,
        keys: &[impl ToArgs],
        args: &[impl ToArgs],
    ) -> PipeSlot<T> {
        let mut c = cmd("FCALL");
        c.arg(function).arg(keys.len()).arg(keys).arg(args);
        let idx = self.push_cmd(c);
        self.slot(idx)
    }

    /// Add an arbitrary command to the pipeline, returning a typed slot.
    pub fn cmd<T: FromValue>(&mut self, name: &str) -> PipeCmdBuilder<'_, T> {
        PipeCmdBuilder {
            pipeline: self,
            cmd: cmd(name),
            _marker: std::marker::PhantomData,
        }
    }

    /// Execute all queued commands. After this returns, [`PipeSlot::value()`]
    /// can be called on any slot returned by prior pipeline methods.
    ///
    /// Automatically uses MULTI/EXEC when [`atomic()`](TypedPipeline::atomic)
    /// was called (or when created via [`Client::transaction()`]).
    ///
    /// Returns an error if called more than once on the same pipeline, since
    /// the second execution's results would silently be lost.
    pub async fn execute(&mut self) -> Result<()> {
        if self.results.get().is_some() {
            return Err(Error::from((
                ErrorKind::ClientError,
                "Pipeline already executed; create a new pipeline for additional commands",
            )));
        }

        let mut inner = (*self.client).clone();
        let value = if self.inner.is_atomic() {
            inner.send_transaction(&self.inner, None, None, true).await?
        } else {
            inner
                .send_pipeline(
                    &self.inner,
                    None,
                    true,
                    None,
                    crate::pipeline::PipelineRetryStrategy::default(),
                )
                .await?
        };
        let vals = match value {
            crate::value::Value::Array(v) => v,
            other => vec![Ok(other)],
        };
        let _ = self.results.set(vals);
        Ok(())
    }
}

/// Builder for adding an arbitrary command to a [`TypedPipeline`].
pub struct PipeCmdBuilder<'a, T> {
    pipeline: &'a mut TypedPipeline,
    cmd: Cmd,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T: FromValue> PipeCmdBuilder<'a, T> {
    /// Append an argument to the command.
    pub fn arg(mut self, arg: impl ToArgs) -> Self {
        self.cmd.arg(arg);
        self
    }

    /// Finalize the command and return a typed slot for its result.
    pub fn finish(self) -> PipeSlot<T> {
        let idx = self.pipeline.push_cmd(self.cmd);
        self.pipeline.slot(idx)
    }
}

impl<T: FromValue> PipeSlot<T> {
    /// Extract the typed value from the pipeline results.
    ///
    /// Returns an error if called before [`TypedPipeline::execute()`].
    pub fn value(self) -> Result<T> {
        let vals = self
            .results
            .get()
            .ok_or_else(|| Error::from((
                ErrorKind::ClientError,
                "PipeSlot::value() called before pipeline.execute()",
            )))?;
        let val = vals
            .get(self.index)
            .cloned()
            .ok_or_else(|| Error::from((
                ErrorKind::ClientError,
                "Pipeline result index out of bounds",
                format!(
                    "Requested index {} but pipeline returned only {} results",
                    self.index, vals.len()
                ),
            )))?;
        from_owned_value(val?)
    }
}

fn connection_request_from_url(url: &str, cluster_mode_enabled: bool) -> Result<ConnectionRequest> {
    let info = url.into_connection_info()?;
    connection_request_from_info(info, cluster_mode_enabled)
}

fn connection_request_from_info(
    info: ConnectionInfo,
    cluster_mode_enabled: bool,
) -> Result<ConnectionRequest> {
    let mut request = ConnectionRequest {
        cluster_mode_enabled,
        ..ConnectionRequest::default()
    };
    apply_connection_info(&mut request, info)?;
    Ok(request)
}

fn shared_connection_options(info: &ConnectionInfo) -> Result<SharedConnectionOptions> {
    Ok(SharedConnectionOptions {
        tls_mode: tls_mode_from_addr(&info.addr),
        username: info.valkey.username.clone(),
        password: info.valkey.password.clone(),
        database_id: info.valkey.db,
        protocol: info.valkey.protocol,
        client_name: info.valkey.client_name.clone(),
        lib_name: info.valkey.lib_name.clone(),
    })
}

fn tls_mode_from_addr(addr: &ConnectionAddr) -> ClientTlsMode {
    match addr {
        ConnectionAddr::Tcp(..) => ClientTlsMode::NoTls,
        ConnectionAddr::TcpTls { insecure, .. } => {
            if *insecure {
                ClientTlsMode::InsecureTls
            } else {
                ClientTlsMode::SecureTls
            }
        }
        ConnectionAddr::Unix(_) => ClientTlsMode::NoTls,
    }
}

fn node_address_from_addr(addr: ConnectionAddr) -> Result<NodeAddress> {
    match addr {
        ConnectionAddr::Tcp(host, port) => Ok(NodeAddress { host, port }),
        ConnectionAddr::TcpTls { host, port, .. } => Ok(NodeAddress { host, port }),
        ConnectionAddr::Unix(_) => Err(Error::from((
            ErrorKind::InvalidClientConfig,
            "Unix socket URLs are not supported by the high-level Client wrapper",
        ))),
    }
}

fn authentication_from_parts(
    username: &Option<String>,
    password: &Option<String>,
) -> Option<AuthenticationInfo> {
    if username.is_none() && password.is_none() {
        None
    } else {
        Some(AuthenticationInfo {
            username: username.clone(),
            password: password.clone(),
            iam_config: None,
        })
    }
}

fn authentication_info_mut(request: &mut ConnectionRequest) -> &mut AuthenticationInfo {
    request
        .authentication_info
        .get_or_insert_with(AuthenticationInfo::default)
}

fn apply_connection_info(request: &mut ConnectionRequest, info: ConnectionInfo) -> Result<()> {
    let ConnectionInfo { addr, valkey } = info;
    request
        .addresses
        .push(node_address_from_addr(addr.clone())?);
    request.tls_mode = Some(tls_mode_from_addr(&addr));
    if let Some(auth) = authentication_from_parts(&valkey.username, &valkey.password) {
        request.authentication_info = Some(auth);
    }
    request.database_id = valkey.db;
    request.protocol = Some(valkey.protocol);
    request.client_name = valkey.client_name;
    request.lib_name = valkey.lib_name;
    Ok(())
}

fn duration_to_millis(ttl: Duration) -> Result<u64> {
    u64::try_from(ttl.as_millis()).map_err(|_| {
        Error::from((
            ErrorKind::InvalidClientConfig,
            "TTL is too large to encode in milliseconds",
        ))
    })
}

fn duration_to_request_millis(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verify the pipeline API compiles with correct types.
    // Not meant to run — just type-checks the slot pattern.
    fn _pipeline_type_check() {
        let _: fn(&mut TypedPipeline, &str) -> PipeSlot<Option<String>> =
            |p, k| p.get::<String>(k);
        let _: fn(&mut TypedPipeline, &str, &str) -> PipeSlot<()> = |p, k, v| p.set(k, v);
        let _: fn(&mut TypedPipeline, &str) -> PipeSlot<i64> = |p, k| p.del(k);
        let _: fn(&mut TypedPipeline, &str) -> PipeSlot<i64> = |p, k| p.incr(k);
        let _: fn(&mut TypedPipeline, &str) -> PipeSlot<bool> = |p, k| p.exists(k);
        let _: fn(&mut TypedPipeline, &str, &str) -> PipeSlot<Option<String>> =
            |p, k, f| p.hget::<String>(k, f);
    }
}
