//! The [`Engine`] state owner.

use tokio::sync::watch::Sender;

use crate::EngineState;

/// The [`Engine`] state owner.
///
/// The engine actor owns one [`Engine`] and calls direct methods for each request, providing
/// synchronization guarantees for the L2 execution layer and other actors.
///
/// Because operations are executed one at a time, they are considered to be atomic operations over
/// the [`EngineState`], and are given exclusive access to the engine state during execution.
///
#[derive(Debug)]
pub struct Engine {
    /// The state of the engine.
    pub(super) state: EngineState,
    /// A sender that can be used to notify the engine actor of state changes.
    pub(super) state_sender: Sender<EngineState>,
}

impl Engine {
    /// Creates a new [`Engine`] with the passed initial [`EngineState`].
    pub const fn new(initial_state: EngineState, state_sender: Sender<EngineState>) -> Self {
        Self { state: initial_state, state_sender }
    }

    /// Returns a reference to the inner [`EngineState`].
    pub const fn state(&self) -> &EngineState {
        &self.state
    }

    /// Returns a receiver that can be used to listen to engine state updates.
    pub fn state_subscribe(&self) -> tokio::sync::watch::Receiver<EngineState> {
        self.state_sender.subscribe()
    }
}
