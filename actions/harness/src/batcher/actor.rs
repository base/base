//! [`Batcher`] actor driving a production [`BatchDriver`] through [`L1Miner`].

use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use alloy_signer_local::PrivateKeySigner;
use base_batcher_core::{
    AdminError, AdminHandle, BatchDriver, BatchDriverConfig, BatchDriverError, BatchDriverInputs,
    DaThrottle, NoopThrottleClient, ThrottleController,
};
use base_batcher_encoder::{BatchEncoder, EncoderConfig};
use base_batcher_source::{L2BlockEvent, test_utils::ChannelBlockSource};
use base_common_consensus::BaseBlock;
use base_common_genesis::RollupConfig;
use base_protocol::BlockInfo;
use base_runtime::TokioRuntime;
use base_tx_manager::TxManager;
use tokio::sync::{mpsc, oneshot};

use crate::{ActionL2Source, HarnessL1HeadSource, L1Block, L1HeadItem, L1Miner, L1MinerTxManager};

/// Configuration for the [`Batcher`] actor.
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// Address of the batcher account. Used as the `from` field on L1
    /// transactions so the derivation pipeline can filter by sender.
    pub batcher_address: alloy_primitives::Address,
    /// Batch inbox address on L1. Used as the `to` field on L1 transactions.
    pub inbox_address: alloy_primitives::Address,
    /// Encoder configuration forwarded to [`BatchEncoder`].
    pub encoder: EncoderConfig,
    /// L1 signer used to produce signed `TxEnvelope`s for production-mode DA tests.
    ///
    /// When changed via [`with_l1_signer`](BatcherConfig::with_l1_signer), the
    /// signer address becomes [`batcher_address`](BatcherConfig::batcher_address)
    /// so production calldata/blob sources can recover the expected sender.
    pub l1_signer: PrivateKeySigner,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        let l1_signer = Self::default_l1_signer();
        Self {
            batcher_address: l1_signer.address(),
            inbox_address: alloy_primitives::Address::repeat_byte(0xCA),
            encoder: EncoderConfig::default(),
            l1_signer,
        }
    }
}

impl BatcherConfig {
    /// Return the deterministic default L1 signer used by action tests.
    pub fn default_l1_signer() -> PrivateKeySigner {
        PrivateKeySigner::from_bytes(&B256::repeat_byte(0xBA)).expect("valid default L1 signer")
    }

    /// Configure a signer for production-mode L1 transaction construction.
    pub fn with_l1_signer(mut self, signer: PrivateKeySigner) -> Self {
        self.batcher_address = signer.address();
        self.l1_signer = signer;
        self
    }
}

