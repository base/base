//! State commit hook.

use crate::core_memory::EvmState;

/// A hook that is called when state changes are committed.
pub trait OnStateHook: Send + 'static {
    /// Invoked with the state being committed.
    fn on_state(&mut self, state: EvmState);
}

impl<F> OnStateHook for F
where
    F: FnMut(EvmState) + Send + 'static,
{
    fn on_state(&mut self, state: EvmState) {
        self(state)
    }
}
