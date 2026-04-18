// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

use crate::client::ClientWrapper;
pub use crate::connection::info::{
    PubSubChannelOrPattern, PubSubSubscriptionInfo, PubSubSubscriptionKind,
};
use crate::pubsub::push_manager::PushInfo;
pub use crate::pubsub::synchronizer_trait::PubSubSynchronizer;
pub use crate::value::{ErrorKind, Error};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};

#[cfg(feature = "test-util")]
mod mock;

#[cfg(feature = "test-util")]
pub use mock::MockPubSubBroker;

pub(crate) mod push_manager;
pub(crate) mod synchronizer_trait;

#[cfg(not(feature = "test-util"))]
pub mod synchronizer;

/// Factory function to create a synchronizer with internal client reference
pub async fn create_pubsub_synchronizer(
    _push_sender: Option<mpsc::UnboundedSender<PushInfo>>,
    initial_subscriptions: Option<crate::connection::info::PubSubSubscriptionInfo>,
    is_cluster: bool,
    internal_client: Weak<RwLock<ClientWrapper>>,
    reconciliation_interval: Option<Duration>,
    _request_timeout: Duration,
) -> Arc<dyn PubSubSynchronizer> {
    #[cfg(feature = "test-util")]
    {
        let sync = mock::MockPubSubSynchronizer::create(
            _push_sender,
            initial_subscriptions,
            is_cluster,
            reconciliation_interval,
        )
        .await;
        // Only set if the weak pointer can be upgraded (is not empty).
        // The downcast is safe here because we just created the concrete type above,
        // but we use if-let to avoid panicking if the trait object is ever swapped.
        if internal_client.upgrade().is_some() {
            if let Some(concrete) = sync.as_any().downcast_ref::<mock::MockPubSubSynchronizer>() {
                concrete.set_internal_client(internal_client);
            }
        }
        sync
    }

    #[cfg(not(feature = "test-util"))]
    {
        // `new()` returns `Arc<EventDrivenSynchronizer>` -- call set_internal_client
        // directly on the concrete type to avoid a panicking downcast.
        let sync = synchronizer::EventDrivenSynchronizer::new(
            initial_subscriptions,
            is_cluster,
            reconciliation_interval,
            _request_timeout,
        );
        if internal_client.upgrade().is_some() {
            sync.set_internal_client(internal_client);
        }
        sync
    }
}

/// Attach a `Weak<Arc<RwLock<ClientWrapper>>>` to an already-created
/// synchronizer.
///
/// The direct `create_pubsub_synchronizer` call takes the weak
/// handle at construction time because historically the shared
/// client `Arc` was built before connect — initialised with a
/// `ClientWrapper::Lazy` placeholder. The Lazy placeholder is gone;
/// `Client::new` now builds the real wrapper, then the `Arc`, then
/// calls this helper to wire the synchronizer's Weak reference.
/// Separating "create" from "attach" keeps the ordering legal
/// without changing the trait surface.
pub fn attach_internal_client(
    sync: &Arc<dyn PubSubSynchronizer>,
    internal_client: Weak<RwLock<ClientWrapper>>,
) {
    if internal_client.upgrade().is_none() {
        return;
    }
    #[cfg(feature = "test-util")]
    if let Some(concrete) = sync.as_any().downcast_ref::<mock::MockPubSubSynchronizer>() {
        concrete.set_internal_client(internal_client);
        return;
    }
    #[cfg(not(feature = "test-util"))]
    if let Some(concrete) = sync
        .as_any()
        .downcast_ref::<synchronizer::EventDrivenSynchronizer>()
    {
        concrete.set_internal_client(internal_client);
    }
}
