use std::{fmt::Debug, sync::Arc};

use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, Bloom, Bytes, U256};
use alloy_rlp::Decodable;
use alloy_rpc_types_engine::{
    ExecutionPayloadInputV2, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
    ForkchoiceState,
};
use base_alloy_consensus::{OpBlock, OpTxEnvelope, TxDeposit};
use base_alloy_provider::OpEngineApi;
use base_alloy_rpc_types_engine::OpExecutionPayloadV4;
use base_consensus_derive::{
    ActivationSignal, DerivationPipeline, IndexedAttributesQueueStage, Pipeline, PipelineError,
    PipelineErrorKind, ResetSignal, Signal, SignalReceiver, StatefulAttributesBuilder, StepResult,
};
use base_consensus_engine::{EngineForkchoiceVersion, EngineNewPayloadVersion};
use base_consensus_genesis::RollupConfig;
use base_consensus_safedb::{
    SafeDB, SafeDBError, SafeDBReader, SafeHeadListener, SafeHeadResponse,
};
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo, OpAttributesWithParent};

use crate::{
    ActionBlobDataSource, ActionDataSource, ActionEngineClient, ActionL1ChainProvider,
    ActionL2ChainProvider, TestGossipTransport,
};

/// The generic pipeline type used by [`TestRollupNode`] for any DA source `D`.
pub type ActionPipeline<D> = DerivationPipeline<
    IndexedAttributesQueueStage<
        D,
        ActionL1ChainProvider,
        ActionL2ChainProvider,
        StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
    >,
    ActionL2ChainProvider,
>;

/// The concrete pipeline type used by [`TestRollupNode`] with calldata DA.
pub type VerifierPipeline = DerivationPipeline<
    IndexedAttributesQueueStage<
        ActionDataSource,
        ActionL1ChainProvider,
        ActionL2ChainProvider,
        StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
    >,
    ActionL2ChainProvider,
>;

/// The concrete pipeline type used by [`TestRollupNode`] with blob DA.
pub type BlobVerifierPipeline = DerivationPipeline<
    IndexedAttributesQueueStage<
        ActionBlobDataSource,
        ActionL1ChainProvider,
        ActionL2ChainProvider,
        StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
    >,
    ActionL2ChainProvider,
>;

/// Errors returned by stepped action methods on [`TestRollupNode`].
#[derive(Debug, thiserror::Error)]
pub enum VerifierError {
    /// The pipeline returned a critical error.
    #[error("pipeline error: {0}")]
    Pipeline(Box<PipelineErrorKind>),
    /// A pipeline signal failed.
    #[error("signal error: {0}")]
    Signal(Box<PipelineErrorKind>),
}

/// A lightweight view of a derived L2 block's transaction counts.
///
/// Returned by [`TestRollupNode::derived_block`]. Use [`is_deposit_only`] to
/// assert that derivation force-generated a deposit-only block rather than
/// consuming a submitted batch (e.g. after a `NonEmptyTransitionBlock`
/// rejection or a sequencer-drift violation).
///
/// [`is_deposit_only`]: DerivedBlock::is_deposit_only
#[derive(Debug, Clone, Copy)]
pub struct DerivedBlock {
    /// L2 block number.
    pub number: u64,
    /// Total number of transactions as recorded in the payload attributes.
    pub tx_count: usize,
    /// Number of user transactions (excludes the L1 info deposit).
    ///
    /// `0` means the block is deposit-only.
    pub user_tx_count: usize,
}

impl DerivedBlock {
    /// Returns `true` if this block contains no user transactions.
    pub const fn is_deposit_only(&self) -> bool {
        self.user_tx_count == 0
    }
}

/// The result of a single [`TestRollupNode::step`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStepResult {
    /// The pipeline derived an L2 block, executed it through the engine, and advanced the safe
    /// head.
    DerivationProgress,
    /// The pipeline advanced to the next L1 origin or returned a transient status without
    /// producing a block.
    AdvancedOrigin,
    /// The pipeline is idle — all available L1 data has been consumed.
    Idle,
}

