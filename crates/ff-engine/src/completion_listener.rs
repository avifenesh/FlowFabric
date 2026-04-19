//! Push-based DAG promotion listener (Batch C item 6, issue #9).
//!
//! SUBSCRIBEs to the `ff:dag:completions` channel on a dedicated RESP3
//! client and dispatches `resolve_dependency` for each received
//! completion. This turns the `dependency_reconciler` scanner into a
//! pure safety net — user-perceived DAG latency drops from
//! `interval × levels` to `RTT × levels` under normal operation.
//!
//! # Design notes
//!
//! - **Dedicated client.** `SUBSCRIBE` puts a connection into pubsub
//!   mode; the client that serves `FCALL ff_resolve_dependency` must
//!   be a different handle. Built via `ClientBuilder::push_sender` +
//!   `ProtocolVersion::RESP3` (the RESP3 gate is enforced at build
//!   time — see ferriskey PR #38).
//!
//! - **Broadcast semantics in cluster mode.** Plain `PUBLISH` in
//!   Valkey cluster fans out to every node. Every ff-server instance
//!   that runs an engine will receive every completion. Duplicate
//!   dispatches are harmless — `ff_resolve_dependency` is idempotent
//!   — but redundant. If contention surfaces, a follow-up can move
//!   to sharded pubsub (`SPUBLISH`/`SSUBSCRIBE`) keyed by flow
//!   partition.
//!
//! - **Safety net.** Messages missed during listener restart, server
//!   disconnect, or the (rare) outage window are picked up by the
//!   `dependency_reconciler` scanner at its configured interval. The
//!   listener is an optimization, not a correctness dependency.

use std::sync::Arc;

use ferriskey::{Client, ClientBuilder, PushInfo, PushKind, Value};
use ferriskey::value::ProtocolVersion;
use ff_core::types::ExecutionId;
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::partition_router::{dispatch_dependency_resolution, PartitionRouter};

/// Channel name used by `ff_complete_execution` / `ff_fail_execution` /
/// `ff_cancel_execution` to notify the engine of terminal transitions.
pub const COMPLETION_CHANNEL: &str = "ff:dag:completions";

/// Wire format of a completion message. JSON-encoded by the Lua
/// producers; parsed with serde here so field drift surfaces as a
/// typed error rather than silent skip.
#[derive(Debug, Deserialize)]
struct CompletionPayload {
    execution_id: String,
    flow_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    outcome: String,
}

/// Build the dedicated RESP3 client the listener uses to receive push
/// frames. Takes the same address/auth config the dispatcher client was
/// built with; just adds the push_sender wiring and pins RESP3.
async fn build_listener_client(
    addresses: &[(String, u16)],
    tls: bool,
    cluster: bool,
    push_tx: mpsc::UnboundedSender<PushInfo>,
) -> Result<Client, ferriskey::value::Error> {
    let mut builder = ClientBuilder::new()
        .protocol(ProtocolVersion::RESP3)
        .push_sender(push_tx);
    if cluster {
        builder = builder.cluster();
    }
    if tls {
        builder = builder.tls_insecure();
    }
    for (host, port) in addresses {
        builder = builder.host(host, *port);
    }
    builder.build().await
}

