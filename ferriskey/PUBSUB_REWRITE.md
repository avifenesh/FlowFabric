# PubSub Synchronizer Rewrite Design

## Current State (1,109 lines)

### Architecture
The synchronizer (`src/pubsub/synchronizer.rs`) uses a **two-state diff + polling** model:

1. **`desired_subscriptions`** (`RwLock<PubSubSubscriptionInfo>`) — what the user wants
2. **`current_subscriptions_by_address`** (`RwLock<HashMap<String, PubSubSubscriptionInfo>>`) — what we're actually subscribed to, per node address
3. **`pending_unsubscribes`** (`RwLock<HashMap<String, PubSubSubscriptionInfo>>`) — queued unsubscribes from topology changes

A background task runs every 3 seconds (`reconciliation_interval`) and:
- Computes a `SyncDiff` (O(desired + current) set operations)
- Sends SUBSCRIBE/UNSUBSCRIBE/SSUBSCRIBE/SUNSUBSCRIBE commands for any diffs
- Processes pending unsubscribes from slot migrations

### Data Flow
```
User API (SUBSCRIBE cmd) → intercept_pubsub_command()
  → add to desired_subscriptions
  → trigger_reconciliation() (notify background task)
  → background task wakes, computes diff, sends commands

Push notifications (server confirms) → PushManager::try_send_raw()
  → add_current_subscriptions(channels, kind, address)

Topology change → handle_topology_refresh(new_slot_map)
  → detect migrated channels (slot owner changed)
  → queue pending_unsubscribes for old addresses
  → remove from current_subscriptions_by_address
  → trigger reconciliation (resubscribe on new owners)

Node disconnect → PipelineSink drop handler
  → remove_current_subscriptions_for_addresses()
  → trigger reconciliation (resubscribe)
```

### Integration Points (external API surface)
- `PubSubSynchronizer` trait (11 methods) in `synchronizer_trait.rs`
- Called from:
  - `client/mod.rs:872` — `intercept_pubsub_command()` (command interception)
  - `client/mod.rs:1979` — `trigger_reconciliation()` + `wait_for_sync()` (after client init)
  - `cluster/mod.rs:2770` — `handle_topology_refresh()` (after slot refresh)
  - `connection/multiplexed.rs:127` — `remove_current_subscriptions_for_addresses()` (on disconnect)
  - `pubsub/push_manager.rs:102` — `add_current_subscriptions()` / `remove_current_subscriptions()` (push confirmations)

### Problems
1. **Polling waste** — 3s interval means up to 3s latency for subscription changes, burns CPU checking when nothing changed
2. **Two-state complexity** — `desired` vs `current_by_address` diff is O(n) per reconciliation, complex to reason about
3. **Pending unsubscribes** — Third state (`pending_unsubscribes`) adds yet another data structure and ordering concern
4. **Address-level tracking** — `current_subscriptions_by_address` is correct for cluster topology but adds complexity for standalone
5. **Lock contention** — 5 separate `RwLock`s, all accessed during reconciliation

---

## Proposed Event-Driven Design (~450 lines)

### Core Principle
**Single source of truth**: only `desired_subscriptions` exists. Current state is derived from server push confirmations. No polling — all reconciliation is event-triggered.

### Data Structures

```rust
pub struct EventDrivenSynchronizer {
    internal_client: OnceCell<Weak<TokioRwLock<ClientWrapper>>>,
    is_cluster: bool,

    /// Single source of truth: what the user wants subscribed
    desired: RwLock<PubSubSubscriptionInfo>,

    /// Confirmed by server push messages (derived state, not authoritative)
    confirmed: RwLock<ConfirmedSubscriptions>,

    /// Event channel — replaces polling
    events_tx: mpsc::UnboundedSender<SyncEvent>,

    /// Background task handle
    task_handle: Mutex<Option<JoinHandle<()>>>,

    /// Completion notifier for blocking subscribe/wait_for_sync
    sync_notify: Notify,

    request_timeout: Duration,
}

/// Confirmed subscriptions, tracked per-address for cluster topology
struct ConfirmedSubscriptions {
    /// address → { kind → channels }
    by_address: HashMap<String, PubSubSubscriptionInfo>,
}

enum SyncEvent {
    /// User changed desired state — reconcile
    DesiredChanged,
    /// Server confirmed a subscription (from push message)
    Confirmed { kind: PubSubSubscriptionKind, channel: Vec<u8>, address: String },
    /// Server confirmed an unsubscription
    Unconfirmed { kind: PubSubSubscriptionKind, channel: Vec<u8>, address: String },
    /// Node reconnected — reapply all subscriptions for this node
    NodeReconnected { address: String },
    /// Topology changed — resubscribe migrated sharded channels
    TopologyChanged { new_slot_map: SlotMap },
    /// Node disconnected — clear confirmations for this address
    NodeDisconnected { addresses: HashSet<String> },
}
```

### Event Handlers

**`on_desired_changed()`** (replaces poll-diff):
- Compute diff between `desired` and `confirmed.aggregate()`
- Send SUBSCRIBE for missing, SUNSUBSCRIBE for excess
- No polling — fires immediately on user action

**`on_confirmed()` / `on_unconfirmed()`** (from push messages):
- Update `confirmed.by_address`
- Check if now fully synced, notify waiters
- No diff computation needed — just update tracking

