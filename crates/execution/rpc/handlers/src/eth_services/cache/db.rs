//! Cached execution state over the provider's shared database interface.

use base_execution_evm_runtime::database::State;
use base_execution_state_api::StateProviderBox;

/// Mutable execution cache backed directly by the node's state provider.
pub type StateCacheDb = State<StateProviderBox>;
