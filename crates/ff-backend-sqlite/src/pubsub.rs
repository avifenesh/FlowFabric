//! In-process pub/sub scaffolding (RFC-023 §4.2).
//!
//! Phase 1a lands the empty broadcast-channel struct. Phase 3 wires
//! up the outbox-inside-tx + broadcast-as-wakeup pattern per §4.2
//! A2.

use tokio::sync::broadcast;

/// Placeholder capacity for the broadcast channels. Tuned in Phase 3
/// when real producers + consumers arrive.
const DEFAULT_CAPACITY: usize = 256;

/// Per-backend in-process wakeup channels. One broadcast channel per
/// RFC-019 subscription family; Phase 3 populates the `Sender` halves
/// inside `SqliteBackendInner` and consumers on the subscribe side
/// hold `Receiver` handles.
#[allow(dead_code)] // Phase 1a scaffolding — handed to Phase 3.
pub(crate) struct PubSub {
    pub(crate) lease_history: broadcast::Sender<()>,
    pub(crate) completion: broadcast::Sender<()>,
    pub(crate) signal_delivery: broadcast::Sender<()>,
}

impl PubSub {
    #[allow(dead_code)] // Phase 1a scaffolding — handed to Phase 3.
    pub(crate) fn new() -> Self {
        let (lease_history, _) = broadcast::channel(DEFAULT_CAPACITY);
        let (completion, _) = broadcast::channel(DEFAULT_CAPACITY);
        let (signal_delivery, _) = broadcast::channel(DEFAULT_CAPACITY);
        Self {
            lease_history,
            completion,
            signal_delivery,
        }
    }
}

impl Default for PubSub {
    fn default() -> Self {
        Self::new()
    }
}