**`on_node_reconnected(address)`** (replaces `remove_current_subscriptions_for_addresses` + poll):
- Clear all confirmations for `address`
- Reapply all desired exact+pattern subscriptions to that node
- For sharded: only reapply channels whose slot maps to that node

**`on_topology_changed(new_slot_map)`** (replaces `handle_topology_refresh` + `pending_unsubscribes`):
- For each confirmed sharded subscription, check if slot owner changed
- If changed: send SUNSUBSCRIBE to old owner, SSUBSCRIBE to new owner
- Remove gone addresses from `confirmed`
- No pending queue needed — act immediately

**`on_node_disconnected(addresses)`**:
- Clear all confirmations for those addresses
- Trigger `on_desired_changed()` to resubscribe

### Background Task
The task is event-driven via `mpsc::UnboundedReceiver<SyncEvent>`:
```rust
loop {
    match events_rx.recv().await {
        Some(SyncEvent::DesiredChanged) => self.reconcile().await,
        Some(SyncEvent::Confirmed { .. }) => self.update_confirmed(..),
        Some(SyncEvent::NodeReconnected { address }) => self.reapply(address).await,
        Some(SyncEvent::TopologyChanged { new_slot_map }) => self.handle_topology(new_slot_map).await,
        Some(SyncEvent::NodeDisconnected { addresses }) => self.clear_and_reconcile(addresses).await,
        None => break, // channel closed, synchronizer dropped
    }
    self.sync_notify.notify_waiters();
}
```
No sleep, no polling interval. Events are coalesced naturally by the channel.

### Confirmation Tracking
Push messages already flow through `PushManager::try_send_raw()` which calls `add_current_subscriptions`. In the new design, this becomes `events_tx.send(SyncEvent::Confirmed { .. })`. The trait methods `add_current_subscriptions` / `remove_current_subscriptions` remain unchanged — they just send events instead of mutating state directly.

---

## Migration Path

### Can we swap without changing external API?
**Yes.** The `PubSubSynchronizer` trait is the only external contract. The new implementation implements the same trait. Changes:

1. Replace `ValkeyPubSubSynchronizer` in `synchronizer.rs` with `EventDrivenSynchronizer`
2. Same `new()` signature, same `set_internal_client()` method
3. All 11 trait methods remain identical in signature
4. `create_pubsub_synchronizer()` in `pubsub/mod.rs` instantiates the new type
5. No changes to `client/mod.rs`, `cluster/mod.rs`, `connection/multiplexed.rs`, or `push_manager.rs`

### Mock synchronizer
The mock (`src/pubsub/mock.rs`, 1,316 lines) is independent — it doesn't use the real synchronizer. No changes needed.

---

## Line Count Estimate

| Component | Current | Proposed |
|---|---|---|
| Synchronizer struct + constructor | 80 | 60 |
| Event handlers (reconcile, topology, reconnect) | 350 | 200 |
| Trait impl (add/remove desired/current, intercept) | 350 | 120 |
| wait_for_sync + blocking subscribe | 200 | 70 |
| Background task loop | 30 | 30 |
| **Total** | **1,109** | **~450** |

Reduction comes from:
- Eliminating `SyncDiff` computation (~60 lines)
- Eliminating `pending_unsubscribes` queue + processing (~80 lines)
- Eliminating `compute_actual_subscriptions()` aggregation (~30 lines)
- Simplifying `handle_topology_refresh()` from 100→40 lines (no pending queue)
- Simplifying `intercept_pubsub_command()` — thinner dispatch, same match arms (~350→120)
- Removing `check_and_record_sync_state()` polling telemetry (inline into event handler)

---

## Risk Assessment

### Test Coverage
6 integration tests in `tests/test_pubsub.rs` (677 lines), all `#[serial_test::serial]`:
1. `test_sharded_subscriptions_survive_slot_migrations` (1 and 100 channels)
2. `test_exact_subscriptions_survive_slot_migrations` (1 and 100 channels)
3. `test_pattern_subscriptions_survive_slot_migrations` (1 and 100 patterns)
4. `test_all_subscription_types_survive_same_slot_migration`
5. `test_all_subscription_types_survive_different_slot_migrations`
6. `test_all_subscription_types_survive_failover`

These require a live cluster (`ValkeyCluster::new`) and are gated by `#[cfg(not(feature = "test-util"))]`. They exercise the real synchronizer through actual slot migrations and failovers.

**Coverage assessment:**
- Slot migration: well covered (sharded, exact, pattern, mixed)
- Failover: covered (test #6)
- Node disconnect/reconnect: implicitly covered through failover
- Blocking subscribe/unsubscribe: NOT covered in integration tests (only via mock)
- Standalone mode: NOT covered (all tests use cluster)
- Race conditions (rapid subscribe/unsubscribe): NOT covered

### Risks
1. **Low** — Trait contract is stable, implementation swap is clean
2. **Medium** — `wait_for_sync` semantics must match exactly (blocking tests depend on it)
3. **Medium** — Topology change handling must maintain the same unsubscribe-from-old + subscribe-to-new sequence, or sharded channels may receive duplicate messages during migration
4. **Low** — The mock synchronizer is unaffected; mock-pubsub integration tests will continue to pass unchanged
5. **Medium** — Event coalescing behavior differs from polling: multiple rapid topology changes may interleave differently than with 3s polling. The existing tests exercise this with `MIGRATION_DELAY = 0ms`, so they should catch regressions.
