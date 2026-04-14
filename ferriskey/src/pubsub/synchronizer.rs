// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

//! Event-driven PubSub synchronizer.
//!
//! Replaces the polling-based synchronizer with an event-driven model:
//! - Single source of truth: `desired` subscriptions
//! - `confirmed` state derived from server push messages
//! - All reconciliation is event-triggered, no polling interval

use crate::client::{ClientWrapper, PubSubCommandApplier};
use crate::cluster::routing::{Routable, SingleNodeRoutingInfo};
use crate::cluster::slotmap::SlotMap;
use crate::cmd::{self, Cmd};
use crate::connection::info::{
    PubSubChannelOrPattern, PubSubSubscriptionInfo, PubSubSubscriptionKind,
};
use crate::pubsub::synchronizer_trait::PubSubSynchronizer;
use crate::value::{ErrorKind, Error, Result, Value};
use async_trait::async_trait;
use logger_core::{log_debug, log_error};
use once_cell::sync::OnceCell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};
use telemetrylib::FerrisKeyOtel;
use tokio::sync::{mpsc, Notify, RwLock as TokioRwLock};

/// Subscription kinds for cluster mode
const CLUSTER_KINDS: &[PubSubSubscriptionKind] = &[
    PubSubSubscriptionKind::Exact,
    PubSubSubscriptionKind::Pattern,
    PubSubSubscriptionKind::Sharded,
];

/// Subscription kinds for standalone mode
const STANDALONE_KINDS: &[PubSubSubscriptionKind] = &[
    PubSubSubscriptionKind::Exact,
    PubSubSubscriptionKind::Pattern,
];

/// Events that drive synchronizer state changes.
enum SyncEvent {
    /// User changed desired subscriptions — reconcile immediately
    DesiredChanged,
    /// Topology changed — pre-computed migrations to unsubscribe + reconcile
    TopologyChanged {
        migrations: Vec<(String, PubSubSubscriptionKind, HashSet<PubSubChannelOrPattern>)>,
        gone_subs: Vec<(String, PubSubSubscriptionKind, HashSet<PubSubChannelOrPattern>)>,
    },
    /// Node(s) disconnected — clear confirmations and reconcile
    NodeDisconnected { addresses: HashSet<String> },
}

/// Confirmed subscriptions tracked per node address.
#[derive(Default)]
struct ConfirmedState {
    by_address: HashMap<String, PubSubSubscriptionInfo>,
}

impl ConfirmedState {
    /// Aggregate all confirmed subscriptions across addresses into a flat map.
    fn aggregate(&self) -> PubSubSubscriptionInfo {
        let mut result = PubSubSubscriptionInfo::new();
        for subs in self.by_address.values() {
            for (kind, channels) in subs {
                result.entry(*kind).or_default().extend(channels.clone());
            }
        }
        result
    }

    fn add(&mut self, kind: PubSubSubscriptionKind, channel: Vec<u8>, address: String) {
        self.by_address
            .entry(address)
            .or_default()
            .entry(kind)
            .or_default()
            .insert(channel);
    }

    fn remove_exact(&mut self, kind: PubSubSubscriptionKind, channel: &[u8], address: &str) {
        if kind == PubSubSubscriptionKind::Sharded {
            // Sharded: only remove from the specific address
            if let Some(addr_subs) = self.by_address.get_mut(address)
                && let Some(channels) = addr_subs.get_mut(&kind)
            {
                channels.remove(channel);
            }
        } else {
            // Exact/Pattern: remove from ALL addresses (server unsubscribe is authoritative)
            for addr_subs in self.by_address.values_mut() {
                if let Some(channels) = addr_subs.get_mut(&kind) {
                    channels.remove(channel);
                }
            }
        }
        self.gc();
    }

    fn clear_addresses(&mut self, addresses: &HashSet<String>) {
        for addr in addresses {
            self.by_address.remove(addr);
        }
    }

    /// Remove empty entries
    fn gc(&mut self) {
        self.by_address.retain(|_, subs| {
            subs.retain(|_, channels| !channels.is_empty());
            !subs.is_empty()
        });
    }
}

/// Event-driven PubSub synchronizer.
pub struct EventDrivenSynchronizer {
    internal_client: OnceCell<Weak<TokioRwLock<ClientWrapper>>>,
    is_cluster: bool,

    /// Single source of truth: what the user wants
    desired: RwLock<PubSubSubscriptionInfo>,

    /// Confirmed by server push messages
    confirmed: RwLock<ConfirmedState>,

    /// Event channel
    events_tx: mpsc::UnboundedSender<SyncEvent>,

    /// Background task handle
    task_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// Notified when confirmed == desired (for wait_for_sync)
    sync_notify: Notify,

