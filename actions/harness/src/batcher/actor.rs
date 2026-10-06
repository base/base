//! [`Batcher`] actor driving a production [`BatchDriver`] through [`L1Miner`].

use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use alloy_signer_local::PrivateKeySigner;
use base_batcher_core::{
    AdminHandle, BatchDriver, BatchDriverConfig, BatchDriverError, BatchDriverInputs, DaThrottle,
    DerivationStatus, ThrottleController,
};
use base_batcher_encoder::{BatchEncoder, EncoderConfig};
use base_common_consensus::BaseBlock;
use base_common_genesis::RollupConfig;
use base_protocol::BlockInfo;
use base_runtime::TokioRuntime;
use base_tx_manager::TxManager;
use tokio::sync::{mpsc, oneshot};

use crate::{
    ActionL2Source, HarnessBlockSource, HarnessL1HeadSource, L1Block, L1HeadItem, L1Miner,
    L1MinerTxManager, SharedL2Chain,
};

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
    /// L1 signer of the batcher's submissions.
    ///
    /// When changed via [`with_l1_signer`](BatcherConfig::with_l1_signer), the
    /// signer address becomes [`batcher_address`](BatcherConfig::batcher_address)
    /// so production calldata/blob sources can recover the expected sender.
    pub l1_signer: PrivateKeySigner,
    /// The safe L2 head the batcher starts from, as its node would report it. The batcher
    /// posts the blocks above it. `None` means the parent of the first block given to
    /// [`Batcher::new`], or the L2 genesis of the rollup config when it is given none, so the
    /// first block pushed to such a batcher must be block 1.
    pub initial_safe_head: Option<BlockInfo>,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        let l1_signer = Self::default_l1_signer();
        Self {
            batcher_address: l1_signer.address(),
            inbox_address: alloy_primitives::Address::repeat_byte(0xCA),
            encoder: EncoderConfig::default(),
            l1_signer,
            initial_safe_head: None,
        }
    }
}

impl BatcherConfig {
    /// Return the deterministic default L1 signer used by action tests.
    pub fn default_l1_signer() -> PrivateKeySigner {
        PrivateKeySigner::from_bytes(&B256::repeat_byte(0xBA)).expect("valid default L1 signer")
    }

    /// Sign the batcher's submissions with `signer`, and make its address the batcher address.
    pub fn with_l1_signer(mut self, signer: PrivateKeySigner) -> Self {
        self.batcher_address = signer.address();
        self.l1_signer = signer;
        self
    }
}

/// Batcher actor that drives a persistent [`BatchDriver`] through [`L1Miner`].
///
/// On construction, `Batcher` spawns a [`BatchDriver`] as a background tokio task backed by
/// a [`HarnessBlockSource`] polling the L2 chain the test builds, a [`HarnessL1HeadSource`]
/// for L1 heads, a derivation-status channel and an admin channel. As in production, the
/// driver owns its encoding pipeline and transaction manager, runs its own async loop and
/// catches up from the safe head again after a reset. The test plays the world around it.
/// It pushes L2 blocks with [`push_block`], mines L1 blocks and shows them to the driver
/// with [`observe_l1_block`], and reports derivation progress with [`observe_derivation`].
///
/// Every `async` method returns once the driver is idle again, that is once it has taken
/// what it was given, encoded it, handed the resulting submissions to the tx manager and
/// applied every receipt. The harness waits for that with a marker queued in the L1 head
/// source, see [`L1HeadItem::Marker`]. Each of them panics if the driver task has exited, or
/// did not go idle within [`IDLE_TIMEOUT`](Batcher::IDLE_TIMEOUT).
///
/// Each call to [`advance`] drives one complete batch cycle:
/// 1. Wait for the driver to take and encode every block pushed so far.
/// 2. Flush through the admin channel, exactly as an operator would, to close and
///    release the current channel.
/// 3. Wait for the driver to hand every resulting submission to the tx manager.
/// 4. Stage every pending submission, mine one L1 block and show it to the driver.
/// 5. Wait for the driver to apply the block's receipts and its new L1 head.
///
/// The driver's [`BatchEncoder`] state is persistent across `advance()` calls.
/// The driver task continues running between cycles, waiting for new events.
///
/// [`advance`]: Batcher::advance
/// [`push_block`]: Batcher::push_block
/// [`observe_l1_block`]: Batcher::observe_l1_block
/// [`observe_derivation`]: Batcher::observe_derivation
/// [`BatchDriver`]: base_batcher_core::BatchDriver
#[derive(Debug)]
pub struct Batcher {
    /// The L2 chain the driver's block source polls.
    chain: SharedL2Chain,
    /// Feeds the driver's L1 head source with mined heads, and with markers.
    l1_head_tx: mpsc::UnboundedSender<L1HeadItem>,
    /// Feeds the driver with derivation progress.
    derivation_status_tx: mpsc::Sender<DerivationStatus>,
    /// Admin channel to the driver, used to flush at the end of a cycle.
    admin: AdminHandle,
    /// Shared tx manager, used to stage submissions and fire their receipts.
    tx_manager: L1MinerTxManager,
    /// Background driver task, aborted on drop.
    driver_task: tokio::task::JoinHandle<Result<(), BatchDriverError>>,
}

