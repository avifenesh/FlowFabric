//! RFC-019 Stage C integration test — typed `subscribe_lease_history`
//! on the Valkey backend.
//!
//! Gated `#[ignore]` because it requires a live Valkey at
//! `localhost:6379` (the default deployment used by the project's
//! smoke gate). Run with:
//!
//! ```
//! valkey-cli -h localhost -p 6379 ping
//! cargo test -p ff-backend-valkey --test subscribe_lease_history -- --ignored
//! ```
//!
//! The test:
//! 1. Dials the backend + takes the `subscribe_lease_history` stream
//!    with an empty cursor ("tail from now").
//! 2. Writes a synthetic `reclaimed` lease-history event via raw
//!    `XADD` on the partition-level aggregate stream key
//!    (`ff:part:{fp:0}:lease_history`).
//! 3. Asserts the subscription yields a
//!    `LeaseHistoryEvent::Reclaimed` inside a 10-second window.

use std::time::Duration;

use ff_backend_valkey::{partition_lease_history_key, ValkeyBackend};
use ff_core::backend::{BackendConfig, ScannerFilter};
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::stream_events::LeaseHistoryEvent;
use ff_core::stream_subscribe::StreamCursor;
use futures_core::Stream;
use tokio::time::timeout;
use uuid::Uuid;

/// Poll `.next()` on a pinned stream without dragging in `futures`.
async fn next_item<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}

#[tokio::test]
#[ignore = "requires live Valkey at localhost:6379"]
async fn subscribe_lease_history_yields_typed_reclaimed_event() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("localhost", 6379))
        .await
        .expect("connect to localhost valkey");

    let mut sub = backend
        .subscribe_lease_history(StreamCursor::empty(), &ScannerFilter::default())
        .await
        .expect("subscribe_lease_history");

    // Let XREAD BLOCK register before we write.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let writer = ferriskey::ClientBuilder::new()
        .host("localhost", 6379)
        .build()
        .await
        .expect("build raw writer client");

    let partition = Partition {
        family: PartitionFamily::Flow,
        index: 0,
    };
    let stream_key = partition_lease_history_key(&partition);

    let exec_uuid = Uuid::new_v4();
    let execution_id_str = format!("{{fp:0}}:{exec_uuid}");
    let lease_uuid = Uuid::new_v4();

    let _: ferriskey::Value = writer
        .cmd("XADD")
        .arg(stream_key.as_str())
        .arg("*")
        .arg("event")
        .arg("reclaimed")
        .arg("execution_id")
        .arg(execution_id_str.as_str())
        .arg("new_lease_id")
        .arg(lease_uuid.to_string().as_str())
        .arg("worker_instance_id")
        .arg("worker-test")
        .arg("ts")
        .arg("1700000000000")
        .execute()
        .await
        .expect("XADD synthetic lease-history event");

    let event = timeout(Duration::from_secs(10), next_item(&mut sub))
        .await
        .expect("subscription yielded within 10s")
        .expect("stream ended unexpectedly")
        .expect("backend surfaced error before event");

    match event {
        LeaseHistoryEvent::Reclaimed {
            cursor,
            execution_id,
            new_lease_id,
            new_owner,
            at,
        } => {
            assert_eq!(
                cursor.as_bytes()[0],
                ff_core::stream_subscribe::VALKEY_CURSOR_PREFIX,
                "cursor prefix must be VALKEY_CURSOR_PREFIX (0x01)"
            );
            assert_eq!(execution_id.to_string(), execution_id_str);
            assert_eq!(new_lease_id.as_ref().map(|l| l.0), Some(lease_uuid));
            assert_eq!(
                new_owner.as_ref().map(|w| w.as_str()),
                Some("worker-test")
            );
            assert_eq!(at.0, 1_700_000_000_000);
        }
        other => panic!("expected LeaseHistoryEvent::Reclaimed, got {other:?}"),
    }

    // Cleanup.
    let _: ferriskey::Value = writer
        .cmd("DEL")
        .arg(stream_key.as_str())
        .execute()
        .await
        .unwrap_or(ferriskey::Value::Nil);
}
