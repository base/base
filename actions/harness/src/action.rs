use crate::MockL2Block;

/// A discrete step that a test actor can perform.
///
/// Every actor in the action test framework implements this trait. A test
/// drives actors by calling [`act`](Action::act) in sequence, then asserting
/// on the resulting state held by each actor.
///
/// The associated types let callers inspect what each step produced without
/// coupling the harness to a concrete actor type. Errors use
/// [`core::fmt::Debug`] so tests can call `.unwrap()` on them directly.
pub trait Action {
    /// The value produced by a single action step.
    type Output;

    /// The error type that can occur during a step.
    type Error: core::fmt::Debug;

    /// Perform one action step and return the result.
    fn act(&mut self) -> Result<Self::Output, Self::Error>;
}

/// A source of L2 blocks for the batcher to consume.
///
/// Implementations return blocks in order. Once exhausted, [`next_block`]
/// returns `None`. The batcher drains the source when it runs.
pub trait L2BlockProvider {
    /// Return the next L2 block, or `None` if the source is exhausted.
    fn next_block(&mut self) -> Option<MockL2Block>;
}
