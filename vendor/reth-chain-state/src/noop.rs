//! Noop impls for testing.

use reth_storage_api::noop::NoopProvider;
use tokio::sync::{broadcast, watch};

use crate::{
    CanonStateNotifications, CanonStateSubscriptions, ForkChoiceNotifications,
    ForkChoiceSubscriptions, PersistedBlockNotifications, PersistedBlockSubscriptions,
};

impl<C: Send + Sync> CanonStateSubscriptions for NoopProvider<C> {
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications {
        broadcast::channel(1).1
    }
}

impl<C: Send + Sync> ForkChoiceSubscriptions for NoopProvider<C> {
    type Header = alloy_consensus::Header;

    fn subscribe_safe_block(&self) -> ForkChoiceNotifications<alloy_consensus::Header> {
        let (_, rx) = watch::channel(None);
        ForkChoiceNotifications(rx)
    }

    fn subscribe_finalized_block(&self) -> ForkChoiceNotifications<alloy_consensus::Header> {
        let (_, rx) = watch::channel(None);
        ForkChoiceNotifications(rx)
    }
}

impl<C: Send + Sync> PersistedBlockSubscriptions for NoopProvider<C> {
    fn subscribe_persisted_block(&self) -> PersistedBlockNotifications {
        let (_, rx) = watch::channel(None);
        PersistedBlockNotifications(rx)
    }
}
