// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

// Use mimalloc as the global allocator on supported platforms.
// mimalloc works on Linux, macOS, Windows, and FreeBSD.
// Falls back to system allocator on unsupported targets.
// Guard with not(test): when running `cargo test --lib`, the crate is compiled
// both as the test binary *and* as a dependency (via `ferriskey = { path = "." }`
// in dev-deps). Without this guard, two #[global_allocator] registrations would
// collide and fail to compile.
#[cfg(all(
    not(test),
    any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd"
    )
))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[macro_use]
mod macros;

pub mod cluster;
pub mod cmd;
pub mod connection;
pub mod pipeline;
pub mod protocol;
pub(crate) mod retry_strategies;
pub mod value;

// Flat re-exports — canonical paths for crate consumers.
// These replace the old ferriskey::valkey::X paths.
pub use cmd::{Cmd, cmd, pipe};
pub use connection::info::{
    ConnectionAddr, ConnectionInfo, IntoConnectionInfo, PubSubChannelOrPattern,
    PubSubSubscriptionInfo, PubSubSubscriptionKind, TlsMode, ValkeyConnectionInfo,
};
pub use pubsub::push_manager::PushInfo;
pub use retry_strategies::RetryStrategy;
pub use value::{
    Error, ErrorKind, FromValue, InfoDict, ProtocolVersion, PushKind, Result, ToArgs, Value,
    from_owned_value, from_value,
};

pub mod client;
pub mod compression;
#[allow(dead_code)]
pub(crate) mod errors;
pub(crate) mod ferriskey_client;
#[allow(dead_code)]
pub(crate) mod otel_db_semantics;
#[allow(dead_code)]
pub(crate) mod scripts_container;

// High-level public API — the entry point for library users.
pub use ferriskey_client::{
    Client, ClientBuilder, CommandBuilder, PipeCmdBuilder, PipeSlot, ReadFrom, TypedPipeline,
};

/// Connect to a standalone Valkey server.
///
/// ```no_run
/// # async fn example() -> ferriskey::Result<()> {
/// let client = ferriskey::connect("valkey://localhost:6379").await?;
/// # Ok(()) }
/// ```
pub async fn connect(url: &str) -> Result<Client> {
    Client::connect(url).await
}

/// Connect to a Valkey cluster.
///
/// ```no_run
/// # async fn example() -> ferriskey::Result<()> {
/// let client = ferriskey::connect_cluster(&["valkey://node1:6379", "valkey://node2:6379"]).await?;
/// # Ok(()) }
/// ```
pub async fn connect_cluster(urls: &[&str]) -> Result<Client> {
    Client::connect_cluster(urls).await
}
#[allow(dead_code)]
pub(crate) mod cluster_scan_container;
pub(crate) mod iam;
pub mod pubsub;
pub mod request_type;
pub use telemetrylib::Telemetry;