/// Errors returned by [`Batcher`] methods.
#[derive(Debug, thiserror::Error)]
pub enum BatcherError {
    /// The L2 source was exhausted before any blocks could be batched.
    #[error("no L2 blocks available to batch")]
    NoBlocks,
    /// A new batch cycle was started before prior submissions were mined.
    #[error("cannot start a batch cycle with outstanding frame submissions")]
    OutstandingSubmissions,
    /// The end-of-cycle flush failed, for example because the driver is stopped.
    #[error("flush failed: {0}")]
    Flush(#[from] AdminError),
    /// The driver exited, or did not catch up with the harness within
    /// [`Batcher::IDLE_TIMEOUT`].
    #[error("the batch driver exited or stalled")]
    DriverUnavailable,
}

/// Batcher actor that drives a persistent [`BatchDriver`] through [`L1Miner`].
///
/// On construction, `Batcher` spawns a [`BatchDriver`] as a background tokio task backed by
/// a [`ChannelBlockSource`] for L2 block delivery, a [`HarnessL1HeadSource`] for L1 heads,
/// and an admin channel. This mirrors the production batcher architecture: the driver owns
/// its encoding pipeline and transaction manager and runs its own async loop.
///
/// Every `async` method returns once the driver is idle again, that is once it has taken
/// what it was given, encoded it, handed the resulting submissions to the tx manager and
/// applied every receipt. The harness waits for that with a marker queued in the L1 head
/// source; see [`L1HeadItem::Marker`].
///
/// Each call to [`advance`] drives one complete batch cycle:
/// 1. Drain the L2 source and forward each block to the driver via the block source.
/// 2. Wait for the driver to encode every block.
/// 3. Flush through the admin channel, exactly as an operator would, to close and
///    release the current channel.
/// 4. Wait for the driver to hand every resulting submission to the tx manager.
/// 5. Stage every pending submission, mine one L1 block, fire the receipts of the
///    submissions it includes and deliver the new L1 head to the driver.
/// 6. Wait for the driver to confirm the receipts and advance its L1 head.
///
/// The driver's [`BatchEncoder`] state is persistent across `advance()` calls.
/// The driver task continues running between cycles, waiting for new events.
///
/// [`advance`]: Batcher::advance
/// [`BatchDriver`]: base_batcher_core::BatchDriver
#[derive(Debug)]
pub struct Batcher {
    /// The L2 block source to drain on each [`advance`](Batcher::advance) cycle.
    l2_source: ActionL2Source,
    /// Feeds the driver's block source with block and reorg events.
    source_tx: mpsc::UnboundedSender<L2BlockEvent>,
    /// Feeds the driver's L1 head source with mined heads, and with markers.
    l1_head_tx: mpsc::UnboundedSender<L1HeadItem>,
    /// Admin channel to the driver, used to flush at the end of a cycle.
    admin: AdminHandle,
    /// Shared tx manager — used to stage submissions and fire their receipts.
    tx_manager: L1MinerTxManager,
    /// Background driver task, aborted on drop.
    driver_task: tokio::task::JoinHandle<Result<(), BatchDriverError>>,
}

impl Batcher {
    /// How long a method waits for the driver to go idle before giving up.
    pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Create a new [`Batcher`] backed by a persistent [`BatchDriver`] task.
    ///
    /// Spawns the driver immediately.
    ///
    /// # Panics
    ///
    /// Panics if `config.encoder` is invalid, or if `config.batcher_address` is not the
    /// address of `config.l1_signer`.
    pub fn new(
        l2_source: ActionL2Source,
        rollup_config: &RollupConfig,
        config: BatcherConfig,
    ) -> Self {
        let l1_chain_id = rollup_config.l1_chain_id;
        let pipeline = BatchEncoder::new(Arc::new(rollup_config.clone()), config.encoder.clone())
            .expect("valid encoder config");

        let (source, source_tx) = ChannelBlockSource::new();
        let (l1_head_source, l1_head_tx) = HarnessL1HeadSource::new();
        let (admin, admin_rx) = AdminHandle::channel();

        let tx_manager =
            L1MinerTxManager::new(config.l1_signer.clone(), config.inbox_address, l1_chain_id);
        assert_eq!(
            config.batcher_address,
            tx_manager.sender_address(),
            "BatcherConfig::batcher_address must match BatcherConfig::l1_signer"
        );

        let runtime = TokioRuntime::new();

        let (derivation_status_tx, derivation_status_rx) = mpsc::channel(1);

        let driver = BatchDriver::new(
            runtime,
            pipeline,
            tx_manager.clone(),
            BatchDriverConfig {
                inbox: config.inbox_address,
                // Past this many transactions in flight the driver goes idle with submissions
                // still in the pipeline, so `encode_only` would return before handing them all
                // to the tx manager. No action test comes close.
                max_pending_transactions: 16,
                drain_timeout: Duration::from_secs(10),
                force_blobs_when_throttling: true,
                stopped: false,
            },
            DaThrottle::new(ThrottleController::disabled(), Arc::new(NoopThrottleClient)),
            BatchDriverInputs {
                source,
                l1_head_source,
                // The driver learns the L1 head from the blocks the tests mine.
                initial_l1_head: 0,
                initial_safe_head: BlockInfo::from_l2_genesis(&rollup_config.genesis),
                derivation_status_rx,
                admin_rx,
            },
        );

        // No action test exercises derivation status: the driver task keeps the sender, so
        // the channel stays open, and silent, for as long as the driver runs.
        let driver_task = tokio::spawn(async move {
            let _derivation_status_tx = derivation_status_tx;
            driver.run().await
        });

        Self { l2_source, source_tx, l1_head_tx, admin, tx_manager, driver_task }
    }

    /// Push a block into the L2 source for the next [`advance`] call.
    ///
    /// [`advance`]: Batcher::advance
    pub fn push_block(&mut self, block: BaseBlock) {
        self.l2_source.push(block);
    }

    /// Drain the L2 source and forward all blocks to the driver, then flush.
    ///
    /// Performs steps 1–4 of [`advance`] without mining. Once it returns, every frame the
    /// flush released has been handed to the tx manager, so [`pending_count`] counts them all.
    ///
    /// # Panics
    ///
    /// Panics if the L2 source is empty. Use [`try_advance`] if you need to
    /// test the empty-source error path.
    ///
    /// [`advance`]: Batcher::advance
    /// [`try_advance`]: Batcher::try_advance
    /// [`pending_count`]: Batcher::pending_count
    pub async fn encode_only(&mut self) {
        self.try_encode_only().await.unwrap_or_else(|e| panic!("Batcher::encode_only failed: {e}"))
    }