    /// Notified after each reconciliation cycle completes
    reconcile_complete_notify: Notify,

    request_timeout: Duration,

    /// Tracks whether on_topology_changed is currently executing.
    /// Prevents backoff tasks from spawning for our own UNSUBSCRIBE command responses,
    /// which would create compounding retry loops.
    in_topology_change: std::sync::atomic::AtomicBool,
}

impl EventDrivenSynchronizer {
    pub fn new(
        initial_subscriptions: Option<PubSubSubscriptionInfo>,
        is_cluster: bool,
        _reconciliation_interval: Option<Duration>,
        request_timeout: Duration,
    ) -> Arc<Self> {
        let (events_tx, events_rx) = mpsc::unbounded_channel();

        let sync = Arc::new(Self {
            internal_client: OnceCell::new(),
            is_cluster,
            desired: RwLock::new(initial_subscriptions.unwrap_or_default()),
            confirmed: RwLock::new(ConfirmedState::default()),
            events_tx,
            task_handle: Mutex::new(None),
            sync_notify: Notify::new(),
            reconcile_complete_notify: Notify::new(),
            request_timeout,
            in_topology_change: std::sync::atomic::AtomicBool::new(false),
        });

        sync.start_event_loop(events_rx);
        sync
    }

    pub fn set_internal_client(&self, client: Weak<TokioRwLock<ClientWrapper>>) {
        let _ = self.internal_client.set(client);
    }

    /// Returns a snapshot of confirmed subscriptions keyed by node address.
    /// Used by test utilities to inspect synchronizer state without polling.
    pub fn get_current_subscriptions_by_address(&self) -> HashMap<String, PubSubSubscriptionInfo> {
        self.confirmed.read().unwrap().by_address.clone()
    }

