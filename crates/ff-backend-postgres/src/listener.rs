//! Shared LISTEN connection + in-memory stream notifier.
//!
//! **RFC-v0.7 Wave 4 (Q2 adjudication).** Parked stream tailers must
//! not hold a pg connection while blocked. This module owns ONE
//! long-lived `sqlx::postgres::PgListener` task per `PostgresBackend`
//! instance; consumers register interest in a channel via
//! [`StreamNotifier::subscribe`], park on a tokio `broadcast::Receiver`,
//! and wake on NOTIFY. When awoken the tailer acquires a pool
//! connection, runs its SELECT, and releases — so pg connection count
//! scales with *active* reads, not waiter count.
//!
//! ## Reconnect strategy
//!
//! `PgListener` auto-reconnects on its next `.recv()` after a
//! connection drop. Our task loop catches the returned error, logs it,
//! waits a short backoff (100 ms → 1 s exponential, capped), then
//! re-issues `LISTEN` for every live channel recorded in
//! [`StreamNotifier::channels`]. A `StreamEventId::Reconnect` signal
//! is broadcast on each channel so parked tailers re-read fresh state
//! via SELECT after the reconnect.
//!
//! ## PgBouncer compatibility
//!
//! LISTEN/NOTIFY require session-level pool mode. PgBouncer's
//! transaction-pool mode drops the LISTEN registration as soon as the
//! transaction ends, silently losing notifications. Operators MUST
//! configure session-pool (or direct) pooling.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

type ChannelMap = Arc<Mutex<HashMap<String, broadcast::Sender<StreamEventId>>>>;

/// Event broadcast to waiters when something changes on a channel.
#[derive(Clone, Debug)]
pub enum StreamEventId {
    /// NOTIFY payload parsed as `(ts_ms, seq)`.
    Frame { ts_ms: i64, seq: i32 },
    /// The listener reconnected — waiters should re-SELECT.
    Reconnect,
}

/// Shared LISTEN router. One instance per `PostgresBackend`.
pub struct StreamNotifier {
    channels: ChannelMap,
    command_tx: mpsc::UnboundedSender<NotifierCommand>,
    _listener_task: JoinHandle<()>,
}

enum NotifierCommand {
    Listen(String, tokio::sync::oneshot::Sender<()>),
}

impl StreamNotifier {
    /// Spawn the listener task against `pool`. The task owns a
    /// dedicated connection via `PgListener::connect_with`; it does
    /// NOT hold a pool slot once up.
    pub fn spawn(pool: PgPool) -> Arc<Self> {
        let channels: ChannelMap = Arc::new(Mutex::new(HashMap::new()));
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let task_channels = channels.clone();
        let listener_task = tokio::spawn(async move {
            run_listener(pool, task_channels, command_rx).await;
        });
        Arc::new(Self {
            channels,
            command_tx,
            _listener_task: listener_task,
        })
    }

    /// Non-functional constructor retained for Wave-0 API parity.
    /// Production paths use [`Self::spawn`].
    pub fn placeholder() -> Arc<Self> {
        let channels: ChannelMap = Arc::new(Mutex::new(HashMap::new()));
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let listener_task = tokio::spawn(async move {});
        Arc::new(Self {
            channels,
            command_tx,
            _listener_task: listener_task,
        })
    }

    /// Subscribe to `channel`. First subscription triggers a LISTEN
    /// on the shared connection.
    /// Subscribe and wait for the LISTEN to be registered pg-side.
    /// Waiting for the ack is essential: otherwise a NOTIFY that
    /// fires between `subscribe()` returning and the task issuing
    /// LISTEN is lost.
    pub async fn subscribe(&self, channel: &str) -> broadcast::Receiver<StreamEventId> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (needs_listen, sender) = {
            let mut guard = self.channels.lock().expect("stream notifier mutex");
            if let Some(existing) = guard.get(channel) {
                (false, existing.clone())
            } else {
                let (tx, _rx) = broadcast::channel(64);
                guard.insert(channel.to_owned(), tx.clone());
                (true, tx)
            }
        };
        if needs_listen {
            let _ = self
                .command_tx
                .send(NotifierCommand::Listen(channel.to_owned(), ack_tx));
            let _ = ack_rx.await;
        }
        sender.subscribe()
    }

    /// Live channel count — observability / tests.
    pub fn active_channel_count(&self) -> usize {
        self.channels.lock().expect("stream notifier mutex").len()
    }
}

async fn run_listener(
    pool: PgPool,
    channels: ChannelMap,
    mut command_rx: mpsc::UnboundedReceiver<NotifierCommand>,
) {
    let mut backoff_ms: u64 = 100;
    loop {
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %e, "stream notifier: connect failed, backing off");
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(1_000);
                continue;
            }
        };
        backoff_ms = 100;

        // Re-subscribe every currently-live channel.
        let snapshot: Vec<(String, broadcast::Sender<StreamEventId>)> = {
            let guard = channels.lock().expect("stream notifier mutex");
            guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        for (ch, _) in &snapshot {
            if let Err(e) = listener.listen(ch).await {
                warn!(channel = %ch, error = %e, "LISTEN failed during re-subscribe");
            }
        }
        for (_, sender) in &snapshot {
            let _ = sender.send(StreamEventId::Reconnect);
        }

        loop {
            tokio::select! {
                maybe_cmd = command_rx.recv() => {
                    match maybe_cmd {
                        Some(NotifierCommand::Listen(ch, ack)) => {
                            if let Err(e) = listener.listen(&ch).await {
                                warn!(channel = %ch, error = %e, "LISTEN failed");
                                // Still ack — caller drops and proceeds.
                                let _ = ack.send(());
                                break;
                            }
                            let _ = ack.send(());
                        }
                        None => return,
                    }
                }
                notify = listener.recv() => {
                    match notify {
                        Ok(n) => {
                            let channel = n.channel().to_owned();
                            let event = parse_payload(n.payload());
                            let sender_opt = channels
                                .lock()
                                .expect("stream notifier mutex")
                                .get(&channel)
                                .cloned();
                            if let Some(sender) = sender_opt {
                                let _ = sender.send(event);
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "PgListener recv error; reconnecting");
                            break;
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
    }
}

fn parse_payload(raw: &str) -> StreamEventId {
    if let Some((ms, sq)) = raw.split_once('-')
        && let (Ok(ts), Ok(seq)) = (ms.parse::<i64>(), sq.parse::<i32>())
    {
        debug!(ts_ms = ts, seq, "stream notify");
        return StreamEventId::Frame { ts_ms: ts, seq };
    }
    StreamEventId::Reconnect
}

/// Channel-name helper — mirrors the trigger in `0001_initial.sql`.
pub fn channel_name(execution_uuid: &uuid::Uuid, attempt_index: u32) -> String {
    format!("ff_stream_{execution_uuid}_{attempt_index}")
}
