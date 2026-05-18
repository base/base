use base_common_consensus::BaseBlock;

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
/// Implementations yield fully-formed [`BaseBlock`]s in order. The batcher
/// extracts the L1 epoch from the first (deposit) transaction in each block,
/// filters out all deposit transactions, and encodes the remaining user
/// transactions into a [`SingleBatch`] for submission.
///
/// [`SingleBatch`]: base_protocol::SingleBatch
pub trait L2BlockProvider {
    /// Return the next L2 block, or `None` if the source is exhausted.
    fn next_block(&mut self) -> Option<BaseBlock>;
}
