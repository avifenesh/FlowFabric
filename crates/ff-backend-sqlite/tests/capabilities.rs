//! RFC-023 §4.3 / §6.3 / §9 capability-matrix snapshot — v0.12
//! Phase 4c (Wave 10 live).
//!
//! Pre-v0.12 (Phase 1a) asserted every `Supports::*` flag `false` —
//! the scaffolding shipped but no trait body did. v0.12 Phase 4c
//! flips every flag whose backing method landed across Phase 1-3
//! (hot path, flow, waitpoints, reads, Wave 9 operator/reads/budget/
//! quota, RFC-019 subscriptions, streaming, admin HMAC). Two flags
//! stay `false`:
//!
//! - `claim_for_worker` — RFC-023 §5 permanent non-goal.
//! - `subscribe_instance_tags` — #311 deferred on all backends.
//!
//! Mirrors `crates/ff-backend-postgres/tests/capabilities.rs` for
//! consumer copy-paste parity per cairn #277.

use ff_backend_sqlite::SqliteBackend;
use ff_core::engine_backend::EngineBackend;
use serial_test::serial;

#[tokio::test]
#[serial(ff_dev_mode)]
async fn capabilities_identity_and_supports() {
    // SAFETY: test-only env mutation; `serial_test` serialises across
    // the `ff_dev_mode` key so no other test races this.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let backend = SqliteBackend::new(":memory:")
        .await
        .expect("sqlite init under FF_DEV_MODE=1");

    let caps = backend.capabilities();
    assert_eq!(caps.identity.family, "sqlite");
    // Phase 4 rfc017_stage label (flipped from "Phase-1a" at v0.12).
    assert_eq!(caps.identity.rfc017_stage, "Phase-4");

    // ── Flow bulk cancel (Phase 2b.1 Group A) ──
    assert!(caps.supports.cancel_flow_wait_timeout);
    assert!(caps.supports.cancel_flow_wait_indefinite);

    // ── Admin seed + rotate HMAC (Phase 2b.1) ──
    assert!(caps.supports.rotate_waitpoint_hmac_secret_all);
    assert!(caps.supports.seed_waitpoint_hmac_secret);

    // ── RFC-019 subscriptions (Phase 3.1) ──
    assert!(caps.supports.subscribe_lease_history);
    assert!(caps.supports.subscribe_completion);
    assert!(caps.supports.subscribe_signal_delivery);

    // ── Streaming (Phase 2b.2.2) ──
    assert!(caps.supports.stream_durable_summary);
    assert!(caps.supports.stream_best_effort_live);

    // ── Wave 9 (Phase 3.2-3.5) ──
    assert!(caps.supports.cancel_execution);
    assert!(caps.supports.change_priority);
    assert!(caps.supports.replay_execution);
    assert!(caps.supports.revoke_lease);
    assert!(caps.supports.read_execution_state);
    assert!(caps.supports.read_execution_info);
    assert!(caps.supports.get_execution_result);
    assert!(caps.supports.budget_admin);
    assert!(caps.supports.quota_admin);
    assert!(caps.supports.list_pending_waitpoints);
    assert!(caps.supports.cancel_flow_header);
    assert!(caps.supports.ack_cancel_member);

    // SqliteBackend::prepare() returns NoOp (migrations run inside
    // `SqliteBackend::new`, matching PG's posture). NoOp is a callable
    // + correct outcome, not Unavailable.
    assert!(caps.supports.prepare);

    // claim_for_worker: Unavailable on SQLite by RFC-023 §5 non-goal
    // (scheduler routing is not in scope for the dev-only backend).
    assert!(!caps.supports.claim_for_worker);

    // subscribe_instance_tags: Deferred on all backends per RFC-020
    // §3.2 / issue #311; served by list_executions +
    // ScannerFilter::with_instance_tag.
    assert!(!caps.supports.subscribe_instance_tags);

    assert_eq!(backend.backend_label(), "sqlite");
}
