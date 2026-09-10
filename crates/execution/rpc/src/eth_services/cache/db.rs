//! Cached execution state over the provider's shared database interface.

use base_execution_evm_runtime::State;
use base_execution_state_types::StateProviderBox;

/// Mutable execution cache backed directly by the node's state provider.
pub type StateCacheDb = State<StateProviderBox>;
