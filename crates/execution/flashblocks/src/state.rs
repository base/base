//! Flashblocks state management.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use alloy_consensus::Header;
use arc_swap::{ArcSwapOption, Guard};
use base_common_chains::Upgrades;
use base_common_consensus::BaseBlock;
use base_common_flashblocks::Flashblock;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{BlockReaderIdExt, StateProviderFactory};
use tokio::sync::{
    Mutex,
    broadcast::{self, Sender},
    mpsc,
};

use crate::{
    FlashblocksAPI, FlashblocksReceiver, PendingBlocks,
    metrics::Metrics,
    processor::{StateProcessor, StateUpdate},
};

// Buffer 4s of flashblocks for flashblock_sender
const BUFFER_SIZE: usize = 20;

/// Manages the pending flashblock state and processes incoming updates.
#[derive(Debug)]
pub struct FlashblocksState {
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    queue: mpsc::UnboundedSender<StateUpdate>,
    rx: Arc<Mutex<mpsc::UnboundedReceiver<StateUpdate>>>,
    flashblock_sender: Sender<Arc<PendingBlocks>>,
    max_pending_blocks_depth: u64,
    last_canonical_block: AtomicU64,
}

impl FlashblocksState {
    /// Creates a new flashblocks state manager.
    ///
    /// The state is created without a client. Call [`start`](Self::start) with a client
    /// to spawn the state processor after the node is launched.
    pub fn new(max_pending_blocks_depth: u64) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<StateUpdate>();
        let pending_blocks: Arc<ArcSwapOption<PendingBlocks>> = Arc::new(ArcSwapOption::new(None));
        let (flashblock_sender, _) = broadcast::channel(BUFFER_SIZE);

        Self {
            pending_blocks,
            queue: tx,
            rx: Arc::new(Mutex::new(rx)),
            flashblock_sender,
            max_pending_blocks_depth,
            last_canonical_block: AtomicU64::new(0),
        }
    }

    /// Starts the flashblocks state processor with the given client.
    ///
    /// This spawns a background task that processes canonical blocks and flashblocks.
    /// Should be called after the node is launched and the provider is available.
    pub fn start<Client>(&self, client: Client)
    where
        Client: StateProviderFactory
            + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + Upgrades>
            + BlockReaderIdExt<Header = Header>
            + Clone
            + 'static,
    {
        let state_processor = StateProcessor::new(
            client,
            Arc::clone(&self.pending_blocks),
            self.max_pending_blocks_depth,
            Arc::clone(&self.rx),
            self.flashblock_sender.clone(),
        );

        tokio::spawn(async move {
            state_processor.start().await;
        });
    }

    /// Drops the published snapshot when it is anchored more than `max_pending_blocks_depth`
    /// blocks behind `canonical_block_number`.
    ///
    /// [`StateProcessor`] enforces the same bound, but only once it reaches the matching queue
    /// entry. Repeating it on the task that receives the notification is what keeps staleness
    /// bounded by chain progress rather than by processor progress: a snapshot cannot stay
    /// readable through a long apply merely because the processor has not dequeued the
    /// notification yet.
    fn drop_pending_behind(&self, canonical_block_number: u64) {
        let published = self.pending_blocks.load();
        let Some(stale) = published.as_ref() else { return };

        // Measured from the earliest pending block so this matches the bound the processor and
        // the reconciler apply, and the three cannot disagree about which snapshots survive.
        let earliest_pending_block = stale.earliest_block_number();
        if canonical_block_number.saturating_sub(earliest_pending_block)
            <= self.max_pending_blocks_depth
        {
            return;
        }

        // Clear only the snapshot that was judged. The processor publishes concurrently, and
        // anything it published after the load above is anchored on a later tip than this one.
        // Losing this race costs nothing, because an absent snapshot is always safe to serve.
        let current = self.pending_blocks.compare_and_swap(&published, None);
        if !current.as_ref().is_some_and(|current| Arc::ptr_eq(current, stale)) {
            return;
        }

        debug!(
            message = "dropping pending snapshot anchored too far behind the canonical tip",
            canonical_block_number,
            earliest_pending_block,
            max_depth = self.max_pending_blocks_depth,
        );
        Metrics::pending_drop_stale().increment(1);
    }

    /// Handles a canonical block being received.
    pub fn on_canonical_block_received(&self, block: RecoveredBlock<BaseBlock>) {
        let block_number = block.number;

        // Deliberately not `fetch_max`: a reorg can move the tip down, and keeping the higher
        // height would suppress every flashblock built on the replacement chain.
        self.last_canonical_block.store(block_number, Ordering::Relaxed);
        self.drop_pending_behind(block_number);

        match self.queue.send(StateUpdate::Canonical(block)) {
            Ok(_) => {
                info!(message = "added canonical block to processing queue", block_number)
            }
            Err(e) => {
                error!(message = "could not add canonical block to processing queue", block_number, error = %e);
            }
        }
    }
}

impl FlashblocksReceiver for FlashblocksState {
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        let flashblock_index = flashblock.index;
        let block_number = flashblock.metadata.block_number;