    #[inline]
    fn kinds(&self) -> &'static [PubSubSubscriptionKind] {
        if self.is_cluster {
            CLUSTER_KINDS
        } else {
            STANDALONE_KINDS
        }
    }

    fn send_event(&self, event: SyncEvent) {
        let _ = self.events_tx.send(event);
    }

    /// Check if confirmed state matches desired and notify waiters if so.
    fn check_sync_and_notify(&self) {
        let desired = self.desired.read().unwrap_or_else(|e| e.into_inner());
        let confirmed = self.confirmed.read().unwrap_or_else(|e| e.into_inner());
        let actual = confirmed.aggregate();

        let is_synced = self.kinds().iter().all(|kind| {
            let d = desired.get(kind).map(|s| s.len()).unwrap_or(0);
            let a = actual.get(kind).map(|s| s.len()).unwrap_or(0);
            if d != a {
                return false;
            }
            match (desired.get(kind), actual.get(kind)) {
                (Some(d_set), Some(a_set)) => d_set == a_set,
                (None, None) => true,
                (Some(d_set), None) => d_set.is_empty(),
                (None, Some(a_set)) => a_set.is_empty(),
            }
        });

        if is_synced {
            let _ = FerrisKeyOtel::update_subscription_last_sync_timestamp();
            self.sync_notify.notify_waiters();
        } else {
            let _ = FerrisKeyOtel::record_subscription_out_of_sync();
        }
    }

    fn start_event_loop(self: &Arc<Self>, mut events_rx: mpsc::UnboundedReceiver<SyncEvent>) {
        let sync_weak = Arc::downgrade(self);

        let handle = tokio::spawn(async move {
            loop {
                let event = events_rx.recv().await;
                let Some(sync) = sync_weak.upgrade() else {
                    break; // synchronizer dropped
                };

                let Some(event) = event else {
                    break; // channel closed
                };

                match event {
                    SyncEvent::DesiredChanged => {
                        // Drain any additional DesiredChanged events (coalesce).
                        // Collect non-matching events to re-queue after drain.
                        let mut deferred = Vec::new();
                        while let Ok(evt) = events_rx.try_recv() {
                            match evt {
                                SyncEvent::DesiredChanged => {} // coalesce
                                other => { deferred.push(other); break; }
                            }
                        }
                        for evt in deferred { let _ = sync.events_tx.send(evt); }
                        if let Err(e) = sync.reconcile().await {
                            log_error("pubsub_sync", format!("Reconcile failed: {e:?}"));
                        }
                    }
                    SyncEvent::TopologyChanged { migrations, gone_subs } => {
                        // Drain additional TopologyChanged events, MERGING their data.
                        // Do NOT replace — a subsequent refresh with empty gone_subs would
                        // discard the important gone/migration data from the first event.
                        // Collect non-matching events to re-queue after drain (NOT during,
                        // which would create an infinite loop).
                        let mut latest_mig = migrations;
                        let mut latest_gone = gone_subs;
                        let mut deferred = Vec::new();
                        while let Ok(evt) = events_rx.try_recv() {
                            match evt {
                                SyncEvent::TopologyChanged { migrations, gone_subs } => {
                                    latest_mig.extend(migrations);
                                    latest_gone.extend(gone_subs);
                                }
                                other => { deferred.push(other); break; }
                            }
                        }
                        for evt in deferred { let _ = sync.events_tx.send(evt); }
                        sync.on_topology_changed(latest_mig, latest_gone).await;
                    }
                    SyncEvent::NodeDisconnected { addresses } => {
                        sync.on_node_disconnected(&addresses).await;
                    }
                }

                sync.check_sync_and_notify();
                sync.reconcile_complete_notify.notify_waiters();
            }
        });

        *self
            .task_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Compute diff between desired and confirmed, send subscribe/unsubscribe commands.
    async fn reconcile(&self) -> Result<()> {
        let desired = self
            .desired
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let actual = self
            .confirmed
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .aggregate();

        // Subscribe: in desired but not confirmed
        for kind in self.kinds() {
            let desired_channels = desired.get(kind);
            let actual_channels = actual.get(kind);

            let to_sub: HashSet<_> = desired_channels
                .iter()
                .flat_map(|d| d.iter())
                .filter(|ch| actual_channels.as_ref().is_none_or(|a| !a.contains(*ch)))
                .cloned()
                .collect();

            if !to_sub.is_empty() {
                self.send_subscription_cmd(to_sub, *kind, true, None)
                    .await;
            }
        }

        // Unsubscribe: in confirmed but not desired (grouped by address)
        // Collect under lock, then send commands outside lock
        let unsub_work: Vec<(String, PubSubSubscriptionKind, HashSet<PubSubChannelOrPattern>)> = {
            let confirmed = self
                .confirmed
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let mut work = Vec::new();
            for (addr, addr_subs) in &confirmed.by_address {
                for (kind, channels) in addr_subs {
                    let desired_for_kind = desired.get(kind);
                    let to_unsub: HashSet<_> = channels
                        .iter()
                        .filter(|ch| desired_for_kind.is_none_or(|d| !d.contains(*ch)))
                        .cloned()
                        .collect();
                    if !to_unsub.is_empty() {
                        work.push((addr.clone(), *kind, to_unsub));
                    }
                }
            }
            work
        };

        for (addr, kind, to_unsub) in unsub_work {
            let routing = parse_address_routing(&addr).ok();
            if kind == PubSubSubscriptionKind::Sharded {
                self.send_sharded_unsubscribe_by_slot(to_unsub, routing)
                    .await;
            } else {
                self.send_subscription_cmd(to_unsub, kind, false, routing)
                    .await;
            }
        }

        Ok(())
    }

    /// Handle pre-computed topology migrations. Confirmed state is already
    /// updated synchronously in `handle_topology_refresh()` — this method
    /// just sends the unsubscribe commands and reconciles.
    async fn on_topology_changed(
        &self,
        migrations: Vec<(String, PubSubSubscriptionKind, HashSet<PubSubChannelOrPattern>)>,
        gone_subs: Vec<(String, PubSubSubscriptionKind, HashSet<PubSubChannelOrPattern>)>,
    ) {
        if migrations.is_empty() && gone_subs.is_empty() {
            return;
        }

        // Suppress backoff-task spawning during this handler so UNSUBSCRIBE echo responses
        // don't spawn additional retry loops on top of the ones already running.
        self.in_topology_change.store(true, std::sync::atomic::Ordering::Release);

        // Step 1: Unsubscribe from old owners FIRST
        for (addr, kind, channels) in migrations.iter().chain(gone_subs.iter()) {
            let routing = parse_address_routing(addr).ok();
            if *kind == PubSubSubscriptionKind::Sharded {
                self.send_sharded_unsubscribe_by_slot(channels.clone(), routing)
                    .await;
            } else {
                self.send_subscription_cmd(channels.clone(), *kind, false, routing)
                    .await;
            }
        }

        self.in_topology_change.store(false, std::sync::atomic::Ordering::Release);

        // Step 2: Resubscribe to new owners via reconcile
        if let Err(e) = self.reconcile().await {
            log_error("pubsub_sync", format!("Post-topology reconcile failed: {e:?}"));
        }

        // No explicit retry needed — periodic topology refreshes call handle_topology_refresh,
        // which checks desired != confirmed and sends DesiredChanged if they diverge.
    }

    async fn on_node_disconnected(&self, addresses: &HashSet<String>) {
        if addresses.is_empty() {
            return;
        }
        log_debug(
            "pubsub_sync",
            format!("Clearing confirmations for disconnected: {addresses:?}"),
        );
        {
            let mut confirmed = self.confirmed.write().unwrap_or_else(|e| e.into_inner());
            confirmed.clear_addresses(addresses);
        }
        if let Err(e) = self.reconcile().await {
            log_error("pubsub_sync", format!("Post-disconnect reconcile failed: {e:?}"));
        }
    }

    async fn send_subscription_cmd(
        &self,
        channels: HashSet<PubSubChannelOrPattern>,
        kind: PubSubSubscriptionKind,
        is_subscribe: bool,
        routing: Option<SingleNodeRoutingInfo>,
    ) {
        if channels.is_empty() {
            return;
        }

        let cmd_name = match (kind, is_subscribe) {
            (PubSubSubscriptionKind::Exact, true) => "SUBSCRIBE",
            (PubSubSubscriptionKind::Exact, false) => "UNSUBSCRIBE",
            (PubSubSubscriptionKind::Pattern, true) => "PSUBSCRIBE",
            (PubSubSubscriptionKind::Pattern, false) => "PUNSUBSCRIBE",
            (PubSubSubscriptionKind::Sharded, true) => "SSUBSCRIBE",
            (PubSubSubscriptionKind::Sharded, false) => "SUNSUBSCRIBE",
        };

        let mut command = cmd::cmd(cmd_name);
        for channel in &channels {
            command.arg(channel.as_slice());
        }
        if kind == PubSubSubscriptionKind::Sharded && !is_subscribe {
            command.set_fenced(true);
        }

        match self.apply_pubsub(&mut command, routing).await {
            Ok(_) => {}
            Err(e) => {
                let action = if is_subscribe { "subscribe" } else { "unsubscribe" };
                log_error(
                    "pubsub_sync",
                    format!("Failed to {action} {kind:?}: {e:?}"),
                );
            }
        }
    }

    async fn send_sharded_unsubscribe_by_slot(
        &self,
        channels: HashSet<PubSubChannelOrPattern>,
        routing: Option<SingleNodeRoutingInfo>,
    ) {
        // Group by slot so each SUNSUBSCRIBE goes to the right node
        let by_slot: HashMap<u16, HashSet<_>> =
            channels.into_iter().fold(HashMap::new(), |mut acc, ch| {
                let slot = crate::cluster::topology::get_slot(&ch);
                acc.entry(slot).or_default().insert(ch);
                acc
            });

        for (_, slot_channels) in by_slot {
            self.send_subscription_cmd(
                slot_channels,
                PubSubSubscriptionKind::Sharded,
                false,
                routing.clone(),
            )
            .await;
        }
    }

    async fn apply_pubsub(
        &self,
        cmd: &mut Cmd,
        routing: Option<SingleNodeRoutingInfo>,
    ) -> Result<Value> {
        let client_arc = self
            .internal_client
            .get()
            .ok_or_else(|| {
                Error::from((
                    ErrorKind::ClientError,
                    "Internal client not set in synchronizer",
                ))
            })?
            .upgrade()
            .ok_or_else(|| {
                Error::from((ErrorKind::ClientError, "Internal client has been dropped"))
            })?;

        let mut client_wrapper = {
            let guard = client_arc.read().await;
            guard.clone()
        };

        client_wrapper.apply_pubsub_command(cmd, routing).await
    }

    // --- Command interception helpers ---

    fn extract_channels(cmd: &Cmd) -> Vec<PubSubChannelOrPattern> {
        cmd.args_iter()
            .skip(1)
            .filter_map(|arg| match arg {
                cmd::Arg::Simple(bytes) => Some(bytes.to_vec()),
                cmd::Arg::Cursor => None,
            })
            .collect()
    }

    /// Parse a blocking subscribe/unsubscribe command into (channels, timeout_ms).
    ///
    /// Protocol convention for `*_BLOCKING` commands: all arguments except the
    /// last are channel names; the last argument is the timeout in milliseconds.
    /// If the last argument is not a valid u64 it is treated as a channel name
    /// with timeout 0 (no timeout).
    fn extract_channels_and_timeout(cmd: &Cmd) -> (Vec<PubSubChannelOrPattern>, u64) {
        let args: Vec<_> = cmd
            .args_iter()
            .skip(1)
            .filter_map(|arg| match arg {
                cmd::Arg::Simple(bytes) => Some(bytes.to_vec()),
                cmd::Arg::Cursor => None,
            })
            .collect();

        if args.is_empty() {
            return (Vec::new(), 0);
        }

        // Try to parse the last argument as a timeout. If it parses as u64
        // AND there are other arguments, use it as the timeout. When there is
        // only a single argument that happens to look numeric (e.g. a channel
        // named "42"), treat it as a channel with timeout 0 to avoid silently
        // discarding subscriptions.
        let last_is_timeout = args.len() > 1
            && args
                .last()
                .and_then(|arg| String::from_utf8_lossy(arg).parse::<u64>().ok())
                .is_some();

        if last_is_timeout {
            let timeout_ms = String::from_utf8_lossy(args.last().unwrap())
                .parse::<u64>()
                .unwrap_or(0);
            let channels = args[..args.len() - 1].to_vec();
            (channels, timeout_ms)
        } else {
            // Single arg or last arg non-numeric: all args are channels, no timeout
            (args, 0)
        }
    }

    fn handle_lazy(
        &self,
        cmd: &Cmd,
        kind: PubSubSubscriptionKind,
        is_subscribe: bool,
    ) -> Result<Value> {
        let channels = Self::extract_channels(cmd);

        if is_subscribe && channels.is_empty() {
            return Err(Error::from((
                ErrorKind::ClientError,
                "No channels provided for subscription",
            )));
        }

        let channels_set = if channels.is_empty() {
            None
        } else {
            Some(channels.into_iter().collect())
        };

        if is_subscribe {
            self.add_desired_subscriptions(channels_set.unwrap(), kind);
        } else {
            self.remove_desired_subscriptions(channels_set, kind);
        }

        Ok(Value::Nil)
    }

    async fn handle_blocking(
        &self,
        cmd: &Cmd,
        kind: PubSubSubscriptionKind,
        is_subscribe: bool,
    ) -> Result<Value> {
        let (channels, timeout_ms) = Self::extract_channels_and_timeout(cmd);

        if is_subscribe && channels.is_empty() {
            return Err(Error::from((
                ErrorKind::ClientError,
                "No channels provided for subscription",
            )));
        }

        let channels_set: HashSet<PubSubChannelOrPattern> = channels.into_iter().collect();

        if is_subscribe {
            self.add_desired_subscriptions(channels_set.clone(), kind);
        } else {
            let to_remove = if channels_set.is_empty() {
                None
            } else {
                Some(channels_set.clone())
            };
            self.remove_desired_subscriptions(to_remove, kind);
        }

        let (expected_channels, expected_patterns, expected_sharded) = match kind {
            PubSubSubscriptionKind::Exact => (Some(channels_set), None, None),
            PubSubSubscriptionKind::Pattern => (None, Some(channels_set), None),
            PubSubSubscriptionKind::Sharded => (None, None, Some(channels_set)),
        };

        self.wait_for_sync(timeout_ms, expected_channels, expected_patterns, expected_sharded)
            .await?;

        Ok(Value::Nil)
    }

    fn get_subscriptions_value(&self) -> Value {
        let (desired, actual) = self.get_subscription_state();

        Value::Array(vec![
            Ok(Value::BulkString(bytes::Bytes::from_static(b"desired"))),
            Ok(sub_map_to_value(desired)),
            Ok(Value::BulkString(bytes::Bytes::from_static(b"actual"))),
            Ok(sub_map_to_value(actual)),
        ])
    }

    async fn run_with_timeout<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Result<T> + Send,
        T: Send,
    {
        match tokio::time::timeout(self.request_timeout, async move { f() }).await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into()),
        }
    }

}

