use std::collections::HashMap;

use ff_core::contracts::CreateExecutionArgs;
use ff_core::partition::execution_partition;
use ff_core::policy::ExecutionPolicy;
use ff_core::types::{ExecutionId, LaneId, TimestampMs};

pub use ff_core::contracts::CreateExecutionResult as SubmitResult;

use crate::client::{BackendImpl, Client};
use crate::error::{ClientError, Result};

/// Inputs to [`Client::submit`].
///
/// Builders fill in only what they care about; the remaining fields
/// take backend-aligned defaults. Add fields additively — the struct
/// is `#[non_exhaustive]` so external struct-literal construction is
/// blocked.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SubmitRequest {
    /// Caller-defined workflow / task type. Maps to `execution_kind`
    /// in the underlying contract.
    pub workflow_kind: String,
    /// Workflow input — opaque bytes from the backend's perspective.
    pub payload: Vec<u8>,
    /// Submission lane. Defaults to `"default"`.
    pub lane: LaneId,
    /// Optional payload encoding hint (e.g. `"json"`).
    pub payload_encoding: Option<String>,
    /// Scheduling priority (signed; higher runs sooner). Defaults to 0.
    pub priority: i32,
    /// Free-form caller identity, recorded on the execution row.
    /// Defaults to empty.
    pub creator_identity: String,
    /// Idempotency key. When set, repeat submissions within the
    /// backend's `dedup_ttl_ms` window return `SubmitResult::Duplicate`
    /// instead of creating a second execution.
    pub idempotency_key: Option<String>,
    /// Arbitrary key/value tags stored alongside the execution.
    pub tags: HashMap<String, String>,
    /// Retry / timeout / suspension policy. When `None`, backend
    /// defaults apply.
    pub policy: Option<ExecutionPolicy>,
    /// If set and in the future, the execution starts delayed.
    pub delay_until: Option<TimestampMs>,
    /// Absolute deadline timestamp (ms since epoch). Execution
    /// expires if not complete by this time.
    pub execution_deadline_at: Option<TimestampMs>,
}

impl SubmitRequest {
    /// Minimum-information constructor: just a workflow kind + payload.
    /// Lane defaults to `"default"`; every other field takes its
    /// type-level default.
    pub fn new(workflow_kind: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            workflow_kind: workflow_kind.into(),
            payload,
            lane: LaneId::new("default"),
            payload_encoding: None,
            priority: 0,
            creator_identity: String::new(),
            idempotency_key: None,
            tags: HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
        }
    }

    pub fn lane(mut self, lane: LaneId) -> Self {
        self.lane = lane;
        self
    }

    pub fn payload_encoding(mut self, encoding: impl Into<String>) -> Self {
        self.payload_encoding = Some(encoding.into());
        self
    }

    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn creator_identity(mut self, identity: impl Into<String>) -> Self {
        self.creator_identity = identity.into();
        self
    }

    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    pub fn tags(mut self, tags: HashMap<String, String>) -> Self {
        self.tags = tags;
        self
    }

    pub fn policy(mut self, policy: ExecutionPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn delay_until(mut self, ts: TimestampMs) -> Self {
        self.delay_until = Some(ts);
        self
    }

    pub fn execution_deadline_at(mut self, ts: TimestampMs) -> Self {
        self.execution_deadline_at = Some(ts);
        self
    }
}

impl Client {
    /// Submit a new execution. Mints a partition-correct
    /// [`ExecutionId`] using the backend's loaded partition config,
    /// then calls the backend's `create_execution` op.
    ///
    /// Returns [`SubmitResult::Created`] on first submission;
    /// [`SubmitResult::Duplicate`] when the request's `idempotency_key`
    /// matches a still-live previous submission (default dedup TTL is
    /// 24h backend-side).
    pub async fn submit(&self, req: SubmitRequest) -> Result<SubmitResult> {
        // `match` over a single-variant enum is intentional: more
        // backend variants land here additively, and `let-else`
        // wouldn't extend cleanly.
        #[allow(clippy::infallible_destructuring_match)]
        let backend = match &self.backend {
            BackendImpl::Valkey(b) => b,
        };
        let partition_config = backend.partition_config();
        let execution_id = ExecutionId::solo(&req.lane, partition_config);
        let partition_id = execution_partition(&execution_id, partition_config).index;
        let now = TimestampMs::now();

        let args = CreateExecutionArgs {
            execution_id,
            namespace: self.namespace().clone(),
            lane_id: req.lane,
            execution_kind: req.workflow_kind,
            input_payload: req.payload,
            payload_encoding: req.payload_encoding,
            priority: req.priority,
            creator_identity: req.creator_identity,
            idempotency_key: req.idempotency_key,
            tags: req.tags,
            policy: req.policy,
            delay_until: req.delay_until,
            execution_deadline_at: req.execution_deadline_at,
            partition_id,
            now,
        };

        use ff_core::engine_backend::EngineBackend;
        backend
            .create_execution(args)
            .await
            .map_err(ClientError::from)
    }
}
