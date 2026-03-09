use alloy_primitives::{Address, Bytes};
use base_batcher_driver::{ChannelDriver, ChannelDriverConfig, ChannelDriverError};
use base_consensus_genesis::RollupConfig;
use base_protocol::{DERIVATION_VERSION_0, Frame, SingleBatch};
use tracing::info;

use crate::{Action, L1Miner, L2BlockProvider, PendingTx};

/// Configuration for the [`Batcher`] actor.
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// Address of the batcher account. Used as the `from` field on L1
    /// transactions so the derivation pipeline can filter by sender.
    pub batcher_address: Address,
    /// Batch inbox address on L1. Used as the `to` field on L1 transactions.
    pub inbox_address: Address,
    /// [`ChannelDriverConfig`] passed through to the underlying encoder.
    pub driver: ChannelDriverConfig,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        Self {
            batcher_address: Address::repeat_byte(0xBA),
            inbox_address: Address::repeat_byte(0xCA),
            driver: ChannelDriverConfig::default(),
        }
    }
}

/// Errors returned by [`Batcher::advance`].
#[derive(Debug, thiserror::Error)]
pub enum BatcherError {
    /// The L2 source was exhausted before any blocks could be batched.
    #[error("no L2 blocks available to batch")]
    NoBlocks,
    /// The channel driver failed to encode or compress the batches.
    #[error("channel driver error: {0}")]
    Driver(#[from] ChannelDriverError),
}

/// Batcher actor for action tests.
///
/// `Batcher` drains [`MockL2Block`]s from an [`L2BlockProvider`], encodes each
/// one as a [`SingleBatch`], compresses all batches for the current cycle into
/// a channel via [`ChannelDriver`] (Brotli-10), and submits the resulting
/// frame data to the [`L1Miner`] as a [`PendingTx`].
///
/// A single call to [`advance`] (or [`Action::act`]) runs one full encode
/// cycle: drain all available L2 blocks → encode → flush → submit to L1.
/// Callers then mine an L1 block to include the submitted transactions.
///
/// # Example
///
/// ```rust,ignore
/// let mut batcher = Batcher::new(&mut h.l1, &mut h.l2, &h.rollup_config, BatcherConfig::default());
/// let frames = batcher.advance().unwrap();
/// h.l1.mine_block(); // include the batcher tx
/// ```
///
/// [`advance`]: Batcher::advance
/// [`MockL2Block`]: crate::MockL2Block
#[derive(Debug)]
pub struct Batcher<'a, S: L2BlockProvider> {
    l1_miner: &'a mut L1Miner,
    l2_source: S,
    driver: ChannelDriver,
    config: BatcherConfig,
}

impl<'a, S: L2BlockProvider> Batcher<'a, S> {
    /// Create a new [`Batcher`].
    ///
    /// The batcher borrows `l1_miner` mutably so it can submit transactions
    /// directly. `l2_source` is moved in so the batcher owns the block queue.
    /// `rollup_config` is cloned into the [`ChannelDriver`].
    pub fn new(
        l1_miner: &'a mut L1Miner,
        l2_source: S,
        rollup_config: &RollupConfig,
        config: BatcherConfig,
    ) -> Self {
        let driver = ChannelDriver::new(rollup_config.clone(), config.driver.clone());
        Self { l1_miner, l2_source, driver, config }
    }

    /// Drain all available L2 blocks, encode them, and submit the resulting
    /// frames to the L1 miner as pending transactions.
    ///
    /// Returns the [`Frame`]s produced so callers can inspect the encoded
    /// output without mining a block first.
    ///
    /// # Errors
    ///
    /// Returns [`BatcherError::NoBlocks`] if the L2 source is empty.
    /// Returns [`BatcherError::Driver`] if channel encoding fails.
    pub fn advance(&mut self) -> Result<Vec<Frame>, BatcherError> {
        let mut batch_count = 0u64;

        while let Some(block) = self.l2_source.next_block() {
            let batch = SingleBatch {
                parent_hash: block.parent_hash,
                epoch_num: block.l1_origin_number,
                epoch_hash: block.l1_origin_hash,
                timestamp: block.timestamp,
                transactions: block.transactions,
            };
            self.driver.add_batch(batch);
            batch_count += 1;
        }

        if batch_count == 0 {
            return Err(BatcherError::NoBlocks);
        }

        let frames = self.driver.flush()?;

        for frame in &frames {
            // L1 tx payload: DERIVATION_VERSION_0 ++ encoded frame
            let encoded = frame.encode();
            let mut input = Vec::with_capacity(1 + encoded.len());
            input.push(DERIVATION_VERSION_0);
            input.extend_from_slice(&encoded);

            self.l1_miner.submit_tx(PendingTx {
                from: self.config.batcher_address,
                to: self.config.inbox_address,
                input: Bytes::from(input),
            });
        }

        info!(batches = batch_count, frames = frames.len(), "batcher submitted frames to L1");

        Ok(frames)
    }
}

impl<S: L2BlockProvider> Action for Batcher<'_, S> {
    type Output = Vec<Frame>;
    type Error = BatcherError;

    fn act(&mut self) -> Result<Vec<Frame>, BatcherError> {
        self.advance()
    }
}
