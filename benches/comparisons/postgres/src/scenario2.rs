//! Postgres comparison — scenario 2 (suspend / signal / resume).
//!
//! SKIPPED in the v0.7 phase-0 pass. Rationale:
//!
//! The Valkey-side bench drives a full suspend -> deliver_signal ->
//! claim_resumed_execution roundtrip via the server-side waitpoint +
//! HMAC token path. The Postgres backend's trio is implemented
//! in-crate (crates/ff-backend-postgres/src/suspend_ops.rs) but the
//! ingress contract (SuspendArgs / WaitpointBinding / ResumeCondition /
//! ResumePolicy / SuspensionRequester / TimeoutBehavior +
//! idempotency-key plumbing) is RFC-014 Pattern-3-shaped and intended
//! to be driven by ff-server. Mirroring it at the direct-backend layer
//! here would duplicate ~150 lines of plumbing already covered by the
//! suspend_signal.rs integration test — without yielding a number a
//! Valkey-vs-Postgres comparison can honestly cite.
//!
//! Bench status: documented in COMPARISON.md + the v0.7 phase 0 RFC.
//! The Postgres suspend/signal surface has parity coverage in
//! integration tests already; the perf cut belongs in v0.7 Phase 1
//! once the server-side ingress plumbing stops shifting.

fn main() {
    eprintln!("[pg-s2] skipped — see module docstring + benches/results/COMPARISON.md");
    std::process::exit(0);
}