/// A deterministic in-process rollup node for action tests.
///
/// `TestRollupNode` composes the real 8-stage derivation pipeline with
/// [`ActionEngineClient`] (real EVM execution via revm) and
/// [`TestGossipTransport`] (in-memory P2P gossip). Tests drive it step-by-step:
///
/// # `SafeDB`
///
/// Each node opens a real [`SafeDB`] in a fresh temporary directory. This
/// exercises the actual persistence layer (redb) rather than a mock, so tests
/// catch encoding bugs and real read/write behaviour.
///
/// 1. Mine and push an L1 block: `h.mine_and_push(&chain)`.
/// 2. Signal the new head: `node.act_l1_head_signal(block_info).await`.
/// 3. Step the node: `node.step().await`.
/// 4. Assert safe head advanced: `node.l2_safe().block_info.number`.
///
/// Each call to [`step`] first drains any gossiped unsafe blocks from the P2P
/// transport, then performs one pipeline step. When the pipeline produces
/// derived attributes, they are executed through the engine and the safe head
/// is advanced.
///
/// Call [`run_until_idle`] to exhaust all available L1 data in one shot.
///
/// Use [`ActionTestHarness::create_test_rollup_node`] to construct an instance
/// wired to a running [`L2Sequencer`].
///
/// [`step`]: TestRollupNode::step
/// [`run_until_idle`]: TestRollupNode::run_until_idle
/// [`ActionTestHarness::create_test_rollup_node`]: crate::ActionTestHarness::create_test_rollup_node
/// [`L2Sequencer`]: crate::L2Sequencer
#[derive(Debug)]
pub struct TestRollupNode<P: Pipeline + SignalReceiver + Debug + Send = VerifierPipeline> {
    /// The real derivation pipeline wired to in-memory providers.
    pipeline: P,
    /// In-process engine client performing real EVM execution.
    engine: ActionEngineClient,
    /// Channel-backed gossip transport for receiving unsafe blocks.
    p2p: TestGossipTransport,
    /// Current L2 safe head (advances as derived attributes are applied).
    safe_head: L2BlockInfo,
    /// Current L2 unsafe head (advances from P2P gossip or derivation).
    unsafe_head: L2BlockInfo,
    /// Current L2 finalized head.
    finalized_head: L2BlockInfo,
    /// Most recently signalled finalized L1 block number.
    finalized_l1_number: u64,
    /// Most recently signalled L1 safe block number.
    safe_l1_number: u64,
    /// History of safe head updates paired with their L1 origin number.
    ///
    /// Each entry is `(l2_block_info, l1_origin_number)`. Used by
    /// [`act_l1_finalized_signal`] to find the highest L2 block whose L1
    /// origin is finalized.
    ///
    /// [`act_l1_finalized_signal`]: TestRollupNode::act_l1_finalized_signal
    safe_head_history: Vec<(L2BlockInfo, u64)>,
    /// Total transaction count per derived L2 block.
    derived_tx_counts: Vec<(u64, usize)>,
    /// User transaction count per derived L2 block (excludes L1 info deposit).
    derived_user_tx_counts: Vec<(u64, usize)>,
    /// Decoded L1 info transactions per derived L2 block.
    derived_l1_info_txs: Vec<(u64, L1BlockInfoTx)>,
    /// Shared rollup configuration used for Engine API version selection.
    rollup_config: Arc<RollupConfig>,
    /// redb-backed safe head database for assertion-level testing.
    safe_db: Arc<SafeDB>,
}

