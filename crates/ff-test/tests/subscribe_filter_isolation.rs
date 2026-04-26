//! Issue #282 — per-subscriber `ScannerFilter` isolation for
//! `subscribe_lease_history` + `subscribe_signal_delivery` on the
//! Valkey backend.
//!
//! Mirrors `scanner_filter_isolation.rs`: simulates two FlowFabric
//! consumers sharing a single Valkey keyspace, each with a distinct
//! `instance_tag` filter, and asserts each subscriber receives ONLY
//! the events whose execution carries its tag.
//!
//! Run with: `cargo test -p ff-test --test subscribe_filter_isolation -- --test-threads=1`

use std::time::Duration;

use ferriskey::ClientBuilder;
use ff_backend_valkey::{
    partition_lease_history_key, partition_signal_delivery_key, ValkeyBackend,
};
use ff_core::backend::{ScannerFilter, ValkeyConnection};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily};
use ff_core::stream_events::SignalDeliveryEvent;
use ff_core::stream_subscribe::StreamCursor;
use futures::Stream;
use uuid::Uuid;

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

/// Partition 0 — the `ValkeyBackend` hardcodes partition 0 for
/// subscribe aggregate streams. Fixture sharing key-space lives on the
/// matching `{fp:0}` hash-tag slot.
const PARTITION: u16 = 0;

/// Production exec-key format is DOUBLE-tagged (see
/// `scanner_filter_isolation.rs`).
fn tags_key(full_eid: &str) -> String {
    format!("ff:exec:{{fp:{PARTITION}}}:{full_eid}:tags")
}

/// Full `{fp:N}:<uuid>` ExecutionId string.
fn full_eid(bare_uuid: &str) -> String {
    format!("{{fp:{PARTITION}}}:{bare_uuid}")
}

/// Poll `.next()` on a pinned stream without dragging in `futures`.
async fn next_item<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}

/// Write a synthetic `reclaimed` lease-history event to the aggregate
/// stream. The field shape mirrors what `lua/lease.lua` emits.
async fn xadd_reclaimed(writer: &ferriskey::Client, stream_key: &str, execution_id: &str) {
    let lease_uuid = Uuid::new_v4();
    let _: ferriskey::Value = writer
        .cmd("XADD")
        .arg(stream_key)
        .arg("*")
        .arg("event")
        .arg("reclaimed")
        .arg("execution_id")
        .arg(execution_id)
        .arg("new_lease_id")
        .arg(lease_uuid.to_string().as_str())
        .arg("worker_instance_id")
        .arg("worker-filter-iso")
        .arg("ts")
        .arg("1700000000000")
        .execute()
        .await
        .expect("XADD lease-history event");
}

/// Lease-history isolation: two filtered subscriptions each see only
/// events whose execution carries their instance tag.
#[tokio::test]
#[ignore = "requires live Valkey at localhost:6379"]
async fn subscribe_lease_history_instance_tag_filter_isolates() {
    let backend = backend().await;
    let writer = build_client().await;

    // Two executions on partition 0, different instance tags.
    let eid_i1 = full_eid("cccccccc-cccc-4ccc-accc-cccccccccccc");
    let eid_i2 = full_eid("dddddddd-dddd-4ddd-addd-dddddddddddd");
    let _: i64 = writer
        .cmd("HSET")
        .arg(tags_key(&eid_i1))
        .arg("cairn.instance_id")
        .arg("lease-instance-1")
        .execute()
        .await
        .expect("HSET tags i1");
    let _: i64 = writer
        .cmd("HSET")
        .arg(tags_key(&eid_i2))
        .arg("cairn.instance_id")
        .arg("lease-instance-2")
        .execute()
        .await
        .expect("HSET tags i2");

    let mut filter_i1 = ScannerFilter::default();
    filter_i1.instance_tag = Some(("cairn.instance_id".into(), "lease-instance-1".into()));
    let mut filter_i2 = ScannerFilter::default();
    filter_i2.instance_tag = Some(("cairn.instance_id".into(), "lease-instance-2".into()));

    let mut sub_i1 = backend
        .subscribe_lease_history(StreamCursor::empty(), &filter_i1)
        .await
        .expect("subscribe i1");
    let mut sub_i2 = backend
        .subscribe_lease_history(StreamCursor::empty(), &filter_i2)
        .await
        .expect("subscribe i2");

    // Let XREAD BLOCK register before writes.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let partition = Partition {
        family: PartitionFamily::Flow,
        index: 0,
    };
    let stream_key = partition_lease_history_key(&partition);
    xadd_reclaimed(&writer, &stream_key, &eid_i1).await;
    xadd_reclaimed(&writer, &stream_key, &eid_i2).await;

    // Collect exactly one event per subscriber (with a grace window
    // afterwards to catch any leak).
    let got_i1 = collect_one_lease(&mut sub_i1).await;
    let got_i2 = collect_one_lease(&mut sub_i2).await;

    assert_eq!(
        got_i1.as_deref(),
        Some(eid_i1.as_str()),
        "subscriber i1 must receive exactly the i1 reclaimed event"
    );
    assert_eq!(
        got_i2.as_deref(),
        Some(eid_i2.as_str()),
        "subscriber i2 must receive exactly the i2 reclaimed event"
    );

    // Cleanup.
    let _: i64 = writer
        .cmd("DEL")
        .arg(tags_key(&eid_i1))
        .arg(tags_key(&eid_i2))
        .execute()
        .await
        .unwrap_or(0);
}