impl Drop for EventDrivenSynchronizer {
    fn drop(&mut self) {
        if let Some(handle) = self
            .task_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
    }
}

#[async_trait]
impl PubSubSynchronizer for EventDrivenSynchronizer {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn add_desired_subscriptions(
        &self,
        channels: HashSet<PubSubChannelOrPattern>,
        subscription_type: PubSubSubscriptionKind,
    ) {
        {
            let mut desired = self.desired.write().unwrap_or_else(|e| e.into_inner());
            desired.entry(subscription_type).or_default().extend(channels);
        }
        self.send_event(SyncEvent::DesiredChanged);
    }

    fn remove_desired_subscriptions(
        &self,
        channels: Option<HashSet<PubSubChannelOrPattern>>,
        subscription_type: PubSubSubscriptionKind,
    ) {
        {
            let mut desired = self.desired.write().unwrap_or_else(|e| e.into_inner());
            match channels {
                Some(to_remove) => {
                    if let Some(existing) = desired.get_mut(&subscription_type) {
                        for ch in to_remove {
                            existing.remove(&ch);
                        }
                    }
                }
                None => {
                    desired.remove(&subscription_type);
                }
            }
        }
        self.send_event(SyncEvent::DesiredChanged);
    }

    fn add_current_subscriptions(
        &self,
        channels: HashSet<PubSubChannelOrPattern>,
        subscription_type: PubSubSubscriptionKind,
        address: String,
    ) {
        let mut confirmed = self.confirmed.write().unwrap_or_else(|e| e.into_inner());
        for channel in channels {
            confirmed.add(subscription_type, channel, address.clone());
        }
        drop(confirmed);
        self.check_sync_and_notify();
    }

