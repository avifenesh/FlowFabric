// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

#![allow(dead_code)]
use crate::constants::{HOSTNAME_TLS, IP_ADDRESS_V4, IP_ADDRESS_V6};
use ferriskey::cluster::routing::{MultipleNodeRoutingInfo, RoutingInfo};
use ferriskey::connection::factory::FerrisKeyConnectionOptions;
use ferriskey::{
    ConnectionAddr, ProtocolVersion, PushInfo, ValkeyConnectionInfo, Result,
    Value,
};
use ferriskey::{
    client::types::{
        AuthenticationInfo, ConnectionRequest, ConnectionRetryStrategy, NodeAddress, ReadFrom,
        TlsMode,
    },
    client::{Client, StandaloneClient},
};
use futures::Future;
use once_cell::sync::Lazy;
use rand::{Rng, distr::Alphanumeric};
use socket2::{Domain, Socket, Type};
use std::{
    env, fs, io, net::SocketAddr, net::TcpListener, path::PathBuf, process, sync::Mutex,
    time::Duration,
};
use tokio::sync::mpsc;
use versions::Versioning;

pub mod cluster;
pub mod mocks;

/// Feature-aware shim around [`StandaloneClient::create_client`] for tests.
///
/// The real function takes an extra `iam_token_manager` argument under
/// `#[cfg(feature = "iam")]`. Tests don't exercise IAM but still need to
/// compile in both configurations, so this wrapper absorbs the variance
/// and presents a 2-arg surface (`request`, `push_sender`). The remaining
/// `iam_token_manager` / `pubsub_synchronizer` args are always `None` for
/// test harnesses.
#[allow(dead_code)]
pub async fn create_test_standalone_client(
    connection_request: ConnectionRequest,
    push_sender: Option<mpsc::UnboundedSender<PushInfo>>,
) -> std::result::Result<StandaloneClient, ferriskey::value::Error> {
    #[cfg(feature = "iam")]
    {
        StandaloneClient::create_client(connection_request, push_sender, None, None).await
    }
    #[cfg(not(feature = "iam"))]
    {
        StandaloneClient::create_client(connection_request, push_sender, None).await
    }
}

pub(crate) const SHORT_STANDALONE_TEST_TIMEOUT: Duration = Duration::from_millis(20_000);
pub(crate) const LONG_STANDALONE_TEST_TIMEOUT: Duration = Duration::from_millis(40_000);

// Code copied from ferriskey

#[derive(PartialEq, Eq)]
pub enum ServerType {
    Tcp { tls: bool },
    Unix,
}

type SharedServer = Lazy<Mutex<Option<ValkeyServer>>>;
static SHARED_SERVER: SharedServer =
    Lazy::new(|| Mutex::new(Some(ValkeyServer::new(ServerType::Tcp { tls: false }))));

static SHARED_TLS_SERVER: SharedServer =
    Lazy::new(|| Mutex::new(Some(ValkeyServer::new(ServerType::Tcp { tls: true }))));

static SHARED_SERVER_ADDRESS: Lazy<ConnectionAddr> = Lazy::new(|| {
    SHARED_SERVER
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .get_client_addr()
});

static SHARED_TLS_SERVER_ADDRESS: Lazy<ConnectionAddr> = Lazy::new(|| {
    SHARED_TLS_SERVER
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .get_client_addr()
});

pub fn get_shared_server_address(use_tls: bool) -> ConnectionAddr {
    if use_tls {
        SHARED_TLS_SERVER_ADDRESS.clone()
    } else {
        SHARED_SERVER_ADDRESS.clone()
    }
}

#[ctor::dtor]
fn clean_shared_clusters() {
    if let Some(mutex) = SharedServer::get(&SHARED_SERVER) {
        drop(mutex.lock().unwrap().take());
    }
    if let Some(mutex) = SharedServer::get(&SHARED_TLS_SERVER) {
        drop(mutex.lock().unwrap().take());
    }
}

pub struct ValkeyServer {
    pub process: process::Child,
    tempdir: Option<tempfile::TempDir>,
    addr: ferriskey::ConnectionAddr,
}

