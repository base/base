//! Noop impls for testing.

use base_execution_state_api::NoopProvider;
use tokio::sync::{broadcast, watch};

use crate::canonical_state::{
    CanonStateNotifications, CanonStateSubscriptions, ForkChoiceNotifications,
    ForkChoiceSubscriptions, PersistedBlockNotifications, PersistedBlockSubscriptions,
};

impl CanonStateSubscriptions for NoopProvider {
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications {
        broadcast::channel(1).1
    }
}

impl ForkChoiceSubscriptions for NoopProvider {
    fn subscribe_safe_block(&self) -> ForkChoiceNotifications {
        let (_, rx) = watch::channel(None);
        ForkChoiceNotifications(rx)
    }

    fn subscribe_finalized_block(&self) -> ForkChoiceNotifications {
        let (_, rx) = watch::channel(None);
        ForkChoiceNotifications(rx)
    }
}

impl PersistedBlockSubscriptions for NoopProvider {
    fn subscribe_persisted_block(&self) -> PersistedBlockNotifications {
        let (_, rx) = watch::channel(None);
        PersistedBlockNotifications(rx)
    }
}
