//! This module provides Redis Cluster support.
//!
//! The primary entry point is [`ClusterClientBuilder`] which configures
//! cluster connections used by the async cluster module.

pub use crate::valkey::cluster_client::{ClusterClient, ClusterClientBuilder};
use crate::valkey::cmd::Cmd;
use crate::valkey::connection::{ConnectionAddr, ConnectionInfo, ValkeyConnectionInfo};
use crate::valkey::tls::TlsConnParams;
use crate::valkey::types::{ErrorKind, ValkeyResult};
use std::str::FromStr;

pub use crate::valkey::connection::TlsMode;

pub(crate) fn get_connection_info(
    node: &str,
    cluster_params: crate::valkey::cluster_client::ClusterParams,
) -> ValkeyResult<ConnectionInfo> {
    let invalid_error = || (ErrorKind::InvalidClientConfig, "Invalid node string");
    let (host, port) = node
        .rsplit_once(':')
        .and_then(|(host, port)| {
            Some(host.trim_start_matches('[').trim_end_matches(']'))
                .filter(|h| !h.is_empty())
                .zip(u16::from_str(port).ok())
        })
        .ok_or_else(invalid_error)?;
    Ok(ConnectionInfo {
        addr: get_connection_addr(host.to_string(), port, cluster_params.tls, cluster_params.tls_params),
        valkey: ValkeyConnectionInfo {
            password: cluster_params.password,
            username: cluster_params.username,
            client_name: cluster_params.client_name,
            lib_name: cluster_params.lib_name,
            protocol: cluster_params.protocol,
            db: cluster_params.database_id,
        },
    })
}

pub(crate) fn get_connection_addr(
    host: String,
    port: u16,
    tls: Option<TlsMode>,
    tls_params: Option<TlsConnParams>,
) -> ConnectionAddr {
    match tls {
        Some(TlsMode::Secure) => ConnectionAddr::TcpTls { host, port, insecure: false, tls_params },
        Some(TlsMode::Insecure) => ConnectionAddr::TcpTls { host, port, insecure: true, tls_params },
        _ => ConnectionAddr::Tcp(host, port),
    }
}

pub(crate) fn slot_cmd() -> Cmd {
    let mut cmd = Cmd::new();
    cmd.arg("CLUSTER").arg("SLOTS");
    cmd
}