impl Batcher {
    /// How long a method waits for the driver to go idle before giving up.
    pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Create a new [`Batcher`] backed by a persistent [`BatchDriver`] task, with the blocks
    /// of `l2_source` as its L2 chain so far, see
    /// [`BatcherConfig::initial_safe_head`] for where it starts.
    ///
    /// Spawns the driver immediately.
    ///
    /// # Panics
    ///
    /// Panics if `config.encoder` is one production would refuse for `rollup_config` at the
    /// timestamp of the block after the safe head, if `config.batcher_address` is not the
    /// address of `config.l1_signer`, or if `config.initial_safe_head` is `None` and the first
    /// block is the genesis block, which no batcher posts.
    pub fn new(
        l2_source: ActionL2Source,
        rollup_config: &RollupConfig,
        config: BatcherConfig,
    ) -> Self {
        let l1_chain_id = rollup_config.l1_chain_id;
        let blocks: Vec<BaseBlock> = l2_source.into_iter().collect();
        let initial_safe_head = config.initial_safe_head.unwrap_or_else(|| {
            blocks.first().map_or_else(
                || BlockInfo::from_l2_genesis(&rollup_config.genesis),
                |block| BlockInfo {
                    hash: block.header.parent_hash,
                    number: block.header.number.checked_sub(1).expect("a block above genesis"),
                    timestamp: block.header.timestamp - rollup_config.block_time,
                    ..Default::default()
                },
            )
        });
        let next_l2_timestamp = initial_safe_head.timestamp + rollup_config.block_time;
        config
            .encoder
            .validate_for_rollup_config(rollup_config, next_l2_timestamp)
            .expect("an encoder config production accepts");
        let pipeline = BatchEncoder::new(Arc::new(rollup_config.clone()), config.encoder.clone())
            .expect("the config was validated");

        let chain = SharedL2Chain::new();
        for block in blocks {
            chain.push(block);
        }
        let source = HarnessBlockSource::new(&chain, initial_safe_head);
        let (l1_head_source, l1_head_tx) = HarnessL1HeadSource::new();
        let (admin, admin_rx) = AdminHandle::channel();

        let tx_manager = L1MinerTxManager::new(config.l1_signer.clone(), l1_chain_id);
        assert_eq!(
            config.batcher_address,
            tx_manager.sender_address(),
            "BatcherConfig::batcher_address must match BatcherConfig::l1_signer"
        );

        let (derivation_status_tx, derivation_status_rx) = mpsc::channel(1);

        let driver = BatchDriver::new(
            TokioRuntime::new(),
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
            DaThrottle::new(ThrottleController::disabled()),
            BatchDriverInputs {
                source,
                l1_head_source,
                // The driver learns the L1 head from the blocks the tests mine.
                initial_l1_head: 0,
                initial_safe_head,
                derivation_status_rx,
                admin_rx,
            },
        );

        let driver_task = tokio::spawn(driver.run());

        Self { chain, l1_head_tx, derivation_status_tx, admin, tx_manager, driver_task }
    }

    /// Make `block` the head of the L2 chain, dropping the blocks at or above its number, as
    /// a node would. The driver resets to its safe head when the next block it expects does not
    /// build on the last one it took.
    pub fn push_block(&self, block: BaseBlock) {
        self.chain.push(block);
    }

    /// Wait for the driver to encode every block pushed so far, then flush.
    ///
    /// Performs steps 1–3 of [`advance`] without mining. Once it returns, every submission the
    /// flush released has been handed to the tx manager, so [`pending_count`] counts them all.
    ///
    /// # Panics
    ///
    /// Panics if submissions are still pending or staged, or if the flush is rejected.
    ///
    /// [`advance`]: Batcher::advance
    /// [`pending_count`]: Batcher::pending_count
    pub async fn encode_only(&self) {
        assert!(
            self.pending_count() == 0 && self.staged_count() == 0,
            "cannot start a batch cycle with outstanding submissions"
        );

        // Admin commands outrank the block source in the driver's select, so wait until every
        // block pushed so far is taken and encoded before asking for the flush.
        self.wait_until_idle().await;
        self.admin.flush().await.unwrap_or_else(|e| panic!("flush failed: {e}"));

        // The flush is answered before the driver's next encode-and-submit pass. Wait for that
        // pass so every submission the flush released has been handed to the tx manager.
        self.wait_until_idle().await;
    }

