//! Issue #122 — two-backend same-Valkey isolation test.
//!
//! Simulates two FlowFabric consumers sharing a single Valkey
//! keyspace, each with a distinct [`ScannerFilter`]. The test
//! exercises two isolation paths:
//!
//! 1. **Scanner path** — `scanner::should_skip_candidate` applied
//!    to a mixed-namespace set of candidates. We seed two exec_core
//!    hashes on the same partition, one with `namespace = "alpha"`
//!    and one with `namespace = "beta"`, then verify the helper
//!    passes / rejects each under each of two filters.
//!
//! 2. **Completion subscriber path** —
//!    `CompletionBackend::subscribe_completions_filtered` with an
//!    `instance_tag` filter. We seed two executions with different
//!    `cairn.instance_id` tag values, open two filtered subscribers
//!    (one per instance tag), PUBLISH a completion for each on the
//!    shared channel, and assert each subscriber only receives the
//!    completion whose execution carries its tag.
//!
//! Run with: `cargo test -p ff-test --test scanner_filter_isolation -- --test-threads=1`

use std::time::Duration;

use ferriskey::ClientBuilder;
use ff_backend_valkey::{ValkeyBackend, COMPLETION_CHANNEL};
use ff_core::backend::{ScannerFilter, ValkeyConnection};
use ff_core::completion_backend::CompletionBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{FlowId, Namespace};
use ff_engine::scanner::should_skip_candidate;
use futures::StreamExt;

fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name).ok().as_deref(), Some("1" | "true"))
}

