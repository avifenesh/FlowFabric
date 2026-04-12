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
pub use cmd::{Arg, Cmd, cmd, pipe};
pub use connection::info::{
    ConnectionAddr, ConnectionInfo, IntoConnectionInfo, PubSubChannelOrPattern,
    PubSubSubscriptionInfo, PubSubSubscriptionKind, TlsMode, ValkeyConnectionInfo,
};
pub use pubsub::push_manager::PushInfo;
pub use retry_strategies::RetryStrategy;
pub use value::{
    ErrorKind, FromValkeyValue, InfoDict, ProtocolVersion, PushKind, ToValkeyArgs, ValkeyError,
    ValkeyFuture, ValkeyResult, ValkeyWrite, Value, from_owned_valkey_value, from_valkey_value,
};

pub mod client;
pub mod compression;
pub mod errors;
pub mod ferriskey_client;
pub mod otel_db_semantics;
pub mod scripts_container;
pub use client::ConnectionRequest;

// High-level public API — the entry point for library users.
pub type Error = ValkeyError;
pub type Result<T> = FerrisKeyResult<T>;
pub use ferriskey_client::Result as FerrisKeyResult;
pub use ferriskey_client::{
    Client, ClientBuilder, CommandBuilder, FerrisKeyError, FromValue, PipeCmdBuilder, PipeSlot,
    ReadFrom, ToArgs, TypedPipeline,
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
pub mod cluster_scan_container;
pub mod iam;
pub mod pubsub;
pub mod request_type;
pub use telemetrylib::{
    DEFAULT_FLUSH_SIGNAL_INTERVAL_MS, DEFAULT_TRACE_SAMPLE_PERCENTAGE, FerrisKeyOtel,
    FerrisKeyOtelConfigBuilder, FerrisKeySpan, OtelSignalsExporter, Telemetry,
};
