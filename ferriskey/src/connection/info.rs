use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;

use crate::valkey::ProtocolVersion;
use crate::pipeline::Pipeline;
use crate::value::{ErrorKind, ValkeyError, ValkeyResult};

use crate::connection::tls::TlsConnParams;

static CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

static DEFAULT_PORT: u16 = 6379;

/// Checks if a given string is a valid redis URL.
pub fn parse_redis_url(input: &str) -> Option<url::Url> {
    match url::Url::parse(input) {
        Ok(result) => match result.scheme() {
            "redis" | "rediss" | "redis+unix" | "unix" => Some(result),
            _ => None,
        },
        Err(_) => None,
    }
}

/// TlsMode indicates use or do not use verification of certification.
#[derive(Clone, Copy)]
pub enum TlsMode {
    /// Secure verify certification.
    Secure,
    /// Insecure do not verify certification.
    Insecure,
}

/// Defines the connection address.
#[derive(Clone, Debug)]
pub enum ConnectionAddr {
    /// Format for this is `(host, port)`.
    Tcp(String, u16),
    /// Format for this is `(host, port)`.
    TcpTls {
        /// Hostname
        host: String,
        /// Port
        port: u16,
        /// Disable hostname verification when connecting.
        insecure: bool,
        /// TLS certificates and client key.
        tls_params: Option<TlsConnParams>,
    },
    /// Format for this is the path to the unix socket.
    Unix(PathBuf),
}

impl PartialEq for ConnectionAddr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ConnectionAddr::Tcp(host1, port1), ConnectionAddr::Tcp(host2, port2)) => {
                host1 == host2 && port1 == port2
            }
            (
                ConnectionAddr::TcpTls {
                    host: host1,
                    port: port1,
                    insecure: insecure1,
                    tls_params: _,
                },
                ConnectionAddr::TcpTls {
                    host: host2,
                    port: port2,
                    insecure: insecure2,
                    tls_params: _,
                },
            ) => port1 == port2 && host1 == host2 && insecure1 == insecure2,
            (ConnectionAddr::Unix(path1), ConnectionAddr::Unix(path2)) => path1 == path2,
            _ => false,
        }
    }
}

impl Eq for ConnectionAddr {}

impl ConnectionAddr {
    /// Checks if this address is supported.
    pub fn is_supported(&self) -> bool {
        match *self {
            ConnectionAddr::Tcp(_, _) => true,
            ConnectionAddr::TcpTls { .. } => true,
            ConnectionAddr::Unix(_) => cfg!(unix),
        }
    }
}

impl fmt::Display for ConnectionAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ConnectionAddr::Tcp(ref host, port) => write!(f, "{host}:{port}"),
            ConnectionAddr::TcpTls { ref host, port, .. } => write!(f, "{host}:{port}"),
            ConnectionAddr::Unix(ref path) => write!(f, "{}", path.display()),
        }
    }
}

/// Holds the connection information that redis should use for connecting.
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    /// A connection address for where to connect to.
    pub addr: ConnectionAddr,
    /// A boxed connection address for where to connect to.
    pub valkey: ValkeyConnectionInfo,
}

/// Types of pubsub subscriptions
#[derive(Clone, Debug, PartialEq, Eq, Hash, Copy)]
pub enum PubSubSubscriptionKind {
    /// Exact channel name.
    Exact = 0,
    /// Pattern-based channel name.
    Pattern = 1,
    /// Sharded pubsub mode.
    Sharded = 2,
}

impl From<PubSubSubscriptionKind> for usize {
    fn from(val: PubSubSubscriptionKind) -> Self {
        val as usize
    }
}

/// Type for pubsub channels/patterns
pub type PubSubChannelOrPattern = Vec<u8>;

/// Type for pubsub channels/patterns
pub type PubSubSubscriptionInfo = HashMap<PubSubSubscriptionKind, HashSet<PubSubChannelOrPattern>>;