        // Rejecting superseded payloads here keeps them out of the queue entirely, so a backlog
        // cannot grow on work that could never produce a publishable snapshot. The processor
        // repeats the check for payloads that were fresh on arrival but went stale while queued.
        if block_number <= self.last_canonical_block.load(Ordering::Relaxed) {
            debug!(
                message = "dropping flashblock for an already canonical block",
                block_number, flashblock_index,
            );
            Metrics::flashblock_superseded().increment(1);
            return;
        }

        match self.queue.send(StateUpdate::Flashblock(flashblock)) {
            Ok(_) => {
                debug!(
                    message = "added flashblock to processing queue",
                    block_number, flashblock_index,
                );
            }
            Err(e) => {
                error!(message = "could not add flashblock to processing queue", block_number, flashblock_index, error = %e);
            }
        }
    }
}

impl Default for FlashblocksState {
    fn default() -> Self {
        Self::new(10)
    }
}

impl FlashblocksAPI for FlashblocksState {
    fn get_pending_blocks(&self) -> Guard<Option<Arc<PendingBlocks>>> {
        self.pending_blocks.load()
    }

    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Arc<PendingBlocks>> {
        self.flashblock_sender.subscribe()
    }
}

impl FlashblocksState {
    /// Sets the pending blocks directly for testing purposes.
    ///
    /// This bypasses the normal flashblock processing pipeline and allows
    /// tests to inject a pre-built `PendingBlocks` state.
    pub fn set_pending_blocks_for_testing(&self, pending_blocks: Option<PendingBlocks>) {
        self.pending_blocks.store(pending_blocks.map(Arc::new));
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Block, BlockBody, Sealed};
    use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Metadata,
    };

    use super::*;
    use crate::PendingBlocksBuilder;

    const MAX_DEPTH: u64 = 3;

    fn flashblock_for_block(block_number: u64) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                timestamp: 1_700_000_000,
                extra_data: Bytes::default(),
                base_fee_per_gas: U256::from(1_000_000_000u64),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 21_000,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata::new(block_number),
        }
    }

    /// Builds a snapshot whose earliest pending block is `block_number`, which is the height
    /// the staleness bound is measured from.
    fn pending_anchored_at(block_number: u64) -> PendingBlocks {
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([flashblock_for_block(block_number)]);
        builder.with_header(Sealed::new_unchecked(
            Header { number: block_number, ..Default::default() },
            B256::ZERO,
        ));
        builder.build().expect("pending fixture builds")
    }

    fn canonical_block(block_number: u64) -> RecoveredBlock<BaseBlock> {
        RecoveredBlock::new_unhashed(
            Block {
                header: Header { number: block_number, ..Default::default() },
                body: BlockBody::default(),
            },
            vec![],
        )
    }

    /// The processor is never started here, so only the notification path can clear the
    /// snapshot. That is the property that bounds staleness while the processor is busy.
    #[test]
    fn canonical_notification_drops_stale_pending_without_the_processor() {
        let state = FlashblocksState::new(MAX_DEPTH);
        state.set_pending_blocks_for_testing(Some(pending_anchored_at(1)));

        state.on_canonical_block_received(canonical_block(1 + MAX_DEPTH + 1));

        assert!(
            state.get_pending_blocks().is_none(),
            "a snapshot anchored past max_pending_blocks_depth must not survive the notification"
        );
    }

    #[test]
    fn canonical_notification_keeps_pending_within_max_depth() {
        let state = FlashblocksState::new(MAX_DEPTH);
        state.set_pending_blocks_for_testing(Some(pending_anchored_at(1)));

        state.on_canonical_block_received(canonical_block(1 + MAX_DEPTH));

        assert!(
            state.get_pending_blocks().is_some(),
            "normal lag must not clear pending, or every notification would drop live state"
        );
    }

    #[tokio::test]
    async fn superseded_flashblock_never_enters_the_queue() {
        let state = FlashblocksState::new(MAX_DEPTH);
        state.on_canonical_block_received(canonical_block(7));

        state.on_flashblock_received(flashblock_for_block(3));

        let mut rx = state.rx.lock().await;
        assert!(
            matches!(rx.try_recv(), Ok(StateUpdate::Canonical(_))),
            "the canonical notification itself is still queued for reconciliation"
        );
        assert!(
            rx.try_recv().is_err(),
            "a flashblock for an already canonical block must not consume queue capacity"
        );
    }

    #[tokio::test]
    async fn flashblock_ahead_of_canonical_enters_the_queue() {
        let state = FlashblocksState::new(MAX_DEPTH);
        state.on_canonical_block_received(canonical_block(7));

        state.on_flashblock_received(flashblock_for_block(8));

        let mut rx = state.rx.lock().await;
        assert!(matches!(rx.try_recv(), Ok(StateUpdate::Canonical(_))));
        assert!(
            matches!(rx.try_recv(), Ok(StateUpdate::Flashblock(_))),
            "flashblocks extending the tip must still reach the processor"
        );
    }
}
