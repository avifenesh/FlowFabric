//! `CompletionBackend` implementation for Valkey (issue #90).
//!
//! Opens a dedicated RESP3 subscriber client per
//! `subscribe_completions()` call and forwards parsed
//! [`CompletionPayload`]s over a bounded `mpsc`. The Lua emitters on
//! `ff_complete_execution` / `ff_fail_execution` /
//! `ff_cancel_execution` PUBLISH to [`COMPLETION_CHANNEL`]; this impl
//! consumes that wire format and hides the channel string behind the
//! `CompletionBackend` trait.
//!
//! The previous `ff_engine::completion_listener::spawn_completion_listener`
//! also opened a dedicated RESP3 client + SUBSCRIBE loop. Issue #90
//! moves that wire-level work into the Valkey backend crate so
//! ff-engine becomes backend-agnostic: it now consumes the stream
//! produced here rather than calling ferriskey directly.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ff_core::backend::{CompletionPayload, ScannerFilter, ValkeyConnection};
use ff_core::completion_backend::{CompletionBackend, CompletionStream};
use ff_core::engine_error::EngineError;
use ff_core::partition::execution_partition;
use ff_core::types::{ExecutionId, FlowId, Namespace, TimestampMs};
use ferriskey::value::ProtocolVersion;
use ferriskey::{Client, ClientBuilder, PushInfo, PushKind, Value};
use futures_core::Stream;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::ValkeyBackend;

/// Channel name the Lua completion emitters publish to. Canonical
/// location (amendment to RFC namespace #122). Lives in
/// ff-backend-valkey only — ff-core's `CompletionBackend` trait
/// deliberately does not carry this string.
pub const COMPLETION_CHANNEL: &str = "ff:dag:completions";

/// Bounded channel capacity for the mpsc that fans push frames out to
/// the stream consumer. Chosen to absorb transient bursts without
/// unbounded memory growth; backpressure applies on overflow (the
/// subscriber task `send()` awaits, which stalls the push drain —
/// acceptable because the `dependency_reconciler` safety net covers
/// any dropped-due-to-disconnect completions anyway).
const STREAM_CAPACITY: usize = 1024;

/// Wire shape of the PUBLISH payload. Matches the Lua emitters.
/// `ExecutionId`'s Deserialize impl validates the `{fp:N}:<uuid>`
/// shape at parse time; `flow_id` is a bare UUID string.
#[derive(Debug, Deserialize)]
struct WirePayload {
    execution_id: ExecutionId,
    flow_id: String,
    #[serde(default)]
    outcome: String,
}

#[async_trait]
impl CompletionBackend for ValkeyBackend {
    async fn subscribe_completions(&self) -> Result<CompletionStream, EngineError> {
        let Some(conn) = self.subscriber_connection.clone() else {
            return Err(EngineError::Unavailable {
                op: "ValkeyBackend::subscribe_completions (no subscriber connection)",
            });
        };

        let (tx, rx) = mpsc::channel::<CompletionPayload>(STREAM_CAPACITY);

        // Spawn the reconnect/subscribe loop. Lifetime is tied to the
        // stream: when the consumer drops the `Receiver`, `tx.send()`
        // starts failing and the loop exits.
        tokio::spawn(subscriber_loop(conn, tx, None));

        let stream = ReceiverStream { rx };
        Ok(Box::pin(stream))
    }

    async fn subscribe_completions_filtered(
        &self,
        filter: &ScannerFilter,
    ) -> Result<CompletionStream, EngineError> {
        if filter.is_noop() {
            return self.subscribe_completions().await;
        }
        let Some(conn) = self.subscriber_connection.clone() else {
            return Err(EngineError::Unavailable {
                op: "ValkeyBackend::subscribe_completions_filtered (no subscriber connection)",
            });
        };

        let (tx, rx) = mpsc::channel::<CompletionPayload>(STREAM_CAPACITY);

        // The filtered subscriber uses the same subscriber connection
        // for the SUBSCRIBE loop, plus a clone of the main
        // (non-subscriber) client for per-push HGETs. Cloning
        // `ferriskey::Client` shares the underlying multiplexed
        // connection — no extra TCP handshake.
        let gate_client = self.client.clone();
        let partition_config = self.partition_config;
        let filter_owned = filter.clone();
        tokio::spawn(subscriber_loop(
            conn,
            tx,
            Some(FilterGate {
                client: gate_client,
                partition_config,
                filter: filter_owned,
            }),
        ));

        let stream = ReceiverStream { rx };
        Ok(Box::pin(stream))
    }
}

