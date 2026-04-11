//! Async Valkey client library - connection management, RESP protocol,
//! cluster topology, and multiplexed connections.

#![allow(missing_docs)]

// public api
pub use crate::valkey::client::Client;
pub use crate::valkey::client::GlideConnectionOptions;
pub use crate::valkey::client::IAMTokenProvider;
pub use crate::valkey::cmd::{cmd, fenced_cmd, pack_command, pipe, Arg, Cmd};
pub use crate::valkey::connection::{
    TlsMode,
    parse_redis_url, ConnectionAddr, ConnectionInfo,
    IntoConnectionInfo, PubSubChannelOrPattern, PubSubSubscriptionInfo,
    PubSubSubscriptionKind, ValkeyConnectionInfo,
};
pub use crate::valkey::parser::{parse_valkey_value, Parser};
pub use crate::valkey::pipeline::{Pipeline, PipelineRetryStrategy};
pub use crate::valkey::pubsub_synchronizer::PubSubSynchronizer;
pub use push_manager::{PushInfo, PushManager};
pub use retry_strategies::RetryStrategy;

// preserve grouping and order
#[rustfmt::skip]
pub use crate::valkey::types::{
    // utility functions
    from_valkey_value,
    from_owned_valkey_value,

    // error kinds
    ErrorKind,

    // conversion traits
    FromValkeyValue,

    // utility types
    InfoDict,
    NumericBehavior,
    Expiry,
    SetExpiry,
    ExistenceCheck,

    // error and result types
    ValkeyError,
    ValkeyResult,
    ValkeyWrite,
    ToValkeyArgs,

    // low level values
    Value,
    PushKind,
    VerbatimFormat,
    ProtocolVersion
};

pub use crate::valkey::{
    cmd::AsyncIter, parser::parse_valkey_value_async, types::ValkeyFuture,
};

mod macros;
mod pipeline;

pub mod aio;
pub mod cluster;
pub mod cluster_slotmap;
pub use cluster_slotmap::SlotMap;
mod cluster_client;

/// For testing purposes
pub mod testing {
    pub use crate::valkey::cluster_client::ClusterParams;
}

pub mod cluster_routing;
pub mod cluster_topology;
pub mod cluster_async;

pub(crate) mod cluster_scan;
pub use cluster_scan::{ClusterScanArgs, ObjectType, ScanStateRC};

mod tls;
pub use crate::valkey::tls::{retrieve_tls_certificates, ClientTlsConfig, TlsCertificates, TlsConnParams};

mod client;
mod cmd;
mod connection;
mod parser;
mod pubsub_synchronizer;
mod push_manager;
mod retry_strategies;
mod types;
