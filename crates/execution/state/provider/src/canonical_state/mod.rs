//! Canonical in-memory chain views and notification streams.

use base_execution_state_types::{CanonStateNotification, ExecutedBlock};

mod in_memory;
pub use in_memory::*;

mod preserved_sparse_trie;
pub use preserved_sparse_trie::*;

mod noop;

mod chain_info;
pub use chain_info::ChainInfoTracker;

mod notifications;
pub use notifications::{
    CanonStateNotificationSender, CanonStateNotificationStream, CanonStateNotifications,
    CanonStateSubscriptions, ForkChoiceNotifications, ForkChoiceStream, ForkChoiceSubscriptions,
    PersistedBlockNotifications, PersistedBlockSubscriptions, WatchValueStream,
};

mod memory_overlay;
pub use memory_overlay::{MemoryOverlayStateProvider, MemoryOverlayStateProviderRef};

#[cfg(any(test, feature = "test-utils"))]
/// Common test helpers
pub mod test_utils;
