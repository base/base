//! Cached execution state over the provider's shared database interface.

use base_evm_handler::database::State;
use reth_storage_api::StateProviderBox;

/// Mutable execution cache backed directly by the node's state provider.
pub type StateCacheDb = State<StateProviderBox>;
