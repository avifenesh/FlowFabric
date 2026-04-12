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
pub mod connection;
pub mod protocol;
pub mod cmd;
pub mod pipeline;
pub(crate) mod retry_strategies;
pub mod value;

// Flat re-exports — canonical paths for crate consumers.
// These replace the old ferriskey::valkey::X paths.
pub use cmd::{Arg, Cmd, cmd, pipe};
pub use value::{
    ErrorKind, FromValkeyValue, InfoDict, ProtocolVersion, PushKind, ToValkeyArgs, Value,
    ValkeyError, ValkeyFuture, ValkeyResult, ValkeyWrite,
    from_valkey_value, from_owned_valkey_value,
};
pub use connection::info::{
    ConnectionAddr, ConnectionInfo, IntoConnectionInfo, PubSubChannelOrPattern,
    PubSubSubscriptionInfo, PubSubSubscriptionKind, TlsMode, ValkeyConnectionInfo,
};
pub use pubsub::push_manager::PushInfo;
pub use retry_strategies::RetryStrategy;

pub mod client;
pub mod compression;
pub mod errors;
pub mod ferriskey_client;
pub mod otel_db_semantics;
pub mod scripts_container;
pub use client::ConnectionRequest;

// High-level public API — the entry point for library users.
pub use ferriskey_client::Result as FerrisKeyResult;
pub use ferriskey_client::{
    Client, ClientBuilder, CommandBuilder, FerrisKeyError, FromValue, ReadFrom, ToArgs,
};
pub mod cluster_scan_container;
pub mod iam;
pub mod pubsub;
pub mod request_type;
pub use telemetrylib::{
    DEFAULT_FLUSH_SIGNAL_INTERVAL_MS, DEFAULT_TRACE_SAMPLE_PERCENTAGE, FerrisKeyOtel,
    FerrisKeyOtelConfigBuilder, FerrisKeySpan, OtelSignalsExporter, Telemetry,
};