    fn remove_current_subscriptions(
        &self,
        channels: HashSet<PubSubChannelOrPattern>,
        subscription_type: PubSubSubscriptionKind,
        address: String,
    ) {
        let mut confirmed = self.confirmed.write().unwrap_or_else(|e| e.into_inner());
        for channel in &channels {
            confirmed.remove_exact(subscription_type, channel, &address);
        }
        drop(confirmed);
        self.check_sync_and_notify();
        // Trigger reconcile with exponential backoff + jitter.
        // When a server-initiated SUnsubscribe clears our subscription, we need to
        // resubscribe on the new slot owner. We retry with backoff so that:
        // - First attempt is fast (200ms) → quick resubscription after slot migration
        // - Subsequent attempts back off (400ms, 800ms, 1600ms, 2000ms max) → if the
        //   cluster is settling after failover, we naturally reduce retry frequency
        //   instead of hammering at 5 retries/second.
        let tx = self.events_tx.clone();
        let desired = self.desired.read().unwrap_or_else(|e| e.into_inner()).clone();
        // Only schedule retries if we actually want this subscription type
        let still_desired = desired.get(&subscription_type)
            .is_some_and(|channels_set| channels.iter().any(|ch| channels_set.contains(ch)));
        // Don't spawn a backoff task if we're inside on_topology_changed — those UNSUBSCRIBE
        // responses are our own commands and should not trigger additional retry loops.
        let in_change = self.in_topology_change.load(std::sync::atomic::Ordering::Acquire);
        if still_desired && !in_change {
            tokio::spawn(async move {
                let mut delay_ms = 200u64;
                const MAX_DELAY_MS: u64 = 2000;
                const MAX_ATTEMPTS: u32 = 8;
                for _ in 0..MAX_ATTEMPTS {
                    // Jitter: 80%-120% of base delay
                    let jitter_range = delay_ms / 5; // 20% jitter
                    let jitter = if jitter_range > 0 {
                        (rand::random::<u64>() % (2 * jitter_range)).saturating_sub(jitter_range)
                    } else { 0 };
                    let actual_delay = Duration::from_millis(delay_ms.saturating_add(jitter));
                    tokio::time::sleep(actual_delay).await;
                    let _ = tx.send(SyncEvent::DesiredChanged);
                    delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
                }
            });
        }
    }

