//! Ordered L2 block source backed by subscription and RPC polling.

use std::time::Duration;

use async_trait::async_trait;
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::Clock;
use futures::{StreamExt, stream::BoxStream};

use crate::{BlockSubscription, L2BlockEvent, PollingSource, SourceError, UnsafeBlockSource};

/// Delivers every L2 block in order above a trusted safe head.
///
/// Subscription events provide low-latency updates. RPC polling always fetches
/// the exact next height, so delayed or skipped subscription events cannot
/// create holes in the stream delivered to the batcher.
#[derive(derive_more::Debug)]
pub struct HybridBlockSource<S, P> {
    #[debug(skip)]
    sub: Option<BoxStream<'static, Result<BaseBlock, SourceError>>>,
    #[debug(skip)]
    _subscription: S,
    #[debug(skip)]
    poller: P,
    #[debug(skip)]
    interval: BoxStream<'static, ()>,
    tip: BlockInfo,
    poll_immediately: bool,
}

impl<S, P> HybridBlockSource<S, P>
where
    S: BlockSubscription,
    P: PollingSource,
{
    /// Create a source anchored to `safe_head`.
    pub fn new<C: Clock>(
        clock: C,
        mut subscription: S,
        poller: P,
        safe_head: BlockInfo,
        poll_interval: Duration,
    ) -> Self {
        let sub = subscription.take_stream();
        let interval = clock.interval(poll_interval);
        Self {
            sub: Some(sub),
            _subscription: subscription,
            poller,
            interval,
            tip: safe_head,
            poll_immediately: false,
        }
    }

    const fn next_number(&self) -> u64 {
        self.tip.number.saturating_add(1)
    }

    fn process(&mut self, block: BaseBlock) -> Option<L2BlockEvent> {
        let number = block.header.number;
        let hash = block.header.hash_slow();

        if number < self.tip.number {
            return None;
        }
        if number == self.tip.number {
            if hash == self.tip.hash {
                return None;
            }
            tracing::warn!(block = %number, "L2 reorg detected at current block height");
            return Some(L2BlockEvent::Reorg);
        }

        let expected = self.next_number();
        if number > expected {
            tracing::warn!(
                expected = %expected,
                received = %number,
                "L2 block delivery gap detected; fetching missing blocks"
            );
            self.poll_immediately = true;
            return None;
        }

        if block.header.parent_hash != self.tip.hash {
            tracing::warn!(
                block = %number,
                expected_parent = %self.tip.hash,
                received_parent = %block.header.parent_hash,
                "L2 reorg detected from parent hash mismatch"
            );
            return Some(L2BlockEvent::Reorg);
        }

        self.tip = BlockInfo::from(&block);
        Some(L2BlockEvent::Block(Box::new(block)))
    }
}

