//! Core trait for L1 head sources.

use async_trait::async_trait;

/// A source of L1 head block numbers, streaming head updates as they arrive.
///
/// The batcher driver calls [`next`][L1HeadSource::next] in a loop to track
/// L1 chain head advancement, enabling channel timeout detection.
#[async_trait]
pub trait L1HeadSource: Send {
    /// Wait for the next L1 head block number.
    ///
    /// Blocks (asynchronously) until a new L1 head block number is available.
    /// Implementations are responsible for deduplicating redundant head updates:
    /// if both a subscription and a poller deliver the same block number, it is
    /// emitted once.
    async fn next(&mut self) -> u64;
}
