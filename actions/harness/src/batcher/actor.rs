use std::{sync::Arc, time::Duration};

use base_batcher_core::{
    BatchDriver, BatchDriverConfig, BatchDriverError, DaThrottle, NoopThrottleClient,
    ThrottleConfig, ThrottleController, ThrottleStrategy,
};
use base_batcher_encoder::{BatchEncoder, EncoderConfig};
use base_batcher_source::{ChannelBlockSource, ChannelL1HeadSource, L2BlockEvent};
use base_common_consensus::BaseBlock;
use base_consensus_genesis::RollupConfig;
use base_protocol::{BatchType, L2BlockInfo};
use base_runtime::TokioRuntime;
use tokio_util::sync::CancellationToken;

use crate::{ActionL2Source, L1Miner, L1MinerTxManager, L2BlockProvider};

/// Configuration for the [`Batcher`] actor.
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// Address of the batcher account. Used as the `from` field on L1
    /// transactions so the derivation pipeline can filter by sender.
    pub batcher_address: alloy_primitives::Address,
    /// Batch inbox address on L1. Used as the `to` field on L1 transactions.
    pub inbox_address: alloy_primitives::Address,
    /// Whether to encode blocks as [`SingleBatch`](base_protocol::SingleBatch)es
    /// or a [`SpanBatch`](base_protocol::SpanBatch).
    pub batch_type: BatchType,
    /// Encoder configuration forwarded to [`BatchEncoder`].
    pub encoder: EncoderConfig,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        Self {
            batcher_address: alloy_primitives::Address::repeat_byte(0xBA),
            inbox_address: alloy_primitives::Address::repeat_byte(0xCA),
            batch_type: BatchType::Single,
            encoder: EncoderConfig::default(),
        }
    }
}

/// Errors returned by [`Batcher`] methods.
#[derive(Debug, thiserror::Error)]
pub enum BatcherError {
    /// The L2 source was exhausted before any blocks could be batched.
    #[error("no L2 blocks available to batch")]
    NoBlocks,
}

/// Batcher actor that drives a persistent [`BatchDriver`] through [`L1Miner`].
///
/// On construction, `Batcher` spawns a [`BatchDriver`] as a background tokio
/// task backed by a [`ChannelBlockSource`] for L2 block delivery and a
/// [`ChannelL1HeadSource`] fed by [`L1MinerTxManager`]. This mirrors the op-batcher
/// production architecture: the driver owns its encoding pipeline and
/// transaction manager and runs its own async loop.
///
/// Each call to [`advance`] drives one complete batch cycle:
/// 1. Drain the L2 source and forward each block to the driver via the channel.
/// 2. Send a [`L2BlockEvent::Flush`] to force-close the current channel.
/// 3. Yield to let the driver encode blocks, submit frames, and suspend.
/// 4. Mine one L1 block via the shared [`L1MinerTxManager`], firing all
///    receipt oneshots and delivering an [`L1HeadEvent::NewHead`] to the driver.
/// 5. Yield to let the driver confirm receipts and advance its L1 head.
///
/// The driver's [`BatchEncoder`] state is persistent across `advance()` calls.
/// The driver task continues running between cycles, waiting for new events.
///
/// [`advance`]: Batcher::advance
/// [`BatchDriver`]: base_batcher_core::BatchDriver
/// [`ChannelL1HeadSource`]: base_batcher_source::ChannelL1HeadSource
/// [`L1HeadEvent::NewHead`]: base_batcher_source::L1HeadEvent
/// [`L2BlockEvent::Flush`]: base_batcher_source::L2BlockEvent::Flush
pub struct Batcher<S: L2BlockProvider> {
    /// The L2 block source to drain on each [`advance`](Batcher::advance) cycle.
    l2_source: S,
    /// Channel sender for forwarding L2 block events to the background driver task.
    block_tx: tokio::sync::mpsc::UnboundedSender<L2BlockEvent>,
    /// Shared tx manager — used to mine blocks and fire receipt/L1 head events.
    tx_manager: L1MinerTxManager,
    /// Background driver task handle.
    driver_task: tokio::task::JoinHandle<Result<(), BatchDriverError>>,
    /// Token used to cancel the background driver on drop.
    cancel: CancellationToken,
}