pub enum Module {
    Json,
}

pub fn get_available_port() -> u16 {
    let attempts = 100;
    for _ in 0..attempts {
        let port = rand::random::<u16>().max(6379);

        let addr4 = format!("{}:{}", IP_ADDRESS_V4, port)
            .parse::<SocketAddr>()
            .unwrap();

        let sock4 = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        if sock4.bind(&socket2::SockAddr::from(addr4)).is_err() {
            continue;
        }

        let addr6 = format!("[{}]:{}", IP_ADDRESS_V6, port)
            .parse::<SocketAddr>()
            .unwrap();

        let sock6 = Socket::new(Domain::IPV6, Type::STREAM, None).unwrap();
        sock6.set_only_v6(true).unwrap();
        if sock6.bind(&socket2::SockAddr::from(addr6)).is_err() {
            continue;
        }

        return port;
    }

    panic!("Failed to find available port after {} attempts", attempts);
}

pub fn get_listener_on_available_port() -> TcpListener {
    let port = get_available_port();
    let addr = &format!("{}:{}", IP_ADDRESS_V4, port)
        .parse::<SocketAddr>()
        .unwrap();

    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket.set_reuse_address(true).unwrap();
    socket.bind(&socket2::SockAddr::from(*addr)).unwrap();
    socket.listen(1).unwrap();

    std::net::TcpListener::from(socket)
}

impl ValkeyServer {
    pub fn new(server_type: ServerType) -> ValkeyServer {
        ValkeyServer::with_modules(server_type, &[])
    }

    pub fn with_modules(server_type: ServerType, modules: &[Module]) -> ValkeyServer {
        let addr = match server_type {
            ServerType::Tcp { tls } => {
                let valkey_port = get_available_port();
                if tls {
                    ferriskey::ConnectionAddr::TcpTls {
                        host: IP_ADDRESS_V4.to_string(),
                        port: valkey_port,
                        insecure: true,
                        tls_params: None,
                    }
                } else {
                    ferriskey::ConnectionAddr::Tcp(IP_ADDRESS_V4.to_string(), valkey_port)
                }
            }
            ServerType::Unix => {
                let (a, b) = rand::random::<(u64, u64)>();
                let path = format!("/tmp/ferriskey-test-{a}-{b}.sock");
                ferriskey::ConnectionAddr::Unix(PathBuf::from(&path))
            }
        };
        ValkeyServer::new_with_addr_and_modules(addr, modules)
    }

    pub fn new_with_tls(use_tls: bool, tls_paths: Option<TlsFilePaths>) -> ValkeyServer {
        let valkey_port = get_available_port();
        let addr = if use_tls {
            ferriskey::ConnectionAddr::TcpTls {
                host: IP_ADDRESS_V4.to_string(),
                port: valkey_port,
                insecure: true,
                tls_params: None,
            }
        } else {
            ferriskey::ConnectionAddr::Tcp(IP_ADDRESS_V4.to_string(), valkey_port)
        };

        ValkeyServer::new_with_addr_tls_modules_and_spawner(addr, tls_paths, &[], false, |cmd| {
            cmd.spawn()
                .unwrap_or_else(|err| panic!("Failed to run {cmd:?}: {err}"))
        })
    }

    pub fn new_with_addr_and_modules(
        addr: ferriskey::ConnectionAddr,
        modules: &[Module],
    ) -> ValkeyServer {
        ValkeyServer::new_with_addr_tls_modules_and_spawner(addr, None, modules, false, |cmd| {
            cmd.spawn()
                .unwrap_or_else(|err| panic!("Failed to run {cmd:?}: {err}"))
        })
    }