impl<P: Pipeline + SignalReceiver + Debug + Send> TestRollupNode<P> {
    /// Construct a new `TestRollupNode` from pre-built components.
    ///
    /// The `pipeline`, `engine`, and `p2p` are the three actors that the node
    /// orchestrates. `safe_head` is the L2 genesis head from which derivation
    /// starts. `rollup_config` is used for Engine API version selection.
    ///
    /// Call [`initialize`] before the first [`step`] or [`run_until_idle`].
    ///
    /// [`initialize`]: TestRollupNode::initialize
    /// [`step`]: TestRollupNode::step
    /// [`run_until_idle`]: TestRollupNode::run_until_idle
    pub fn new(
        pipeline: P,
        engine: ActionEngineClient,
        p2p: TestGossipTransport,
        safe_head: L2BlockInfo,
        rollup_config: Arc<RollupConfig>,
    ) -> Self {
        let safedb_dir =
            tempfile::TempDir::new().expect("TestRollupNode: failed to create safedb temp dir");
        let safe_db = Arc::new(
            SafeDB::open(safedb_dir.path().join("safedb"))
                .expect("TestRollupNode: failed to open safedb"),
        );
        Self {
            pipeline,
            engine,
            p2p,
            safe_head,
            unsafe_head: safe_head,
            finalized_head: safe_head,
            finalized_l1_number: 0,
            safe_l1_number: 0,
            safe_head_history: Vec::new(),
            derived_tx_counts: Vec::new(),
            derived_user_tx_counts: Vec::new(),
            derived_l1_info_txs: Vec::new(),
            rollup_config,
            safe_db,
        }
    }

    /// Initialize the pipeline by sending the genesis activation signal and
    /// draining the genesis L1 block.
    ///
    /// Must be called once before any [`step`] or [`run_until_idle`] calls.
    ///
    /// [`step`]: TestRollupNode::step
    /// [`run_until_idle`]: TestRollupNode::run_until_idle
    pub async fn initialize(&mut self) {
        self.pipeline
            .signal(ActivationSignal { l2_safe_head: self.safe_head }.signal())
            .await
            .expect("TestRollupNode: initialize signal failed");
        self.run_until_idle().await;
    }

    /// Return the current L2 safe head.
    pub const fn l2_safe(&self) -> L2BlockInfo {
        self.safe_head
    }

    /// Return the block number of the current L2 safe head.
    pub const fn l2_safe_number(&self) -> u64 {
        self.safe_head.block_info.number
    }

    /// Return the current L2 unsafe head.
    ///
    /// Advances ahead of the safe head as P2P gossip blocks are received.
    /// Falls back to the safe head when no gossip has been received.
    pub const fn l2_unsafe(&self) -> L2BlockInfo {
        self.unsafe_head
    }

    /// Return the block number of the current L2 unsafe head.
    pub const fn l2_unsafe_number(&self) -> u64 {
        self.unsafe_head.block_info.number
    }

    /// Return the current L2 finalized head.
    pub const fn l2_finalized(&self) -> L2BlockInfo {
        self.finalized_head
    }

    /// Return the block number of the current L2 finalized head.
    pub const fn l2_finalized_number(&self) -> u64 {
        self.finalized_head.block_info.number
    }

    /// Return the most recently signalled L1 safe block number.
    pub const fn safe_l1_number(&self) -> u64 {
        self.safe_l1_number
    }

    /// Return the current L1 origin the pipeline is positioned at.
    pub fn l1_origin(&self) -> Option<BlockInfo> {
        self.pipeline.origin()
    }

    /// Query the safe head recorded for a given L1 block number from the persistent `SafeDB`.
    ///
    /// Returns the safe head at or before `l1_block_num`, or a [`SafeDBError`]
    /// if no entry exists (e.g. `NotFound` when no L2 block has been derived yet).
    /// Call `.unwrap()` at the test site when success is expected, or match on
    /// the error to assert specific error conditions.
    pub async fn safe_head_at_l1(
        &self,
        l1_block_num: u64,
    ) -> Result<SafeHeadResponse, SafeDBError> {
        self.safe_db.safe_head_at_l1(l1_block_num).await
    }

    /// Return the total transaction counts for each derived L2 block.
    ///
    /// Each entry is `(l2_block_number, tx_count)`.
    pub fn derived_tx_counts(&self) -> &[(u64, usize)] {
        &self.derived_tx_counts
    }

    /// Return the user transaction counts for each derived L2 block.
    ///
    /// Each entry is `(l2_block_number, user_tx_count)`. A count of `0` means
    /// deposit-only.
    pub fn derived_user_tx_counts(&self) -> &[(u64, usize)] {
        &self.derived_user_tx_counts
    }

    /// Return the decoded L1 info transactions for each derived L2 block.
    ///
    /// Each entry is `(l2_block_number, l1_info_tx)`.
    pub fn derived_l1_info_txs(&self) -> &[(u64, L1BlockInfoTx)] {
        &self.derived_l1_info_txs
    }