impl<S: L2BlockProvider + std::fmt::Debug> std::fmt::Debug for Batcher<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Batcher")
            .field("l2_source", &self.l2_source)
            .field("tx_manager", &self.tx_manager)
            .finish_non_exhaustive()
    }
}

impl<S: L2BlockProvider> Batcher<S> {
    /// Create a new [`Batcher`] backed by a persistent [`BatchDriver`] task.
    ///
    /// Spawns the driver immediately. The driver will not process any events
    /// until the first [`advance`] call.
    ///
    /// [`advance`]: Batcher::advance
    pub fn new(l2_source: S, rollup_config: &RollupConfig, config: BatcherConfig) -> Self {
        Self::build(l2_source, rollup_config, config, None)
    }

    /// Create a new [`Batcher`] with an external L2 safe head watch channel.
    ///
    /// When the receiver fires, the [`BatchDriver`] prunes confirmed blocks
    /// from the encoder and uses the safe head value to determine the
    /// catchup position after a reorg or [`signal_reorg`] call.
    ///
    /// Without this, the driver defaults to `safe_head = 0` when computing
    /// `catchup_from = safe_head + 1`, which is correct for fresh starts
    /// but does not model the production batcher's awareness of the current
    /// safe head.
    ///
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    /// [`signal_reorg`]: Batcher::signal_reorg
    pub fn with_safe_head_rx(
        l2_source: S,
        rollup_config: &RollupConfig,
        config: BatcherConfig,
        safe_head_rx: tokio::sync::watch::Receiver<u64>,
    ) -> Self {
        Self::build(l2_source, rollup_config, config, Some(safe_head_rx))
    }