    pub fn new_with_addr_tls_modules_and_spawner<
        F: FnOnce(&mut process::Command) -> process::Child,
    >(
        addr: ferriskey::ConnectionAddr,
        tls_paths: Option<TlsFilePaths>,
        modules: &[Module],
        tls_auth_clients: bool,
        spawner: F,
    ) -> ValkeyServer {
        let mut valkey_cmd = process::Command::new("valkey-server");
        valkey_cmd
            .arg("--save")
            .arg("")
            .arg("--appendonly")
            .arg("no");

        for module in modules {
            match module {
                Module::Json => {
                    valkey_cmd
                        .arg("--loadmodule")
                        .arg(env::var("VALKEY_JSON_PATH").expect(
                            "Unable to find path to RedisJSON at VALKEY_JSON_PATH, is it set?",
                        ));
                }
            };
        }

        valkey_cmd
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null());
        let tempdir = tempfile::Builder::new()
            .prefix("valkey")
            .tempdir()
            .expect("failed to create tempdir");
        match addr {
            ferriskey::ConnectionAddr::Tcp(ref bind, server_port) => {
                valkey_cmd
                    .arg("--port")
                    .arg(server_port.to_string())
                    .arg("--bind")
                    .arg(bind);

                ValkeyServer {
                    process: spawner(&mut valkey_cmd),
                    tempdir: None,
                    addr,
                }
            }
            ferriskey::ConnectionAddr::TcpTls { ref host, port, .. } => {
                let tls_paths = tls_paths.unwrap_or_else(|| build_tls_file_paths(&tempdir));
                let tls_auth_clients_arg_value = match tls_auth_clients {
                    true => "yes",
                    _ => "no",
                };

                // prepare valkey with TLS
                valkey_cmd
                    .arg("--tls-port")
                    .arg(port.to_string())
                    .arg("--port")
                    .arg("0")
                    .arg("--tls-cert-file")
                    .arg(&tls_paths.valkey_crt)
                    .arg("--tls-key-file")
                    .arg(&tls_paths.valkey_key)
                    .arg("--tls-ca-cert-file")
                    .arg(&tls_paths.ca_crt)
                    .arg("--tls-auth-clients") // Make it so client doesn't have to send cert
                    .arg(tls_auth_clients_arg_value)
                    .arg("--bind")
                    .arg(host);

                let addr = ferriskey::ConnectionAddr::TcpTls {
                    host: host.clone(),
                    port,
                    insecure: true,
                    tls_params: None,
                };

                ValkeyServer {
                    process: spawner(&mut valkey_cmd),
                    tempdir: Some(tempdir),
                    addr,
                }
            }
            ferriskey::ConnectionAddr::Unix(ref path) => {
                valkey_cmd
                    .arg("--port")
                    .arg("0")
                    .arg("--unixsocket")
                    .arg(path);
                ValkeyServer {
                    process: spawner(&mut valkey_cmd),
                    tempdir: Some(tempdir),
                    addr,
                }
            }
        }
    }

    pub fn get_client_addr(&self) -> ferriskey::ConnectionAddr {
        self.addr.clone()
    }

    pub fn connection_info(&self) -> ferriskey::ConnectionInfo {
        ferriskey::ConnectionInfo {
            addr: self.get_client_addr(),
            valkey: Default::default(),
        }
    }

    pub fn stop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        if let ferriskey::ConnectionAddr::Unix(ref path) = self.get_client_addr() {
            fs::remove_file(path).ok();
        }
    }
}

impl Drop for ValkeyServer {
    fn drop(&mut self) {
        self.stop()
    }
}

fn encode_iter<W>(values: &[Value], writer: &mut W, prefix: &str) -> io::Result<()>
where
    W: io::Write,
{
    write!(writer, "{}{}\r\n", prefix, values.len())?;
    for val in values.iter() {
        encode_value(val, writer)?;
    }
    Ok(())
}

fn encode_result_iter<W>(
    values: &[Result<Value>],
    writer: &mut W,
    prefix: &str,
) -> io::Result<()>
where
    W: io::Write,
{
    write!(writer, "{}{}\r\n", prefix, values.len())?;
    for val in values.iter() {
        match val {
            Ok(v) => encode_value(v, writer)?,
            Err(e) => write!(writer, "-{}\r\n", e)?,
        }
    }
    Ok(())
}

