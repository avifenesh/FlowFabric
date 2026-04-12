use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::client::{
    AuthenticationInfo, Client as ClientInner, ConnectionError, ConnectionRequest, NodeAddress,
    TlsMode as ClientTlsMode,
};
use crate::valkey::{
    Cmd, ConnectionAddr, ConnectionInfo, ErrorKind, IntoConnectionInfo, ProtocolVersion,
    ValkeyError, cmd, from_owned_valkey_value,
};

pub use crate::client::ReadFrom;
pub use crate::valkey::FromValkeyValue as FromValue;
pub use crate::valkey::ToValkeyArgs as ToArgs;

pub type FerrisKeyError = ValkeyError;
pub type Result<T> = std::result::Result<T, FerrisKeyError>;

#[derive(Clone)]
pub struct Client(Arc<ClientInner>);

pub struct ClientBuilder {
    request: ConnectionRequest,
}

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
    pub async fn connect(url: &str) -> Result<Client> {
        let request = connection_request_from_url(url, false)?;
        let inner = ClientInner::new(request, None)
            .await
            .map_err(connection_error_to_ferriskey_error)?;
        Ok(Self(Arc::new(inner)))
    }

    pub async fn connect_cluster(urls: &[&str]) -> Result<Client> {
        if urls.is_empty() {
            return Err(ValkeyError::from((
                ErrorKind::InvalidClientConfig,
                "Cluster URLs cannot be empty",
            )));
        }

        let infos = urls
            .iter()
            .map(|url| (*url).into_connection_info())
            .collect::<crate::valkey::ValkeyResult<Vec<_>>>()?;

        let mut iter = infos.into_iter();
        let first = iter.next().expect("checked non-empty cluster URLs");
        let baseline = shared_connection_options(&first)?;
        let mut request = connection_request_from_info(first, true)?;

        for info in iter {
            let options = shared_connection_options(&info)?;
            if options != baseline {
                return Err(ValkeyError::from((
                    ErrorKind::InvalidClientConfig,
                    "All cluster URLs must share TLS, auth, database, protocol, and client metadata",
                )));
            }
            request.addresses.push(node_address_from_addr(info.addr)?);
        }

        let inner = ClientInner::new(request, None)
            .await
            .map_err(connection_error_to_ferriskey_error)?;
        Ok(Self(Arc::new(inner)))
    }

    pub async fn get<T: FromValue>(&self, key: impl ToArgs) -> Result<Option<T>> {
        let mut cmd = cmd("GET");
        cmd.arg(key);
        self.execute(cmd).await
    }

    pub async fn incr(&self, key: impl ToArgs) -> Result<i64> {
        let mut cmd = cmd("INCR");
        cmd.arg(key);
        self.execute(cmd).await
    }

    pub async fn set(&self, key: impl ToArgs, value: impl ToArgs) -> Result<()> {
        let mut cmd = cmd("SET");
        cmd.arg(key).arg(value);
        self.execute(cmd).await
    }

    pub async fn get_set<T: FromValue>(
        &self,
        key: impl ToArgs,
        value: impl ToArgs,
    ) -> Result<Option<T>> {
        let mut cmd = cmd("GETSET");
        cmd.arg(key).arg(value);
        self.execute(cmd).await
    }

    pub async fn set_ex(&self, key: impl ToArgs, value: impl ToArgs, ttl: Duration) -> Result<()> {
        let mut cmd = cmd("PSETEX");
        cmd.arg(key).arg(duration_to_millis(ttl)?).arg(value);
        self.execute(cmd).await
    }

    pub async fn del(&self, keys: &[impl ToArgs]) -> Result<i64> {
        let mut cmd = cmd("DEL");
        cmd.arg(keys);
        self.execute(cmd).await
    }

    pub async fn expire(&self, key: impl ToArgs, ttl: Duration) -> Result<bool> {
        let mut cmd = cmd("PEXPIRE");
        cmd.arg(key).arg(duration_to_millis(ttl)?);
        self.execute(cmd).await
    }

    pub async fn exists(&self, key: impl ToArgs) -> Result<bool> {
        let mut cmd = cmd("EXISTS");
        cmd.arg(key);
        let exists: i64 = self.execute(cmd).await?;
        Ok(exists != 0)
    }

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

    pub async fn hget<T: FromValue>(
        &self,
        key: impl ToArgs,
        field: impl ToArgs,
    ) -> Result<Option<T>> {
        let mut cmd = cmd("HGET");
        cmd.arg(key).arg(field);
        self.execute(cmd).await
    }

    pub async fn hgetall(&self, key: impl ToArgs) -> Result<HashMap<String, String>> {
        let mut cmd = cmd("HGETALL");
        cmd.arg(key);
        self.execute(cmd).await
    }

    pub async fn lpush(&self, key: impl ToArgs, elements: &[impl ToArgs]) -> Result<i64> {
        let mut cmd = cmd("LPUSH");
        cmd.arg(key).arg(elements);
        self.execute(cmd).await
    }

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
            Some(_) => Err(ValkeyError::from((
                ErrorKind::InvalidClientConfig,
                "rpop(count > 1) is not representable as Option<String>; use cmd(\"RPOP\") instead",
            ))),
        }
    }

    pub fn cmd(&self, name: &str) -> CommandBuilder {
        CommandBuilder {
            client: self.0.clone(),
            cmd: cmd(name),
        }
    }

    async fn execute<T: FromValue>(&self, mut cmd: Cmd) -> Result<T> {
        let mut inner = (*self.0).clone();
        let value = inner.send_command(&mut cmd, None).await?;
        from_owned_valkey_value(value)
    }
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self {
            request: ConnectionRequest::default(),
        }
    }

    pub fn host(mut self, host: &str, port: u16) -> Self {
        self.request.addresses.push(NodeAddress {
            host: host.to_string(),
            port,
        });
        self
    }

    pub fn url(mut self, url: &str) -> Result<Self> {
        let info = url.into_connection_info()?;
        apply_connection_info(&mut self.request, info)?;
        Ok(self)
    }

    pub fn cluster(mut self) -> Self {
        self.request.cluster_mode_enabled = true;
        self
    }

    pub fn tls(mut self) -> Self {
        self.request.tls_mode = Some(ClientTlsMode::SecureTls);
        self
    }

    pub fn tls_insecure(mut self) -> Self {
        self.request.tls_mode = Some(ClientTlsMode::InsecureTls);
        self
    }

    pub fn password(mut self, pw: impl Into<String>) -> Self {
        authentication_info_mut(&mut self.request).password = Some(pw.into());
        self
    }

    pub fn username(mut self, username: impl Into<String>) -> Self {
        authentication_info_mut(&mut self.request).username = Some(username.into());
        self
    }

    pub fn database(mut self, db: i64) -> Self {
        self.request.database_id = db;
        self
    }

    pub fn read_from(mut self, strategy: ReadFrom) -> Self {
        self.request.read_from = Some(strategy);
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.request.connection_timeout = Some(duration_to_request_millis(timeout));
        self
    }

    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request.request_timeout = Some(duration_to_request_millis(timeout));
        self
    }

    pub fn max_inflight(mut self, max_inflight: u32) -> Self {
        self.request.inflight_requests_limit = Some(max_inflight);
        self
    }

    pub async fn build(self) -> Result<Client> {
        if self.request.addresses.is_empty() {
            return Err(ValkeyError::from((
                ErrorKind::InvalidClientConfig,
                "ClientBuilder requires at least one address",
            )));
        }

        let inner = ClientInner::new(self.request, None)
            .await
            .map_err(connection_error_to_ferriskey_error)?;
        Ok(Client(Arc::new(inner)))
    }
}