    /// Fallible variant of [`encode_only`] that returns an error instead of panicking.
    ///
    /// [`encode_only`]: Batcher::encode_only
    async fn try_encode_only(&mut self) -> Result<(), BatcherError> {
        if self.pending_count() > 0 || self.staged_count() > 0 {
            return Err(BatcherError::OutstandingSubmissions);
        }

        let mut block_count = 0u64;
        while let Some(block) = self.l2_source.next_block() {
            self.send_event(L2BlockEvent::Block(Box::new(block)))?;
            block_count += 1;
        }
        if block_count == 0 {
            return Err(BatcherError::NoBlocks);
        }

        // Admin commands outrank the block source in the driver's select, so wait until every
        // block above is taken and encoded before asking for the flush.
        self.wait_until_idle().await?;
        self.admin.flush().await?;

        // The flush is answered before the driver's next encode-and-submit pass. Wait for that
        // pass so every frame the flush released has been handed to the tx manager.
        self.wait_until_idle().await
    }

    /// Queue an event for the driver's block source. Fails if the driver task has exited.
    fn send_event(&self, event: L2BlockEvent) -> Result<(), BatcherError> {
        self.source_tx.send(event).map_err(|_| BatcherError::DriverUnavailable)
    }

    /// Queue an item for the driver's L1 head source. Fails if the driver task has exited.
    fn send_l1_head_item(&self, item: L1HeadItem) -> Result<(), BatcherError> {
        self.l1_head_tx.send(item).map_err(|_| BatcherError::DriverUnavailable)
    }

    /// Wait until the driver has nothing left to do: everything sent so far is taken,
    /// encoded and submitted, and every receipt is applied.
    async fn wait_until_idle(&self) -> Result<(), BatcherError> {
        let (reached_tx, reached_rx) = oneshot::channel();
        self.send_l1_head_item(L1HeadItem::Marker(reached_tx))?;
        match tokio::time::timeout(Self::IDLE_TIMEOUT, reached_rx).await {
            Ok(Ok(())) => Ok(()),
            _ => Err(BatcherError::DriverUnavailable),
        }
    }

    /// Deliver `head` to the driver as the new L1 head and wait until it is applied.
    async fn deliver_l1_head(&self, head: u64) -> Result<(), BatcherError> {
        self.send_l1_head_item(L1HeadItem::Head(head))?;
        self.wait_until_idle().await
    }

    /// Fire receipts for the staged items included in `block`, then deliver its number as
    /// the new L1 head.
    async fn try_confirm_staged(&self, block: &L1Block) -> Result<(), BatcherError> {
        // Receipts first: the driver serves them before L1 heads, so a failed submission is
        // requeued before the head advances.
        self.tx_manager.confirm_block(block);
        self.deliver_l1_head(block.number()).await
    }

    /// Stage every pending submission, mine one L1 block and confirm it.
    async fn try_mine_pending(&self, l1: &mut L1Miner) -> Result<u64, BatcherError> {
        self.tx_manager.stage_n_to_l1(l1, usize::MAX);
        let block = l1.mine_block().clone();
        self.try_confirm_staged(&block).await?;
        Ok(block.number())
    }

    /// Returns the number of encoded-but-not-yet-staged pending frame submissions.
    pub fn pending_count(&self) -> usize {
        self.tx_manager.pending_count()
    }

    /// Returns the number of submitted frame transactions waiting for inclusion receipts.
    pub fn staged_count(&self) -> usize {
        self.tx_manager.staged_count()
    }

    /// Submit the first `n` pending frame txs/blobs to the L1 miner's queue
    /// without mining. Returns the actual count staged.
    pub fn stage_n_frames(&self, l1: &mut L1Miner, n: usize) -> usize {
        self.tx_manager.stage_n_to_l1(l1, n)
    }

    /// Schedule the next `n` frame submissions to fail immediately.
    ///
    /// Each of the next `n` calls the background [`BatchDriver`] makes to
    /// [`TxManager::send_async`] will resolve with
    /// [`TxManagerError::Rpc`] instead of queuing to the L1 miner. The driver
    /// requeues the frame and retries, so calling this before [`encode_only`]
    /// simulates transient L1 submission failures without losing data. Once
    /// [`encode_only`] returns, the retried frames are back in the pending queue.
    ///
    /// [`TxManager::send_async`]: base_tx_manager::TxManager::send_async
    /// [`TxManagerError::Rpc`]: base_tx_manager::TxManagerError::Rpc
    /// [`encode_only`]: Batcher::encode_only
    pub fn fail_next_n_submissions(&self, n: usize) {
        self.tx_manager.fail_next_n(n);
    }

