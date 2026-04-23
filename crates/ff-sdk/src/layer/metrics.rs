//! `MetricsLayer` — emit per-call counters/histograms through a
//! pluggable [`MetricsSink`].
//!
//! Deliberately sink-agnostic per the OTEL-via-ferriskey owner lock:
//! FF does not ship vendor-specific exporters. Consumers plug in
//! Prometheus, StatsD, OTEL, or a custom sink.

use std::sync::Arc;
use std::time::Duration;

use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;

use super::EngineBackendLayer;
use super::hooks::{Admit, HookOutcome, HookedBackend, LayerHooks};

/// Classified outcome reported to the sink. `Err` carries a
/// `&'static str` category (allocation-free — matches
/// `BackendErrorKind::as_stable_str`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Err(&'static str),
}

/// Plug for a metrics pipeline. Implement on your own type to route
/// `record_call` events to Prometheus / StatsD / OTEL / vendor-SDK.
pub trait MetricsSink: Send + Sync + 'static {
    fn record_call(&self, method: &'static str, elapsed: Duration, outcome: Outcome);
}

/// No-op sink. Default when `MetricsLayer` is constructed without a
/// sink — keeps the feature compile-time zero-cost for consumers
/// that flip the layer on for plumbing but haven't chosen a sink
/// yet.
pub struct NoopSink;

impl MetricsSink for NoopSink {
    fn record_call(&self, _method: &'static str, _elapsed: Duration, _outcome: Outcome) {}
}

/// See module-level docs.
pub struct MetricsLayer {
    sink: Arc<dyn MetricsSink>,
}

impl MetricsLayer {
    /// Construct with a consumer-supplied sink.
    pub fn new(sink: Arc<dyn MetricsSink>) -> Self {
        Self { sink }
    }

    /// Construct with the no-op sink.
    pub fn noop() -> Self {
        Self {
            sink: Arc::new(NoopSink),
        }
    }
}

impl Default for MetricsLayer {
    fn default() -> Self {
        Self::noop()
    }
}

impl EngineBackendLayer for MetricsLayer {
    fn layer(&self, inner: Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend> {
        Arc::new(HookedBackend::new(
            inner,
            MetricsHooks {
                sink: self.sink.clone(),
            },
        ))
    }
}

pub(crate) struct MetricsHooks {
    sink: Arc<dyn MetricsSink>,
}

impl LayerHooks for MetricsHooks {
    fn before(&self, _method_name: &'static str) -> Admit {
        Admit::Proceed
    }

    fn after(&self, method_name: &'static str, elapsed: Duration, outcome: HookOutcome<'_>) {
        let outcome_owned = match outcome {
            HookOutcome::Ok => Outcome::Ok,
            HookOutcome::Err(e) => Outcome::Err(engine_error_category(e)),
        };
        self.sink.record_call(method_name, elapsed, outcome_owned);
    }
}

/// Map an `EngineError` to a stable category string. Allocation-free;
/// every match arm returns a `&'static str`. Mirrors
/// `BackendErrorKind::as_stable_str` for transport, with additional
/// top-level variants for the full engine error taxonomy.
fn engine_error_category(err: &EngineError) -> &'static str {
    match err {
        EngineError::NotFound { .. } => "not_found",
        EngineError::Validation { .. } => "validation",
        EngineError::Contention { .. } => "contention",
        EngineError::Conflict { .. } => "conflict",
        EngineError::State { .. } => "state",
        EngineError::Bug(_) => "bug",
        EngineError::Transport { .. } => "transport",
        EngineError::Unavailable { .. } => "unavailable",
        EngineError::Contextual { source, .. } => engine_error_category(source),
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::{EngineBackendLayerExt, test_support::PassthroughBackend};
    use std::sync::Mutex;

    fn test_exec_id() -> ff_core::types::ExecutionId {
        ff_core::types::ExecutionId::parse("{fp:0}:00000000-0000-0000-0000-000000000000").unwrap()
    }

    #[derive(Default)]
    struct CaptureSink {
        records: Mutex<Vec<(&'static str, Outcome)>>,
    }

    impl MetricsSink for CaptureSink {
        fn record_call(&self, method: &'static str, _elapsed: Duration, outcome: Outcome) {
            self.records.lock().unwrap().push((method, outcome));
        }
    }

    #[tokio::test]
    async fn records_ok_and_err() {
        let sink = Arc::new(CaptureSink::default());
        let raw = Arc::new(PassthroughBackend::default());
        let inner: Arc<dyn EngineBackend> = raw.clone();
        let layered = inner.layer(MetricsLayer::new(sink.clone()));

        let id = test_exec_id();
        let _ok = layered.describe_execution(&id).await;
        raw.set_fail_transport(true);
        let _err = layered.describe_execution(&id).await;

        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], ("describe_execution", Outcome::Ok));
        assert_eq!(
            records[1],
            ("describe_execution", Outcome::Err("transport"))
        );
    }
}
