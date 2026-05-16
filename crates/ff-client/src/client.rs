use std::sync::Arc;

use ff_core::backend::{BackendConfig, BackendTag};
use ff_core::types::Namespace;

use crate::builder::ClientBuilder;

/// Internal backend dispatch. Variants are added as backend impls land.
/// Kept private — consumers see only the public [`Client`] surface.
pub(crate) enum BackendImpl {
    // Held but currently unread — the first FF operation that touches
    // the wire will consume it. `build()` still establishes the
    // connection so a failed transport surfaces at construction.
    Valkey(#[allow(dead_code)] Arc<ferriskey::Client>),
    // Postgres(...) — future
    // Sqlite(...) — future
}

/// FlowFabric client.
///
/// Construct via [`Client::builder`]. The set of operations exposed here
/// grows incrementally; this foundation crate ships only the connection
/// machinery. Readiness is signalled by [`ClientBuilder::build`] succeeding —
/// it opens the backend connection, so a failed transport surfaces at
/// construction without committing the public API to a Valkey-flavoured
/// verb like `ping`.
pub struct Client {
    namespace: Namespace,
    backend: BackendImpl,
    config: BackendConfig,
}

impl Client {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    pub(crate) fn new(namespace: Namespace, backend: BackendImpl, config: BackendConfig) -> Self {
        Self {
            namespace,
            backend,
            config,
        }
    }

    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    pub fn backend_tag(&self) -> BackendTag {
        match self.backend {
            BackendImpl::Valkey(_) => BackendTag::Valkey,
        }
    }

    pub fn config(&self) -> &BackendConfig {
        &self.config
    }
}
