//! Hybrid block source that races a subscription stream against interval-based polling.

use std::{collections::HashMap, pin::Pin};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_alloy_consensus::OpBlock;
use base_protocol::{BlockInfo, L2BlockInfo};
use futures::{Stream, StreamExt};
use tokio::time::{Duration, Interval, interval};

use crate::{L2BlockEvent, PollingSource, SourceError, UnsafeBlockSource};

/// A block source that races a subscription stream against an interval-based poller.
///
/// Deduplicates blocks by hash and detects reorgs when the same block number
/// yields different hashes from different sources.
pub struct HybridBlockSource<S, P> {
    /// Subscription stream of blocks.
    sub: Pin<Box<S>>,
    /// Polling source for fetching the unsafe head.
    poller: P,
    /// Polling interval timer.
    interval: Interval,
    /// Block number to hash mapping for deduplication and reorg detection.
    /// Bounded to a sliding window of recent blocks; old entries are pruned.
    seen: HashMap<u64, B256>,
}

impl<S, P> std::fmt::Debug for HybridBlockSource<S, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridBlockSource").field("seen", &self.seen).finish_non_exhaustive()
    }
}

impl<S, P> HybridBlockSource<S, P>
where
    S: Stream<Item = Result<OpBlock, SourceError>> + Send,
    P: PollingSource,
{
    /// Create a new hybrid block source.
    ///
    /// Combines a subscription stream with a poller that fires at `poll_interval`.
    pub fn new(sub_stream: S, poller: P, poll_interval: Duration) -> Self {
        Self {
            sub: Box::pin(sub_stream),
            poller,
            interval: interval(poll_interval),
            seen: HashMap::new(),
        }
    }

    /// Process a received block, returning an event if the block is new or a reorg.
    ///
    /// Returns `None` if the block is a duplicate (already seen with the same hash).
    fn process(&mut self, block: OpBlock) -> Option<L2BlockEvent> {
        let number = block.header.number;
        let hash = block.header.hash_slow();

        // Prune entries older than 256 blocks to bound memory usage.
        let cutoff = number.saturating_sub(256);
        self.seen.retain(|&k, _| k >= cutoff);

        match self.seen.get(&number) {
            None => {
                // New block number — insert and emit.
                self.seen.insert(number, hash);
                Some(L2BlockEvent::Block(Box::new(block)))
            }
            Some(existing_hash) if *existing_hash == hash => {
                // Duplicate — already seen this exact block.
                tracing::debug!(block = number, "duplicate block, skipping");
                None
            }
            Some(_) => {
                // Same number, different hash — reorg detected.
                tracing::warn!(
                    block = number,
                    "reorg detected: different hash for same block number"
                );
                self.seen.insert(number, hash);
                // TODO: L2BlockInfo requires l1_origin and seq_num which need deposit tx parsing.
                // For now, construct a minimal L2BlockInfo from the block header.
                let block_info = BlockInfo::from(&block);
                let new_safe_head = L2BlockInfo::new(block_info, Default::default(), 0);
                // Note: we intentionally do not emit a Block event for the reorg block itself.
                // The reorg block (at `number`) is the new safe head — already confirmed on-chain
                // — so the batcher does not need to encode it. After the consumer resets,
                // BatchEncoder.add_block skips the parent-hash check when its block queue is
                // empty, so the first post-reorg unsafe block (number+1) is accepted as the
                // new chain tip without needing block `number` to be re-ingested.
                // Removing `number` from `seen` here would cause an infinite reorg loop if the
                // subscription re-delivers the same block.
                Some(L2BlockEvent::Reorg { new_safe_head })
            }
        }
    }
}

#[async_trait]
impl<S, P> UnsafeBlockSource for HybridBlockSource<S, P>
where
    S: Stream<Item = Result<OpBlock, SourceError>> + Send,
    P: PollingSource,
{
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        loop {
            tokio::select! {
                block = self.sub.next() => {
                    match block {
                        Some(Ok(b)) => {
                            if let Some(event) = self.process(b) {
                                return Ok(event);
                            }
                            // Duplicate — loop for next event.
                        }
                        Some(Err(e)) => return Err(e),
                        None => return Err(SourceError::Closed),
                    }
                }
                _ = self.interval.tick() => {
                    match self.poller.unsafe_head().await {
                        Ok(b) => {
                            if let Some(event) = self.process(b) {
                                return Ok(event);
                            }
                            // Duplicate — loop for next event.
                        }
                        Err(SourceError::Provider(msg)) => {
                            tracing::warn!(error = %msg, "polling source error, retrying on next tick");
                            // Transient provider error — continue to next tick.
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a minimal [`OpBlock`] with a given number and parent hash.
    fn make_block(number: u64, parent_hash: B256) -> OpBlock {
        OpBlock {
            header: alloy_consensus::Header { number, parent_hash, ..Default::default() },
            body: Default::default(),
        }
    }

    /// A polling source that always returns a fixed block.
    struct FixedPoller(OpBlock);

    #[async_trait]
    impl PollingSource for FixedPoller {
        async fn unsafe_head(&self) -> Result<OpBlock, SourceError> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn test_hybrid_new_block() {
        let block = make_block(1, B256::ZERO);
        let poller = FixedPoller(block.clone());
        let stream = futures::stream::once(async move { Ok(block) });
        let mut source = HybridBlockSource::new(stream, poller, Duration::from_secs(100));

        let event = source.next().await.unwrap();
        assert!(matches!(event, L2BlockEvent::Block(_)));
    }

    #[tokio::test]
    async fn test_hybrid_duplicate_skipped() {
        let block = make_block(1, B256::ZERO);
        let poller = FixedPoller(block.clone());
        // Two identical blocks in sequence — second should be skipped, stream ends.
        let stream = futures::stream::iter(vec![Ok(block.clone()), Ok(block)]);
        let mut source = HybridBlockSource::new(stream, poller, Duration::from_secs(100));

        // First call returns the block.
        let event = source.next().await.unwrap();
        assert!(matches!(event, L2BlockEvent::Block(_)));

        // Second call: duplicate is skipped, stream exhausted -> Closed.
        let err = source.next().await.unwrap_err();
        assert!(matches!(err, SourceError::Closed));
    }

    #[tokio::test]
    async fn test_hybrid_reorg_detected() {
        let block1 = make_block(1, B256::ZERO);
        // Different block at same number (different parent hash -> different hash).
        let block2 = make_block(1, B256::from([1u8; 32]));
        let poller = FixedPoller(block1.clone());
        let stream = futures::stream::iter(vec![Ok(block1), Ok(block2)]);
        let mut source = HybridBlockSource::new(stream, poller, Duration::from_secs(100));

        // First block.
        let event = source.next().await.unwrap();
        assert!(matches!(event, L2BlockEvent::Block(_)));

        // Second block at same number with different hash -> reorg.
        let event = source.next().await.unwrap();
        assert!(matches!(event, L2BlockEvent::Reorg { .. }));
    }

    #[tokio::test]
    async fn test_hybrid_stream_error() {
        let stream =
            futures::stream::once(async { Err(SourceError::Provider("rpc down".to_string())) });
        let poller = FixedPoller(make_block(1, B256::ZERO));
        let mut source = HybridBlockSource::new(stream, poller, Duration::from_secs(100));

        let err = source.next().await.unwrap_err();
        assert!(matches!(err, SourceError::Provider(_)));
    }
}
