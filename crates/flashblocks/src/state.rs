//! Flashblocks state management.

use std::sync::Arc;

use alloy_consensus::Header;
use arc_swap::{ArcSwapOption, Guard};
use reth::{
    chainspec::{ChainSpecProvider, EthChainSpec},
    providers::{BlockReaderIdExt, StateProviderFactory},
};
use reth_optimism_chainspec::OpHardforks;
use reth_optimism_primitives::OpBlock;
use reth_primitives::RecoveredBlock;
use tokio::sync::{
    Mutex,
    broadcast::{self, Sender},
    mpsc,
};
use tracing::{error, info};

use crate::{
    Flashblock, FlashblocksAPI, FlashblocksReceiver, PendingBlocks,
    processor::{StateProcessor, StateUpdate},
};

// Buffer 4s of flashblocks for flashblock_sender
const BUFFER_SIZE: usize = 20;

/// Manages the pending flashblock state and processes incoming updates.
#[derive(Debug, Clone)]
pub struct FlashblocksState<Client> {
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    queue: mpsc::UnboundedSender<StateUpdate>,
    flashblock_sender: Sender<Arc<PendingBlocks>>,
    state_processor: StateProcessor<Client>,
}

impl<Client> FlashblocksState<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + 'static,
{
    /// Creates a new flashblocks state manager.
    pub fn new(client: Client, max_pending_blocks_depth: u64) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<StateUpdate>();
        let pending_blocks: Arc<ArcSwapOption<PendingBlocks>> = Arc::new(ArcSwapOption::new(None));
        let (flashblock_sender, _) = broadcast::channel(BUFFER_SIZE);
        let state_processor = StateProcessor::new(
            client,
            pending_blocks.clone(),
            max_pending_blocks_depth,
            Arc::new(Mutex::new(rx)),
            flashblock_sender.clone(),
        );

        Self { pending_blocks, queue: tx, flashblock_sender, state_processor }
    }

    /// Starts the flashblocks state processor.
    pub fn start(&self) {
        let sp = self.state_processor.clone();
        tokio::spawn(async move {
            sp.start().await;
        });
    }

    /// Handles a canonical block being received.
    pub fn on_canonical_block_received(&self, block: RecoveredBlock<OpBlock>) {
        let block_number = block.number;
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

impl<Client> FlashblocksReceiver for FlashblocksState<Client> {
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        let flashblock_index = flashblock.index;
        let block_number = flashblock.metadata.block_number;
        match self.queue.send(StateUpdate::Flashblock(flashblock)) {
            Ok(_) => {
                info!(
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

impl<Client> FlashblocksAPI for FlashblocksState<Client> {
    fn get_pending_blocks(&self) -> Guard<Option<Arc<PendingBlocks>>> {
        self.pending_blocks.load()
    }

    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Arc<PendingBlocks>> {
        self.flashblock_sender.subscribe()
    }
}
