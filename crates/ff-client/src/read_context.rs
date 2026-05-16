use ff_core::contracts::ExecutionContext;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::types::ExecutionId;

use crate::client::{BackendImpl, Client};
use crate::error::Result;

impl Client {
    /// Point-read of the execution's `(input_payload, execution_kind,
    /// tags)` bundle. Mirrors `EngineBackend::read_execution_context`.
    ///
    /// Returns `Ok(None)` when the execution doesn't exist — distinct
    /// from the upstream trait, which raises `EngineError::Validation`
    /// in that case (it assumes the caller is a worker post-claim, so
    /// a missing row is an invariant violation). At the SDK boundary
    /// "not found" is a routine outcome (e.g. checkpoint readers that
    /// don't yet have a write), so we normalise it.
    pub async fn read_context(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionContext>> {
        #[allow(clippy::infallible_destructuring_match)]
        let backend = match &self.backend {
            BackendImpl::Valkey(b) => b,
        };
        match backend.read_execution_context(id).await {
            Ok(ctx) => Ok(Some(ctx)),
            Err(EngineError::Validation { kind: ValidationKind::InvalidInput, .. }) => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}
