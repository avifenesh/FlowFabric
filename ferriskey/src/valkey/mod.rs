//! Async Valkey client library - connection management, RESP protocol,
//! cluster topology, and multiplexed connections.

#![allow(missing_docs)]

// public api
pub use crate::connection::factory::{
    Client, Client as ValkeyClient, FerrisKeyConnectionOptions, IAMTokenProvider,
};
pub use crate::pubsub::push_manager::{PushInfo, PushManager};
pub use crate::pubsub::synchronizer_trait::PubSubSynchronizer;
pub(crate) use crate::retry_strategies;
pub use crate::retry_strategies::RetryStrategy;
pub use crate::valkey::cmd::{Arg, Cmd, cmd, fenced_cmd, pack_command, pipe};
pub use crate::valkey::connection::{
    ConnectionAddr, ConnectionInfo, IntoConnectionInfo, PubSubChannelOrPattern,
    PubSubSubscriptionInfo, PubSubSubscriptionKind, TlsMode, ValkeyConnectionInfo, parse_redis_url,
};
pub use crate::valkey::parser::{Parser, parse_valkey_value};
pub use crate::valkey::pipeline::{Pipeline, PipelineRetryStrategy};

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
    ProtocolVersion,

    // inflight tracking
    InflightRequestTracker,
};

pub use crate::valkey::{cmd::AsyncIter, parser::parse_valkey_value_async, types::ValkeyFuture};

pub(crate) use crate::pipeline;

pub use crate::connection as aio;

// Re-export cluster modules from their new home in crate::cluster
pub use crate::cluster as cluster_async;
pub use crate::cluster::compat as cluster;
pub use crate::cluster::routing as cluster_routing;
pub use crate::cluster::scan::{ClusterScanArgs, ObjectType, ScanStateRC};
pub use crate::cluster::slotmap as cluster_slotmap;
pub use crate::cluster::slotmap::SlotMap;
pub use crate::cluster::topology as cluster_topology;

/// For testing purposes
pub mod testing {
    pub use crate::cluster::client::ClusterParams;
}

pub(crate) use crate::connection::tls;
pub use crate::connection::tls::{
    ClientTlsConfig, TlsCertificates, TlsConnParams, retrieve_tls_certificates,
};

pub(crate) use crate::cmd;
pub(crate) use crate::connection::factory as client;
pub(crate) use crate::connection::info as connection;
pub(crate) use crate::protocol::parser;
pub(crate) use crate::pubsub::synchronizer_trait as pubsub_synchronizer;
pub(crate) use crate::pubsub::push_manager;
pub(crate) use crate::value as types;