    /// Look up the derived-block summary for the given L2 block number.
    ///
    /// Returns `None` if no block with that number has been derived in this
    /// session. Use [`DerivedBlock::is_deposit_only`] to assert that
    /// derivation force-generated a deposit-only block.
    pub fn derived_block(&self, number: u64) -> Option<DerivedBlock> {
        let tx_count =
            self.derived_tx_counts.iter().find(|(n, _)| *n == number).map(|(_, c)| *c)?;
        let user_tx_count = self
            .derived_user_tx_counts
            .iter()
            .find(|(n, _)| *n == number)
            .map(|(_, c)| *c)
            .unwrap_or(0);
        Some(DerivedBlock { number, tx_count, user_tx_count })
    }

    /// Manually register the block hash for a given L2 block number.
    ///
    /// Not needed when the node was created via
    /// [`create_test_rollup_node`] — the sequencer populates the shared
    /// registry automatically. Use this only for blocks produced outside the
    /// sequencer.
    ///
    /// [`create_test_rollup_node`]: crate::ActionTestHarness::create_test_rollup_node
    pub fn register_block_hash(&mut self, number: u64, hash: B256) {
        self.engine.block_hash_registry().insert(number, hash, None);
    }

    /// Signal the pipeline that a new L1 block is available.
    pub async fn act_l1_head_signal(&mut self, head: BlockInfo) {
        self.pipeline
            .signal(Signal::ProvideBlock(head))
            .await
            .expect("TestRollupNode: act_l1_head_signal failed");
    }

    /// Record the L1 safe head number. Does not drive additional pipeline steps.
    pub async fn act_l1_safe_signal(&mut self, head: BlockInfo) -> Result<(), VerifierError> {
        self.safe_l1_number = head.number;
        Ok(())
    }

    /// Update the L2 finalized head by scanning safe-head history.
    ///
    /// Finds the highest L2 safe block whose L1 origin number is ≤
    /// `head.number` and promotes it to `finalized_head`.
    pub async fn act_l1_finalized_signal(&mut self, head: BlockInfo) {
        self.finalized_l1_number = head.number;
        let candidate = self
            .safe_head_history
            .iter()
            .rev()
            .find(|(_, l1_origin_number)| *l1_origin_number <= self.finalized_l1_number)
            .map(|(l2_info, _)| *l2_info);
        if let Some(l2) = candidate
            && l2.block_info.number > self.finalized_head.block_info.number
        {
            self.finalized_head = l2;
        }
    }

    /// Reset the pipeline to a new L1 origin and L2 safe head after a reorg.
    ///
    /// Sends a [`ResetSignal`] to the pipeline, resets all head pointers,
    /// clears all per-block metadata, and replaces the engine with a fresh
    /// instance that will re-execute from genesis on the new fork.
    pub async fn act_reset(&mut self, l2_safe_head: L2BlockInfo) {
        self.pipeline
            .signal(ResetSignal { l2_safe_head }.signal())
            .await
            .expect("TestRollupNode: act_reset signal failed");
        self.safe_head = l2_safe_head;
        self.unsafe_head = l2_safe_head;
        self.finalized_head = l2_safe_head;
        self.finalized_l1_number = 0;
        self.safe_l1_number = 0;
        self.safe_head_history.clear();
        self.derived_tx_counts.clear();
        self.derived_user_tx_counts.clear();
        self.derived_l1_info_txs.clear();
        self.safe_db
            .safe_head_reset(l2_safe_head)
            .await
            .expect("TestRollupNode: safe_db reset failed");
        // Replace the engine with a fresh instance: the stateful EVM must restart
        // from genesis to correctly re-execute the new fork's blocks.
        self.engine = ActionEngineClient::new(
            Arc::clone(&self.rollup_config),
            l2_safe_head,
            self.engine.block_hash_registry(),
            self.engine.l1_chain(),
        );
    }

