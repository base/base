//! Cached execution state over the provider's shared database interface.

use reth_storage_api::StateProviderBox;
use revm::database::State;

/// Mutable execution cache backed directly by the node's state provider.
pub type StateCacheDb = State<StateProviderBox>;
