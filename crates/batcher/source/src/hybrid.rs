//! Hybrid block source that races a subscription stream against interval-based polling.

use std::collections::HashMap;

use alloy_primitives::B256;
use async_trait::async_trait;
use base_alloy_consensus::OpBlock;
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};
use futures::{StreamExt, stream::BoxStream};
use tokio::time::{Duration, Interval, interval};

use crate::{BlockSubscription, L2BlockEvent, PollingSource, SourceError, UnsafeBlockSource};

/// A block source that races a subscription stream against an interval-based poller.
///
/// Deduplicates blocks by hash and detects reorgs when the same block number
/// yields different hashes from different sources.
pub struct HybridBlockSource<S, P> {
    /// The block stream returned by `S::take_stream`.
    ///
    /// Declared before `_subscription` so it is dropped first, ensuring the
    /// stream's underlying transport is released before the provider is torn down.
    sub: BoxStream<'static, Result<OpBlock, SourceError>>,
    /// The original subscription, kept alive so its resources remain open.
    _subscription: S,
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
    S: BlockSubscription,
    P: PollingSource,
{
    /// Create a new hybrid block source.
    ///
    /// Calls [`BlockSubscription::take_stream`] once to obtain the live block
    /// stream, then retains the subscription to keep any underlying resources
    /// (e.g. a WebSocket provider) alive. Combines the stream with a poller
    /// that fires at `poll_interval`.
    pub fn new(mut subscription: S, poller: P, poll_interval: Duration) -> Self {
        let sub = subscription.take_stream();
        Self {
            sub,
            _subscription: subscription,
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
                tracing::debug!(block = %number, "duplicate block, skipping");
                None
            }
            Some(_) => {
                // Same number, different hash — reorg detected.
                tracing::warn!(
                    block = %number,
                    "reorg detected: different hash for same block number"
                );
                self.seen.insert(number, hash);
                let block_info = BlockInfo::from(&block);
                // Parse l1_origin and seq_num from the L1 info deposit tx embedded in every L2
                // block. Fall back to zeroed fields with a warning when the deposit is absent.
                let new_safe_head = block
                    .body
                    .transactions
                    .first()
                    .and_then(|tx| tx.as_deposit())
                    .and_then(|dep| L1BlockInfoTx::decode_calldata(&dep.input).ok())
                    .map(|l1_info| {
                        L2BlockInfo::new(block_info, l1_info.id(), l1_info.sequence_number())
                    })
                    .unwrap_or_else(|| {
                        tracing::warn!(
                            block = number,
                            "reorg block missing deposit tx, l1_origin will be zeroed"
                        );
                        L2BlockInfo::new(block_info, Default::default(), 0)
                    });
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
    S: BlockSubscription,
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
    use alloy_consensus::BlockBody;
    use alloy_primitives::Sealed;
    use base_alloy_consensus::{OpTxEnvelope, TxDeposit};
    use base_protocol::L1BlockInfoBedrock;
    use futures::{StreamExt, stream::BoxStream};

    use super::*;

    /// Test helper: wraps a concrete stream as a [`BlockSubscription`].
    struct StreamSub(BoxStream<'static, Result<OpBlock, SourceError>>);

    impl BlockSubscription for StreamSub {
        fn take_stream(&mut self) -> BoxStream<'static, Result<OpBlock, SourceError>> {
            std::mem::replace(&mut self.0, futures::stream::pending().boxed())
        }
    }

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
        let mut source =
            HybridBlockSource::new(StreamSub(stream.boxed()), poller, Duration::from_secs(100));

        let event = source.next().await.unwrap();
        assert!(matches!(event, L2BlockEvent::Block(_)));
    }

    #[tokio::test]
    async fn test_hybrid_duplicate_skipped() {
        let block = make_block(1, B256::ZERO);
        let poller = FixedPoller(block.clone());
        // Two identical blocks in sequence — second should be skipped, stream ends.
        let stream = futures::stream::iter(vec![Ok(block.clone()), Ok(block)]);
        let mut source =
            HybridBlockSource::new(StreamSub(stream.boxed()), poller, Duration::from_secs(100));

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
        let mut source =
            HybridBlockSource::new(StreamSub(stream.boxed()), poller, Duration::from_secs(100));

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
        let mut source =
            HybridBlockSource::new(StreamSub(stream.boxed()), poller, Duration::from_secs(100));

        let err = source.next().await.unwrap_err();
        assert!(matches!(err, SourceError::Provider(_)));
    }

    #[tokio::test]
    async fn test_reorg_safe_head_has_l1_origin() {
        let l1_number = 42u64;
        let l1_hash = B256::from([0xABu8; 32]);
        let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::new_from_number_and_block_hash(
            l1_number, l1_hash,
        ))
        .encode_calldata();

        let deposit =
            OpTxEnvelope::Deposit(Sealed::new(TxDeposit { input: calldata, ..Default::default() }));

        let block1 = OpBlock {
            header: alloy_consensus::Header {
                number: 1,
                parent_hash: B256::ZERO,
                ..Default::default()
            },
            body: BlockBody { transactions: vec![deposit.clone()], ..Default::default() },
        };
        // Reorg block — same number, different parent hash produces a different block hash.
        let block2 = OpBlock {
            header: alloy_consensus::Header {
                number: 1,
                parent_hash: B256::from([1u8; 32]),
                ..Default::default()
            },
            body: BlockBody { transactions: vec![deposit], ..Default::default() },
        };

        let poller = FixedPoller(block1.clone());
        let stream = futures::stream::iter(vec![Ok(block1), Ok(block2)]);
        let mut source =
            HybridBlockSource::new(StreamSub(stream.boxed()), poller, Duration::from_secs(100));

        // First block arrives normally.
        let event = source.next().await.unwrap();
        assert!(matches!(event, L2BlockEvent::Block(_)));

        // Second block at same number with different hash → reorg.
        let event = source.next().await.unwrap();
        let L2BlockEvent::Reorg { new_safe_head } = event else {
            panic!("expected Reorg event");
        };
        assert_eq!(new_safe_head.l1_origin.number, l1_number);
        assert_eq!(new_safe_head.l1_origin.hash, l1_hash);
    }
}