    /// Inject an unsafe L2 block directly, as if received via P2P gossip.
    ///
    /// This bypasses the [`TestGossipTransport`] channel and directly advances
    /// `unsafe_head`. Only accepts the strictly next sequential block; gaps and
    /// duplicates are silently dropped.
    ///
    /// The block hash is registered in the engine's shared registry so that
    /// parent-hash chaining works in subsequent derivation.
    ///
    /// # Panics
    ///
    /// Panics if the first transaction is not a valid L1 info deposit.
    pub fn act_l2_unsafe_gossip_receive(&mut self, block: &OpBlock) {
        if block.header.number != self.unsafe_head.block_info.number + 1 {
            return;
        }
        let hash = block.header.hash_slow();
        self.engine.block_hash_registry().insert(
            block.header.number,
            hash,
            Some(block.header.state_root),
        );
        let l1_origin = self.l1_origin_from_block(block).unwrap_or_else(|| {
            panic!(
                "TestRollupNode: missing or invalid L1 info deposit in block {}",
                block.header.number
            )
        });
        let seq_num =
            if l1_origin == self.unsafe_head.l1_origin { self.unsafe_head.seq_num + 1 } else { 0 };
        self.unsafe_head = L2BlockInfo {
            block_info: BlockInfo {
                number: block.header.number,
                hash,
                parent_hash: block.header.parent_hash,
                timestamp: block.header.timestamp,
            },
            l1_origin,
            seq_num,
        };
    }

    /// Perform one complete derivation-and-execution cycle.
    ///
    /// Drains any pending gossip first, then performs one pipeline step. When
    /// the pipeline produces derived attributes they are executed through the
    /// engine and the safe head is advanced.
    pub async fn step(&mut self) -> NodeStepResult {
        self.drain_gossip();

        match self.pipeline.step(self.safe_head).await {
            StepResult::PreparedAttributes => {
                let Some(attrs) = self.pipeline.next() else {
                    return NodeStepResult::AdvancedOrigin;
                };
                self.execute_and_advance(attrs).await;
                NodeStepResult::DerivationProgress
            }
            StepResult::AdvancedOrigin => NodeStepResult::AdvancedOrigin,
            StepResult::StepFailed(err) => match err {
                PipelineErrorKind::Temporary(PipelineError::Eof) => NodeStepResult::Idle,
                PipelineErrorKind::Temporary(
                    PipelineError::NotEnoughData | PipelineError::ChannelReaderEmpty,
                ) => NodeStepResult::AdvancedOrigin,
                err => panic!("TestRollupNode: pipeline error: {err}"),
            },
            StepResult::OriginAdvanceErr(err) => match err {
                PipelineErrorKind::Temporary(PipelineError::Eof) => NodeStepResult::Idle,
                err => panic!("TestRollupNode: origin advance error: {err}"),
            },
        }
    }

    /// Execute exactly one pipeline step and return the raw [`StepResult`].
    ///
    /// Drains any pending gossip first. When the step returns
    /// [`StepResult::PreparedAttributes`] the attributes are executed through
    /// the engine and the safe head is advanced automatically. Transient errors
    /// (`Eof`, `NotEnoughData`) are returned as-is so the caller can decide how
    /// to handle them.
    ///
    /// Returns [`VerifierError::Pipeline`] on any non-transient pipeline error.
    pub async fn act_l2_pipeline_step(&mut self) -> Result<StepResult, VerifierError> {
        self.drain_gossip();
        let result = self.pipeline.step(self.safe_head).await;
        if matches!(result, StepResult::PreparedAttributes)
            && let Some(attrs) = self.pipeline.next()
        {
            self.execute_and_advance(attrs).await;
        }
        match result {
            StepResult::PreparedAttributes | StepResult::AdvancedOrigin => Ok(result),
            StepResult::StepFailed(PipelineErrorKind::Temporary(e)) => {
                Ok(StepResult::StepFailed(PipelineErrorKind::Temporary(e)))
            }
            StepResult::OriginAdvanceErr(PipelineErrorKind::Temporary(e)) => {
                Ok(StepResult::OriginAdvanceErr(PipelineErrorKind::Temporary(e)))
            }
            StepResult::StepFailed(err) | StepResult::OriginAdvanceErr(err) => {
                Err(VerifierError::Pipeline(Box::new(err)))
            }
        }
    }

