//! Helpers for testing.

use revm::database::State;

use crate::execute::BasicBlockExecutor;

impl<Factory, DB> BasicBlockExecutor<Factory, DB> {
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
