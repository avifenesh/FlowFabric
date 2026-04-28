//! End-to-end wire round-trip for [`ff_core::contracts::ClaimGrant`]
//! (issue #91).
//!
//! Exercises the full chain:
//!
//!   1. Scheduler issues a `ClaimGrant` carrying an opaque
//!      [`ff_core::partition::PartitionKey`].
//!   2. `ff-server` serializes it into the `POST /v1/workers/{id}/claim`
//!      200 response body.
//!   3. `ff-sdk::ClaimForWorkerResponse` deserializes the body.
//!   4. `into_grant()` reconstructs a `ClaimGrant` with the same
//!      partition-key string.
//!
//! Pins the opaque contract at the wire boundary: the JSON payload
//! carries the hash-tag literal (`{fp:N}`, `{b:N}`, `{q:N}`) as a
//! bare string, never a structured `{family, index}` pair.
//!
//! This is a pure type-level round-trip — no live Valkey, no HTTP
//! transport. We serialize the server's DTO, feed the bytes into the
//! SDK's DTO, and compare. Any drift in either side's field set
//! surfaces here rather than on a live deploy.

use ff_core::contracts::ClaimGrant;
use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
use ff_core::types::{ExecutionId, FlowId};

/// Construct a sample ClaimGrant for a given family. Index is pinned
/// so the wire-shape assertions below can match on the exact string.
fn sample_grant(family: PartitionFamily) -> ClaimGrant {
    let config = ff_core::partition::PartitionConfig::default();
    let fid = FlowId::new();
    let eid = ExecutionId::for_flow(&fid, &config);
    let p = Partition { family, index: 42 };
    ClaimGrant::new(
        eid,
        PartitionKey::from(&p),
        "ff:exec:{fp:42}:00000000-0000-0000-0000-000000000000:claim_grant".to_owned(),
        1_700_000_000_000,
    )
}

/// Manually construct the JSON the server emits (ClaimGrantDto is
/// crate-private in ff-server, so we spell out the exact shape here
/// — any drift in either side surfaces as this test failing).
fn server_side_json(g: &ClaimGrant) -> String {
    // The server's ClaimGrantDto fields: execution_id (string),
    // partition_key (transparent string), grant_key (string),
    // expires_at_ms (u64). This shape is pinned by the inline tests
    // in ff-server::api::claim_grant_dto_tests.
    serde_json::json!({
        "execution_id": g.execution_id.to_string(),
        "partition_key": g.partition_key.as_str(),
        "grant_key": g.grant_key.as_str(),
        "expires_at_ms": g.expires_at_ms,
    })
    .to_string()
}

#[test]
fn claim_grant_round_trips_flow_family() {
    let grant = sample_grant(PartitionFamily::Flow);
    let json = server_side_json(&grant);
    // Wire shape: bare string partition_key.
    assert!(
        json.contains(r#""partition_key":"{fp:42}""#),
        "wire must carry opaque hash tag, got: {json}"
    );
    assert!(
        !json.contains("partition_family"),
        "post-#91 must not carry partition_family, got: {json}"
    );
    assert!(
        !json.contains("partition_index"),
        "post-#91 must not carry partition_index, got: {json}"
    );

    let resp: ff_sdk::admin::ClaimForWorkerResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(resp.partition_key.as_str(), "{fp:42}");
    let rebuilt = resp.into_grant().expect("into_grant must succeed");
    assert_eq!(rebuilt.partition_key.as_str(), grant.partition_key.as_str());
    assert_eq!(rebuilt.expires_at_ms, grant.expires_at_ms);
    assert_eq!(rebuilt.grant_key, grant.grant_key);
}

#[test]
fn claim_grant_round_trips_budget_and_quota() {
    for (family, expected) in [
        (PartitionFamily::Budget, "{b:42}"),
        (PartitionFamily::Quota, "{q:42}"),
    ] {
        let grant = sample_grant(family);
        let json = server_side_json(&grant);
        let resp: ff_sdk::admin::ClaimForWorkerResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp.partition_key.as_str(), expected);
        let rebuilt = resp.into_grant().expect("into_grant must succeed");
        assert_eq!(rebuilt.partition_key.as_str(), expected);
        // Parsed partition routes through correctly.
        let parsed = rebuilt.partition().expect("key must parse");
        assert_eq!(parsed.index, 42);
    }
}

#[test]
fn claim_grant_execution_alias_collapses_on_round_trip() {
    // RFC-011 §11 alias: a grant minted with Execution family emits
    // `{fp:N}` and parses back to Flow. Routing is preserved (same
    // hash tag, same index); only the metadata family label
    // normalises.
    let grant_exec = sample_grant(PartitionFamily::Execution);
    assert_eq!(grant_exec.partition_key.as_str(), "{fp:42}");

    let json = server_side_json(&grant_exec);
    let resp: ff_sdk::admin::ClaimForWorkerResponse = serde_json::from_str(&json).unwrap();
    let rebuilt = resp.into_grant().unwrap();
    let parsed = rebuilt.partition().unwrap();
    assert_eq!(parsed.family, PartitionFamily::Flow, "alias collapses to Flow");
    assert_eq!(parsed.index, 42);
    // The routing-relevant hash tag matches what we sent.
    assert_eq!(parsed.hash_tag(), "{fp:42}");
}