async fn collect_one_lease(
    stream: &mut ff_core::stream_events::LeaseHistorySubscription,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, next_item(stream)).await {
            Ok(Some(Ok(ev))) => {
                let eid = ev.execution_id().to_string();
                // Grace window to ensure no cross-instance leak follows.
                match tokio::time::timeout(Duration::from_millis(400), next_item(stream)).await {
                    Ok(Some(Ok(extra))) => {
                        panic!(
                            "subscriber leaked an extra event: first={eid}, leaked={}",
                            extra.execution_id()
                        );
                    }
                    _ => return Some(eid),
                }
            }
            Ok(Some(Err(e))) => panic!("subscription yielded error: {e:?}"),
            Ok(None) | Err(_) => return None,
        }
    }
}

/// Signal-delivery isolation: mirror of the lease_history test
/// against the `ff:part:{fp:0}:signal_delivery` stream.
#[tokio::test]
#[ignore = "requires live Valkey at localhost:6379"]
async fn subscribe_signal_delivery_instance_tag_filter_isolates() {
    let backend = backend().await;
    let writer = build_client().await;

    let eid_i1 = full_eid("eeeeeeee-eeee-4eee-aeee-eeeeeeeeeeee");
    let eid_i2 = full_eid("ffffffff-ffff-4fff-afff-ffffffffffff");
    let _: i64 = writer
        .cmd("HSET")
        .arg(tags_key(&eid_i1))
        .arg("cairn.instance_id")
        .arg("signal-instance-1")
        .execute()
        .await
        .expect("HSET tags i1");
    let _: i64 = writer
        .cmd("HSET")
        .arg(tags_key(&eid_i2))
        .arg("cairn.instance_id")
        .arg("signal-instance-2")
        .execute()
        .await
        .expect("HSET tags i2");

    let mut filter_i1 = ScannerFilter::default();
    filter_i1.instance_tag = Some(("cairn.instance_id".into(), "signal-instance-1".into()));
    let mut filter_i2 = ScannerFilter::default();
    filter_i2.instance_tag = Some(("cairn.instance_id".into(), "signal-instance-2".into()));

    let mut sub_i1 = backend
        .subscribe_signal_delivery(StreamCursor::empty(), &filter_i1)
        .await
        .expect("subscribe i1");
    let mut sub_i2 = backend
        .subscribe_signal_delivery(StreamCursor::empty(), &filter_i2)
        .await
        .expect("subscribe i2");

    tokio::time::sleep(Duration::from_millis(300)).await;

    let partition = Partition {
        family: PartitionFamily::Flow,
        index: 0,
    };
    let stream_key = partition_signal_delivery_key(&partition);

    let signal_a = Uuid::new_v4();
    let signal_b = Uuid::new_v4();
    xadd_signal_delivery(&writer, &stream_key, &eid_i1, signal_a).await;
    xadd_signal_delivery(&writer, &stream_key, &eid_i2, signal_b).await;

    let got_i1 = collect_one_signal(&mut sub_i1).await;
    let got_i2 = collect_one_signal(&mut sub_i2).await;

    assert_eq!(
        got_i1.as_deref(),
        Some(eid_i1.as_str()),
        "subscriber i1 must receive exactly the i1 signal-delivery event"
    );
    assert_eq!(
        got_i2.as_deref(),
        Some(eid_i2.as_str()),
        "subscriber i2 must receive exactly the i2 signal-delivery event"
    );

    let _: i64 = writer
        .cmd("DEL")
        .arg(tags_key(&eid_i1))
        .arg(tags_key(&eid_i2))
        .execute()
        .await
        .unwrap_or(0);
}

/// XADD a synthetic signal-delivery event. Field shape matches
/// `lua/signal.lua::ff_deliver_signal` at KEYS[15].
async fn xadd_signal_delivery(
    writer: &ferriskey::Client,
    stream_key: &str,
    execution_id: &str,
    signal_id: Uuid,
) {
    let _: ferriskey::Value = writer
        .cmd("XADD")
        .arg(stream_key)
        .arg("*")
        .arg("execution_id")
        .arg(execution_id)
        .arg("signal_id")
        .arg(signal_id.to_string().as_str())
        .arg("effect")
        .arg("satisfied")
        .arg("delivered_at_ms")
        .arg("1700000000000")
        .execute()
        .await
        .expect("XADD signal-delivery event");
}

async fn collect_one_signal(
    stream: &mut ff_core::stream_events::SignalDeliverySubscription,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, next_item(stream)).await {
            Ok(Some(Ok(ev))) => {
                let SignalDeliveryEvent { execution_id, .. } = &ev;
                let eid = execution_id.to_string();
                match tokio::time::timeout(Duration::from_millis(400), next_item(stream)).await {
                    Ok(Some(Ok(extra))) => {
                        panic!(
                            "subscriber leaked an extra event: first={}, leaked={}",
                            eid, extra.execution_id
                        );
                    }
                    _ => return Some(eid),
                }
            }
            Ok(Some(Err(e))) => panic!("subscription yielded error: {e:?}"),
            Ok(None) | Err(_) => return None,
        }
    }
}