impl CommandBuilder {
    pub fn arg(mut self, arg: impl ToArgs) -> Self {
        self.cmd.arg(arg);
        self
    }

    pub async fn execute<T: FromValue>(self) -> Result<T> {
        let mut inner = (*self.client).clone();
        let mut cmd = self.cmd;
        let value = inner.send_command(&mut cmd, None).await?;
        from_owned_valkey_value(value)
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
        ConnectionAddr::Unix(_) => Err(ValkeyError::from((
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

fn connection_error_to_ferriskey_error(error: ConnectionError) -> FerrisKeyError {
    match error {
        ConnectionError::Cluster(error) => error,
        ConnectionError::Timeout => std::io::Error::from(std::io::ErrorKind::TimedOut).into(),
        ConnectionError::IoError(error) => error.into(),
        ConnectionError::Configuration(message) => ValkeyError::from((
            ErrorKind::InvalidClientConfig,
            "Connection configuration error",
            message,
        )),
        ConnectionError::Standalone(error) => ValkeyError::from((
            ErrorKind::ClientError,
            "Standalone connection failed",
            format!("{:?}", error),
        )),
    }
}

fn duration_to_millis(ttl: Duration) -> Result<u64> {
    u64::try_from(ttl.as_millis()).map_err(|_| {
        ValkeyError::from((
            ErrorKind::InvalidClientConfig,
            "TTL is too large to encode in milliseconds",
        ))
    })
}

fn duration_to_request_millis(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX)
}