/// Per-push filter gate: holds the resources needed to perform the
/// HGET(s) that decide whether a completion payload passes the
/// configured [`ScannerFilter`].
#[derive(Clone)]
pub(crate) struct FilterGate {
    pub(crate) client: ferriskey::Client,
    pub(crate) partition_config: ff_core::partition::PartitionConfig,
    pub(crate) filter: ScannerFilter,
}

impl FilterGate {
    /// Returns true iff the execution identified by `eid` passes the
    /// gate's [`ScannerFilter`]. On HGET failure conservatively drops
    /// the event (returns false) so a backend hiccup can't leak a
    /// cross-tenant completion.
    async fn admits(&self, eid: &ExecutionId) -> bool {
        let partition = execution_partition(eid, &self.partition_config);
        let tag = partition.hash_tag();
        let eid_str = eid.to_string();

        // Check namespace first (cheaper — one HGET on exec_core, the
        // hash every scanner / op already touches).
        if let Some(ref want_ns) = self.filter.namespace {
            let core_key = format!("ff:exec:{}:{}:core", tag, eid_str);
            let got: Result<Option<String>, _> = self
                .client
                .cmd("HGET")
                .arg(&core_key)
                .arg("namespace")
                .execute()
                .await;
            match got {
                Ok(Some(s)) => {
                    let have = Namespace::new(s);
                    if &have != want_ns {
                        return false;
                    }
                }
                Ok(None) | Err(_) => return false,
            }
        }

        if let Some((ref tag_key, ref want_value)) = self.filter.instance_tag {
            let tags_key = format!("ff:exec:{}:{}:tags", tag, eid_str);
            let got: Result<Option<String>, _> = self
                .client
                .cmd("HGET")
                .arg(&tags_key)
                .arg(tag_key)
                .execute()
                .await;
            match got {
                Ok(Some(v)) if &v == want_value => {}
                _ => return false,
            }
        }

        true
    }
}

/// `mpsc::Receiver<CompletionPayload>` → `Stream` adapter. Kept
/// in-crate (vs. pulling `tokio-stream`) to avoid adding a new dep
/// for a 5-line wrapper.
struct ReceiverStream {
    rx: mpsc::Receiver<CompletionPayload>,
}

