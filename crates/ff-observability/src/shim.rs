//! No-op shim used when the `enabled` feature is OFF.
//!
//! Every method here is a zero-sized no-op so call sites compile
//! identically under both features. The `render` method returns an
//! empty string so a caller that forgot to gate a route mount cannot
//! emit misleading /metrics output (it will serve empty text, not
//! fabricated data).

use std::time::Duration;

/// Zero-sized no-op registry. Identical public shape to the real
/// implementation (see `real.rs`) so call sites don't know which
/// backend they're hitting.
#[derive(Clone, Default)]
pub struct Metrics;

impl Metrics {
    /// Construct a disabled (no-op) metrics registry. The `enabled`-OFF
    /// configuration has no fallible initialization, so this returns
    /// `Self` directly (not `Result<Self, _>`) to keep call sites
    /// feature-symmetric.
    pub fn new() -> Self {
        Self
    }

    /// Render the Prometheus text exposition. Empty when disabled.
    pub fn render(&self) -> String {
        String::new()
    }

    // ── HTTP ──

    pub fn record_http_request(
        &self,
        _method: &str,
        _path: &str,
        _status: u16,
        _elapsed: Duration,
    ) {
    }

    // ── Scanner ──

    pub fn record_scanner_cycle(&self, _scanner: &'static str, _elapsed: Duration) {}

    // ── Cancel backlog (gauge) ──

    pub fn set_cancel_backlog_depth(&self, _depth: u64) {}

    // ── Claim ──

    pub fn record_claim_from_grant(&self, _lane: &str, _elapsed: Duration) {}

    // ── Lease renewal ──

    pub fn inc_lease_renewal(&self, _outcome: &'static str) {}

    // ── Attempt terminal outcome ──

    pub fn inc_attempt_outcome(&self, _lane: &str, _outcome: super::AttemptOutcome) {}

    // ── Worker-at-capacity ──

    pub fn inc_worker_at_capacity(&self) {}

    // ── Budget / quota admission ──

    pub fn inc_budget_hit(&self, _dimension: &str) {}
    pub fn inc_quota_hit(&self, _reason: &'static str) {}

    // ── RFC-016 Stage A: edge-group policy ──

    pub fn inc_edge_group_policy(&self, _policy: &'static str) {}
}
