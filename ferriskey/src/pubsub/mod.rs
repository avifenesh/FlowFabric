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
        // Only set if the weak pointer can be upgraded (is not empty)
        if internal_client.upgrade().is_some() {
            sync.as_any()
                .downcast_ref::<mock::MockPubSubSynchronizer>()
                .expect("Expected MockPubSubSynchronizer")
                .set_internal_client(internal_client);
        }
        sync
    }

    #[cfg(not(feature = "test-util"))]
    {
        let sync = synchronizer::EventDrivenSynchronizer::new(
            initial_subscriptions,
            is_cluster,
            reconciliation_interval,
            _request_timeout,
        );
        if internal_client.upgrade().is_some() {
            sync.as_any()
                .downcast_ref::<synchronizer::EventDrivenSynchronizer>()
                .expect("Expected EventDrivenSynchronizer")
                .set_internal_client(internal_client);
        }
        sync
    }
}
