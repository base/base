//! The L2 chain a test builds, and the block source that polls it for the driver the way the
//! production source polls its L2 node.
//!
//! Hand-rolled rather than built on [`PollingBlockSource`]: that one waits its poll interval
//! when a block is not there yet, so a push would not wake it and the driver could answer an
//! idle marker before taking the block. This source wakes on every push, which keeps "idle"
//! meaning "every pushed block was taken". The test `ChannelBlockSource` would not do either:
//! once delivered, a block is gone from it, so it cannot catch up from the safe head after a
//! reset.
//!
//! [`PollingBlockSource`]: base_batcher_source::PollingBlockSource

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base_batcher_source::{L2BlockEvent, UnsafeBlockSource};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use tokio::sync::watch;

/// The L2 chain a test builds for its batcher, shared between the test and the driver's
/// block source.
#[derive(Debug, Clone, Default)]
pub struct SharedL2Chain {
    /// The chain, one block per number.
    blocks: Arc<Mutex<Vec<BaseBlock>>>,
    /// Counts the pushes, so a waiting source polls again.
    pushes: watch::Sender<usize>,
}

impl SharedL2Chain {
    /// Create an empty chain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make `block` the head of the chain, as a node would: the blocks at or above its number
    /// are gone, so a block on another parent is a fork replacing them.
    pub fn push(&self, block: BaseBlock) {
        let mut blocks = self.blocks.lock().unwrap();
        blocks.retain(|kept| kept.header.number < block.header.number);
        blocks.push(block);
        self.pushes.send_modify(|pushes| *pushes += 1);
    }
}

/// [`UnsafeBlockSource`] over a [`SharedL2Chain`].
///
/// Like [`PollingBlockSource`](base_batcher_source::PollingBlockSource), it delivers the
/// block after its tip once the test has pushed it, reports a reorg when that block does not
/// build on the tip, and starts again above the safe head on `reset_catchup`. A reorg is
/// reported once per state of the chain: a reset that finds the same block again waits for
/// the next push, as production waits its poll interval, instead of reporting it forever.
/// Until that push the source is silent, even if the safe head moves meanwhile.
#[derive(Debug)]
pub struct HarnessBlockSource {
    /// The chain, shared with the test.
    blocks: Arc<Mutex<Vec<BaseBlock>>>,
    /// Wakes the source on every push, and parks it once the chain is dropped.
    pushes: watch::Receiver<usize>,
    /// The last delivered block, or the safe head the source last started from.
    tip: BlockInfo,
    /// The push count a reorg was last reported at.
    reorg_reported_at: Option<usize>,
}

impl HarnessBlockSource {
    /// Create a source over `chain`, starting above `safe_head`.
    pub fn new(chain: &SharedL2Chain, safe_head: BlockInfo) -> Self {
        Self {
            blocks: Arc::clone(&chain.blocks),
            pushes: chain.pushes.subscribe(),
            tip: safe_head,
            reorg_reported_at: None,
        }
    }
}

#[async_trait]
impl UnsafeBlockSource for HarnessBlockSource {
    async fn next(&mut self) -> L2BlockEvent {
        loop {
            let pushes = *self.pushes.borrow_and_update();
            let next = self
                .blocks
                .lock()
                .unwrap()
                .iter()
                .find(|block| block.header.number == self.tip.number + 1)
                .cloned();
            if let Some(block) = next {
                if block.header.parent_hash == self.tip.hash {
                    self.tip = BlockInfo::from(&block);
                    return L2BlockEvent::Block(Box::new(block));
                }
                if self.reorg_reported_at != Some(pushes) {
                    self.reorg_reported_at = Some(pushes);
                    return L2BlockEvent::Reorg;
                }
            }
            if self.pushes.changed().await.is_err() {
                std::future::pending().await
            }
        }
    }

    fn reset_catchup(&mut self, safe_head: BlockInfo) {
        self.tip = safe_head;
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, time::Duration};

    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use tokio::time::timeout;

    use super::*;

    /// A wait long enough for a source that had something to deliver to have done so.
    const SETTLE: Duration = Duration::from_millis(10);