impl Stream for ReceiverStream {
    type Item = CompletionPayload;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// Reconnect loop: build a RESP3 subscriber, SUBSCRIBE, drain push
/// frames until the channel closes or the consumer drops the stream.
/// Transient errors sleep 1s and retry.
///
/// `gate` (issue #122): when Some, every parsed push is asked
/// `gate.admits(execution_id)` before being forwarded; rejected
/// events are dropped silently. None preserves the pre-#122 unfiltered
/// behaviour.
///
async fn subscriber_loop(
    conn: ValkeyConnection,
    tx: mpsc::Sender<CompletionPayload>,
    gate: Option<FilterGate>,
) {
    while !tx.is_closed() {
        let (push_tx, mut push_rx) = mpsc::unbounded_channel();
        let client = match build_subscriber_client(&conn, push_tx).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "completion subscriber: build client failed, retrying in 1s"
                );
                if sleep_or_closed(&tx, Duration::from_secs(1)).await {
                    return;
                }
                continue;
            }
        };

        let sub_result: Result<(), _> = client
            .cmd("SUBSCRIBE")
            .arg(COMPLETION_CHANNEL)
            .execute()
            .await;
        if let Err(e) = sub_result {
            tracing::warn!(
                error = %e,
                "completion subscriber: SUBSCRIBE failed, retrying in 1s"
            );
            if sleep_or_closed(&tx, Duration::from_secs(1)).await {
                return;
            }
            continue;
        }

        tracing::info!(
            channel = COMPLETION_CHANNEL,
            "completion subscriber: subscribed, awaiting push frames"
        );

        // Drain pushes until the connection drops or the consumer
        // closes the stream.
        loop {
            tokio::select! {
                _ = tx.closed() => return,
                msg = push_rx.recv() => {
                    let Some(push) = msg else {
                        tracing::warn!(
                            "completion subscriber: push channel closed, reconnecting"
                        );
                        break;
                    };
                    if let Some(payload) = parse_push(push) {
                        // Issue #122: gate the push via HGET(s) when
                        // a ScannerFilter is installed. A non-admit
                        // silently drops the event — the other
                        // instance(s) sharing the Valkey keyspace
                        // will receive their own copy.
                        if let Some(ref g) = gate
                            && !g.admits(&payload.execution_id).await
                        {
                            continue;
                        }
                        // `send()` awaits on backpressure; on
                        // consumer drop it returns Err, terminating
                        // the task cleanly.
                        if tx.send(payload).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }

        // Client drops here → connection closes. Loop head checks
        // `tx.is_closed()` before reconnecting.
    }
}

async fn build_subscriber_client(
    conn: &ValkeyConnection,
    push_tx: mpsc::UnboundedSender<PushInfo>,
) -> Result<Client, ferriskey::value::Error> {
    let mut builder = ClientBuilder::new()
        .protocol(ProtocolVersion::RESP3)
        .push_sender(push_tx)
        .host(&conn.host, conn.port);
    if conn.cluster {
        builder = builder.cluster();
    }
    if conn.tls {
        builder = builder.tls();
    }
    builder.build().await
}

/// Parse one push frame. Returns `None` for frames we should silently
/// drop (non-Message pushes, malformed payloads) — the reconciler
/// safety-net picks completions the subscriber misses.
fn parse_push(push: PushInfo) -> Option<CompletionPayload> {
    if push.kind != PushKind::Message {
        return None;
    }

    // RESP3 Message layout: [channel, payload]. Verify the channel
    // to guard against unexpected fan-in.
    let channel_match = matches!(
        push.data.first(),
        Some(Value::BulkString(b)) if b.as_ref() == COMPLETION_CHANNEL.as_bytes()
    ) || matches!(
        push.data.first(),
        Some(Value::SimpleString(s)) if s == COMPLETION_CHANNEL
    );
    if !channel_match {
        tracing::debug!(
            data = ?push.data.first(),
            "completion subscriber: Message on unexpected channel, ignoring"
        );
        return None;
    }

    let payload_str = match push.data.get(1) {
        Some(Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
        Some(Value::SimpleString(s)) => s.clone(),
        _ => {
            tracing::warn!(
                data = ?push.data,
                "completion subscriber: malformed Message frame, skipping"
            );
            return None;
        }
    };

    let parsed: WirePayload = match serde_json::from_str(&payload_str) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                error = %e,
                payload = %payload_str,
                "completion subscriber: json decode failed, skipping"
            );
            return None;
        }
    };

    let flow_id = match FlowId::parse(&parsed.flow_id) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                error = ?e,
                flow_id = %parsed.flow_id,
                "completion subscriber: flow_id parse failed, skipping"
            );
            return None;
        }
    };

    Some(
        CompletionPayload::new(
            parsed.execution_id,
            parsed.outcome,
            None,
            TimestampMs::from_millis(0),
        )
        .with_flow_id(flow_id),
    )
}

/// Sleep for `dur` or return early if the stream consumer drops.
/// Returns true if the consumer closed (caller should terminate).
async fn sleep_or_closed(tx: &mpsc::Sender<CompletionPayload>, dur: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = tx.closed() => true,
    }
}

/// Compile-time dyn-safety assertion for `CompletionBackend` on
/// `ValkeyBackend`. Mirrors `_dyn_compatible` for `EngineBackend`.
#[allow(dead_code)]
fn _completion_dyn_compatible(b: Arc<ValkeyBackend>) -> Arc<dyn CompletionBackend> {
    b
}