/// Redis specific/connection independent information used to establish a connection to redis.
#[derive(Clone, Debug, Default)]
pub struct ValkeyConnectionInfo {
    /// The database number to use.  This is usually `0`.
    pub db: i64,
    /// Optionally a username that should be used for connection.
    pub username: Option<String>,
    /// Optionally a password that should be used for connection.
    pub password: Option<String>,
    /// Version of the protocol to use.
    pub protocol: ProtocolVersion,
    /// Optionally a client name that should be used for connection
    pub client_name: Option<String>,
    /// Optionally a library name that should be used for connection
    pub lib_name: Option<String>,
}

impl FromStr for ConnectionInfo {
    type Err = ValkeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.into_connection_info()
    }
}

/// Converts an object into a connection info struct.
pub trait IntoConnectionInfo {
    /// Converts the object into a connection info object.
    fn into_connection_info(self) -> ValkeyResult<ConnectionInfo>;
}

impl IntoConnectionInfo for ConnectionInfo {
    fn into_connection_info(self) -> ValkeyResult<ConnectionInfo> {
        Ok(self)
    }
}

impl IntoConnectionInfo for &str {
    fn into_connection_info(self) -> ValkeyResult<ConnectionInfo> {
        match parse_redis_url(self) {
            Some(u) => u.into_connection_info(),
            None => fail!((ErrorKind::InvalidClientConfig, "Redis URL did not parse")),
        }
    }
}

impl<T> IntoConnectionInfo for (T, u16)
where
    T: Into<String>,
{
    fn into_connection_info(self) -> ValkeyResult<ConnectionInfo> {
        Ok(ConnectionInfo {
            addr: ConnectionAddr::Tcp(self.0.into(), self.1),
            valkey: ValkeyConnectionInfo::default(),
        })
    }
}

impl IntoConnectionInfo for String {
    fn into_connection_info(self) -> ValkeyResult<ConnectionInfo> {
        match parse_redis_url(&self) {
            Some(u) => u.into_connection_info(),
            None => fail!((ErrorKind::InvalidClientConfig, "Redis URL did not parse")),
        }
    }
}

fn url_to_tcp_connection_info(url: url::Url) -> ValkeyResult<ConnectionInfo> {
    let host = match url.host() {
        Some(host) => match host {
            url::Host::Domain(path) => path.to_string(),
            url::Host::Ipv4(v4) => v4.to_string(),
            url::Host::Ipv6(v6) => v6.to_string(),
        },
        None => fail!((ErrorKind::InvalidClientConfig, "Missing hostname")),
    };
    let port = url.port().unwrap_or(DEFAULT_PORT);
    let addr = if url.scheme() == "rediss" {
        match url.fragment() {
            Some("insecure") => ConnectionAddr::TcpTls {
                host,
                port,
                insecure: true,
                tls_params: None,
            },
            Some(_) => fail!((
                ErrorKind::InvalidClientConfig,
                "only #insecure is supported as URL fragment"
            )),
            _ => ConnectionAddr::TcpTls {
                host,
                port,
                insecure: false,
                tls_params: None,
            },
        }
    } else {
        ConnectionAddr::Tcp(host, port)
    };
    let query: HashMap<_, _> = url.query_pairs().collect();
    Ok(ConnectionInfo {
        addr,
        valkey: ValkeyConnectionInfo {
            db: match url.path().trim_matches('/') {
                "" => 0,
                path => path.parse::<i64>().map_err(|_| -> ValkeyError {
                    (ErrorKind::InvalidClientConfig, "Invalid database number").into()
                })?,
            },
            username: if url.username().is_empty() {
                None
            } else {
                match percent_encoding::percent_decode(url.username().as_bytes()).decode_utf8() {
                    Ok(decoded) => Some(decoded.into_owned()),
                    Err(_) => fail!((
                        ErrorKind::InvalidClientConfig,
                        "Username is not valid UTF-8 string"
                    )),
                }
            },
            password: match url.password() {
                Some(pw) => match percent_encoding::percent_decode(pw.as_bytes()).decode_utf8() {
                    Ok(decoded) => Some(decoded.into_owned()),
                    Err(_) => fail!((
                        ErrorKind::InvalidClientConfig,
                        "Password is not valid UTF-8 string"
                    )),
                },
                None => None,
            },
            protocol: match query.get("resp3") {
                Some(v) => {
                    if v == "true" {
                        ProtocolVersion::RESP3
                    } else {
                        ProtocolVersion::RESP2
                    }
                }
                _ => ProtocolVersion::RESP2,
            },
            client_name: None,
            lib_name: None,
        },
    })
}