fn encode_map<W>(values: &[(Value, Value)], writer: &mut W, prefix: &str) -> io::Result<()>
where
    W: io::Write,
{
    write!(writer, "{}{}\r\n", prefix, values.len())?;
    for (k, v) in values.iter() {
        encode_value(k, writer)?;
        encode_value(v, writer)?;
    }
    Ok(())
}

pub fn encode_value<W>(value: &Value, writer: &mut W) -> io::Result<()>
where
    W: io::Write,
{
    #![allow(clippy::write_with_newline)]
    match *value {
        Value::Nil => write!(writer, "$-1\r\n"),
        Value::Int(val) => write!(writer, ":{val}\r\n"),
        Value::BulkString(ref val) => {
            write!(writer, "${}\r\n", val.len())?;
            writer.write_all(val)?;
            writer.write_all(b"\r\n")
        }
        Value::Array(ref values) => encode_result_iter(values, writer, "*"),
        Value::Okay => write!(writer, "+OK\r\n"),
        Value::SimpleString(ref s) => write!(writer, "+{s}\r\n"),
        Value::Map(ref values) => encode_map(values, writer, "%"),
        Value::Attribute {
            ref data,
            ref attributes,
        } => {
            encode_map(attributes, writer, "|")?;
            encode_value(data, writer)?;
            Ok(())
        }
        Value::Set(ref values) => encode_iter(values, writer, "~"),
        Value::Double(val) => write!(writer, ",{val}\r\n"),
        Value::Boolean(v) => {
            if v {
                write!(writer, "#t\r\n")
            } else {
                write!(writer, "#f\r\n")
            }
        }
        Value::VerbatimString {
            ref format,
            ref text,
        } => {
            // format is always 3 bytes
            write!(writer, "={}\r\n{}:{}\r\n", 3 + text.len(), format, text)
        }
        Value::BigNumber(ref val) => write!(writer, "({val}\r\n"),
        Value::Push { ref kind, ref data } => {
            write!(writer, ">{}\r\n+{kind}\r\n", data.len() + 1)?;
            for val in data.iter() {
                encode_value(val, writer)?;
            }
            Ok(())
        }
        // Value::ServerError was removed; errors are now in Result wrapping
    }
}

#[derive(Clone)]
pub struct TlsFilePaths {
    valkey_crt: PathBuf,
    valkey_key: PathBuf,
    ca_crt: PathBuf,
}

