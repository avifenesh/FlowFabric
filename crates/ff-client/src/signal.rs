use ff_core::contracts::DeliverSignalArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{ExecutionId, SignalId, TimestampMs, WaitpointId, WaitpointToken};

pub use ff_core::contracts::DeliverSignalResult as SignalResult;

use crate::client::{BackendImpl, Client};
use crate::error::Result;

/// Inputs to [`Client::deliver_signal`].
///
/// `signal_id` defaults to a freshly-minted UUID; `now` defaults to
/// the current wall clock. Pass either explicitly when you need
/// caller-controlled identity or timestamping.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SignalRequest {
    pub execution_id: ExecutionId,
    pub waitpoint_id: WaitpointId,
    pub waitpoint_token: WaitpointToken,
    pub signal_name: String,
    pub signal_category: String,
    pub source_type: String,
    pub source_identity: String,
    pub target_scope: String,
    pub signal_id: Option<SignalId>,
    pub payload: Option<Vec<u8>>,
    pub payload_encoding: Option<String>,
    pub correlation_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub created_at: Option<TimestampMs>,
    pub dedup_ttl_ms: Option<u64>,
    pub resume_delay_ms: Option<u64>,
    pub max_signals_per_execution: Option<u64>,
    pub signal_maxlen: Option<u64>,
    pub now: Option<TimestampMs>,
}

impl SignalRequest {
    /// Minimal-information constructor. Everything optional defaults.
    pub fn new(
        execution_id: ExecutionId,
        waitpoint_id: WaitpointId,
        waitpoint_token: WaitpointToken,
        signal_name: impl Into<String>,
    ) -> Self {
        Self {
            execution_id,
            waitpoint_id,
            waitpoint_token,
            signal_name: signal_name.into(),
            signal_category: "default".to_owned(),
            source_type: "external".to_owned(),
            source_identity: String::new(),
            target_scope: "execution".to_owned(),
            signal_id: None,
            payload: None,
            payload_encoding: None,
            correlation_id: None,
            idempotency_key: None,
            created_at: None,
            dedup_ttl_ms: None,
            resume_delay_ms: None,
            max_signals_per_execution: None,
            signal_maxlen: None,
            now: None,
        }
    }

    pub fn signal_category(mut self, c: impl Into<String>) -> Self {
        self.signal_category = c.into();
        self
    }

    pub fn source(mut self, source_type: impl Into<String>, source_identity: impl Into<String>) -> Self {
        self.source_type = source_type.into();
        self.source_identity = source_identity.into();
        self
    }

    pub fn target_scope(mut self, s: impl Into<String>) -> Self {
        self.target_scope = s.into();
        self
    }

    pub fn payload(mut self, bytes: Vec<u8>, encoding: Option<String>) -> Self {
        self.payload = Some(bytes);
        self.payload_encoding = encoding;
        self
    }

    pub fn correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    pub fn idempotency_key(mut self, k: impl Into<String>) -> Self {
        self.idempotency_key = Some(k.into());
        self
    }
}

impl Client {
    /// Deliver a signal to a suspended execution's waitpoint.
    ///
    /// Mints a [`SignalId`] when [`SignalRequest::signal_id`] is
    /// `None` and stamps `now` when not supplied; everything else
    /// is forwarded to `backend.deliver_signal`.
    ///
    /// Returns [`SignalResult::Accepted`] with the assigned id +
    /// effect string, or [`SignalResult::Duplicate`] when an
    /// `idempotency_key` matches a still-live previous delivery.
    /// Bad tokens, missing waitpoints, and similar invariant
    /// failures surface as [`crate::ClientError::Engine`] with the
    /// upstream typed shape.
    pub async fn deliver_signal(&self, req: SignalRequest) -> Result<SignalResult> {
        #[allow(clippy::infallible_destructuring_match)]
        let backend = match &self.backend {
            BackendImpl::Valkey(b) => b,
        };
        let args = DeliverSignalArgs {
            execution_id: req.execution_id,
            waitpoint_id: req.waitpoint_id,
            signal_id: req.signal_id.unwrap_or_else(SignalId::new),
            signal_name: req.signal_name,
            signal_category: req.signal_category,
            source_type: req.source_type,
            source_identity: req.source_identity,
            payload: req.payload,
            payload_encoding: req.payload_encoding,
            correlation_id: req.correlation_id,
            idempotency_key: req.idempotency_key,
            target_scope: req.target_scope,
            created_at: req.created_at,
            dedup_ttl_ms: req.dedup_ttl_ms,
            resume_delay_ms: req.resume_delay_ms,
            max_signals_per_execution: req.max_signals_per_execution,
            signal_maxlen: req.signal_maxlen,
            waitpoint_token: req.waitpoint_token,
            now: req.now.unwrap_or_else(TimestampMs::now),
        };
        backend.deliver_signal(args).await.map_err(Into::into)
    }
}
