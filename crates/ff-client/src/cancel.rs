use ff_core::contracts::CancelExecutionArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{CancelSource, ExecutionId, TimestampMs};

pub use ff_core::contracts::CancelExecutionResult as CancelResult;

use crate::client::{BackendImpl, Client};
use crate::error::Result;

impl Client {
    /// Cancel an execution. Uses `CancelSource::OperatorOverride`,
    /// which bypasses the lease-fence check — appropriate for
    /// external clients that aren't holding the current attempt's
    /// lease. The lease holder itself (a worker) should call the
    /// underlying backend op directly with its fence triple.
    ///
    /// Returns [`CancelResult::Cancelled`] with the new public state
    /// (terminal: `Cancelled`).
    pub async fn cancel(
        &self,
        id: &ExecutionId,
        reason: impl Into<String>,
    ) -> Result<CancelResult> {
        #[allow(clippy::infallible_destructuring_match)]
        let backend = match &self.backend {
            BackendImpl::Valkey(b) => b,
        };
        let args = CancelExecutionArgs {
            execution_id: id.clone(),
            reason: reason.into(),
            source: CancelSource::OperatorOverride,
            lease_id: None,
            lease_epoch: None,
            attempt_id: None,
            now: TimestampMs::now(),
        };
        backend.cancel_execution(args).await.map_err(Into::into)
    }
}
