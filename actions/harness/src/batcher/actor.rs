use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes};
use base_alloy_consensus::OpTxEnvelope;
use base_batcher_driver::{ChannelDriver, ChannelDriverConfig, ChannelDriverError};
use base_consensus_genesis::RollupConfig;
use base_protocol::{DERIVATION_VERSION_0, Frame, L1BlockInfoTx, SingleBatch};
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
    /// The L1 info deposit transaction could not be decoded.
    #[error("missing or undecodable L1 info deposit in block")]
    MissingL1Info,
    /// The channel driver failed to encode or compress the batches.
    #[error("channel driver error: {0}")]
    Driver(#[from] ChannelDriverError),
}

/// Batcher actor for action tests.
///
/// `Batcher` drains [`OpBlock`]s from an [`L2BlockProvider`], encodes each
/// one as a [`SingleBatch`], compresses all batches for the current cycle into
/// a channel via [`ChannelDriver`] (Brotli-10), and submits the resulting
/// frame data to the [`L1Miner`] as a [`PendingTx`].
///
/// The translation from `OpBlock` to `SingleBatch` mirrors op-batcher:
/// 1. Decode the L1 info from the first (deposit) transaction to get the epoch.
/// 2. Filter out all deposit transactions.
/// 3. EIP-2718-encode the remaining user transactions.
///
/// A single call to [`advance`] (or [`Action::act`]) runs one full encode
/// cycle: drain all available L2 blocks → encode → flush → submit to L1.
/// Callers then mine an L1 block to include the submitted transactions.
///
/// [`advance`]: Batcher::advance
/// [`OpBlock`]: base_alloy_consensus::OpBlock
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

    /// Drain all available L2 blocks and encode them into frames without
    /// submitting to L1.
    ///
    /// Returns the encoded frames so callers can inspect or submit them
    /// selectively. Use [`submit_frames`] to submit a subset of frames to
    /// the L1 miner.
    ///
    /// [`submit_frames`]: Batcher::submit_frames
    ///
    /// # Errors
    ///
    /// Returns [`BatcherError::NoBlocks`] if the L2 source is empty.
    /// Returns [`BatcherError::MissingL1Info`] if the first tx is not a valid deposit.
    /// Returns [`BatcherError::Driver`] if channel encoding fails.
    pub fn encode_frames(&mut self) -> Result<Vec<Frame>, BatcherError> {
        let mut batch_count = 0u64;

        while let Some(block) = self.l2_source.next_block() {
            // The first transaction in every L2 block must be the L1 info deposit.
            let first_tx = block.body.transactions.first().ok_or(BatcherError::MissingL1Info)?;
            let deposit = first_tx.as_deposit().ok_or(BatcherError::MissingL1Info)?;
            let l1_info = L1BlockInfoTx::decode_calldata(&deposit.input)
                .map_err(|_| BatcherError::MissingL1Info)?;
            let epoch = l1_info.id();

            // Filter deposits; EIP-2718-encode user transactions.
            let transactions: Vec<Bytes> = block
                .body
                .transactions
                .iter()
                .filter(|tx| !matches!(tx, OpTxEnvelope::Deposit(_)))
                .map(|tx| tx.encoded_2718().into())
                .collect();

            let batch = SingleBatch {
                parent_hash: block.header.parent_hash,
                epoch_num: epoch.number,
                epoch_hash: epoch.hash,
                timestamp: block.header.timestamp,
                transactions,
            };
            self.driver.add_batch(batch);
            batch_count += 1;
        }

        if batch_count == 0 {
            return Err(BatcherError::NoBlocks);
        }

        let frames = self.driver.flush()?;
        info!(batches = batch_count, frames = frames.len(), "batcher encoded frames");
        Ok(frames)
    }

    /// Submit the given frames to the L1 miner as pending transactions.
    ///
    /// Each frame is submitted as a separate [`PendingTx`].
    pub fn submit_frames(&mut self, frames: &[Frame]) {
        for frame in frames {
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
        info!(frames = frames.len(), "batcher submitted frames to L1");
    }

    /// Encode and submit all frames in one step (convenience wrapper).
    ///
    /// Equivalent to calling [`encode_frames`] followed by [`submit_frames`]
    /// with all produced frames.
    ///
    /// [`encode_frames`]: Batcher::encode_frames
    /// [`submit_frames`]: Batcher::submit_frames
    pub fn advance(&mut self) -> Result<Vec<Frame>, BatcherError> {
        let frames = self.encode_frames()?;
        self.submit_frames(&frames);
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