/// Build and returns TLS file paths using the provided temp directory.
pub fn build_tls_file_paths(tempdir: &tempfile::TempDir) -> TlsFilePaths {
    // Based on shell script in redis's server tests
    // https://github.com/redis/redis/blob/8c291b97b95f2e011977b522acf77ead23e26f55/utils/gen-test-certs.sh

    let temp_dir_path: &std::path::Path = tempdir.path();
    let ca_crt = temp_dir_path.join("ca.crt");
    let ca_key = temp_dir_path.join("ca.key");
    let ca_serial = temp_dir_path.join("ca.txt");
    let valkey_crt = temp_dir_path.join("redis.crt");
    let valkey_key = temp_dir_path.join("redis.key");
    let ext_file = temp_dir_path.join("openssl.cnf");

    fn make_key<S: AsRef<std::ffi::OsStr>>(name: S, size: usize) {
        process::Command::new("openssl")
            .arg("genrsa")
            .arg("-out")
            .arg(name)
            .arg(format!("{size}"))
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null())
            .spawn()
            .expect("failed to spawn openssl")
            .wait()
            .expect("failed to create key");
    }

    // Build CA Key
    make_key(&ca_key, 4096);

    // Build valkey key
    make_key(&valkey_key, 2048);

    // Build CA Cert
    process::Command::new("openssl")
        .arg("req")
        .arg("-x509")
        .arg("-new")
        .arg("-nodes")
        .arg("-sha256")
        .arg("-key")
        .arg(&ca_key)
        .arg("-days")
        .arg("3650")
        .arg("-subj")
        .arg("/O=Valkey Test/CN=Certificate Authority")
        .arg("-out")
        .arg(&ca_crt)
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .spawn()
        .expect("failed to spawn openssl")
        .wait()
        .expect("failed to create CA cert");

    // Build x509v3 extensions file with SAN for IPv4, IPv6, localhost, and test hostname
    fs::write(
        &ext_file,
        format!(
            "keyUsage = digitalSignature, keyEncipherment\nsubjectAltName = IP:{},IP:{},DNS:localhost,DNS:{}",
            IP_ADDRESS_V4, IP_ADDRESS_V6, HOSTNAME_TLS
        ),
    )
    .expect("failed to create x509v3 extensions file");

    // Read valkey key
    let mut key_cmd = process::Command::new("openssl")
        .arg("req")
        .arg("-new")
        .arg("-sha256")
        .arg("-subj")
        .arg("/O=Valkey Test/CN=Generic-cert")
        .arg("-key")
        .arg(&valkey_key)
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::null())
        .spawn()
        .expect("failed to spawn openssl");

    // build valkey cert
    process::Command::new("openssl")
        .arg("x509")
        .arg("-req")
        .arg("-sha256")
        .arg("-CA")
        .arg(&ca_crt)
        .arg("-CAkey")
        .arg(&ca_key)
        .arg("-CAserial")
        .arg(&ca_serial)
        .arg("-CAcreateserial")
        .arg("-days")
        .arg("365")
        .arg("-extfile")
        .arg(&ext_file)
        .arg("-out")
        .arg(&valkey_crt)
        .stdin(key_cmd.stdout.take().expect("should have stdout"))
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .spawn()
        .expect("failed to spawn openssl")
        .wait()
        .expect("failed to create valkey cert");

    key_cmd.wait().expect("failed to create valkey key");

    TlsFilePaths {
        valkey_crt,
        valkey_key,
        ca_crt,
    }
}

impl TlsFilePaths {
    pub fn read_ca_cert_as_bytes(&self) -> Vec<u8> {
        fs::read(&self.ca_crt).expect("Failed to read CA certificate file")
    }
    pub fn read_valkey_cert_as_bytes(&self) -> Vec<u8> {
        fs::read(&self.valkey_crt).expect("Failed to read redis certificate file")
    }
    pub fn read_valkey_key_as_bytes(&self) -> Vec<u8> {
        fs::read(&self.valkey_key).expect("Failed to read redis private key file")
    }
}

pub async fn wait_for_server_to_become_ready(server_address: &ConnectionAddr) {
    let millisecond = Duration::from_millis(1);
    let mut retries = 0;
    let client = ferriskey::connection::factory::Client::open(ferriskey::ConnectionInfo {
        addr: server_address.clone(),
        valkey: ValkeyConnectionInfo::default(),
    })
    .unwrap();
    loop {
        match client
            .get_multiplexed_async_connection(FerrisKeyConnectionOptions::default())
            .await
        {
            Err(err) => {
                if err.is_connection_refusal() {
                    tokio::time::sleep(millisecond).await;
                    retries += 1;
                    if retries > 100000 {
                        panic!("Tried to connect too many times, last error: {err}");
                    }
                } else {
                    panic!("Could not connect: {err}");
                }
            }
            Ok(mut con) => {
                while con
                    .send_packed_command(&ferriskey::cmd("PING"))
                    .await
                    .is_err()
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let _: Result<()> = ferriskey::cmd("FLUSHDB")
                    .query_async(&mut con)
                    .await;
                break;
            }
        }
    }
}

pub fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

pub fn block_on_all<F>(f: F) -> F::Output
where
    F: Future,
{
    current_thread_runtime().block_on(f)
}

pub fn get_address_info(address: &ConnectionAddr) -> NodeAddress {
    match address {
        ConnectionAddr::Tcp(host, port) => NodeAddress {
            host: host.to_string(),
            port: *port,
        },
        ConnectionAddr::TcpTls { host, port, .. } => NodeAddress {
            host: host.to_string(),
            port: *port,
        },
        ConnectionAddr::Unix(_) => unreachable!("Unix connection not tested"),
    }
}