    /// Step the pipeline until `condition` returns `true` for a [`StepResult`],
    /// or until the pipeline reaches EOF, or until `max_steps` is exhausted.
    ///
    /// Drains gossip before each step. Attributes are executed through the
    /// engine automatically on each [`StepResult::PreparedAttributes`] step.
    ///
    /// Returns `(steps_taken, condition_met)`. `condition_met` is `false` when
    /// the pipeline reached EOF or `max_steps` before the predicate fired.
    ///
    /// Returns [`VerifierError::Pipeline`] on any non-transient pipeline error,
    /// or when `NotEnoughData` fires more than 1 000 consecutive times.
    pub async fn act_l2_pipeline_until(
        &mut self,
        condition: impl Fn(&StepResult) -> bool,
        max_steps: usize,
    ) -> Result<(usize, bool), VerifierError> {
        let mut steps = 0;
        let mut no_progress = 0usize;
        loop {
            if steps >= max_steps {
                return Ok((steps, false));
            }
            self.drain_gossip();
            let result = self.pipeline.step(self.safe_head).await;
            steps += 1;
            if matches!(result, StepResult::PreparedAttributes)
                && let Some(attrs) = self.pipeline.next()
            {
                self.execute_and_advance(attrs).await;
            }
            if condition(&result) {
                return Ok((steps, true));
            }
            match result {
                StepResult::PreparedAttributes | StepResult::AdvancedOrigin => {
                    no_progress = 0;
                }
                StepResult::StepFailed(err) => match err {
                    PipelineErrorKind::Temporary(PipelineError::Eof) => {
                        return Ok((steps, false));
                    }
                    PipelineErrorKind::Temporary(
                        PipelineError::NotEnoughData | PipelineError::ChannelReaderEmpty,
                    ) => {
                        no_progress += 1;
                        if no_progress > 1_000 {
                            return Err(VerifierError::Pipeline(Box::new(
                                PipelineError::Provider(
                                    "pipeline stuck: 1000 consecutive no-progress without \
                                     derivation"
                                        .into(),
                                )
                                .temp(),
                            )));
                        }
                    }
                    err => return Err(VerifierError::Pipeline(Box::new(err))),
                },
                StepResult::OriginAdvanceErr(err) => match err {
                    PipelineErrorKind::Temporary(PipelineError::Eof) => {
                        return Ok((steps, false));
                    }
                    err => return Err(VerifierError::Pipeline(Box::new(err))),
                },
            }
        }
    }

    /// Loop over the pipeline until it is idle, executing every derived block
    /// through the engine.
    ///
    /// Returns the number of L2 blocks derived and executed.
    pub async fn run_until_idle(&mut self) -> usize {
        let mut derived = 0;
        loop {
            match self.step().await {
                NodeStepResult::DerivationProgress => derived += 1,
                NodeStepResult::AdvancedOrigin => {}
                NodeStepResult::Idle => break,
            }
        }
        derived
    }

    /// Drain any pending gossip blocks from the P2P transport without blocking.
    ///
    /// For each received [`OpNetworkPayloadEnvelope`], the unsafe head is
    /// advanced if the block is the next sequential block. Gaps and duplicates
    /// are silently ignored.
    ///
    /// [`OpNetworkPayloadEnvelope`]: base_alloy_rpc_types_engine::OpNetworkPayloadEnvelope
    fn drain_gossip(&mut self) {
        while let Some(envelope) = self.p2p.try_next_unsafe_block() {
            let payload = &envelope.payload;
            let number = payload.block_number();
            if number != self.unsafe_head.block_info.number + 1 {
                continue;
            }
            let l1_origin = self.l1_origin_from_transactions(payload.transactions());
            let seq_num = l1_origin
                .map(|origin| {
                    if origin == self.unsafe_head.l1_origin {
                        self.unsafe_head.seq_num + 1
                    } else {
                        0
                    }
                })
                .unwrap_or(0);
            self.unsafe_head = L2BlockInfo {
                block_info: BlockInfo {
                    number,
                    hash: payload.block_hash(),
                    parent_hash: payload.parent_hash(),
                    timestamp: payload.timestamp(),
                },
                l1_origin: l1_origin.unwrap_or_default(),
                seq_num,
            };
        }
    }

