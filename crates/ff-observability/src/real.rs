//! OTEL-backed metrics registry.
//!
//! Wires an `opentelemetry_sdk::metrics::SdkMeterProvider` to an
//! `opentelemetry_prometheus` exporter, which reads from a
//! `prometheus::Registry` for text-exposition output.
//!
//! Typed instrument handles are constructed once at startup and cloned
//! into the hot path — OTEL's `Counter<u64>` / `Histogram<f64>` are
//! internally `Arc`-based, so cloning is cheap.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::{Encoder, TextEncoder};

/// Cardinality envelope — see PR-94 plan §3. Each instrument's label
/// set is bounded well below this; the aggregate at steady state stays
/// under 1k series. These consts exist so CI can assert and PR body
/// reviewers can cross-check without reading the whole file.
///
/// Budget (worst case, typical deployment):
///
/// * `ff_http_requests_total` — 20 routes × 4 methods × ~8 statuses = 640
/// * `ff_http_request_duration_seconds` — same key set, histograms
///   generate bucket+sum+count ~= 13 per series but share the same
///   label set. Count the label-set cardinality only (~640 keys).
/// * `ff_scanner_cycle_duration_seconds` / `_total` — 15 scanners ≈ 15
/// * `ff_cancel_backlog_depth` — 1 (no labels)
/// * `ff_claim_from_grant_duration_seconds` — ≤ 16 lanes
/// * `ff_lease_renewal_total` — 2 outcomes (ok, err) = 2
/// * `ff_worker_at_capacity_total` — 1 (no labels)
/// * `ff_budget_hit_total` — per-dimension, typically ≤ 8
/// * `ff_quota_hit_total` — 2 reasons (rate, concurrency) = 2
///
/// Total: ~690 label-sets, 5-10k underlying series with histogram
/// buckets. OK for a per-ff-server /metrics scrape.
const _CARDINALITY_ENVELOPE_MAX: usize = 1_000;

/// Metric names — defined once so tests and docs stay in sync with
/// the call sites.
///
/// OTEL's Prometheus exporter normalizes per Prometheus conventions:
///   * counter instruments get `_total` **appended** at exposition;
///   * instruments with `unit="s"` get `_seconds` **appended**.
/// We therefore DO NOT include `_total` / `_seconds` in the OTEL
/// instrument name — they appear on the wire automatically. This
/// keeps the exported names matching the plan inventory
/// (`ff_http_requests_total`, `ff_http_request_duration_seconds`).
mod name {
    pub const HTTP_REQUESTS: &str = "ff_http_requests";
    pub const HTTP_REQUEST_DURATION: &str = "ff_http_request_duration";
    pub const SCANNER_CYCLE_DURATION: &str = "ff_scanner_cycle_duration";
    pub const SCANNER_CYCLE: &str = "ff_scanner_cycle";
    pub const CANCEL_BACKLOG_DEPTH: &str = "ff_cancel_backlog_depth";
    pub const CLAIM_FROM_GRANT_DURATION: &str = "ff_claim_from_grant_duration";
    pub const LEASE_RENEWAL: &str = "ff_lease_renewal";
    pub const WORKER_AT_CAPACITY: &str = "ff_worker_at_capacity";
    pub const BUDGET_HIT: &str = "ff_budget_hit";
    pub const QUOTA_HIT: &str = "ff_quota_hit";
}

struct Inner {
    registry: prometheus::Registry,
    _provider: SdkMeterProvider,

    http_requests: Counter<u64>,
    http_duration: Histogram<f64>,
    scanner_duration: Histogram<f64>,
    scanner_total: Counter<u64>,
    /// Shared with the observable-gauge callback registered below;
    /// `set_cancel_backlog_depth` stores here, OTEL reads at scrape.
    cancel_backlog_depth: Arc<AtomicU64>,
    claim_duration: Histogram<f64>,
    lease_renewal: Counter<u64>,
    worker_at_capacity: Counter<u64>,
    budget_hit: Counter<u64>,
    quota_hit: Counter<u64>,
}

#[derive(Clone)]
pub struct Metrics(Arc<Inner>);

