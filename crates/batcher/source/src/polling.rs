//! Ordered polling source for L2 blocks.

use std::time::Duration;

use async_trait::async_trait;
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::Clock;
use futures::{StreamExt, stream::BoxStream};

use crate::{L2BlockEvent, SourceError, UnsafeBlockSource};

/// A provider that can fetch an L2 block by number.
#[async_trait]
pub trait PollingSource: Send + Sync {
    /// Fetch `number`, returning [`SourceError::BlockUnavailable`] when the
    /// provider does not have it yet.
    async fn block_by_number(&self, number: u64) -> Result<BaseBlock, SourceError>;
}

/// Delivers consecutive L2 blocks above a trusted safe head.
#[derive(derive_more::Debug)]
pub struct PollingBlockSource<P> {
    #[debug(skip)]
    poller: P,
    #[debug(skip)]
    interval: BoxStream<'static, ()>,
    tip: BlockInfo,
    poll_immediately: bool,
}

impl<P: PollingSource> PollingBlockSource<P> {
    /// Create a source anchored to `safe_head`.
    pub fn new<C: Clock>(
        clock: C,
        poller: P,
        safe_head: BlockInfo,
        poll_interval: Duration,
    ) -> Self {
        Self {
            poller,
            interval: clock.interval(poll_interval),
            tip: safe_head,
            poll_immediately: false,
        }
    }

    const fn next_number(&self) -> u64 {
        self.tip.number.saturating_add(1)
    }

    fn process(&mut self, block: BaseBlock) -> L2BlockEvent {
        let number = block.header.number;
        if block.header.parent_hash != self.tip.hash {
            tracing::warn!(
                block = %number,
                expected_parent = %self.tip.hash,
                received_parent = %block.header.parent_hash,
                "L2 reorg detected from parent hash mismatch"
            );
            self.poll_immediately = false;
            return L2BlockEvent::Reorg;
        }

        self.tip = BlockInfo::from(&block);
        L2BlockEvent::Block(Box::new(block))
    }
}

#[async_trait]
impl<P: PollingSource> UnsafeBlockSource for PollingBlockSource<P> {
    fn reset_catchup(&mut self, safe_head: BlockInfo) {
        self.tip = safe_head;
        self.poll_immediately = false;
    }

    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        loop {
            if !self.poll_immediately {
                self.interval.next().await.ok_or(SourceError::Closed)?;
                self.poll_immediately = true;
            }

            let expected = self.next_number();
            match self.poller.block_by_number(expected).await {
                Ok(block) if block.header.number == expected => {
                    return Ok(self.process(block));
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

    use super::*;

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
            let (poller, requests) = MapPoller::new(blocks[1..].iter().cloned());
            let mut source = PollingBlockSource::new(
                ctx,
                poller,
                BlockInfo::from(&blocks[0]),
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
    fn wrong_parent_is_reported_as_reorg() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(1);
            let bad = block(1, B256::with_last_byte(1), 1);
            let (poller, _) = MapPoller::new([bad]);
            let mut source = PollingBlockSource::new(
                ctx,
                poller,
                BlockInfo::from(&blocks[0]),
                Duration::from_secs(1),
            );

            assert!(matches!(source.next().await.unwrap(), L2BlockEvent::Reorg));
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
            let mut source = PollingBlockSource::new(
                ctx,
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
            let mut source = PollingBlockSource::new(
                ctx,
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