    fn get_subscription_state(
        &self,
    ) -> (PubSubSubscriptionInfo, PubSubSubscriptionInfo) {
        let desired = self
            .desired
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let actual = self
            .confirmed
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .aggregate();
        (desired, actual)
    }

    fn trigger_reconciliation(&self) {
        self.send_event(SyncEvent::DesiredChanged);
    }

    fn remove_current_subscriptions_for_addresses(&self, addresses: &HashSet<String>) {
        if !addresses.is_empty() {
            self.send_event(SyncEvent::NodeDisconnected {
                addresses: addresses.clone(),
            });
        }
    }

    fn handle_topology_refresh(&self, new_slot_map: &SlotMap) {
        // SlotMap doesn't implement Clone — extract the data we need
        let new_addresses: HashSet<String> = new_slot_map
            .all_node_addresses()
            .iter()
            .map(|arc| arc.to_string())
            .collect();

        // Compute migrations synchronously (trait method is sync)
        let migrations: Vec<(String, PubSubSubscriptionKind, HashSet<PubSubChannelOrPattern>)>;
        let gone_subs: Vec<(String, PubSubSubscriptionKind, HashSet<PubSubChannelOrPattern>)>;
        let confirmed_keys: Vec<String>;
        {
            let confirmed = self.confirmed.read().unwrap_or_else(|e| e.into_inner());
            confirmed_keys = confirmed.by_address.keys().cloned().collect();
            let mut mig = Vec::new();
            let mut gone = Vec::new();

            for (addr, addr_subs) in &confirmed.by_address {
                if !new_addresses.contains(addr) {
                    for (kind, channels) in addr_subs {
                        if !channels.is_empty() {
                            gone.push((addr.clone(), *kind, channels.clone()));
                        }
                    }
                    continue;
                }

                for (kind, channels) in addr_subs {
                    let mut migrated = HashSet::new();
                    for channel in channels {
                        let slot = crate::cluster::topology::get_slot(channel);
                        if let Some(shard_addrs) = new_slot_map.shard_addrs_for_slot(slot) {
                            let needs_migration = match kind {
                                // Sharded subs must be on the primary (slot owner)
                                PubSubSubscriptionKind::Sharded => {
                                    shard_addrs.primary().as_str() != addr
                                }
                                // Exact/Pattern are broadcast — any shard member works
                                PubSubSubscriptionKind::Exact
                                | PubSubSubscriptionKind::Pattern => {
                                    !shard_addrs.is_member(addr)
                                }
                            };
                            if needs_migration {
                                migrated.insert(channel.clone());
                            }
                        } else {
                            migrated.insert(channel.clone());
                        }
                    }
                    if !migrated.is_empty() {
                        mig.push((addr.clone(), *kind, migrated));
                    }
                }
            }

            migrations = mig;
            gone_subs = gone;
        }

        log_debug(
            "pubsub_sync",
            format!(
                "handle_topology_refresh: confirmed_addrs={:?}, new_addrs count={}, migrations={}, gone={}",
                confirmed_keys, new_addresses.len(), migrations.len(), gone_subs.len()
            ),
        );

        // Even if no migrations/gone, check if desired != confirmed and nudge reconcile.
        // This makes every topology refresh a self-healing opportunity: if a prior
        // reconcile's subscribe commands didn't get confirmed (cluster was settling),
        // the next refresh will retry — no fixed retry count needed.
        if migrations.is_empty() && gone_subs.is_empty() {
            let desired = self.desired.read().unwrap_or_else(|e| e.into_inner()).clone();
            let actual = self.confirmed.read().unwrap_or_else(|e| e.into_inner()).aggregate();
            if desired != actual {
                self.send_event(SyncEvent::DesiredChanged);
            }
            return;
        }

        // Update confirmed state synchronously
        {
            let mut confirmed = self.confirmed.write().unwrap_or_else(|e| e.into_inner());
            for (addr, _, _) in &gone_subs {
                confirmed.by_address.remove(addr);
            }
            for (addr, kind, channels) in &migrations {
                if let Some(addr_subs) = confirmed.by_address.get_mut(addr)
                    && let Some(existing) = addr_subs.get_mut(kind)
                {
                    for ch in channels {
                        existing.remove(ch);
                    }
                }
            }
            confirmed.gc();
        }

        // Send event with pre-computed data for async unsubscribe + reconcile
        self.send_event(SyncEvent::TopologyChanged {
            migrations,
            gone_subs,
        });
    }

