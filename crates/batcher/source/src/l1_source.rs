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
    /// Blocks (asynchronously) until an L1 head block number is available, and never
    /// resolves once nothing more will arrive. A repeated or lower number is harmless:
    /// the pipeline ignores heads that do not advance.
    async fn next(&mut self) -> u64;
}
