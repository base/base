//! Hybrid block source that races a subscription stream against interval-based polling.

use std::{collections::HashMap, time::Duration};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_alloy_consensus::BaseBlock;
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};
use base_runtime::Clock;
use futures::{StreamExt, stream::BoxStream};

use crate::{BlockSubscription, L2BlockEvent, PollingSource, SourceError, UnsafeBlockSource};

/// Delay between sequential catchup poll retries on transient provider errors.
const CATCHUP_RETRY_DELAY: Duration = Duration::from_millis(100);

/// A block source that races a subscription stream against an interval-based poller.
///
/// Deduplicates blocks by hash and detects reorgs when the same block number
/// yields different hashes from different sources.
#[derive(derive_more::Debug)]
pub struct HybridBlockSource<S, P, C> {
    /// The block stream returned by `S::take_stream`.
    ///
    /// Declared before `_subscription` so it is dropped first, ensuring the
    /// stream's underlying transport is released before the provider is torn down.
    #[debug(skip)]
    sub: BoxStream<'static, Result<BaseBlock, SourceError>>,
    /// The original subscription, kept alive so its resources remain open.
    #[debug(skip)]
    _subscription: S,
    /// Polling source for fetching the unsafe head.
    #[debug(skip)]
    poller: P,
    /// Clock used for the polling interval and catchup retry sleep.
    #[debug(skip)]
    clock: C,
    /// Polling interval stream, yielding `()` every `poll_interval`.
    #[debug(skip)]
    interval: BoxStream<'static, ()>,
    /// Block number to hash mapping for deduplication and reorg detection.
    /// Bounded to a sliding window of recent blocks; old entries are pruned.
    seen: HashMap<u64, B256>,
}

impl<S, P, C> HybridBlockSource<S, P, C>
where
    S: BlockSubscription,
    P: PollingSource,
    C: Clock,
{
    /// Create a new hybrid block source.
    ///
    /// Calls [`BlockSubscription::take_stream`] once to obtain the live block
    /// stream, then retains the subscription to keep any underlying resources
    /// (e.g. a WebSocket provider) alive. Combines the stream with a poller
    /// that fires at `poll_interval` according to `clock`. The clock is also
    /// retained for use in the sequential catchup retry sleep path.
    pub fn new(clock: C, mut subscription: S, poller: P, poll_interval: Duration) -> Self {
        let sub = subscription.take_stream();
        let interval = clock.interval(poll_interval);
        Self { sub, _subscription: subscription, poller, clock, interval, seen: HashMap::new() }
    }

    /// Process a received block, returning an event if the block is new or a reorg.
    ///
    /// Returns `None` if the block is a duplicate (already seen with the same hash).
    fn process(&mut self, block: BaseBlock) -> Option<L2BlockEvent> {
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
                // The driver calls reset_catchup(safe_head + 1) after receiving the Reorg event,
                // so the poller will re-deliver all post-reorg blocks sequentially.
                // Removing `number` from `seen` here would cause an infinite reorg loop if the
                // subscription re-delivers the same block.
                Some(L2BlockEvent::Reorg { new_safe_head })
            }
        }
    }
}