#[async_trait]
impl<S, P> UnsafeBlockSource for HybridBlockSource<S, P>
where
    S: BlockSubscription,
    P: PollingSource,
{
    fn reset_catchup(&mut self, safe_head: BlockInfo) {
        self.tip = safe_head;
        self.poll_immediately = false;
    }

    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        loop {
            if self.poll_immediately {
                let expected = self.next_number();
                match self.poller.block_by_number(expected).await {
                    Ok(block) if block.header.number == expected => {
                        if let Some(event) = self.process(block) {
                            return Ok(event);
                        }
                    }
                    Ok(block) => {
                        tracing::warn!(
                            expected = %expected,
                            received = %block.header.number,
                            "L2 RPC returned an unexpected block number"
                        );
                        self.poll_immediately = false;
                    }
                    Err(SourceError::BlockUnavailable(_)) => {
                        self.poll_immediately = false;
                    }
                    Err(SourceError::Provider(error)) => {
                        tracing::warn!(%error, "failed to poll next L2 block");
                        self.poll_immediately = false;
                    }
                    Err(error) => return Err(error),
                }
                if self.poll_immediately {
                    continue;
                }
            }

            tokio::select! {
                block = async {
                    match self.sub.as_mut() {
                        Some(sub) => sub.next().await,
                        None => std::future::pending().await,
                    }
                } => match block {
                    Some(Ok(block)) => {
                        if let Some(event) = self.process(block) {
                            return Ok(event);
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "L2 subscription failed; using polling");
                        self.sub = None;
                        self.poll_immediately = true;
                    }
                    None => {
                        self.sub = None;
                        self.poll_immediately = true;
                    }
                },
                _ = self.interval.next() => {
                    self.poll_immediately = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use alloy_primitives::B256;
    use base_runtime::{Config, Runner};
    use futures::{StreamExt, stream::BoxStream};

    use super::*;

    struct StreamSub(BoxStream<'static, Result<BaseBlock, SourceError>>);

    impl BlockSubscription for StreamSub {
        fn take_stream(&mut self) -> BoxStream<'static, Result<BaseBlock, SourceError>> {
            std::mem::replace(&mut self.0, futures::stream::pending().boxed())
        }
    }

    fn pending_sub() -> StreamSub {
        StreamSub(futures::stream::pending().boxed())
    }

    fn block(number: u64, parent_hash: B256, marker: u8) -> BaseBlock {
        BaseBlock {
            header: alloy_consensus::Header {
                number,
                parent_hash,
                extra_data: vec![marker].into(),
                ..Default::default()
            },
            body: Default::default(),
        }
    }

    fn chain(length: u64) -> Vec<BaseBlock> {
        let mut blocks = Vec::new();
        let mut parent_hash = B256::ZERO;
        for number in 0..=length {
            let next = block(number, parent_hash, number as u8);
            parent_hash = next.header.hash_slow();
            blocks.push(next);
        }
        blocks
    }

    struct MapPoller {
        blocks: BTreeMap<u64, BaseBlock>,
        requests: Arc<Mutex<Vec<u64>>>,
    }

    impl MapPoller {
        fn new(blocks: impl IntoIterator<Item = BaseBlock>) -> (Self, Arc<Mutex<Vec<u64>>>) {
            let requests = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    blocks: blocks.into_iter().map(|block| (block.header.number, block)).collect(),
                    requests: Arc::clone(&requests),
                },
                requests,
            )
        }
    }

    #[async_trait]
    impl PollingSource for MapPoller {
        async fn block_by_number(&self, number: u64) -> Result<BaseBlock, SourceError> {
            self.requests.lock().unwrap().push(number);
            self.blocks.get(&number).cloned().ok_or(SourceError::BlockUnavailable(number))
        }
    }

    #[test]
    fn polls_consecutive_block_numbers() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(3);
            let safe_head = BlockInfo::from(&blocks[0]);
            let (poller, requests) = MapPoller::new(blocks[1..].iter().cloned());
            let mut source = HybridBlockSource::new(
                ctx,
                pending_sub(),
                poller,
                safe_head,
                Duration::from_secs(1),
            );

            for expected in 1..=3 {
                let L2BlockEvent::Block(block) = source.next().await.unwrap() else {
                    panic!("expected block");
                };
                assert_eq!(block.header.number, expected);
            }
            assert_eq!(*requests.lock().unwrap(), vec![1, 2, 3]);
        });
    }

    #[test]
    fn closed_subscription_falls_back_to_polling() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(1);
            let (poller, requests) = MapPoller::new([blocks[1].clone()]);
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(futures::stream::empty().boxed()),
                poller,
                BlockInfo::from(&blocks[0]),
                Duration::from_secs(1),
            );

            let L2BlockEvent::Block(block) = source.next().await.unwrap() else {
                panic!("expected block");
            };
            assert_eq!(block.header.number, 1);
            assert_eq!(*requests.lock().unwrap(), vec![1]);
        });
    }

    #[test]
    fn subscription_gap_is_backfilled_before_future_block() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(3);
            let safe_head = BlockInfo::from(&blocks[0]);
            let stream = futures::stream::iter(vec![Ok(blocks[3].clone())]);
            let (poller, requests) = MapPoller::new(blocks[1..].iter().cloned());
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                safe_head,
                Duration::from_secs(100),
            );
            source.poll_immediately = false;

            for expected in 1..=3 {
                let L2BlockEvent::Block(block) = source.next().await.unwrap() else {
                    panic!("gap must not be reported as a reorg");
                };
                assert_eq!(block.header.number, expected);
            }
            assert_eq!(*requests.lock().unwrap(), vec![1, 2, 3]);
        });
    }

    #[test]
    fn wrong_parent_is_reported_as_reorg() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let safe = chain(0).remove(0);
            let bad = block(1, B256::with_last_byte(1), 1);
            let (poller, _) = MapPoller::new([bad]);
            let mut source = HybridBlockSource::new(
                ctx,
                pending_sub(),
                poller,
                BlockInfo::from(&safe),
                Duration::from_secs(1),
            );

            assert!(matches!(source.next().await.unwrap(), L2BlockEvent::Reorg));
        });
    }

    #[test]
    fn same_height_conflict_is_reported_as_reorg() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let safe = chain(0).remove(0);
            let conflict = block(0, B256::ZERO, 99);
            let stream = futures::stream::iter(vec![Ok(conflict)]);
            let (poller, _) = MapPoller::new([]);
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                BlockInfo::from(&safe),
                Duration::from_secs(100),
            );
            source.poll_immediately = false;

            assert!(matches!(source.next().await.unwrap(), L2BlockEvent::Reorg));
        });
    }

    #[test]
    fn duplicate_subscription_block_is_ignored() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(1);
            let stream = futures::stream::iter(vec![Ok(blocks[0].clone()), Ok(blocks[1].clone())]);
            let (poller, _) = MapPoller::new([]);
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                BlockInfo::from(&blocks[0]),
                Duration::from_secs(100),
            );
            source.poll_immediately = false;

            let L2BlockEvent::Block(block) = source.next().await.unwrap() else {
                panic!("expected block");
            };
            assert_eq!(block.header.number, 1);
        });
    }

    #[test]
    fn unavailable_subscription_block_falls_back_to_polling() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(1);
            let stream = futures::stream::iter(vec![Err(SourceError::BlockUnavailable(1))]);
            let (poller, requests) = MapPoller::new([blocks[1].clone()]);
            let mut source = HybridBlockSource::new(
                ctx,
                StreamSub(stream.boxed()),
                poller,
                BlockInfo::from(&blocks[0]),
                Duration::from_secs(100),
            );
            source.poll_immediately = false;

            let L2BlockEvent::Block(block) = source.next().await.unwrap() else {
                panic!("expected block");
            };
            assert_eq!(block.header.number, 1);
            assert_eq!(*requests.lock().unwrap(), vec![1]);
        });
    }

    struct FailOncePoller {
        failed: AtomicBool,
        block: BaseBlock,
    }

    #[async_trait]
    impl PollingSource for FailOncePoller {
        async fn block_by_number(&self, _: u64) -> Result<BaseBlock, SourceError> {
            if !self.failed.swap(true, Ordering::Relaxed) {
                return Err(SourceError::Provider("temporary failure".into()));
            }
            Ok(self.block.clone())
        }
    }

    #[test]
    fn polling_provider_error_is_retried() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(1);
            let poller =
                FailOncePoller { failed: AtomicBool::new(false), block: blocks[1].clone() };
            let mut source = HybridBlockSource::new(
                ctx,
                pending_sub(),
                poller,
                BlockInfo::from(&blocks[0]),
                Duration::from_secs(1),
            );

            let L2BlockEvent::Block(block) = source.next().await.unwrap() else {
                panic!("expected block");
            };
            assert_eq!(block.header.number, 1);
        });
    }

    #[test]
    fn reset_reanchors_sequential_polling() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(6);
            let (poller, requests) = MapPoller::new([blocks[6].clone()]);
            let mut source = HybridBlockSource::new(
                ctx,
                pending_sub(),
                poller,
                BlockInfo::from(&blocks[0]),
                Duration::from_secs(1),
            );
            source.reset_catchup(BlockInfo::from(&blocks[5]));

            let L2BlockEvent::Block(block) = source.next().await.unwrap() else {
                panic!("expected block");
            };
            assert_eq!(block.header.number, 6);
            assert_eq!(*requests.lock().unwrap(), vec![6]);
        });
    }
}