fn host() -> String {
    std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

fn port() -> u16 {
    std::env::var("FF_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379)
}

async fn build_client() -> ferriskey::Client {
    let mut builder = ClientBuilder::new().host(&host(), port());
    if env_flag("FF_TLS") {
        builder = builder.tls();
    }
    if env_flag("FF_CLUSTER") {
        builder = builder.cluster();
    }
    builder.build().await.expect("client build")
}

async fn backend() -> std::sync::Arc<ValkeyBackend> {
    let client = build_client().await;
    let mut conn = ValkeyConnection::new(host(), port());
    conn.tls = env_flag("FF_TLS");
    conn.cluster = env_flag("FF_CLUSTER");
    ValkeyBackend::from_client_partitions_and_connection(client, PartitionConfig::default(), conn)
}

/// Shared fixture partition. Using `{fp:7}` picks the same hash-tag
/// slot as other fixture tests so this test coexists cleanly in the
/// shared Valkey keyspace.
const PARTITION: u16 = 7;

/// Production exec-key format is DOUBLE-tagged
/// (`ff:exec:{fp:N}:{fp:N}:<uuid>:<suffix>`): the first `{fp:N}` is
/// the partition routing tag; the second is the `{fp:N}:` prefix
/// embedded inside every `ExecutionId::to_string()`. See
/// `ExecKeyContext::core` / `::tags` in `ff_core::keys`. The test
/// fixtures must match production exactly or they would falsely
/// validate the filter against a non-existent key.
fn exec_core_key(full_eid: &str) -> String {
    format!("ff:exec:{{fp:{PARTITION}}}:{full_eid}:core")
}
fn tags_key(full_eid: &str) -> String {
    format!("ff:exec:{{fp:{PARTITION}}}:{full_eid}:tags")
}

/// Build the full `{fp:N}:<uuid>` ExecutionId string — matches what
/// scanners read out of per-partition index ZSETs (they're members
/// written by Lua `ZADD ... A.execution_id` where execution_id is
/// the full-string form).
fn full_eid(bare_uuid: &str) -> String {
    format!("{{fp:{PARTITION}}}:{bare_uuid}")
}

/// Scanner path: `should_skip_candidate` returns the correct verdict
/// for each (candidate, filter) pair. Seeds two exec_core hashes with
/// different namespaces on the same partition.
#[tokio::test]
async fn scanner_namespace_filter_isolates_candidates() {
    let client = build_client().await;
    // PR-7b Cluster 1: `should_skip_candidate` now dispatches through
    // `EngineBackend` (via `describe_execution` / `get_execution_tag`),
    // not raw `ferriskey::Client` HGETs.
    let be: std::sync::Arc<dyn ff_core::engine_backend::EngineBackend> =
        backend().await;

    // Full execution-id strings matching production shape.
    let alpha_eid = full_eid("11111111-1111-1111-1111-111111111111");
    let beta_eid = full_eid("22222222-2222-2222-2222-222222222222");
    let alpha_core = exec_core_key(&alpha_eid);
    let beta_core = exec_core_key(&beta_eid);

    // Seed exec_core fields: only `namespace` matters for this test.
    let _: i64 = client
        .cmd("HSET")
        .arg(&alpha_core)
        .arg("namespace")
        .arg("alpha")
        .execute()
        .await
        .expect("HSET alpha");
    let _: i64 = client
        .cmd("HSET")
        .arg(&beta_core)
        .arg("namespace")
        .arg("beta")
        .execute()
        .await
        .expect("HSET beta");

    let mut alpha_filter = ScannerFilter::default();
    alpha_filter.namespace = Some(Namespace::new("alpha"));
    let mut beta_filter = ScannerFilter::default();
    beta_filter.namespace = Some(Namespace::new("beta"));

    // Each filter must admit its own candidate and reject the other.
    assert!(
        !should_skip_candidate(Some(&be), &alpha_filter, PARTITION, &alpha_eid).await,
        "alpha filter must admit alpha candidate"
    );
    assert!(
        should_skip_candidate(Some(&be), &alpha_filter, PARTITION, &beta_eid).await,
        "alpha filter must reject beta candidate"
    );
    assert!(
        !should_skip_candidate(Some(&be), &beta_filter, PARTITION, &beta_eid).await,
        "beta filter must admit beta candidate"
    );
    assert!(
        should_skip_candidate(Some(&be), &beta_filter, PARTITION, &alpha_eid).await,
        "beta filter must reject alpha candidate"
    );

    // A no-op filter admits both — confirms the short-circuit path.
    let noop = ScannerFilter::default();
    assert!(!should_skip_candidate(Some(&be), &noop, PARTITION, &alpha_eid).await);
    assert!(!should_skip_candidate(Some(&be), &noop, PARTITION, &beta_eid).await);

    // Cleanup.
    let _: i64 = client
        .cmd("DEL")
        .arg(&alpha_core)
        .arg(&beta_core)
        .execute()
        .await
        .unwrap_or(0);
}

async fn publish(client: &ferriskey::Client, eid: &str, flow_id: &str) {
    let payload = format!(
        r#"{{"execution_id":"{eid}","flow_id":"{flow_id}","outcome":"success"}}"#
    );
    let _: i64 = client
        .cmd("PUBLISH")
        .arg(COMPLETION_CHANNEL)
        .arg(&payload)
        .execute()
        .await
        .expect("PUBLISH");
}

/// Completion subscriber path: two `subscribe_completions_filtered`
/// calls with different `instance_tag` filters each receive ONLY
/// their own instance's completions.
#[tokio::test]
async fn completion_instance_tag_filter_isolates_subscribers() {
    let backend = backend().await;
    let seeder = build_client().await;

    // Two executions on the same partition, different instance tags.
    // Full `{fp:N}:<uuid>` form — matches the wire shape the completion
    // subscriber deserialises out of the PUBLISH payload AND the
    // canonical double-tagged key the backend HGETs against.
    let eid_i1 = full_eid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let eid_i2 = full_eid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");

    let _: i64 = seeder
        .cmd("HSET")
        .arg(tags_key(&eid_i1))
        .arg("cairn.instance_id")
        .arg("instance-1")
        .execute()
        .await
        .expect("HSET tags i1");
    let _: i64 = seeder
        .cmd("HSET")
        .arg(tags_key(&eid_i2))
        .arg("cairn.instance_id")
        .arg("instance-2")
        .execute()
        .await
        .expect("HSET tags i2");

    let mut filter_i1 = ScannerFilter::default();
    filter_i1.instance_tag = Some(("cairn.instance_id".into(), "instance-1".into()));
    let mut filter_i2 = ScannerFilter::default();
    filter_i2.instance_tag = Some(("cairn.instance_id".into(), "instance-2".into()));

    let mut stream_i1 = backend
        .subscribe_completions_filtered(&filter_i1)
        .await
        .expect("subscribe i1");
    let mut stream_i2 = backend
        .subscribe_completions_filtered(&filter_i2)
        .await
        .expect("subscribe i2");

    // Give the subscribers time to SUBSCRIBE before PUBLISHing.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let flow_id = FlowId::new().to_string();
    publish(&seeder, &eid_i1, &flow_id).await;
    publish(&seeder, &eid_i2, &flow_id).await;

    // Subscriber i1 should see ONLY the i1 completion. Drain for up
    // to 2s, asserting we never see an i2 event.
    let mut got_i1 = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream_i1.next()).await {
            Ok(Some(p)) => got_i1.push(p.execution_id.to_string()),
            Ok(None) => break,
            Err(_) => break,
        }
        // If we've already got i1, stop; a second push would be a bug.
        if !got_i1.is_empty() {
            // Brief additional grace to catch any trailing cross-instance leak.
            match tokio::time::timeout(Duration::from_millis(400), stream_i1.next()).await {
                Ok(Some(p)) => got_i1.push(p.execution_id.to_string()),
                _ => break,
            }
        }
    }
    assert_eq!(
        got_i1,
        vec![eid_i1.clone()],
        "subscriber i1 must receive exactly the i1 completion"
    );

    // Same check for i2.
    let mut got_i2 = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream_i2.next()).await {
            Ok(Some(p)) => got_i2.push(p.execution_id.to_string()),
            Ok(None) => break,
            Err(_) => break,
        }
        if !got_i2.is_empty() {
            match tokio::time::timeout(Duration::from_millis(400), stream_i2.next()).await {
                Ok(Some(p)) => got_i2.push(p.execution_id.to_string()),
                _ => break,
            }
        }
    }
    assert_eq!(
        got_i2,
        vec![eid_i2.clone()],
        "subscriber i2 must receive exactly the i2 completion"
    );

    // Cleanup.
    let _: i64 = seeder
        .cmd("DEL")
        .arg(tags_key(&eid_i1))
        .arg(tags_key(&eid_i2))
        .execute()
        .await
        .unwrap_or(0);
}