impl Metrics {
    /// Construct the metrics registry, register the Prometheus exporter
    /// with the OTEL `MeterProvider`, and pre-create every instrument.
    ///
    /// Panics on construction failure — observability init happens once
    /// at startup and a bad registry is a deploy-blocker, not a
    /// runtime-handled condition. (Callers gate construction behind the
    /// `observability` feature, so disabling the feature bypasses this
    /// path entirely.)
    pub fn new() -> Self {
        let registry = prometheus::Registry::new();
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .expect("prometheus exporter builds");
        let provider = SdkMeterProvider::builder().with_reader(exporter).build();
        let meter = provider.meter("ff");

        let http_requests = meter
            .u64_counter(name::HTTP_REQUESTS)
            .with_description("HTTP requests handled, labelled by method/path/status.")
            .build();
        let http_duration = meter
            .f64_histogram(name::HTTP_REQUEST_DURATION)
            .with_description("HTTP request duration in seconds.")
            .with_unit("s")
            .build();
        let scanner_duration = meter
            .f64_histogram(name::SCANNER_CYCLE_DURATION)
            .with_description("Scanner cycle duration in seconds, labelled by scanner.")
            .with_unit("s")
            .build();
        let scanner_total = meter
            .u64_counter(name::SCANNER_CYCLE)
            .with_description("Scanner cycle count, labelled by scanner.")
            .build();
        let claim_duration = meter
            .f64_histogram(name::CLAIM_FROM_GRANT_DURATION)
            .with_description("claim_from_grant latency in seconds, labelled by lane.")
            .with_unit("s")
            .build();
        let lease_renewal = meter
            .u64_counter(name::LEASE_RENEWAL)
            .with_description("Lease renewal attempts, labelled by outcome (ok|err).")
            .build();
        let worker_at_capacity = meter
            .u64_counter(name::WORKER_AT_CAPACITY)
            .with_description("Count of claim attempts rejected with WorkerAtCapacity.")
            .build();
        let budget_hit = meter
            .u64_counter(name::BUDGET_HIT)
            .with_description("Budget hard-breach count, labelled by dimension.")
            .build();
        let quota_hit = meter
            .u64_counter(name::QUOTA_HIT)
            .with_description("Quota admission block count, labelled by reason.")
            .build();

        // Cancel backlog depth — gauge backed by an AtomicU64 so set()
        // from any thread is lock-free. OTEL observable-gauge callback
        // reads the same atomic each scrape.
        let cancel_backlog_depth = Arc::new(AtomicU64::new(0));
        let depth_cb = Arc::clone(&cancel_backlog_depth);
        let _gauge = meter
            .u64_observable_gauge(name::CANCEL_BACKLOG_DEPTH)
            .with_description("Current cancel-reconciler backlog depth.")
            .with_callback(move |o| {
                o.observe(depth_cb.load(Ordering::Relaxed), &[]);
            })
            .build();

        Self(Arc::new(Inner {
            registry,
            _provider: provider,
            http_requests,
            http_duration,
            scanner_duration,
            scanner_total,
            cancel_backlog_depth,
            claim_duration,
            lease_renewal,
            worker_at_capacity,
            budget_hit,
            quota_hit,
        }))
    }

    /// Render the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let metric_families = self.0.registry.gather();
        let encoder = TextEncoder::new();
        let mut buf = Vec::with_capacity(4096);
        // TextEncoder writes valid UTF-8; unwrap on encode is the
        // documented contract (see prometheus::TextEncoder docs).
        encoder.encode(&metric_families, &mut buf).expect("prometheus text encode");
        String::from_utf8(buf).expect("prometheus text is utf-8")
    }

    // ── HTTP ──

    pub fn record_http_request(
        &self,
        method: &str,
        path: &str,
        status: u16,
        elapsed: Duration,
    ) {
        let attrs = [
            KeyValue::new("method", method.to_owned()),
            KeyValue::new("path", path.to_owned()),
            KeyValue::new("status", i64::from(status)),
        ];
        self.0.http_requests.add(1, &attrs);
        self.0.http_duration.record(elapsed.as_secs_f64(), &attrs);
    }

    // ── Scanner ──

    pub fn record_scanner_cycle(&self, scanner: &'static str, elapsed: Duration) {
        let attrs = [KeyValue::new("scanner", scanner)];
        self.0.scanner_total.add(1, &attrs);
        self.0.scanner_duration.record(elapsed.as_secs_f64(), &attrs);
    }

    // ── Cancel backlog ──

    pub fn set_cancel_backlog_depth(&self, depth: u64) {
        self.0.cancel_backlog_depth.store(depth, Ordering::Relaxed);
    }

    // ── Claim ──

    pub fn record_claim_from_grant(&self, lane: &str, elapsed: Duration) {
        let attrs = [KeyValue::new("lane", lane.to_owned())];
        self.0.claim_duration.record(elapsed.as_secs_f64(), &attrs);
    }

    // ── Lease ──

    pub fn inc_lease_renewal(&self, outcome: &'static str) {
        self.0.lease_renewal.add(1, &[KeyValue::new("outcome", outcome)]);
    }

    // ── Worker-at-capacity ──

    pub fn inc_worker_at_capacity(&self) {
        self.0.worker_at_capacity.add(1, &[]);
    }

    // ── Budget / quota ──

    pub fn inc_budget_hit(&self, dimension: &str) {
        self.0
            .budget_hit
            .add(1, &[KeyValue::new("dimension", dimension.to_owned())]);
    }
    pub fn inc_quota_hit(&self, reason: &'static str) {
        self.0.quota_hit.add(1, &[KeyValue::new("reason", reason)]);
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
