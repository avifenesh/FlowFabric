//! RFC-019 Stage C integration test — typed `subscribe_signal_delivery`
//! on the Valkey backend (#310).
//!
//! Gated `#[ignore]` because it requires a live Valkey at
//! `localhost:6379`. Mirrors `subscribe_lease_history.rs`: subscribe
//! + synthetic XADD + observe a typed `SignalDeliveryEvent`.
//!
//! Run with:
//! ```
//! valkey-cli -h localhost -p 6379 ping
//! FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) \
//!   cargo test -p ff-backend-valkey --test subscribe_signal_delivery -- --ignored
//! ```

use std::time::Duration;

use ff_backend_valkey::{partition_signal_delivery_key, ValkeyBackend};
use ff_core::backend::{BackendConfig, ScannerFilter};
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::stream_events::SignalDeliveryEffect;
use ff_core::stream_subscribe::StreamCursor;
use futures_core::Stream;
use tokio::time::timeout;
use uuid::Uuid;

async fn next_item<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}

#[tokio::test]
#[ignore = "requires live Valkey at localhost:6379"]
async fn subscribe_signal_delivery_yields_typed_event() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("localhost", 6379))
        .await
        .expect("connect to localhost valkey");

    let mut sub = backend
        .subscribe_signal_delivery(StreamCursor::empty(), &ScannerFilter::default())
        .await
        .expect("subscribe_signal_delivery");

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
    let stream_key = partition_signal_delivery_key(&partition);

    let exec_uuid = Uuid::new_v4();
    let execution_id_str = format!("{{fp:0}}:{exec_uuid}");
    let signal_uuid = Uuid::new_v4();
    let waitpoint_uuid = Uuid::new_v4();

    let _: ferriskey::Value = writer
        .cmd("XADD")
        .arg(stream_key.as_str())
        .arg("*")
        .arg("signal_id")
        .arg(signal_uuid.to_string().as_str())
        .arg("execution_id")
        .arg(execution_id_str.as_str())
        .arg("waitpoint_id")
        .arg(waitpoint_uuid.to_string().as_str())
        .arg("source_identity")
        .arg("test-source")
        .arg("effect")
        .arg("satisfied")
        .arg("delivered_at_ms")
        .arg("1700000000000")
        .execute()
        .await
        .expect("XADD synthetic signal-delivery event");

    let event = timeout(Duration::from_secs(10), next_item(&mut sub))
        .await
        .expect("subscription yielded within 10s")
        .expect("stream ended unexpectedly")
        .expect("backend surfaced error before event");

    assert_eq!(
        event.cursor.as_bytes()[0],
        ff_core::stream_subscribe::VALKEY_CURSOR_PREFIX,
        "cursor prefix must be VALKEY_CURSOR_PREFIX (0x01)"
    );
    assert_eq!(event.signal_id.0, signal_uuid);
    assert_eq!(event.execution_id.to_string(), execution_id_str);
    assert_eq!(event.waitpoint_id.as_ref().map(|w| w.0), Some(waitpoint_uuid));
    assert_eq!(event.source_identity.as_deref(), Some("test-source"));
    assert_eq!(event.effect, SignalDeliveryEffect::Satisfied);
    assert_eq!(event.at.0, 1_700_000_000_000);

    let _: ferriskey::Value = writer
        .cmd("DEL")
        .arg(stream_key.as_str())
        .execute()
        .await
        .unwrap_or(ferriskey::Value::Nil);
}
