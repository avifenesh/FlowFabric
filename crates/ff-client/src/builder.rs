use std::sync::Arc;

use ff_core::backend::{BackendConfig, BackendConnection, ValkeyConnection};
use ff_core::types::Namespace;

use crate::client::{BackendImpl, Client};
use crate::error::{ClientError, Result};

#[derive(Default)]
pub struct ClientBuilder {
    namespace: Option<Namespace>,
    backend: Option<BackendConfig>,
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn namespace(mut self, namespace: Namespace) -> Self {
        self.namespace = Some(namespace);
        self
    }

    pub fn backend(mut self, backend: BackendConfig) -> Self {
        self.backend = Some(backend);
        self
    }

    pub async fn build(self) -> Result<Client> {
        let namespace = self
            .namespace
            .ok_or_else(|| ClientError::missing_field("namespace"))?;
        let backend = self
            .backend
            .ok_or_else(|| ClientError::missing_field("backend"))?;

        let impl_ = match &backend.connection {
            BackendConnection::Valkey(v) => {
                let url = valkey_url(v);
                let fk = ferriskey::Client::connect(&url).await?;
                BackendImpl::Valkey(Arc::new(fk))
            }
            BackendConnection::Postgres(_) => {
                return Err(ClientError::backend_not_yet_supported("postgres"));
            }
            // Future BackendConnection variants (e.g. Sqlite) will land here.
            // ff-core marks BackendConnection #[non_exhaustive], so this
            // wildcard keeps ff-client building when new variants are added
            // without an impl in this crate.
            _ => return Err(ClientError::backend_not_yet_supported("unknown")),
        };

        Ok(Client::new(namespace, impl_, backend))
    }
}

fn valkey_url(v: &ValkeyConnection) -> String {
    let scheme = if v.tls { "valkeys" } else { "valkey" };
    format!("{scheme}://{}:{}", v.host, v.port)
}