    /// Wait until the driver has nothing left to do: everything sent so far is taken,
    /// encoded and submitted, and every receipt is applied.
    async fn wait_until_idle(&self) {
        let (reached_tx, reached_rx) = oneshot::channel();
        self.l1_head_tx.send(L1HeadItem::Marker(reached_tx)).expect("the batch driver has exited");
        let reached = tokio::time::timeout(Self::IDLE_TIMEOUT, reached_rx).await;
        assert!(matches!(reached, Ok(Ok(()))), "the batch driver exited or stalled");
    }

    /// Returns the number of submissions the driver sent that are not staged to L1 yet.
    pub fn pending_count(&self) -> usize {
        self.tx_manager.pending_count()
    }

    /// Returns the number of staged submissions waiting for inclusion receipts.
    pub fn staged_count(&self) -> usize {
        self.tx_manager.staged_count()
    }

    /// Stage the first `n` pending submissions to the L1 miner's queue without mining.
    /// Returns the actual count staged.
    pub fn stage_n_submissions(&self, l1: &mut L1Miner, n: usize) -> usize {
        self.tx_manager.stage_n_to_l1(l1, n)
    }

    /// Schedule the next `n` submissions to fail immediately.
    ///
    /// Each of the next `n` calls the background [`BatchDriver`] makes to
    /// [`TxManager::send_async`] will resolve with
    /// [`TxManagerError::Rpc`] instead of queuing to the L1 miner. The driver
    /// requeues the submission and retries, so calling this before [`encode_only`]
    /// simulates transient L1 submission failures without losing data. Once
    /// [`encode_only`] returns, the retried submissions are back in the pending queue.
    ///
    /// [`TxManager::send_async`]: base_tx_manager::TxManager::send_async
    /// [`TxManagerError::Rpc`]: base_tx_manager::TxManagerError::Rpc
    /// [`encode_only`]: Batcher::encode_only
    pub fn fail_next_n_submissions(&self, n: usize) {
        self.tx_manager.fail_next_n(n);
    }

    /// Mine all pending submissions in one L1 block.
    ///
    /// Stages every pending submission, mines one L1 block, fires all receipts and waits until
    /// the driver has confirmed them. Returns the mined block number.
    ///
    /// Use this to land what the driver submitted without encoding new L2 blocks.
    pub async fn mine_pending(&self, l1: &mut L1Miner) -> u64 {
        self.tx_manager.stage_n_to_l1(l1, usize::MAX);
        let block = l1.mine_block().clone();
        self.observe_l1_block(&block).await;
        block.number()
    }

    /// Drop the first `n` pending submissions without staging them to L1.
    ///
    /// Returns the actual number dropped. Use this to skip specific submissions when testing
    /// frames that land out of order. The driver sees each dropped submission fail and
    /// resubmits it on its next `async` call.
    pub fn drop_n_submissions(&self, n: usize) -> usize {
        self.tx_manager.drop_n(n)
    }

    /// Show a mined L1 block to the driver, as the tx manager's receipt polling and the L1
    /// head source would. Fires the receipts of the staged submissions the block includes,
    /// delivers its number as the new L1 head and waits until the driver has applied both. A
    /// block without any of the batcher's transactions only advances the L1 head.
    pub async fn observe_l1_block(&self, block: &L1Block) {
        // Fire the receipts first because the driver serves them before L1 heads, so a failed
        // submission is requeued before the head advances.
        self.tx_manager.confirm_block(block);
        self.l1_head_tx
            .send(L1HeadItem::Head(block.number()))
            .expect("the batch driver has exited");
        self.wait_until_idle().await;
    }

    /// Report derivation progress to the driver, as the production `DerivationStatusPoller`
    /// does on each change, and wait until the driver is idle again. By then it has reconciled
    /// with `status` and, after a reset, caught up from the new safe head. See
    /// [`TestRollupNode::derivation_status`](crate::TestRollupNode::derivation_status).
    pub async fn observe_derivation(&self, status: DerivationStatus) {
        // The channel holds one status and is empty here, because every async method returns once
        // the driver answered a marker and the driver takes a waiting status before it answers
        // one. So the send fails only once the driver has exited.
        self.derivation_status_tx
            .try_send(status)
            .unwrap_or_else(|error| panic!("the batch driver did not take the status: {error}"));
        self.wait_until_idle().await;
    }

    /// Run one full batch cycle through the production [`BatchDriver`] path.
    ///
    /// # Panics
    ///
    /// Panics where [`encode_only`](Self::encode_only) and
    /// [`mine_pending`](Self::mine_pending) do.
    pub async fn advance(&self, l1: &mut L1Miner) {
        self.encode_only().await;
        self.mine_pending(l1).await;
    }
}

impl Drop for Batcher {
    fn drop(&mut self) {
        self.driver_task.abort();
    }
}
