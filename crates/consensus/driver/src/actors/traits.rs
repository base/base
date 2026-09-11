//! [`NodeActor`] trait.

use async_trait::async_trait;

/// The [`NodeActor`] is an actor-like service for the node.
///
/// Actors may:
/// - Handle incoming messages.
///     - Perform background tasks.
/// - Emit new events for other actors to process.
#[async_trait]
pub trait NodeActor: Send + 'static {
    /// The error type for the actor.
    type Error: std::fmt::Debug;
    /// The type necessary to pass to the start function.
    /// This is the result of
    type StartData: Sized;

    /// Starts the actor.
    async fn start(self, start_context: Self::StartData) -> Result<(), Self::Error>;
}
