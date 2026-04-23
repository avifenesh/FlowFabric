//! Issue #154 — observability wiring on `EngineBackend` trait methods.
//!
//! Asserts:
//!   1. `ValkeyBackend::describe_execution` emits a `ff.describe_execution`
//!      tracing span at the trait boundary.
//!   2. `ValkeyBackend` accepts an `Arc<ff_observability::Metrics>` via
//!      the `with_metrics` setter and the handle survives on the
//!      backend.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_observability \
//!       -- --test-threads=1

use std::sync::{Arc, Mutex};

use ff_backend_valkey::ValkeyBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{ExecutionId, LaneId};
use ff_test::fixtures::TestCluster;
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Capture span names opened while the subscriber is installed.
#[derive(Clone, Default)]
struct CaptureSpans(Arc<Mutex<Vec<String>>>);

impl<S> Layer<S> for CaptureSpans
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        self.0
            .lock()
            .unwrap()
            .push(attrs.metadata().name().to_string());
    }
}

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

#[tokio::test(flavor = "current_thread")]
async fn describe_execution_emits_tracing_span() {
    let tc = TestCluster::connect().await;
    let backend: Arc<dyn EngineBackend> =
        ValkeyBackend::from_client_and_partitions(tc.client().clone(), test_config());

    let capture = CaptureSpans::default();
    // `set_default` is thread-local; pair it with
    // `flavor = "current_thread"` so the async body stays on the
    // thread that installed the subscriber. Prior `block_on` form
    // starved the runtime; prior `multi_thread` form risked polling
    // on a worker without the subscriber installed.
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));

    // Call an instrumented trait method against a non-existent
    // execution id — the op returns `Ok(None)` but the span MUST
    // still emit because `#[tracing::instrument]` creates the span
    // on entry, before the HGETALL round trip.
    let lane = LaneId::new("obs-test-lane");
    let eid = ExecutionId::solo(&lane, &test_config());
    let _result = backend.describe_execution(&eid).await;

    let names = capture.0.lock().unwrap().clone();
    assert!(
        names.iter().any(|n| n == "ff.describe_execution"),
        "expected `ff.describe_execution` span, captured: {names:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn with_metrics_setter_attaches_handle() {
    let tc = TestCluster::connect().await;
    let mut backend =
        ValkeyBackend::from_client_and_partitions(tc.client().clone(), test_config());
    let metrics = Arc::new(ff_observability::Metrics::new());
    // `Arc::get_mut` path — setter succeeds iff no other `Arc`
    // clones exist. `from_client_and_partitions` returns a
    // freshly-minted `Arc`, so this should return true.
    let attached = ValkeyBackend::with_metrics(&mut backend, metrics.clone());
    assert!(attached, "with_metrics should attach on a fresh Arc");

    // A second call (still no other clones outstanding) still
    // succeeds; the field is overwritten, not appended.
    let attached_again = ValkeyBackend::with_metrics(&mut backend, metrics);
    assert!(attached_again);
}
