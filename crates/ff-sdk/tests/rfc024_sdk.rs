//! RFC-024 PR-G — SDK consumer surface tests.
//!
//! Covers:
//! * `FlowFabricAdminClient::issue_reclaim_grant` request / response
//!   wire-shape round-trip.
//! * `FlowFabricWorker::claim_from_reclaim_grant` backend-agnostic
//!   dispatch compiles under the default feature set.
//! * The same method is addressable under
//!   `default-features = false, features = ["sqlite"]` — see
//!   `sqlite_only_compile_surface_tests` inside `worker.rs` for the
//!   sqlite-only compile anchor that the PR-C surface landed.
//!
//! Integration tests exercising a full claim → lease-expire →
//! issue_reclaim_grant → claim_from_reclaim_grant → complete loop
//! are `#[ignore]`d; they require a live backend and are driven
//! from the smoke scripts (`scripts/smoke-sqlite.sh`,
//! `scripts/smoke-v0.7.sh`) per RFC-024 §9.

#![cfg(feature = "valkey-default")]

use ff_sdk::admin::{IssueReclaimGrantRequest, IssueReclaimGrantResponse};

#[test]
fn issue_reclaim_grant_request_serializes_all_fields() {
    let req = IssueReclaimGrantRequest {
        worker_id: "w1".into(),
        worker_instance_id: "w1-i1".into(),
        lane_id: "main".into(),
        capability_hash: Some("cap-hash-abc".into()),
        grant_ttl_ms: 30_000,
        route_snapshot_json: Some(r#"{"route":"x"}"#.into()),
        admission_summary: Some("admitted".into()),
        worker_capabilities: vec!["cpu".into(), "gpu".into()],
    };
    let json = serde_json::to_value(&req).expect("serialize");
    assert_eq!(json["worker_id"], "w1");
    assert_eq!(json["worker_instance_id"], "w1-i1");
    assert_eq!(json["lane_id"], "main");
    assert_eq!(json["capability_hash"], "cap-hash-abc");
    assert_eq!(json["grant_ttl_ms"], 30_000);
    assert_eq!(json["worker_capabilities"][0], "cpu");
}

#[test]
fn issue_reclaim_grant_request_skips_none_optional_fields() {
    let req = IssueReclaimGrantRequest {
        worker_id: "w1".into(),
        worker_instance_id: "w1-i1".into(),
        lane_id: "main".into(),
        capability_hash: None,
        grant_ttl_ms: 30_000,
        route_snapshot_json: None,
        admission_summary: None,
        worker_capabilities: vec![],
    };
    let json = serde_json::to_value(&req).expect("serialize");
    assert!(json.get("capability_hash").is_none());
    assert!(json.get("route_snapshot_json").is_none());
    assert!(json.get("admission_summary").is_none());
}

#[test]
fn issue_reclaim_grant_response_granted_deserializes() {
    let wire = serde_json::json!({
        "status": "granted",
        "execution_id": "{fp:7}:11111111-1111-4111-8111-111111111111",
        "partition_key": "{fp:7}",
        "grant_key": "reclaim:grant:abc",
        "expires_at_ms": 1_700_000_000_000u64,
        "lane_id": "main",
    });
    let resp: IssueReclaimGrantResponse =
        serde_json::from_value(wire).expect("deserialize granted");
    match resp {
        IssueReclaimGrantResponse::Granted {
            grant_key, lane_id, ..
        } => {
            assert_eq!(grant_key, "reclaim:grant:abc");
            assert_eq!(lane_id, "main");
        }
        other => panic!("expected Granted, got {other:?}"),
    }
}

#[test]
fn issue_reclaim_grant_response_not_reclaimable_deserializes() {
    let wire = serde_json::json!({
        "status": "not_reclaimable",
        "execution_id": "{fp:7}:11111111-1111-4111-8111-111111111111",
        "detail": "capability_mismatch",
    });
    let resp: IssueReclaimGrantResponse = serde_json::from_value(wire).expect("deserialize");
    match resp {
        IssueReclaimGrantResponse::NotReclaimable { detail, .. } => {
            assert_eq!(detail, "capability_mismatch");
        }
        other => panic!("expected NotReclaimable, got {other:?}"),
    }
}

#[test]
fn issue_reclaim_grant_response_reclaim_cap_exceeded_deserializes() {
    let wire = serde_json::json!({
        "status": "reclaim_cap_exceeded",
        "execution_id": "{fp:7}:11111111-1111-4111-8111-111111111111",
        "reclaim_count": 1000,
    });
    let resp: IssueReclaimGrantResponse = serde_json::from_value(wire).expect("deserialize");
    match resp {
        IssueReclaimGrantResponse::ReclaimCapExceeded { reclaim_count, .. } => {
            assert_eq!(reclaim_count, 1000);
        }
        other => panic!("expected ReclaimCapExceeded, got {other:?}"),
    }
}

#[test]
fn issue_reclaim_grant_response_into_grant_rejects_non_granted_variants() {
    let not_reclaimable = IssueReclaimGrantResponse::NotReclaimable {
        execution_id: "{fp:7}:11111111-1111-4111-8111-111111111111"
            .into(),
        detail: "capability_mismatch".into(),
    };
    assert!(not_reclaimable.into_grant().is_err());

    let cap_exceeded = IssueReclaimGrantResponse::ReclaimCapExceeded {
        execution_id: "{fp:7}:11111111-1111-4111-8111-111111111111"
            .into(),
        reclaim_count: 1000,
    };
    assert!(cap_exceeded.into_grant().is_err());
}

#[test]
fn worker_claim_from_reclaim_grant_is_backend_agnostic_at_type_level() {
    // Compile-time assertion that `claim_from_reclaim_grant` is
    // addressable on `FlowFabricWorker` under the default feature
    // set AND returns the advertised `ReclaimExecutionOutcome` type.
    // Parallels the sqlite-only compile anchor inside worker.rs —
    // the type-pin here ensures the public signature matches
    // RFC-024 §3.4 and catches accidental cfg-gating regressions.
    use ff_core::contracts::{ReclaimExecutionArgs, ReclaimExecutionOutcome, ReclaimGrant};
    use ff_sdk::{FlowFabricWorker, SdkError};

    // Type-erase the return via `Box<dyn Future>` so the assignment
    // names only the Output type (no `impl Future` to spell). Any
    // cfg-gate regression on the method fails compilation of this
    // line; any signature drift fails the `Future::Output` match.
    async fn _call(
        w: &FlowFabricWorker,
        g: ReclaimGrant,
        a: ReclaimExecutionArgs,
    ) -> Result<ReclaimExecutionOutcome, SdkError> {
        w.claim_from_reclaim_grant(g, a).await
    }
    let _ = _call;
}
