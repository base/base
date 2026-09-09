//! Core trait for L1 head sources.

use async_trait::async_trait;

use crate::{L1HeadEvent, SourceError};

/// A source of L1 head events, streaming head updates as they arrive.
///
/// The batcher driver calls [`next`][L1HeadSource::next] in a loop to track
/// L1 chain head advancement, enabling channel timeout detection.
#[async_trait]
pub trait L1HeadSource: Send {
    /// Wait for the next L1 head event.
    ///
    /// Blocks (asynchronously) until a new L1 head block number is available.
    /// Implementations are responsible for deduplicating redundant head updates —
    /// if both a subscription and a poller deliver the same block number, only
    /// one `NewHead` event is emitted.
    async fn next(&mut self) -> Result<L1HeadEvent, SourceError>;
}
