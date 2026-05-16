use ff_backend_valkey::{ValkeyBackend, load_partition_config};
use ff_core::backend::{BackendConfig, BackendConnection, ValkeyConnection};
use ff_core::partition::PartitionConfig;
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
                // Ensure the FlowFabric Lua library is registered on
                // the target Valkey. Idempotent: no-op when the
                // current version is already loaded. Without this,
                // every FCALL-backed op (create_execution, claim, …)
                // fails with "Function not found" on a Valkey that
                // ff-server hasn't bootstrapped yet.
                ff_script::loader::ensure_library(&fk)
                    .await
                    .map_err(ClientError::script_load)?;
                // Mirror ValkeyBackend::connect's warn-and-default
                // fallback when the deployment hasn't published
                // ff:config:partitions yet (e.g. local Valkey dev
                // node, SDK-only tests). Surfacing the missing-key
                // case as Ok-with-default here matches the backend's
                // own behaviour and keeps client construction usable
                // against bare Valkey instances.
                let partition_config = load_partition_config(&fk).await.unwrap_or_else(|e| {
                    tracing::warn!(
                        error = %e,
                        "ff:config:partitions not found, using PartitionConfig::default()"
                    );
                    PartitionConfig::default()
                });
                let backend = ValkeyBackend::from_client_and_partitions(fk, partition_config);
                BackendImpl::Valkey(backend)
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
    // ferriskey's string-URL parser only accepts `redis` / `rediss`
    // schemes (the lower-level `url::Url::into_connection_info` impl
    // also accepts `valkey` / `valkeys`, but the string entry point
    // filters them out first). Use the redis schemes here so
    // `ferriskey::Client::connect(&url)` actually parses.
    let scheme = if v.tls { "rediss" } else { "redis" };
    format!("{scheme}://{}:{}", v.host, v.port)
}