    async fn intercept_pubsub_command(&self, cmd: &Cmd) -> Option<Result<Value>> {
        let command_name = cmd.command().unwrap_or_default();
        let command_str = std::str::from_utf8(&command_name).unwrap_or("");

        match command_str {
            "SUBSCRIBE" => {
                let cmd = cmd.clone();
                Some(
                    self.run_with_timeout(|| self.handle_lazy(&cmd, PubSubSubscriptionKind::Exact, true))
                        .await,
                )
            }
            "PSUBSCRIBE" => {
                let cmd = cmd.clone();
                Some(
                    self.run_with_timeout(|| self.handle_lazy(&cmd, PubSubSubscriptionKind::Pattern, true))
                        .await,
                )
            }
            "SSUBSCRIBE" => {
                let cmd = cmd.clone();
                Some(
                    self.run_with_timeout(|| self.handle_lazy(&cmd, PubSubSubscriptionKind::Sharded, true))
                        .await,
                )
            }
            "UNSUBSCRIBE" => {
                let cmd = cmd.clone();
                Some(
                    self.run_with_timeout(|| self.handle_lazy(&cmd, PubSubSubscriptionKind::Exact, false))
                        .await,
                )
            }
            "PUNSUBSCRIBE" => {
                let cmd = cmd.clone();
                Some(
                    self.run_with_timeout(|| self.handle_lazy(&cmd, PubSubSubscriptionKind::Pattern, false))
                        .await,
                )
            }
            "SUNSUBSCRIBE" => {
                let cmd = cmd.clone();
                Some(
                    self.run_with_timeout(|| self.handle_lazy(&cmd, PubSubSubscriptionKind::Sharded, false))
                        .await,
                )
            }
            "SUBSCRIBE_BLOCKING" => Some(
                self.handle_blocking(cmd, PubSubSubscriptionKind::Exact, true).await,
            ),
            "PSUBSCRIBE_BLOCKING" => Some(
                self.handle_blocking(cmd, PubSubSubscriptionKind::Pattern, true).await,
            ),
            "SSUBSCRIBE_BLOCKING" => Some(
                self.handle_blocking(cmd, PubSubSubscriptionKind::Sharded, true).await,
            ),
            "UNSUBSCRIBE_BLOCKING" => Some(
                self.handle_blocking(cmd, PubSubSubscriptionKind::Exact, false).await,
            ),
            "PUNSUBSCRIBE_BLOCKING" => Some(
                self.handle_blocking(cmd, PubSubSubscriptionKind::Pattern, false).await,
            ),
            "SUNSUBSCRIBE_BLOCKING" => Some(
                self.handle_blocking(cmd, PubSubSubscriptionKind::Sharded, false).await,
            ),
            "GET_SUBSCRIPTIONS" => Some(
                self.run_with_timeout(|| Ok(self.get_subscriptions_value())).await,
            ),
            _ => None,
        }
    }