    /// Schedule the next `n` frame submissions to be rejected as if the txpool
    /// nonce slot is held by a stuck transaction.
    ///
    /// Each of the next `n` calls the background [`BatchDriver`] makes to
    /// [`TxManager::send_async`] resolves with [`TxManagerError::AlreadyReserved`].
    /// The driver classifies this as [`TxOutcome::TxpoolBlocked`]: it requeues
    /// the frames, stops submitting, and calls [`TxManager::cancel_tx`] on its
    /// next loop iteration to clear the slot before resubmitting. Once
    /// [`encode_only`] returns, the recovery has run: use [`cancellation_count`]
    /// to assert it did.
    ///
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    /// [`TxManager::send_async`]: base_tx_manager::TxManager::send_async
    /// [`TxManager::cancel_tx`]: base_tx_manager::TxManager::cancel_tx
    /// [`TxManagerError::AlreadyReserved`]: base_tx_manager::TxManagerError::AlreadyReserved
    /// [`TxOutcome::TxpoolBlocked`]: base_batcher_core::TxOutcome::TxpoolBlocked
    /// [`encode_only`]: Batcher::encode_only
    /// [`cancellation_count`]: Batcher::cancellation_count
    pub fn block_next_n_submissions(&self, n: usize) {
        self.tx_manager.block_next_n(n);
    }

    /// Returns how many times the driver has called [`TxManager::cancel_tx`] to
    /// recover from a txpool blockage.
    ///
    /// [`TxManager::cancel_tx`]: base_tx_manager::TxManager::cancel_tx
    pub fn cancellation_count(&self) -> usize {
        self.tx_manager.cancellation_count()
    }

    /// Mine all pending frame submissions in one L1 block.
    ///
    /// Stages every pending frame, mines one L1 block, fires all receipts and waits until
    /// the driver has confirmed them. Returns the mined block number.
    ///
    /// Use this to confirm a requeued batch without encoding new L2 blocks.
    ///
    /// # Panics
    ///
    /// Panics if the driver task has exited, or did not catch up within
    /// [`IDLE_TIMEOUT`](Self::IDLE_TIMEOUT).
    pub async fn mine_pending(&self, l1: &mut L1Miner) -> u64 {
        self.try_mine_pending(l1)
            .await
            .unwrap_or_else(|e| panic!("Batcher::mine_pending failed: {e}"))
    }

    /// Drop the first `n` pending frame submissions without staging them to L1.
    ///
    /// Returns the actual number dropped. Use this to skip specific frame
    /// positions when testing non-sequential frame submission scenarios. The driver sees
    /// each dropped submission fail and resubmits it on its next `async` call.
    pub fn drop_n_frames(&self, n: usize) -> usize {
        self.tx_manager.drop_n(n)
    }

    /// Fire receipts for all staged items included in `block`, deliver its number to the
    /// driver as the new L1 head, and wait until the driver has applied both.
    ///
    /// # Panics
    ///
    /// Panics if the driver task has exited, or did not catch up within
    /// [`IDLE_TIMEOUT`](Self::IDLE_TIMEOUT).
    pub async fn confirm_staged(&self, block: &L1Block) {
        self.try_confirm_staged(block)
            .await
            .unwrap_or_else(|e| panic!("Batcher::confirm_staged failed: {e}"));
    }

    /// Simulate an L1 reorg back to `block_number`.
    ///
    /// Truncates the L1 chain via [`L1Miner::reorg_to`], fires failure receipts for every
    /// item in `pending` and `staged`, delivers the new L1 head to the driver, and waits
    /// until the driver has requeued and resubmitted the failed frames.
    ///
    /// Submissions already confirmed through [`confirm_staged`] are not revisited.
    ///
    /// # Panics
    ///
    /// Panics if `block_number` exceeds the current L1 chain tip
    /// (`ReorgError::BeyondTip`), or if the driver task has exited or did not catch up
    /// within [`IDLE_TIMEOUT`](Self::IDLE_TIMEOUT).
    ///
    /// [`confirm_staged`]: Batcher::confirm_staged
    pub async fn reorg(&self, block_number: u64, l1: &mut L1Miner) {
        // Failure receipts first, for the same reason as in `try_confirm_staged`.
        self.tx_manager.reorg_to(block_number, l1);
        self.deliver_l1_head(block_number)
            .await
            .unwrap_or_else(|e| panic!("Batcher::reorg failed: {e}"));
    }

    /// Signal that the batcher has been repointed to a different L2 node.
    ///
    /// Sends an [`L2BlockEvent::Reorg`] to the background [`BatchDriver`] and waits until
    /// it has applied it, so the encoder is known to be empty on return.
    ///
    /// # Panics
    ///
    /// Panics if the driver task has exited, or did not apply the reorg within
    /// [`IDLE_TIMEOUT`](Self::IDLE_TIMEOUT).
    ///
    /// [`BatchDriver`]: base_batcher_core::BatchDriver
    pub async fn signal_reorg(&self) {
        self.send_event(L2BlockEvent::Reorg).expect("driver task alive");
        self.wait_until_idle().await.expect("driver applies the reorg");
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
        self.try_mine_pending(l1).await?;
        Ok(())
    }
}

impl Drop for Batcher {
    fn drop(&mut self) {
        self.driver_task.abort();
    }
}