#[cfg(unix)]
fn url_to_unix_connection_info(url: url::Url) -> ValkeyResult<ConnectionInfo> {
    let query: HashMap<_, _> = url.query_pairs().collect();
    Ok(ConnectionInfo {
        addr: ConnectionAddr::Unix(url.to_file_path().map_err(|_| -> ValkeyError {
            (ErrorKind::InvalidClientConfig, "Missing path").into()
        })?),
        valkey: ValkeyConnectionInfo {
            db: match query.get("db") {
                Some(db) => db.parse::<i64>().map_err(|_| -> ValkeyError {
                    (ErrorKind::InvalidClientConfig, "Invalid database number").into()
                })?,
                None => 0,
            },
            username: query.get("user").map(|username| username.to_string()),
            password: query.get("pass").map(|password| password.to_string()),
            protocol: match query.get("resp3") {
                Some(v) => {
                    if v == "true" {
                        ProtocolVersion::RESP3
                    } else {
                        ProtocolVersion::RESP2
                    }
                }
                _ => ProtocolVersion::RESP2,
            },
            client_name: None,
            lib_name: None,
        },
    })
}

#[cfg(not(unix))]
fn url_to_unix_connection_info(_: url::Url) -> ValkeyResult<ConnectionInfo> {
    fail!((
        ErrorKind::InvalidClientConfig,
        "Unix sockets are not available on this platform."
    ));
}

impl IntoConnectionInfo for url::Url {
    fn into_connection_info(self) -> ValkeyResult<ConnectionInfo> {
        match self.scheme() {
            "redis" | "rediss" => url_to_tcp_connection_info(self),
            "unix" | "redis+unix" => url_to_unix_connection_info(self),
            _ => fail!((
                ErrorKind::InvalidClientConfig,
                "URL provided is not a valkey URL"
            )),
        }
    }
}

// --- Utilities used by aio and other modules ---

pub(crate) fn create_rustls_config(
    insecure: bool,
    tls_params: Option<TlsConnParams>,
) -> ValkeyResult<rustls::ClientConfig> {
    CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });

    use crate::connection::tls::ClientTlsParams;
    use rustls_platform_verifier::BuilderVerifierExt;

    let config = match tls_params {
        Some(tls_params) if tls_params.root_cert_store.is_some() => {
            let root_cert_store = tls_params.root_cert_store.unwrap();
            let config = rustls::ClientConfig::builder().with_root_certificates(root_cert_store);
            match tls_params.client_tls_params {
                Some(ClientTlsParams {
                    client_cert_chain: client_cert,
                    client_key,
                }) => config
                    .with_client_auth_cert(client_cert, client_key)
                    .map_err(|err| {
                        tls_config_error(
                            "Failed to configure client cert auth with custom root store",
                            err,
                        )
                    })?,
                None => config.with_no_client_auth(),
            }
        }
        Some(tls_params) => {
            let config = rustls::ClientConfig::builder()
                .with_platform_verifier()
                .map_err(|err| {
                    tls_config_error("Failed to configure platform certificate verifier", err)
                })?;
            match tls_params.client_tls_params {
                Some(ClientTlsParams {
                    client_cert_chain: client_cert,
                    client_key,
                }) => config
                    .with_client_auth_cert(client_cert, client_key)
                    .map_err(|err| {
                        tls_config_error(
                            "Failed to configure client cert auth with platform verifier",
                            err,
                        )
                    })?,
                None => config.with_no_client_auth(),
            }
        }
        None => rustls::ClientConfig::builder()
            .with_platform_verifier()
            .map_err(|err| tls_config_error("Failed to configure default TLS settings", err))?
            .with_no_client_auth(),
    };

    if insecure {
        let mut config = config;
        config.enable_sni = false;
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoCertificateVerification {
                supported: rustls::crypto::aws_lc_rs::default_provider()
                    .signature_verification_algorithms,
            }));
        return Ok(config);
    }

    Ok(config)
}

