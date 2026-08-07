//! Ordered polling source for L2 blocks.

use std::time::Duration;

use async_trait::async_trait;
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::Clock;

use crate::{L2BlockEvent, SourceError, UnsafeBlockSource};

/// A provider that can fetch an L2 block by number.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PollingSource: Send + Sync {
    /// Fetch `number`, returning [`SourceError::BlockUnavailable`] when the
    /// provider does not have it yet.
    async fn block_by_number(&self, number: u64) -> Result<BaseBlock, SourceError>;
}

/// Delivers consecutive L2 blocks above a trusted safe head.
#[derive(derive_more::Debug)]
pub struct PollingBlockSource<P, C> {
    #[debug(skip)]
    poller: P,
    #[debug(skip)]
    clock: C,
    poll_interval: Duration,
    tip: BlockInfo,
    next_poll_at: Duration,
}

impl<P, C> PollingBlockSource<P, C>
where
    P: PollingSource,
    C: Clock,
{
    /// Create a source anchored to `safe_head`.
    pub fn new(clock: C, poller: P, safe_head: BlockInfo, poll_interval: Duration) -> Self {
        let next_poll_at = clock.now();
        Self { poller, clock, poll_interval, tip: safe_head, next_poll_at }
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
            self.next_poll_at = self.clock.now().saturating_add(self.poll_interval);
            return L2BlockEvent::Reorg;
        }

        self.tip = BlockInfo::from(&block);
        self.next_poll_at = self.clock.now();
        L2BlockEvent::Block(Box::new(block))
    }
}

#[async_trait]
impl<P, C> UnsafeBlockSource for PollingBlockSource<P, C>
where
    P: PollingSource,
    C: Clock,
{
    fn reset_catchup(&mut self, safe_head: BlockInfo) {
        self.tip = safe_head;
        self.next_poll_at = self.clock.now().saturating_add(self.poll_interval);
    }

    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        let delay = self.next_poll_at.saturating_sub(self.clock.now());
        if !delay.is_zero() {
            self.clock.sleep(delay).await;
        }

        loop {
            let expected = self.tip.number.saturating_add(1);
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
                }
                Err(SourceError::BlockUnavailable(_)) => {}
                Err(SourceError::Provider(error)) => {
                    tracing::warn!(error = %error, "failed to poll next L2 block");
                }
                Err(error) => return Err(error),
            }

            self.next_poll_at = self.clock.now().saturating_add(self.poll_interval);
            self.clock.sleep(self.poll_interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_runtime::{Config, Runner};
    use mockall::{Sequence, predicate::eq};

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

    #[test]
    fn polls_consecutive_block_numbers() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(3);
            let mut poller = MockPollingSource::new();
            let mut sequence = Sequence::new();
            for expected in 1..=3 {
                let block = blocks[expected as usize].clone();
                poller
                    .expect_block_by_number()
                    .with(eq(expected))
                    .once()
                    .in_sequence(&mut sequence)
                    .return_once(move |_| Ok(block));
            }
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
        });
    }

    #[test]
    fn wrong_parent_is_reported_as_reorg() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(1);
            let bad = block(1, B256::with_last_byte(1), 1);
            let mut poller = MockPollingSource::new();
            poller.expect_block_by_number().with(eq(1)).once().return_once(move |_| Ok(bad));
            let mut source = PollingBlockSource::new(
                ctx,
                poller,
                BlockInfo::from(&blocks[0]),
                Duration::from_secs(1),
            );

            assert!(matches!(source.next().await.unwrap(), L2BlockEvent::Reorg));
        });
    }

    #[test]
    fn temporary_poll_errors_are_retried() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(1);
            let mut poller = MockPollingSource::new();
            let mut sequence = Sequence::new();
            poller
                .expect_block_by_number()
                .with(eq(1))
                .once()
                .in_sequence(&mut sequence)
                .return_once(|_| Err(SourceError::BlockUnavailable(1)));
            poller
                .expect_block_by_number()
                .with(eq(1))
                .once()
                .in_sequence(&mut sequence)
                .return_once(|_| Err(SourceError::Provider("temporary failure".into())));
            let block = blocks[1].clone();
            poller
                .expect_block_by_number()
                .with(eq(1))
                .once()
                .in_sequence(&mut sequence)
                .return_once(move |_| Ok(block));
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
    fn retry_deadline_survives_cancelled_next_call() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(1);
            let mut poller = MockPollingSource::new();
            let mut sequence = Sequence::new();
            poller
                .expect_block_by_number()
                .with(eq(1))
                .once()
                .in_sequence(&mut sequence)
                .return_once(|_| Err(SourceError::BlockUnavailable(1)));
            let block = blocks[1].clone();
            poller
                .expect_block_by_number()
                .with(eq(1))
                .once()
                .in_sequence(&mut sequence)
                .return_once(move |_| Ok(block));

            let interval = Duration::from_secs(1);
            let mut source =
                PollingBlockSource::new(ctx.clone(), poller, BlockInfo::from(&blocks[0]), interval);
            let started_at = ctx.now();

            tokio::select! {
                biased;
                result = source.next() => panic!("unexpected polling result: {result:?}"),
                _ = std::future::ready(()) => {}
            }

            let L2BlockEvent::Block(block) = source.next().await.unwrap() else {
                panic!("expected block");
            };
            assert_eq!(block.header.number, 1);
            assert_eq!(ctx.now(), started_at + interval);
        });
    }

    #[test]
    fn reset_reanchors_sequential_polling() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = chain(6);
            let mut poller = MockPollingSource::new();
            let block = blocks[6].clone();
            poller.expect_block_by_number().with(eq(6)).once().return_once(move |_| Ok(block));
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
        });
    }
}