#[async_trait]
impl<S, P, C> UnsafeBlockSource for HybridBlockSource<S, P, C>
where
    S: BlockSubscription,
    P: PollingSource,
    C: Clock,
{
    /// Reset the source for sequential catchup from `start_from`.
    ///
    /// Clears the dedup cache so historical blocks are not filtered as
    /// duplicates, then delegates to the poller to begin sequential delivery.
    fn reset_catchup(&mut self, start_from: u64) {
        self.seen.clear();
        self.poller.reset_catchup(start_from);
    }

    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        loop {
            // During sequential catchup, bypass the live subscription arm entirely.
            // Racing WS events against the poller would deliver future blocks out of order,
            // triggering pipeline resets that discard catchup progress.
            if self.poller.is_catching_up() {
                match self.poller.unsafe_head().await {
                    Ok(b) => {
                        if let Some(event) = self.process(b) {
                            return Ok(event);
                        }
                        // Duplicate — continue sequential polling.
                    }
                    Err(SourceError::Provider(msg)) => {
                        tracing::warn!(error = %msg, "sequential catchup poll error, retrying");
                        self.clock.sleep(CATCHUP_RETRY_DELAY).await;
                    }
                    Err(e) => return Err(e),
                }
                continue;
            }

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
                _ = self.interval.next() => {
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
    use std::sync::{Arc, Mutex};

    use alloy_consensus::BlockBody;
    use alloy_primitives::Sealed;
    use base_alloy_consensus::{BaseTxEnvelope, TxDeposit};
    use base_protocol::L1BlockInfoBedrock;
    use base_runtime::{Config, Runner};
    use futures::{StreamExt, stream::BoxStream};

    use super::*;

    /// Test helper: wraps a concrete stream as a [`BlockSubscription`].
    struct StreamSub(BoxStream<'static, Result<BaseBlock, SourceError>>);

    impl BlockSubscription for StreamSub {
        fn take_stream(&mut self) -> BoxStream<'static, Result<BaseBlock, SourceError>> {
            std::mem::replace(&mut self.0, futures::stream::pending().boxed())
        }
    }

    /// Helper to build a minimal [`BaseBlock`] with a given number and parent hash.
    fn make_block(number: u64, parent_hash: B256) -> BaseBlock {
        BaseBlock {
            header: alloy_consensus::Header { number, parent_hash, ..Default::default() },
            body: Default::default(),
        }
    }

    /// A never-ending pending subscription (interval path tests).
    fn pending_sub() -> StreamSub {
        StreamSub(futures::stream::pending::<Result<BaseBlock, SourceError>>().boxed())
    }

    /// A polling source that always returns a fixed block.
    struct FixedPoller(BaseBlock);

    /// A polling source that returns blocks with a monotonically increasing number on each call.
    struct IncrementingPoller(Arc<Mutex<u64>>);

    impl IncrementingPoller {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(0)))
        }
    }

    #[async_trait]
    impl PollingSource for IncrementingPoller {
        async fn unsafe_head(&self) -> Result<BaseBlock, SourceError> {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            Ok(make_block(*n, B256::ZERO))
        }
    }

    /// A polling source that returns a transient [`SourceError::Provider`] on the first call,
    /// then succeeds with a fixed block on all subsequent calls.
    struct FalliblePoller {
        calls: Arc<Mutex<u32>>,
        block: BaseBlock,
    }

    impl FalliblePoller {
        fn new(block: BaseBlock) -> Self {
            Self { calls: Arc::new(Mutex::new(0)), block }
        }
    }

    #[async_trait]
    impl PollingSource for FalliblePoller {
        async fn unsafe_head(&self) -> Result<BaseBlock, SourceError> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            if *c == 1 {
                Err(SourceError::Provider("transient rpc error".into()))
            } else {
                Ok(self.block.clone())
            }
        }
    }

    /// A polling source that always returns a fatal (non-transient) error.
    struct ClosedPoller;

    #[async_trait]
    impl PollingSource for ClosedPoller {
        async fn unsafe_head(&self) -> Result<BaseBlock, SourceError> {
            Err(SourceError::Closed)
        }
    }

    #[async_trait]
    impl PollingSource for FixedPoller {
        async fn unsafe_head(&self) -> Result<BaseBlock, SourceError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn test_hybrid_new_block() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let block = make_block(1, B256::ZERO);
            let poller = FixedPoller(block.clone());
            let stream = futures::stream::once(std::future::ready(Ok(block)));
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                Duration::from_secs(100),
            );
            let event = source.next().await.unwrap();
            assert!(matches!(event, L2BlockEvent::Block(_)));
        });
    }

    #[test]
    fn test_hybrid_duplicate_skipped() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let block = make_block(1, B256::ZERO);
            let poller = FixedPoller(block.clone());
            // Two identical blocks in sequence — second should be skipped, stream ends.
            let stream = futures::stream::iter(vec![Ok(block.clone()), Ok(block)]);
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                Duration::from_secs(100),
            );
            // First call returns the block.
            let event = source.next().await.unwrap();
            assert!(matches!(event, L2BlockEvent::Block(_)));

            // Second call: duplicate is skipped, stream exhausted -> Closed.
            let err = source.next().await.unwrap_err();
            assert!(matches!(err, SourceError::Closed));
        });
    }

    #[test]
    fn test_hybrid_reorg_detected() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let block1 = make_block(1, B256::ZERO);
            // Different block at same number (different parent hash -> different hash).
            let block2 = make_block(1, B256::from([1u8; 32]));
            let poller = FixedPoller(block1.clone());
            let stream = futures::stream::iter(vec![Ok(block1), Ok(block2)]);
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                Duration::from_secs(100),
            );
            let event = source.next().await.unwrap();
            assert!(matches!(event, L2BlockEvent::Block(_)));

            // Second block at same number with different hash -> reorg.
            let event = source.next().await.unwrap();
            assert!(matches!(event, L2BlockEvent::Reorg { .. }));
        });
    }

    #[test]
    fn test_hybrid_stream_error() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let stream = futures::stream::once(std::future::ready(Err(SourceError::Provider(
                "rpc down".to_string(),
            ))));
            let poller = FixedPoller(make_block(1, B256::ZERO));
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                Duration::from_secs(100),
            );
            // The subscription error and the interval's immediate first tick may both be
            // ready simultaneously. If the interval branch wins the select!, a block event
            // arrives first; the subscription error propagates on the very next call because
            // the next interval tick is 100s away and no time is advanced.
            let err = loop {
                match source.next().await {
                    Ok(_) => {}
                    Err(e) => break e,
                }
            };
            assert!(matches!(err, SourceError::Provider(_)));
        });
    }

    #[test]
    fn test_reorg_safe_head_has_l1_origin() {
        let l1_number = 42u64;
        let l1_hash = B256::from([0xABu8; 32]);
        let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::new_from_number_and_block_hash(
            l1_number, l1_hash,
        ))
        .encode_calldata();

        let deposit = BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
            input: calldata,
            ..Default::default()
        }));

        let block1 = BaseBlock {
            header: alloy_consensus::Header {
                number: 1,
                parent_hash: B256::ZERO,
                ..Default::default()
            },
            body: BlockBody { transactions: vec![deposit.clone()], ..Default::default() },
        };
        // Reorg block — same number, different parent hash produces a different block hash.
        let block2 = BaseBlock {
            header: alloy_consensus::Header {
                number: 1,
                parent_hash: B256::from([1u8; 32]),
                ..Default::default()
            },
            body: BlockBody { transactions: vec![deposit], ..Default::default() },
        };

        Runner::start(Config::seeded(0), |ctx| async move {
            let poller = FixedPoller(block1.clone());
            let stream = futures::stream::iter(vec![Ok(block1), Ok(block2)]);
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                Duration::from_secs(100),
            );

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
        });
    }

    // --- Sequential catchup path tests ---

    /// A polling source in sequential catchup mode.
    ///
    /// Delivers blocks `start..=until` on successive `unsafe_head()` calls,
    /// reporting `is_catching_up() == true` while blocks remain, then switches
    /// to returning `latest` for all subsequent calls.
    struct CatchupPoller {
        state: Arc<Mutex<u64>>,
        until: u64,
        latest: BaseBlock,
    }

    impl CatchupPoller {
        fn new(start: u64, until: u64, latest: BaseBlock) -> Self {
            Self { state: Arc::new(Mutex::new(start)), until, latest }
        }
    }

    #[async_trait]
    impl PollingSource for CatchupPoller {
        async fn unsafe_head(&self) -> Result<BaseBlock, SourceError> {
            let mut n = self.state.lock().unwrap();
            if *n <= self.until {
                let block = make_block(*n, B256::ZERO);
                *n += 1;
                Ok(block)
            } else {
                Ok(self.latest.clone())
            }
        }

        fn is_catching_up(&self) -> bool {
            *self.state.lock().unwrap() <= self.until
        }
    }

    /// A catchup poller that fails with a transient error on its first call,
    /// then succeeds with a fixed block on all subsequent calls.
    struct FallibleCatchupPoller {
        calls: Arc<Mutex<u32>>,
        block: BaseBlock,
    }

    impl FallibleCatchupPoller {
        fn new(block: BaseBlock) -> Self {
            Self { calls: Arc::new(Mutex::new(0)), block }
        }
    }

    #[async_trait]
    impl PollingSource for FallibleCatchupPoller {
        async fn unsafe_head(&self) -> Result<BaseBlock, SourceError> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            if *c == 1 {
                Err(SourceError::Provider("transient rpc error".into()))
            } else {
                Ok(self.block.clone())
            }
        }

        fn is_catching_up(&self) -> bool {
            // Still catching up until the block has been successfully delivered.
            *self.calls.lock().unwrap() < 2
        }
    }

    #[test]
    fn test_catchup_delivers_blocks_sequentially_ignoring_subscription() {
        // Subscription carries a future block (#10) that would appear out-of-order
        // during catchup. The catchup path must bypass the subscription entirely
        // so blocks 1, 2, 3 are delivered in order before block #10 is considered.
        Runner::start(Config::seeded(0), |ctx| async move {
            let future_block = make_block(10, B256::ZERO);
            let stream = futures::stream::iter(vec![Ok(future_block)]);
            let latest = make_block(10, B256::ZERO);
            let poller = CatchupPoller::new(1, 3, latest);
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                Duration::from_secs(100),
            );

            for expected in 1..=3u64 {
                let event = source.next().await.unwrap();
                let L2BlockEvent::Block(b) = event else { panic!("expected Block event") };
                assert_eq!(b.header.number, expected);
            }
        });
    }

    #[test]
    fn test_catchup_transient_error_retries_via_clock_sleep() {
        // The first unsafe_head() call during catchup returns a transient error.
        // The source must sleep via the Clock (not tokio::time::sleep) so that
        // the deterministic executor can advance virtual time and unblock the retry.
        // Under the deterministic Runner, tokio::time::sleep would never resolve
        // (no tokio timer driver), causing this test to stall — confirming the fix.
        Runner::start(Config::seeded(0), |ctx| async move {
            let block = make_block(1, B256::ZERO);
            let poller = FallibleCatchupPoller::new(block);
            let mut source =
                HybridBlockSource::new(ctx, pending_sub(), poller, Duration::from_secs(100));
            // First call fails (transient); clock.sleep fires after virtual time advances;
            // second call succeeds and delivers block #1.
            let event = source.next().await.unwrap();
            assert!(matches!(event, L2BlockEvent::Block(ref b) if b.header.number == 1));
        });
    }

    // --- Interval / polling path tests ---

    #[test]
    fn test_poll_fires_on_initial_tick() {
        // The interval's first tick fires immediately (at t=0), so the poller
        // is called without any time advance when the subscription has no pending events.
        Runner::start(Config::seeded(0), |ctx| async move {
            let block = make_block(1, B256::ZERO);
            let mut source = HybridBlockSource::new(
                ctx,
                pending_sub(),
                FixedPoller(block),
                Duration::from_secs(100),
            );
            let event = source.next().await.unwrap();
            assert!(matches!(event, L2BlockEvent::Block(ref b) if b.header.number == 1));
        });
    }

    #[test]
    fn test_poll_fires_again_after_time_advance() {
        // After the initial tick, the poller is not called again until virtual
        // time advances past the interval period. The deterministic executor
        // skips idle time automatically.
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut source = HybridBlockSource::new(
                ctx,
                pending_sub(),
                IncrementingPoller::new(),
                Duration::from_secs(1),
            );
            // First tick fires immediately — block #1.
            let event = source.next().await.unwrap();
            assert!(matches!(event, L2BlockEvent::Block(ref b) if b.header.number == 1));
            // Executor skips idle time to the next interval tick — block #2.
            let event = source.next().await.unwrap();
            assert!(matches!(event, L2BlockEvent::Block(ref b) if b.header.number == 2));
        });
    }

    #[test]
    fn test_transient_poller_error_is_swallowed() {
        // A SourceError::Provider from the poller is logged and ignored;
        // next() loops back into the select! waiting for the next tick
        // rather than propagating the error to the caller.
        Runner::start(Config::seeded(0), |ctx| async move {
            let block = make_block(1, B256::ZERO);
            let mut source = HybridBlockSource::new(
                ctx,
                pending_sub(),
                FalliblePoller::new(block),
                Duration::from_secs(1),
            );
            // Initial tick fires immediately; poller fails with a transient error
            // (swallowed). Executor skips idle time to the next tick; poller succeeds.
            let event = source.next().await.unwrap();
            assert!(matches!(event, L2BlockEvent::Block(ref b) if b.header.number == 1));
        });
    }

    #[test]
    fn test_fatal_poller_error_propagates() {
        // A non-Provider error from the poller is returned immediately from
        // next() rather than being swallowed.
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut source =
                HybridBlockSource::new(ctx, pending_sub(), ClosedPoller, Duration::from_secs(100));
            let err = source.next().await.unwrap_err();
            assert!(matches!(err, SourceError::Closed));
        });
    }
}