    fn block(number: u64, parent_hash: B256) -> BaseBlock {
        BaseBlock {
            header: Header { number, parent_hash, ..Default::default() },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_push_wakes_the_waiting_source_and_blocks_come_in_order() {
        let chain = SharedL2Chain::new();
        let genesis = BlockInfo::default();
        let mut source = HarnessBlockSource::new(&chain, genesis);
        let first = block(1, genesis.hash);
        let second = block(2, first.header.hash_slow());

        let woken = {
            let mut next = pin!(source.next());
            assert!(timeout(SETTLE, &mut next).await.is_err(), "nothing pushed yet");
            chain.push(first.clone());
            chain.push(second.clone());
            timeout(SETTLE, next).await.expect("the push wakes the waiting source")
        };
        assert!(matches!(woken, L2BlockEvent::Block(block) if *block == first));
        assert!(matches!(source.next().await, L2BlockEvent::Block(block) if *block == second));
    }

    #[tokio::test]
    async fn a_fork_replaces_the_blocks_above_it_and_is_delivered_after_a_reset() {
        let chain = SharedL2Chain::new();
        let genesis = BlockInfo::default();
        let mut source = HarnessBlockSource::new(&chain, genesis);
        let first = block(1, genesis.hash);
        chain.push(first.clone());
        chain.push(block(2, first.header.hash_slow()));
        assert!(matches!(source.next().await, L2BlockEvent::Block(_)));
        assert!(matches!(source.next().await, L2BlockEvent::Block(_)));

        let mut fork = block(2, first.header.hash_slow());
        fork.header.timestamp = 1;
        let after_fork = block(3, fork.header.hash_slow());
        chain.push(fork.clone());
        chain.push(after_fork.clone());

        assert!(matches!(source.next().await, L2BlockEvent::Reorg));
        source.reset_catchup(genesis);
        assert!(matches!(source.next().await, L2BlockEvent::Block(block) if *block == first));
        assert!(matches!(source.next().await, L2BlockEvent::Block(block) if *block == fork));
        assert!(matches!(source.next().await, L2BlockEvent::Block(block) if *block == after_fork));
    }

    #[tokio::test]
    async fn a_reorg_is_reported_once_per_chain_state() {
        let chain = SharedL2Chain::new();
        let genesis = BlockInfo::default();
        let mut source = HarnessBlockSource::new(&chain, genesis);
        chain.push(block(1, B256::repeat_byte(0xab)));

        assert!(matches!(source.next().await, L2BlockEvent::Reorg));
        source.reset_catchup(genesis);
        let again = timeout(SETTLE, source.next()).await;
        assert!(again.is_err(), "the same reorg is not reported again");

        chain.push(block(1, B256::repeat_byte(0xcd)));
        assert!(matches!(source.next().await, L2BlockEvent::Reorg), "a new chain state");
        source.reset_catchup(genesis);
        chain.push(block(1, genesis.hash));
        assert!(matches!(source.next().await, L2BlockEvent::Block(_)));
    }

    #[tokio::test]
    async fn a_reorg_is_not_reported_again_after_replaying_the_blocks_below_it() {
        let chain = SharedL2Chain::new();
        let genesis = BlockInfo::default();
        let mut source = HarnessBlockSource::new(&chain, genesis);
        let first = block(1, genesis.hash);
        let second = block(2, first.header.hash_slow());
        chain.push(first.clone());
        chain.push(second.clone());
        chain.push(block(3, B256::repeat_byte(0xab)));

        assert!(matches!(source.next().await, L2BlockEvent::Block(block) if *block == first));
        assert!(matches!(source.next().await, L2BlockEvent::Block(block) if *block == second));
        assert!(matches!(source.next().await, L2BlockEvent::Reorg));

        // The driver resets to the safe head and takes blocks 1 and 2 again.
        source.reset_catchup(genesis);
        assert!(matches!(source.next().await, L2BlockEvent::Block(block) if *block == first));
        assert!(matches!(source.next().await, L2BlockEvent::Block(block) if *block == second));
        let again = timeout(SETTLE, source.next()).await;
        assert!(again.is_err(), "the same reorg is not reported again");
    }

    #[tokio::test]
    async fn parks_once_the_chain_is_dropped() {
        let chain = SharedL2Chain::new();
        let mut source = HarnessBlockSource::new(&chain, BlockInfo::default());
        drop(chain);

        let next = timeout(SETTLE, source.next()).await;
        assert!(next.is_err(), "a source without a chain must park");
    }
}