fn tls_config_error(context: &'static str, error: impl std::fmt::Display) -> ValkeyError {
    ValkeyError::from((ErrorKind::InvalidClientConfig, context, error.to_string()))
}

struct NoCertificateVerification {
    supported: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer,
        _intermediates: &[rustls_pki_types::CertificateDer],
        _server_name: &rustls::pki_types::ServerName,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported.supported_schemes()
    }
}

impl fmt::Debug for NoCertificateVerification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NoCertificateVerification").finish()
    }
}

pub(crate) fn client_set_info_pipeline(lib_name: Option<&str>) -> Pipeline {
    let mut pipeline = crate::valkey::pipe();
    let lib_name_value = lib_name.unwrap_or("UnknownClient");
    let final_lib_name = option_env!("FERRISKEY_NAME").unwrap_or(lib_name_value);
    pipeline
        .cmd("CLIENT")
        .arg("SETINFO")
        .arg("LIB-NAME")
        .arg(final_lib_name)
        .ignore();
    pipeline
        .cmd("CLIENT")
        .arg("SETINFO")
        .arg("LIB-VER")
        .arg(env!("CARGO_PKG_VERSION"))
        .ignore();
    pipeline
}

/// Common logic for checking real cause of hello3 command error
pub fn get_resp3_hello_command_error(err: ValkeyError) -> ValkeyError {
    if let Some(detail) = err.detail()
        && detail.starts_with("unknown command `HELLO`")
    {
        return (
            ErrorKind::RESP3NotSupported,
            "Redis Server doesn't support HELLO command therefore resp3 cannot be used",
        )
            .into();
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_set_info_pipeline_default_lib_name() {
        let pipeline = client_set_info_pipeline(None);
        let packed_commands = pipeline.get_packed_pipeline();
        let cmd_str = String::from_utf8_lossy(&packed_commands);
        assert!(cmd_str.contains("CLIENT"));
        assert!(cmd_str.contains("SETINFO"));
        assert!(cmd_str.contains("LIB-NAME"));
        assert!(cmd_str.contains("FerrisKey") || cmd_str.contains("UnknownClient"));
    }

    #[test]
    fn test_parse_redis_url() {
        let cases = vec![
            ("redis://127.0.0.1", true),
            ("redis://[::1]", true),
            ("redis+unix:///run/redis.sock", true),
            ("unix:///run/redis.sock", true),
            ("http://127.0.0.1", false),
            ("tcp://127.0.0.1", false),
        ];
        for (url, expected) in cases.into_iter() {
            let res = parse_redis_url(url);
            assert_eq!(
                res.is_some(),
                expected,
                "Parsed result of `{url}` is not expected"
            );
        }
    }

    #[test]
    fn test_url_to_tcp_connection_info() {
        let cases = vec![
            (
                url::Url::parse("redis://127.0.0.1").unwrap(),
                ConnectionInfo {
                    addr: ConnectionAddr::Tcp("127.0.0.1".to_string(), 6379),
                    valkey: Default::default(),
                },
            ),
            (
                url::Url::parse("redis://[::1]").unwrap(),
                ConnectionInfo {
                    addr: ConnectionAddr::Tcp("::1".to_string(), 6379),
                    valkey: Default::default(),
                },
            ),
        ];
        for (url, expected) in cases.into_iter() {
            let res = url_to_tcp_connection_info(url.clone()).unwrap();
            assert_eq!(res.addr, expected.addr, "addr of {url} is not expected");
            assert_eq!(
                res.valkey.db, expected.valkey.db,
                "db of {url} is not expected"
            );
        }
    }
}