    /// Execute derived attributes through the engine, record per-block
    /// metadata, and advance the safe head.
    ///
    /// Extracts transaction counts and the L1 info tx from the attributes
    /// before submission so they are available via [`derived_block`] after
    /// the call returns.
    ///
    /// [`derived_block`]: TestRollupNode::derived_block
    async fn execute_and_advance(&mut self, attrs: OpAttributesWithParent) {
        let block_number = attrs.block_number();
        let parent_hash = self.safe_head.block_info.hash;
        let timestamp = attrs.attributes.payload_attributes.timestamp;
        let gas_limit = attrs.attributes.gas_limit.unwrap_or(30_000_000);
        let raw_txs: Vec<Bytes> = attrs.attributes.transactions.as_deref().unwrap_or(&[]).to_vec();

        // Record per-block metadata before submitting to the engine.
        let tx_count = raw_txs.len();
        let user_tx_count = raw_txs.iter().filter(|tx| !tx.starts_with(&[0x7E])).count();
        self.derived_tx_counts.push((block_number, tx_count));
        self.derived_user_tx_counts.push((block_number, user_tx_count));
        if let Some(l1_info) = self.l1_info_from_attrs(&attrs) {
            self.derived_l1_info_txs.push((block_number, l1_info));
        }

        // Construct a payload shell. The engine fills in state_root, gas_used,
        // and the real block_hash during execution.
        let payload_v1 = ExecutionPayloadV1 {
            parent_hash,
            fee_recipient: attrs.attributes.payload_attributes.suggested_fee_recipient,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::default(),
            prev_randao: attrs.attributes.payload_attributes.prev_randao,
            block_number,
            gas_limit,
            gas_used: 0,
            timestamp,
            extra_data: Bytes::default(),
            base_fee_per_gas: U256::from(1_000_000_000u64),
            block_hash: B256::ZERO,
            transactions: raw_txs,
        };

        let pbr = attrs.attributes.payload_attributes.parent_beacon_block_root.unwrap_or_default();

        let block_hash = match EngineNewPayloadVersion::from_cfg(&self.rollup_config, timestamp) {
            EngineNewPayloadVersion::V2 => {
                let payload =
                    ExecutionPayloadInputV2 { execution_payload: payload_v1, withdrawals: None };
                self.engine
                    .new_payload_v2(payload)
                    .await
                    .expect("TestRollupNode: new_payload_v2 failed")
                    .latest_valid_hash
                    .unwrap_or_default()
            }
            EngineNewPayloadVersion::V3 => {
                let payload = ExecutionPayloadV3 {
                    payload_inner: ExecutionPayloadV2 {
                        payload_inner: payload_v1,
                        withdrawals: vec![],
                    },
                    blob_gas_used: 0,
                    excess_blob_gas: 0,
                };
                self.engine
                    .new_payload_v3(payload, pbr)
                    .await
                    .expect("TestRollupNode: new_payload_v3 failed")
                    .latest_valid_hash
                    .unwrap_or_default()
            }
            EngineNewPayloadVersion::V4 => {
                let payload = OpExecutionPayloadV4 {
                    payload_inner: ExecutionPayloadV3 {
                        payload_inner: ExecutionPayloadV2 {
                            payload_inner: payload_v1,
                            withdrawals: vec![],
                        },
                        blob_gas_used: 0,
                        excess_blob_gas: 0,
                    },
                    withdrawals_root: B256::ZERO,
                };
                self.engine
                    .new_payload_v4(payload, pbr)
                    .await
                    .expect("TestRollupNode: new_payload_v4 failed")
                    .latest_valid_hash
                    .unwrap_or_default()
            }
        };

        // Advance the safe head using the block hash returned by the engine.
        let derived_from = attrs.derived_from;
        let l1_origin = self
            .l1_origin_from_attrs(&attrs)
            .or_else(|| attrs.derived_from.map(|b| BlockNumHash { hash: b.hash, number: b.number }))
            .unwrap_or_default();
        let seq_num =
            if l1_origin == self.safe_head.l1_origin { self.safe_head.seq_num + 1 } else { 0 };
        self.safe_head = L2BlockInfo {
            block_info: BlockInfo {
                hash: block_hash,
                number: block_number,
                parent_hash,
                timestamp,
            },
            l1_origin,
            seq_num,
        };
        self.safe_head_history.push((self.safe_head, l1_origin.number));
        // `attrs.derived_from` carries the full L1 BlockInfo set by the pipeline's
        // AttributesQueueStage. It is always `Some` for attributes produced by the
        // real derivation pipeline. The `None` fallback is defensive — it triggers
        // only if attributes are injected directly without going through the pipeline
        // (not currently done in these tests). In that case we fall back to
        // `l1_origin` (hash + number only); parent_hash and timestamp are zeroed.
        // SafeDB only stores hash and number, so the zeroed fields have no effect.
        let l1_block_for_db = derived_from.unwrap_or(BlockInfo {
            number: l1_origin.number,
            hash: l1_origin.hash,
            parent_hash: B256::ZERO, // unavailable without full L1 block
            timestamp: 0,            // unavailable without full L1 block
        });
        self.safe_db
            .safe_head_updated(self.safe_head, l1_block_for_db)
            .await
            .expect("TestRollupNode: safe_db update failed");

        // If derivation caught up to or past the unsafe head, align them.
        if self.safe_head.block_info.number >= self.unsafe_head.block_info.number {
            self.unsafe_head = self.safe_head;
        }

        // Commit the new safe head to the engine via fork_choice_updated.
        let fcu = ForkchoiceState {
            head_block_hash: block_hash,
            safe_block_hash: block_hash,
            finalized_block_hash: self.finalized_head.block_info.hash,
        };
        match EngineForkchoiceVersion::from_cfg(&self.rollup_config, timestamp) {
            EngineForkchoiceVersion::V2 => {
                self.engine
                    .fork_choice_updated_v2(fcu, None)
                    .await
                    .expect("TestRollupNode: fork_choice_updated_v2 failed");
            }
            EngineForkchoiceVersion::V3 => {
                self.engine
                    .fork_choice_updated_v3(fcu, None)
                    .await
                    .expect("TestRollupNode: fork_choice_updated_v3 failed");
            }
        }
    }