    /// Shared constructor. Builds and spawns the [`BatchDriver`] task.
    fn build(
        l2_source: S,
        rollup_config: &RollupConfig,
        config: BatcherConfig,
        safe_head_rx: Option<tokio::sync::watch::Receiver<u64>>,
    ) -> Self {
        let rollup_config = Arc::new(rollup_config.clone());
        let mut encoder_config = config.encoder.clone();
        encoder_config.batch_type = config.batch_type;
        let pipeline = BatchEncoder::new(rollup_config, encoder_config);

        let (source, block_tx) = ChannelBlockSource::new();

        // L1 head source: mine_block() sends L1HeadEvent::NewHead; the driver
        // calls advance_l1_head() when the channel delivers an event.
        let (l1_source, l1_head_tx) = ChannelL1HeadSource::new();

        let tx_manager = L1MinerTxManager::new(config.batcher_address, config.inbox_address)
            .with_l1_head_tx(l1_head_tx);

        let cancel = CancellationToken::new();
        let runtime = TokioRuntime::with_token(cancel.clone());

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Off);
        let mut driver = BatchDriver::new(
            runtime,
            pipeline,
            source,
            tx_manager.clone(),
            BatchDriverConfig {
                inbox: config.inbox_address,
                max_pending_transactions: 16,
                drain_timeout: Duration::from_secs(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(throttle, Arc::new(NoopThrottleClient)),
            l1_source,
        );

        if let Some(rx) = safe_head_rx {
            driver = driver.with_safe_head_rx(rx);
        }

        let driver_task = tokio::spawn(async move { driver.run().await });

        Self { l2_source, block_tx, tx_manager, driver_task, cancel }
    }

    /// Drain the L2 source and forward all blocks to the driver, then flush.
    ///
    /// Performs steps 1–3 of [`advance`] without mining: sends all L2 blocks,
    /// sends [`L2BlockEvent::Flush`], and yields once so the driver encodes the
    /// blocks and calls [`send_async`] for each submission. After this returns,
    /// [`pending_count`] reflects how many frame transactions are waiting.
    ///
    /// # Panics
    ///
    /// Panics if the L2 source is empty. Use [`try_advance`] if you need to
    /// test the empty-source error path.
    ///
    /// [`advance`]: Batcher::advance
    /// [`try_advance`]: Batcher::try_advance
    /// [`pending_count`]: Batcher::pending_count
    /// [`send_async`]: crate::L1MinerTxManager::send_async
    pub async fn encode_only(&mut self) {
        self.try_encode_only().await.unwrap_or_else(|e| panic!("Batcher::encode_only failed: {e}"))
    }

    /// Fallible variant of [`encode_only`] that returns an error instead of panicking.
    ///
    /// [`encode_only`]: Batcher::encode_only
    async fn try_encode_only(&mut self) -> Result<(), BatcherError> {
        let mut block_count = 0u64;
        while let Some(block) = self.l2_source.next_block() {
            self.block_tx.send(L2BlockEvent::Block(Box::new(block))).expect("driver task alive");
            block_count += 1;
        }
        if block_count == 0 {
            return Err(BatcherError::NoBlocks);
        }
        self.block_tx.send(L2BlockEvent::Flush).expect("driver task alive");
        tokio::task::yield_now().await;
        Ok(())
    }

    /// Returns the number of encoded-but-not-yet-staged pending frame submissions.
    pub fn pending_count(&self) -> usize {
        self.tx_manager.pending_count()
    }

    /// Submit the first `n` pending frame txs/blobs to the L1 miner's queue
    /// without mining. Returns the actual count staged.
    pub fn stage_n_frames(&self, l1: &mut L1Miner, n: usize) -> usize {
        self.tx_manager.stage_n_to_l1(l1, n)
    }

    /// Drop the first `n` pending frame submissions without staging them to L1.
    ///
    /// Returns the actual number dropped. Use this to skip specific frame
    /// positions when testing non-sequential frame submission scenarios.
    pub fn drop_n_frames(&self, n: usize) -> usize {
        self.tx_manager.drop_n(n)
    }

    /// Fire receipts for all staged items at `block_number` and yield to let
    /// the driver process confirmations and the L1 head event.
    pub async fn confirm_staged(&self, block_number: u64) {
        self.tx_manager.confirm_all(block_number);
        tokio::task::yield_now().await;
    }

    /// Simulate an L1 reorg back to `block_number`.
    ///
    /// Truncates the L1 chain via [`L1Miner::reorg_to`], fires failure
    /// receipts for every item in `pending` and `staged`, and publishes
    /// [`L1HeadEvent::NewHead`] to the driver.
    ///
    /// Items already confirmed via [`confirm_staged`] (and thus living in
    /// the driver's own `in_flight` set) are **not** covered — see
    /// [`L1MinerTxManager::reorg_to`] for details.
    ///
    /// This method is synchronous. After returning, call
    /// [`wait_until_requeued`] to wait for the driver to process the failure
    /// receipts and return frames to the pending queue.
    ///
    /// # Panics
    ///
    /// Panics if `block_number` exceeds the current L1 chain tip
    /// (`ReorgError::BeyondTip`).
    ///
    /// [`confirm_staged`]: Batcher::confirm_staged
    /// [`wait_until_requeued`]: Batcher::wait_until_requeued
    pub fn reorg(&self, block_number: u64, l1: &mut L1Miner) {
        self.tx_manager.reorg_to(block_number, l1);
    }

    /// Wait until at least `min_frames` frames are in the pending queue.
    ///
    /// Yields to the tokio scheduler on each iteration to give the background
    /// [`BatchDriver`] task time to encode blocks and call [`send_async`].
    /// Intended to be called after [`encode_only`] when a test needs to inspect
    /// or act on pending frames before mining.
    ///
    /// # Panics
    ///
    /// Panics if `pending_count()` does not reach `min_frames` within the
    /// polling iteration limit (20 yields).
    ///
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    /// [`send_async`]: crate::L1MinerTxManager::send_async
    /// [`encode_only`]: Batcher::encode_only
    pub async fn wait_until_pending(&self, min_frames: usize) {
        for _ in 0..20 {
            if self.pending_count() >= min_frames {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "timed out waiting for {min_frames} pending frames after encoding; got {}",
            self.pending_count()
        );
    }

    /// Wait until at least `expected` frames are back in the pending queue
    /// after an L1 reorg.
    ///
    /// A reorg requires two driver loop iterations to complete: the first
    /// processes each `Receipt(id, Failed)` event and requeues the frames in
    /// the encoder pipeline; the second calls `submit_pending()` →
    /// [`send_async`] to return them to the pending queue. This method polls
    /// [`pending_count`] until the condition is satisfied.
    ///
    /// # Panics
    ///
    /// Panics if `pending_count()` does not reach `expected` within the
    /// polling iteration limit (20 yields).
    ///
    /// [`send_async`]: crate::L1MinerTxManager::send_async
    /// [`pending_count`]: Batcher::pending_count
    pub async fn wait_until_requeued(&self, expected: usize) {
        for _ in 0..20 {
            if self.pending_count() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "timed out waiting for {expected} requeued frames after reorg; got {}",
            self.pending_count()
        );
    }

    /// Signal that the batcher has been repointed to a different L2 node.
    ///
    /// Sends an [`L2BlockEvent::Reorg`] to the background [`BatchDriver`],
    /// which triggers [`BatchPipeline::reset`] (clearing the encoder) and
    /// [`UnsafeBlockSource::reset_catchup`]. After this call returns, the
    /// encoder is empty and ready to accept blocks from the new node's chain.
    ///
    /// In production, this corresponds to the batcher detecting that the
    /// L2 unsafe chain has diverged (e.g. because it was repointed to a
    /// different sequencer node). The op-batcher's `computeSyncActions`
    /// achieves the same effect via `startAfresh` → `channelManager.Clear()`.
    ///
    /// The `new_safe_head` is forwarded to the driver for logging. The
    /// actual catchup position is determined by the [`safe_head_rx`] watch
    /// channel (if wired via [`with_safe_head_rx`]).
    ///
    /// # Panics
    ///
    /// Panics if the driver task has already exited.
    ///
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    /// [`BatchPipeline::reset`]: base_batcher_encoder::BatchPipeline::reset
    /// [`UnsafeBlockSource::reset_catchup`]: base_batcher_source::UnsafeBlockSource::reset_catchup
    /// [`safe_head_rx`]: Batcher::with_safe_head_rx
    /// [`with_safe_head_rx`]: Batcher::with_safe_head_rx
    pub async fn signal_reorg(&self, new_safe_head: L2BlockInfo) {
        self.block_tx.send(L2BlockEvent::Reorg { new_safe_head }).expect("driver task alive");
        tokio::task::yield_now().await;
    }

    /// Run one full batch cycle through the production [`BatchDriver`] path.
    ///
    /// # Panics
    ///
    /// Panics if the L2 source is empty. Use [`try_advance`] to test the
    /// empty-source error path.
    ///
    /// [`try_advance`]: Batcher::try_advance
    pub async fn advance(&mut self, l1: &mut L1Miner) {
        self.try_advance(l1).await.unwrap_or_else(|e| panic!("Batcher::advance failed: {e}"))
    }

    /// Fallible variant of [`advance`] — returns an error instead of panicking.
    ///
    /// Use this when a test needs to assert that `advance` fails (e.g. to
    /// verify that [`BatcherError::NoBlocks`] is returned for an empty source).
    /// For the common happy-path case prefer [`advance`].
    ///
    /// [`advance`]: Batcher::advance
    pub async fn try_advance(&mut self, l1: &mut L1Miner) -> Result<(), BatcherError> {
        self.try_encode_only().await?;

        // Mine one L1 block: submits all pending txs/blobs, fires receipt
        // oneshots, and publishes the block number to the L1 head watch.
        self.tx_manager.mine_block(l1);

        // Yield to let the driver process receipts (in_flight.next())
        // and the L1 head update (l1_head_rx.changed()).
        tokio::task::yield_now().await;

        Ok(())
    }
}

impl Batcher<ActionL2Source> {
    /// Push a block into the L2 source for the next [`advance`] call.
    ///
    /// [`advance`]: Batcher::advance
    pub fn push_block(&mut self, block: BaseBlock) {
        self.l2_source.push(block);
    }
}

impl<S: L2BlockProvider> Drop for Batcher<S> {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.driver_task.abort();
    }
}
