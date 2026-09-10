//! Helpers for testing.

use base_execution_evm_runtime::State;

use crate::execute::BasicBlockExecutor;

impl<DB> BasicBlockExecutor<DB> {
    /// Provides safe read access to the state
    pub fn with_state<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&State<DB>) -> R,
    {
        f(&self.db)
    }

    /// Provides safe write access to the state
    pub fn with_state_mut<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut State<DB>) -> R,
    {
        f(&mut self.db)
    }
}

pub use crate::state_provider_test::StateProviderTest;