pub fn generate_random_string(length: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

pub async fn send_get(client: &mut Client, key: &str) -> Result<Value> {
    let mut get_command = ferriskey::Cmd::new();
    get_command.arg("GET").arg(key);
    client.send_command(&mut get_command, None).await
}

pub async fn send_set_and_get(mut client: Client, key: String) {
    const VALUE_LENGTH: usize = 10;
    let value = generate_random_string(VALUE_LENGTH);

    let mut set_command = ferriskey::Cmd::new();
    set_command.arg("SET").arg(key.as_str()).arg(value.clone());
    let set_result = client.send_command(&mut set_command, None).await.unwrap();
    let mut get_command = ferriskey::Cmd::new();
    get_command.arg("GET").arg(key);
    let get_result = client.send_command(&mut get_command, None).await.unwrap();

    assert_eq!(set_result, Value::Okay);
    assert_eq!(get_result, Value::BulkString(value.into_bytes().into()));
}

pub struct TestBasics {
    pub server: Option<ValkeyServer>,
    pub client: StandaloneClient,
    pub push_receiver: mpsc::UnboundedReceiver<PushInfo>,
}

/// Repeatedly calls `f` until it returns `Some`, using a default timeout of 3 seconds.
/// Panics if the timeout is exceeded.
pub async fn retry<T, Fut>(f: impl Fn() -> Fut) -> T
where
    Fut: Future<Output = Option<T>>,
{
    retry_until_timeout(f, std::time::Duration::from_millis(3000)).await
}

/// Repeatedly calls `f` every 5ms until it returns `Some` or the `timeout` is exceeded.
/// Panics if the timeout is exceeded.
pub async fn retry_until_timeout<T, Fut>(f: impl Fn() -> Fut, timeout: std::time::Duration) -> T
where
    Fut: Future<Output = Option<T>>,
{
    let start = tokio::time::Instant::now();
    while start.elapsed() < timeout {
        if let Some(value) = f().await {
            return value;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("Timed out: retry exceeded {:?}", timeout);
}

pub async fn setup_acl(addr: &ConnectionAddr, connection_info: &ValkeyConnectionInfo) {
    let client = ferriskey::connection::factory::Client::open(ferriskey::ConnectionInfo {
        addr: addr.clone(),
        valkey: ValkeyConnectionInfo::default(),
    })
    .unwrap();
    let mut connection = retry(|| async {
        client
            .get_multiplexed_async_connection(FerrisKeyConnectionOptions::default())
            .await
            .ok()
    })
    .await;

    let password = connection_info.password.clone().unwrap();
    let username = connection_info
        .username
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let mut cmd = ferriskey::cmd("ACL");
    cmd.arg("SETUSER")
        .arg(username)
        .arg("on")
        .arg("allkeys")
        .arg("+@all")
        .arg(format!(">{password}"));
    connection.send_packed_command(&cmd).await.unwrap();
}

#[derive(Eq, PartialEq, Default, Clone, Debug)]
pub enum ClusterMode {
    #[default]
    Disabled,
    Enabled,
}

pub fn create_connection_request(
    addresses: &[ConnectionAddr],
    configuration: &TestConfiguration,
) -> ConnectionRequest {
    let addresses_info: Vec<NodeAddress> = addresses.iter().map(get_address_info).collect();
    let tls_mode = if configuration.use_tls {
        Some(TlsMode::InsecureTls)
    } else {
        None
    };
    let connection_info = configuration.connection_info.clone().unwrap_or_default();
    let auth = if connection_info.password.is_some() || connection_info.username.is_some() {
        Some(AuthenticationInfo {
            password: connection_info.password,
            username: connection_info.username,
            // iam_config is only present on `feature = "iam"` builds.
            // Tests don't exercise IAM, so always default when the field exists.
            #[cfg(feature = "iam")]
            iam_config: None,
        })
    } else {
        None
    };

    ConnectionRequest {
        addresses: addresses_info,
        database_id: configuration.database_id as i64,
        tls_mode,
        cluster_mode_enabled: ClusterMode::Enabled == configuration.cluster_mode,
        request_timeout: configuration.request_timeout,
        read_from: configuration.read_from.clone(),
        connection_retry_strategy: configuration.connection_retry_strategy.or(Some(
            ConnectionRetryStrategy {
                number_of_retries: 5,
                factor: 100,
                exponent_base: 2,
                jitter_percent: Some(20),
            },
        )),
        authentication_info: auth,
        client_name: configuration.client_name.clone(),
        protocol: Some(configuration.protocol),
        lazy_connect: configuration.lazy_connect,
        tcp_nodelay: true,
        ..Default::default()
    }
}

#[derive(Default, Clone, Debug)]
pub struct TestConfiguration {
    pub use_tls: bool,
    pub connection_retry_strategy: Option<ConnectionRetryStrategy>,
    pub connection_info: Option<ValkeyConnectionInfo>,
    pub cluster_mode: ClusterMode,
    pub request_timeout: Option<u32>,
    pub shared_server: bool,
    pub read_from: Option<ReadFrom>,
    pub database_id: u32,
    pub client_name: Option<String>,
    pub client_az: Option<String>,
    pub protocol: ProtocolVersion,
    pub lazy_connect: bool,
}

/// Creates only the server (and optionally ACL) without also creating a StandaloneClient.
/// Use this when the caller needs the server address but will create their own client.
pub(crate) async fn create_standalone_server(
    configuration: &TestConfiguration,
) -> (Option<ValkeyServer>, ConnectionAddr) {
    let server = if !configuration.shared_server {
        Some(ValkeyServer::new(ServerType::Tcp {
            tls: configuration.use_tls,
        }))
    } else {
        None
    };
    let connection_addr = if !configuration.shared_server {
        server.as_ref().unwrap().get_client_addr()
    } else {
        get_shared_server_address(configuration.use_tls)
    };

    if let Some(valkey_connection_info) = &configuration.connection_info
        && valkey_connection_info.password.is_some()
    {
        assert!(!configuration.shared_server);
        setup_acl(&connection_addr, valkey_connection_info).await;
    }

    (server, connection_addr)
}

pub(crate) async fn setup_test_basics_internal(configuration: &TestConfiguration) -> TestBasics {
    let server = if !configuration.shared_server {
        Some(ValkeyServer::new(ServerType::Tcp {
            tls: configuration.use_tls,
        }))
    } else {
        None
    };
    let connection_addr = if !configuration.shared_server {
        server.as_ref().unwrap().get_client_addr()
    } else {
        get_shared_server_address(configuration.use_tls)
    };

    if let Some(valkey_connection_info) = &configuration.connection_info
        && valkey_connection_info.password.is_some()
    {
        assert!(!configuration.shared_server);
        setup_acl(&connection_addr, valkey_connection_info).await;
    }
    let mut connection_request = create_connection_request(&[connection_addr], configuration);
    connection_request.cluster_mode_enabled = false;
    connection_request.protocol = Some(configuration.protocol);
    let (push_sender, push_receiver) = tokio::sync::mpsc::unbounded_channel();
    let client = create_test_standalone_client(connection_request, Some(push_sender))
        .await
        .unwrap();

    TestBasics {
        server,
        client,
        push_receiver,
    }
}

pub async fn setup_test_basics(use_tls: bool) -> TestBasics {
    setup_test_basics_internal(&TestConfiguration {
        use_tls,
        ..Default::default()
    })
    .await
}

#[cfg(test)]
#[ctor::ctor]
fn init() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();

    // This needs to be done before any TLS connections are made
    let _ = rustls::crypto::CryptoProvider::install_default(
        rustls::crypto::aws_lc_rs::default_provider(),
    );
}

pub async fn kill_connection(client: &mut impl ferriskey::client::ValkeyClientForTests) {
    let mut client_kill_cmd = ferriskey::cmd("CLIENT");
    client_kill_cmd.arg("KILL").arg("SKIPME").arg("NO");

    let _ = client
        .send_command(
            &mut client_kill_cmd,
            Some(RoutingInfo::MultiNode((
                MultipleNodeRoutingInfo::AllNodes,
                Some(ferriskey::cluster::routing::ResponsePolicy::AllSucceeded),
            ))),
        )
        .await
        .unwrap();
}

pub async fn kill_connection_for_route(
    client: &mut impl ferriskey::client::ValkeyClientForTests,
    route: RoutingInfo,
) {
    let mut client_kill_cmd = ferriskey::cmd("CLIENT");
    client_kill_cmd.arg("KILL").arg("SKIPME").arg("NO");

    let _ = client
        .send_command(&mut client_kill_cmd, Some(route))
        .await
        .unwrap();
}

pub enum BackingServer {
    Standalone(Option<ValkeyServer>),
    Cluster(Option<cluster::ValkeyCluster>),
}

/// Get the server version from a client connection
pub async fn get_server_version(
    client: &mut impl ferriskey::client::ValkeyClientForTests,
) -> (u16, u16, u16) {
    let mut info_cmd = ferriskey::cmd("INFO");
    info_cmd.arg("SERVER");

    let info_result = client.send_command(&mut info_cmd, None).await.unwrap();
    let info_string = match info_result {
        Value::BulkString(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Value::VerbatimString { text, .. } => text,
        Value::Map(node_results) => {
            // In cluster mode, INFO returns a map of node -> info string
            // We just need to get the version from any node (they should all be the same)
            if let Some((_, node_info)) = node_results.first() {
                match node_info {
                    Value::VerbatimString { text, .. } => text.clone(),
                    Value::BulkString(bytes) => String::from_utf8_lossy(bytes).to_string(),
                    _ => panic!(
                        "Unexpected node info type in cluster INFO response: {:?}",
                        node_info
                    ),
                }
            } else {
                panic!("Empty cluster INFO response");
            }
        }
        _ => panic!("Unexpected INFO response type: {:?}", info_result),
    };

    // Parse the INFO response to extract version
    // First try to find valkey_version, then fall back to redis_version
    for line in info_string.lines() {
        if let Some(version_str) = line.strip_prefix("valkey_version:") {
            return parse_version_string(version_str);
        }
    }

    // If no valkey_version found, look for redis_version
    for line in info_string.lines() {
        if let Some(version_str) = line.strip_prefix("redis_version:") {
            return parse_version_string(version_str);
        }
    }

    panic!("Could not find version information in INFO response");
}

/// Parse a version string like "8.1.3" into (8, 1, 3)
fn parse_version_string(version_str: &str) -> (u16, u16, u16) {
    let parts: Vec<&str> = version_str.split('.').collect();
    if parts.len() >= 3 {
        let major = parts[0].parse().unwrap_or(0);
        let minor = parts[1].parse().unwrap_or(0);
        let patch = parts[2].parse().unwrap_or(0);
        (major, minor, patch)
    } else {
        panic!("Invalid version format: {}", version_str);
    }
}

/// Check if the server version is greater than or equal to the specified version
pub async fn version_greater_or_equal(
    client: &mut impl ferriskey::client::ValkeyClientForTests,
    version: &str,
) -> bool {
    let (major, minor, patch) = get_server_version(client).await;
    let server_version = Versioning::new(format!("{major}.{minor}.{patch}")).unwrap();
    let compared_version = Versioning::new(version).unwrap();
    server_version >= compared_version
}

/// Extract client ID from CLIENT INFO response string
pub fn extract_client_id(client_info: &str) -> Option<String> {
    client_info
        .split_whitespace()
        .find(|part| part.starts_with("id="))
        .and_then(|id_part| id_part.strip_prefix("id="))
        .map(|id| id.to_string())
}

/// Assert that a client is connected by sending a PING command
pub async fn assert_connected(client: &mut impl ferriskey::client::ValkeyClientForTests) {
    let mut ping_cmd = ferriskey::cmd("PING");
    let ping_result = client.send_command(&mut ping_cmd, None).await;
    assert_eq!(
        ping_result.unwrap(),
        Value::SimpleString("PONG".to_string())
    );
}
