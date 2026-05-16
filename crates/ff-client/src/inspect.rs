use ff_core::contracts::ExecutionSnapshot;
use ff_core::engine_backend::EngineBackend;
use ff_core::types::ExecutionId;

pub use ff_core::contracts::{AttemptSummary, LeaseSummary};
pub use ff_core::state::PublicState;

use crate::client::{BackendImpl, Client};
use crate::error::Result;

impl Client {
    /// Snapshot an execution by id. `Ok(None)` ⇒ the backend has no
    /// record of this id (never submitted, or already pruned by
    /// retention policy).
    ///
    /// The snapshot is a read-model projection: identifiers, state,
    /// blocking reason, attempt / lease summaries, timestamps, tags.
    /// It does NOT contain the input payload or the completion result
    /// — those are fetched separately via [`Client::result`] when
    /// `public_state` is [`PublicState::Completed`].
    pub async fn inspect(&self, id: &ExecutionId) -> Result<Option<ExecutionSnapshot>> {
        #[allow(clippy::infallible_destructuring_match)]
        let backend = match &self.backend {
            BackendImpl::Valkey(b) => b,
        };
        backend.describe_execution(id).await.map_err(Into::into)
    }

    /// Fetch the stored result payload of a completed execution.
    /// `Ok(None)` ⇒ execution is missing, not yet complete, or its
    /// payload was trimmed by retention policy. Callers should check
    /// [`Client::inspect`] first if they need to distinguish those
    /// cases.
    pub async fn result(&self, id: &ExecutionId) -> Result<Option<Vec<u8>>> {
        #[allow(clippy::infallible_destructuring_match)]
        let backend = match &self.backend {
            BackendImpl::Valkey(b) => b,
        };
        backend.get_execution_result(id).await.map_err(Into::into)
    }
}