    async fn wait_for_sync(
        &self,
        timeout_ms: u64,
        expected_channels: Option<HashSet<PubSubChannelOrPattern>>,
        expected_patterns: Option<HashSet<PubSubChannelOrPattern>>,
        expected_sharded: Option<HashSet<PubSubChannelOrPattern>>,
    ) -> Result<()> {
        let deadline = if timeout_ms > 0 {
            Some(Instant::now() + Duration::from_millis(timeout_ms))
        } else {
            None
        };

        loop {
            let notified = self.reconcile_complete_notify.notified();

            let condition_met = {
                if expected_channels.is_none()
                    && expected_patterns.is_none()
                    && expected_sharded.is_none()
                {
                    // Check overall sync
                    let desired = self.desired.read().unwrap_or_else(|e| e.into_inner());
                    let actual = self
                        .confirmed
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .aggregate();

                    self.kinds().iter().all(|kind| {
                        let d = desired.get(kind);
                        let a = actual.get(kind);
                        match (d, a) {
                            (Some(d_set), Some(a_set)) => d_set == a_set,
                            (None, None) => true,
                            (Some(d_set), None) => d_set.is_empty(),
                            (None, Some(a_set)) => a_set.is_empty(),
                        }
                    })
                } else {
                    let (desired, actual) = self.get_subscription_state();

                    let check = |channels: &Option<HashSet<PubSubChannelOrPattern>>,
                                 kind: PubSubSubscriptionKind|
                     -> bool {
                        channels.as_ref().is_none_or(|chs| {
                            let d = desired.get(&kind);
                            let a = actual.get(&kind);
                            if chs.is_empty() {
                                let d_empty = d.is_none_or(|s| s.is_empty());
                                let a_empty = a.is_none_or(|s| s.is_empty());
                                d_empty && a_empty
                            } else {
                                chs.iter().all(|ch| {
                                    let in_d = d.is_some_and(|s| s.contains(ch));
                                    let in_a = a.is_some_and(|s| s.contains(ch));
                                    in_d == in_a
                                })
                            }
                        })
                    };

                    check(&expected_channels, PubSubSubscriptionKind::Exact)
                        && check(&expected_patterns, PubSubSubscriptionKind::Pattern)
                        && check(&expected_sharded, PubSubSubscriptionKind::Sharded)
                }
            };

            if condition_met {
                self.check_sync_and_notify();
                return Ok(());
            }

            self.trigger_reconciliation();

            if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
                }
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(remaining) => {
                        return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
                    }
                }
            } else {
                notified.await;
            }
        }
    }
}

fn parse_address_routing(address: &str) -> Result<SingleNodeRoutingInfo> {
    let (host, port_str) = address.rsplit_once(':').ok_or_else(|| {
        Error::from((
            ErrorKind::ClientError,
            "Invalid address format",
            address.to_string(),
        ))
    })?;
    let port = port_str
        .parse()
        .map_err(|_| Error::from((ErrorKind::ClientError, "Invalid port")))?;
    Ok(SingleNodeRoutingInfo::ByAddress {
        host: host.to_string(),
        port,
    })
}

fn sub_map_to_value(map: PubSubSubscriptionInfo) -> Value {
    let entries: Vec<_> = map
        .into_iter()
        .map(|(kind, values)| {
            let key = match kind {
                PubSubSubscriptionKind::Exact => "Exact",
                PubSubSubscriptionKind::Pattern => "Pattern",
                PubSubSubscriptionKind::Sharded => "Sharded",
            };
            let values_array: Vec<Value> = values
                .into_iter()
                .map(|v| Value::BulkString(bytes::Bytes::from(v)))
                .collect();
            (
                Value::BulkString(bytes::Bytes::from(key.as_bytes().to_vec())),
                Value::Array(values_array.into_iter().map(Ok).collect()),
            )
        })
        .collect();
    Value::Map(entries)
}