/// Spawn the completion listener. Returns a `JoinHandle` that resolves
/// when the shutdown watch fires. On transient errors (connect failure,
/// broken pubsub channel) the listener sleeps 1s and retries.
///
/// `addresses` / `tls` / `cluster` mirror the dispatcher client's
/// connection config so the listener reaches the same deployment.
pub fn spawn_completion_listener(
    router: Arc<PartitionRouter>,
    dispatch_client: Client,
    addresses: Vec<(String, u16)>,
    tls: bool,
    cluster: bool,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                return;
            }

            let (push_tx, mut push_rx) = mpsc::unbounded_channel();
            let listener = match build_listener_client(
                &addresses,
                tls,
                cluster,
                push_tx,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "completion_listener: failed to build listener client, retrying in 1s"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        _ = shutdown.changed() => return,
                    }
                    continue;
                }
            };

            // Issue SUBSCRIBE via the facade's command path. The pubsub
            // synchronizer intercepts, records the desired state, and
            // the reconciler issues the actual SUBSCRIBE in the
            // background. Push frames start arriving on `push_rx` once
            // the server confirms.
            let sub_result: Result<(), _> = listener
                .cmd("SUBSCRIBE")
                .arg(COMPLETION_CHANNEL)
                .execute()
                .await;
            if let Err(e) = sub_result {
                tracing::warn!(
                    error = %e,
                    "completion_listener: SUBSCRIBE failed, retrying in 1s"
                );
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                    _ = shutdown.changed() => return,
                }
                continue;
            }

            tracing::info!(
                channel = COMPLETION_CHANNEL,
                "completion_listener: subscribed, awaiting push frames"
            );

            // Drain the push channel until either the connection drops
            // (push_rx closes) or shutdown fires.
            loop {
                tokio::select! {
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return;
                        }
                    }
                    msg = push_rx.recv() => {
                        let Some(push) = msg else {
                            tracing::warn!(
                                "completion_listener: push channel closed, reconnecting"
                            );
                            break;
                        };
                        handle_push(&router, &dispatch_client, push).await;
                    }
                }
            }
        }
    })
}

/// Handle one push frame. Ignores non-Message kinds (Subscribe /
/// Unsubscribe confirmations, keyspace notifications we didn't ask for).
/// Malformed payloads are logged and dropped — safety-net reconciler
/// will still pick the completion up.
async fn handle_push(
    router: &PartitionRouter,
    dispatch_client: &Client,
    push: PushInfo,
) {
    if push.kind != PushKind::Message {
        return;
    }

    // RESP3 Message frame layout (per Valkey docs):
    //   data = [ channel, message_payload ]
    let payload_str = match push.data.get(1) {
        Some(Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
        Some(Value::SimpleString(s)) => s.clone(),
        _ => {
            tracing::warn!(
                data = ?push.data,
                "completion_listener: malformed Message frame, skipping"
            );
            return;
        }
    };

    let parsed: CompletionPayload = match serde_json::from_str(&payload_str) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                error = %e,
                payload = %payload_str,
                "completion_listener: cjson decode failed, skipping"
            );
            return;
        }
    };

    let eid = match ExecutionId::parse(&parsed.execution_id) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = %e,
                raw = %parsed.execution_id,
                "completion_listener: invalid execution_id in payload"
            );
            return;
        }
    };

    tracing::debug!(
        execution_id = %eid,
        flow_id = %parsed.flow_id,
        outcome = %parsed.outcome,
        "completion_listener: dispatching dependency resolution"
    );

    dispatch_dependency_resolution(
        dispatch_client,
        router,
        &eid,
        Some(parsed.flow_id.as_str()),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_payload() {
        let raw = r#"{"execution_id":"{fp:5}:11111111-1111-1111-1111-111111111111","flow_id":"22222222-2222-2222-2222-222222222222","outcome":"success"}"#;
        let p: CompletionPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(p.flow_id, "22222222-2222-2222-2222-222222222222");
        assert_eq!(p.outcome, "success");
    }

    #[test]
    fn missing_outcome_defaults_empty() {
        // defense against older-Lua producers that don't emit outcome yet
        let raw = r#"{"execution_id":"{fp:0}:11111111-1111-1111-1111-111111111111","flow_id":"22222222-2222-2222-2222-222222222222"}"#;
        let p: CompletionPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(p.outcome, "");
    }

    #[test]
    fn malformed_payload_is_error() {
        let raw = r#"{"not":"valid"}"#;
        assert!(serde_json::from_str::<CompletionPayload>(raw).is_err());
    }
}