    /// Decode the L1 epoch from the first L1 info deposit in a raw transaction list.
    fn l1_origin_from_transactions(&self, transactions: &[Bytes]) -> Option<BlockNumHash> {
        Self::decode_deposit_l1_info(transactions.first()?).map(|info| info.id())
    }

    /// Decode the L1 epoch from the first deposit transaction in derived attributes.
    pub fn l1_origin_from_attrs(&self, attrs: &OpAttributesWithParent) -> Option<BlockNumHash> {
        let txs = attrs.attributes.transactions.as_ref()?;
        self.l1_origin_from_transactions(txs)
    }

    /// Decode the L1 epoch from the first deposit transaction in an [`OpBlock`].
    fn l1_origin_from_block(&self, block: &OpBlock) -> Option<BlockNumHash> {
        let first = block.body.transactions.first()?;
        let deposit = match first {
            OpTxEnvelope::Deposit(d) => d,
            _ => return None,
        };
        let l1_info = L1BlockInfoTx::decode_calldata(deposit.inner().input.as_ref()).ok()?;
        Some(l1_info.id())
    }

    /// Decode the full [`L1BlockInfoTx`] from the first deposit transaction in
    /// derived [`OpAttributesWithParent`].
    fn l1_info_from_attrs(&self, attrs: &OpAttributesWithParent) -> Option<L1BlockInfoTx> {
        let txs = attrs.attributes.transactions.as_ref()?;
        Self::decode_deposit_l1_info(txs.first()?)
    }

    /// Strip the deposit type byte, RLP-decode the [`TxDeposit`], then decode
    /// the L1 info calldata. Returns `None` if any step fails.
    fn decode_deposit_l1_info(raw: &Bytes) -> Option<L1BlockInfoTx> {
        let rlp_bytes = raw.strip_prefix(&[0x7E])?;
        let deposit = TxDeposit::decode(&mut &*rlp_bytes).ok()?;
        L1BlockInfoTx::decode_calldata(deposit.input.as_ref()).ok()
    }
}
