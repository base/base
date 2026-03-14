use std::{collections::HashMap, fmt::Debug, sync::Arc};

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use base_alloy_consensus::{OpBlock, OpTxEnvelope, TxDeposit};
use base_consensus_derive::{
    ActivationSignal, IndexedAttributesQueueStage, Pipeline, PipelineBuilder, PipelineError,
    PipelineErrorKind, ResetSignal, Signal, SignalReceiver, StatefulAttributesBuilder, StepResult,
};
use base_consensus_genesis::{L1ChainConfig, RollupConfig, SystemConfig};
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo, OpAttributesWithParent};

use crate::{ActionBlobDataSource, ActionDataSource, ActionL1ChainProvider, ActionL2ChainProvider};

/// The concrete pipeline type used by [`L2Verifier`] with calldata DA.
///
/// Assembled from the indexed traversal path so tests drive derivation block-by-block
/// via [`L2Verifier::act_l1_head_signal`] rather than polling an RPC.
pub type VerifierPipeline = base_consensus_derive::DerivationPipeline<
    IndexedAttributesQueueStage<
        ActionDataSource,
        ActionL1ChainProvider,
        ActionL2ChainProvider,
        StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
    >,
    ActionL2ChainProvider,
>;

/// The concrete pipeline type used by [`L2Verifier`] with blob DA.
pub type BlobVerifierPipeline = base_consensus_derive::DerivationPipeline<
    IndexedAttributesQueueStage<
        ActionBlobDataSource,
        ActionL1ChainProvider,
        ActionL2ChainProvider,
        StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
    >,
    ActionL2ChainProvider,
>;

/// Errors returned by [`L2Verifier`] action methods.
#[derive(Debug, thiserror::Error)]
pub enum VerifierError {
    /// The pipeline returned a critical error.
    #[error("pipeline error: {0}")]
    Pipeline(Box<PipelineErrorKind>),
    /// A pipeline signal failed.
    #[error("signal error: {0}")]
    Signal(Box<PipelineErrorKind>),
    /// The gossiped block has no L1 info deposit as its first transaction.
    #[error("gossip receive: missing or invalid L1 info deposit in block")]
    GossipDecodeFailed,
}

/// In-process rollup node for action tests.
///
/// `L2Verifier` couples the real 8-stage derivation pipeline with an in-memory
/// L2 chain state. Tests drive it step-by-step:
///
/// 1. Mine an L1 block: `h.l1.mine_block()`.
/// 2. Signal the new head: `verifier.act_l1_head_signal(&block_info).await`.
/// 3. Step derivation: `verifier.act_l2_pipeline_full().await`.
/// 4. Assert safe head advanced: `verifier.l2_safe().number`.
///
/// There is no P2P, no background async task, and no real EVM execution.
/// The "engine" is a simple in-memory state that accepts derived
/// [`OpAttributesWithParent`] and advances `l2_safe` accordingly.
///
/// The type parameter `P` is the derivation pipeline. Use [`VerifierPipeline`]
/// for calldata DA or [`BlobVerifierPipeline`] for blob DA.
#[derive(Debug)]
pub struct L2Verifier<P: Pipeline + SignalReceiver + Debug> {
    /// The real derivation pipeline wired to in-memory providers.
    pipeline: P,
    /// The current L2 safe head (advances as attributes are consumed).
    safe_head: L2BlockInfo,
    /// The current L2 unsafe head.
    ///
    /// In a verifier-only setup this equals `safe_head`. When unsafe blocks are
    /// injected via [`act_l2_unsafe_gossip_receive`], it advances independently
    /// and ahead of `safe_head` until derivation catches up.
    ///
    /// [`act_l2_unsafe_gossip_receive`]: L2Verifier::act_l2_unsafe_gossip_receive
    unsafe_head: L2BlockInfo,
    /// The current L2 finalized head.
    ///
    /// Updated via [`act_l1_finalized_signal`] by scanning [`safe_head_history`]
    /// for the highest safe L2 block whose L1 origin is ≤ the finalized L1 number.
    ///
    /// [`act_l1_finalized_signal`]: L2Verifier::act_l1_finalized_signal
    /// [`safe_head_history`]: L2Verifier::safe_head_history
    finalized_head: L2BlockInfo,
    /// Tracks the most recently signalled finalized L1 block number.
    finalized_l1_number: u64,
    /// History of safe head updates paired with the L1 origin number.
    ///
    /// Each entry is `(l2_block_info, l1_origin_number)`. Appended by
    /// [`apply_attributes`] so that [`act_l1_finalized_signal`] can scan
    /// backward to find the highest L2 block whose L1 origin is finalized.
    ///
    /// [`apply_attributes`]: L2Verifier::apply_attributes
    /// [`act_l1_finalized_signal`]: L2Verifier::act_l1_finalized_signal
    safe_head_history: Vec<(L2BlockInfo, u64)>,
    /// Block hashes by L2 block number, registered externally from the
    /// [`L2Sequencer`](crate::L2Sequencer). Used by [`apply_attributes`]
    /// so the verifier's safe-head hash matches the sequencer's real block
    /// hash, enabling correct `parent_hash` validation in [`BatchQueue`].
    ///
    /// [`apply_attributes`]: L2Verifier::apply_attributes
    /// [`BatchQueue`]: base_consensus_derive::BatchQueue
    block_hashes: HashMap<u64, B256>,
    /// User transaction counts per derived L2 block, recorded in [`apply_attributes`].
    ///
    /// Each entry is `(l2_block_number, user_tx_count)`. Deposit-only blocks —
    /// whether force-included after a dropped batch or generated at hardfork
    /// upgrade boundaries — have a count of `0`.
    ///
    /// [`apply_attributes`]: L2Verifier::apply_attributes
    derived_user_tx_counts: Vec<(u64, usize)>,
    /// Decoded L1 info transactions per derived L2 block, recorded in [`apply_attributes`].
    ///
    /// Each entry is `(l2_block_number, l1_info_tx)`. The [`L1BlockInfoTx`]
    /// exposes hardfork-specific fee parameters, including `operator_fee_scalar`
    /// and `operator_fee_constant` for Isthmus/Jovian blocks.
    ///
    /// [`apply_attributes`]: L2Verifier::apply_attributes
    derived_l1_info_txs: Vec<(u64, L1BlockInfoTx)>,
}

impl L2Verifier<VerifierPipeline> {
    /// Construct an [`L2Verifier`] with a calldata DA pipeline.
    ///
    /// `origin` is the L1 genesis block the pipeline starts from. Pass the
    /// genesis block from [`L1Miner`](crate::L1Miner) so parent-hash chaining
    /// is correct from block 1 onwards.
    pub fn new(
        rollup_config: Arc<RollupConfig>,
        l1_chain_config: Arc<L1ChainConfig>,
        l1_provider: ActionL1ChainProvider,
        dap_source: ActionDataSource,
        l2_provider: ActionL2ChainProvider,
        safe_head: L2BlockInfo,
        origin: BlockInfo,
    ) -> Self {
        let attrs_builder = StatefulAttributesBuilder::new(
            Arc::clone(&rollup_config),
            l1_chain_config,
            l2_provider.clone(),
            l1_provider.clone(),
        );

        let pipeline = PipelineBuilder::new()
            .rollup_config(Arc::clone(&rollup_config))
            .origin(origin)
            .chain_provider(l1_provider)
            .dap_source(dap_source)
            .l2_chain_provider(l2_provider)
            .builder(attrs_builder)
            .build_indexed();

        Self::from_pipeline(pipeline, safe_head)
    }
}

impl L2Verifier<BlobVerifierPipeline> {
    /// Construct an [`L2Verifier`] with a blob DA pipeline.
    ///
    /// Identical to [`L2Verifier::new`] but wired to [`ActionBlobDataSource`]
    /// instead of [`ActionDataSource`].
    pub fn new_blob(
        rollup_config: Arc<RollupConfig>,
        l1_chain_config: Arc<L1ChainConfig>,
        l1_provider: ActionL1ChainProvider,
        dap_source: ActionBlobDataSource,
        l2_provider: ActionL2ChainProvider,
        safe_head: L2BlockInfo,
        origin: BlockInfo,
    ) -> Self {
        let attrs_builder = StatefulAttributesBuilder::new(
            Arc::clone(&rollup_config),
            l1_chain_config,
            l2_provider.clone(),
            l1_provider.clone(),
        );

        let pipeline = PipelineBuilder::new()
            .rollup_config(Arc::clone(&rollup_config))
            .origin(origin)
            .chain_provider(l1_provider)
            .dap_source(dap_source)
            .l2_chain_provider(l2_provider)
            .builder(attrs_builder)
            .build_indexed();

        Self::from_pipeline(pipeline, safe_head)
    }
}

impl<P: Pipeline + SignalReceiver + Debug + Send> L2Verifier<P> {
    /// Construct an [`L2Verifier`] from an already-built pipeline.
    pub fn from_pipeline(pipeline: P, safe_head: L2BlockInfo) -> Self {
        Self {
            pipeline,
            safe_head,
            unsafe_head: safe_head,
            finalized_head: safe_head,
            finalized_l1_number: 0,
            safe_head_history: Vec::new(),
            block_hashes: HashMap::new(),
            derived_user_tx_counts: Vec::new(),
            derived_l1_info_txs: Vec::new(),
        }
    }

    /// Initialize the pipeline by seeding the genesis [`SystemConfig`] and
    /// pre-consuming the genesis L1 block.
    ///
    /// This sends an [`ActivationSignal`] through all pipeline stages so that
    /// [`IndexedTraversal`] gets the correct [`SystemConfig`] (including the
    /// batcher address) from the genesis L2 block. It then runs
    /// [`act_l2_pipeline_full`] once to drain the genesis L1 block, which
    /// contains no batcher data. After this call the pipeline's
    /// `IndexedTraversal` is in state `done = true`, ready to accept new L1
    /// blocks via [`act_l1_head_signal`].
    ///
    /// # Errors
    ///
    /// Returns [`VerifierError::Signal`] if the activation signal fails, or
    /// [`VerifierError::Pipeline`] if the initial genesis-drain step fails.
    ///
    /// [`IndexedTraversal`]: base_consensus_derive::IndexedTraversal
    /// [`act_l2_pipeline_full`]: L2Verifier::act_l2_pipeline_full
    /// [`act_l1_head_signal`]: L2Verifier::act_l1_head_signal
    pub async fn initialize(&mut self) -> Result<(), VerifierError> {
        let l1_origin = self.pipeline.origin().unwrap_or_default();
        self.pipeline
            .signal(
                ActivationSignal { l2_safe_head: self.safe_head, l1_origin, system_config: None }
                    .signal(),
            )
            .await
            .map_err(|e| VerifierError::Signal(Box::new(e)))?;

        // Drain the genesis L1 block (no batcher data; sets IndexedTraversal::done = true).
        self.act_l2_pipeline_full().await?;
        Ok(())
    }

    /// Return the current L2 safe head.
    pub const fn l2_safe(&self) -> L2BlockInfo {
        self.safe_head
    }

    /// Return the current L2 unsafe head.
    ///
    /// In a verifier-only setup the unsafe head is the same as the safe head
    /// since no sequencer is operating. When unsafe blocks are injected via
    /// [`act_l2_unsafe_gossip_receive`], this advances ahead of `safe_head`
    /// until derivation catches up.
    ///
    /// [`act_l2_unsafe_gossip_receive`]: L2Verifier::act_l2_unsafe_gossip_receive
    pub const fn l2_unsafe(&self) -> L2BlockInfo {
        self.unsafe_head
    }

    /// Return the current L2 finalized head.
    pub const fn l2_finalized(&self) -> L2BlockInfo {
        self.finalized_head
    }

    /// Signal the pipeline that a new L1 block is available.
    ///
    /// This is equivalent to op-e2e's `ActL1HeadSignal`. The [`IndexedTraversal`]
    /// stage will accept the block only if it is the next sequential block
    /// (number = current + 1 and `parent_hash` matches).
    ///
    /// [`IndexedTraversal`]: base_consensus_derive::IndexedTraversal
    pub async fn act_l1_head_signal(&mut self, head: BlockInfo) -> Result<(), VerifierError> {
        self.pipeline
            .signal(Signal::ProvideBlock(head))
            .await
            .map_err(|e| VerifierError::Signal(Box::new(e)))
    }

    /// Signal the pipeline that a new L1 safe head is available.
    ///
    /// Currently a no-op in the verifier — the safe L2 head is already tracked
    /// by derivation. Stored for future use.
    pub async fn act_l1_safe_signal(&mut self, _head: BlockInfo) -> Result<(), VerifierError> {
        Ok(())
    }

    /// Signal the pipeline that an L1 block has been finalized.
    ///
    /// Scans [`safe_head_history`] to find the highest L2 safe block whose
    /// L1 origin number is ≤ `head.number`, then updates `finalized_head`.
    ///
    /// [`safe_head_history`]: L2Verifier::safe_head_history
    pub async fn act_l1_finalized_signal(&mut self, head: BlockInfo) -> Result<(), VerifierError> {
        self.finalized_l1_number = head.number;

        // Scan history most-recent-first for highest L2 block whose L1 origin
        // is at or before the finalized L1 number.
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
        Ok(())
    }

    /// Reset the pipeline to the given L1 origin and L2 safe head.
    ///
    /// Use this after an L1 reorg to resync derivation from a new fork.
    pub async fn act_reset(
        &mut self,
        l1_origin: BlockInfo,
        l2_safe_head: L2BlockInfo,
        system_config: SystemConfig,
    ) -> Result<(), VerifierError> {
        self.pipeline
            .signal(
                ResetSignal { l1_origin, l2_safe_head, system_config: Some(system_config) }
                    .signal(),
            )
            .await
            .map_err(|e| VerifierError::Signal(Box::new(e)))?;
        self.safe_head = l2_safe_head;
        // Clear stale finalization state so a subsequent act_l1_finalized_signal
        // cannot promote an L2 block that no longer exists on the canonical chain.
        self.safe_head_history.clear();
        self.derived_user_tx_counts.clear();
        self.derived_l1_info_txs.clear();
        self.finalized_head = l2_safe_head;
        self.finalized_l1_number = 0;
        Ok(())
    }

    /// Run the derivation pipeline until it is idle (no more L1 data to consume).
    ///
    /// Each call to [`Pipeline::step`] either prepares an
    /// [`OpAttributesWithParent`] (which is immediately applied to advance the
    /// safe head) or advances the L1 origin. The loop stops on `Eof` — when the
    /// pipeline has consumed all signalled L1 blocks.
    ///
    /// Returns the number of L2 attributes that were applied (i.e. how many L2
    /// blocks were derived).
    ///
    /// # Errors
    ///
    /// Returns [`VerifierError::Pipeline`] if the pipeline returns a
    /// non-transient error, or if `NotEnoughData` is returned more than
    /// 1 000 consecutive times without progress (indicating a stuck pipeline).
    pub async fn act_l2_pipeline_full(&mut self) -> Result<usize, VerifierError> {
        let mut derived = 0;
        let mut no_progress = 0usize;
        loop {
            match self.pipeline.step(self.safe_head).await {
                StepResult::PreparedAttributes => {
                    no_progress = 0;
                    if let Some(attrs) = self.pipeline.next() {
                        self.apply_attributes(attrs);
                        derived += 1;
                    }
                }
                StepResult::AdvancedOrigin => {
                    no_progress = 0;
                    // Pipeline consumed an L1 block and is ready for more; keep stepping.
                }
                StepResult::StepFailed(err) => {
                    match &err {
                        PipelineErrorKind::Temporary(PipelineError::Eof) => {
                            // No more data for now — pipeline is idle.
                            break;
                        }
                        PipelineErrorKind::Temporary(PipelineError::NotEnoughData) => {
                            // The channel bank just ingested a frame but the channel isn't
                            // assembled yet, or the batch reader needs another read attempt.
                            // This is a transient state — step again immediately.
                            no_progress += 1;
                            if no_progress > 1_000 {
                                return Err(VerifierError::Pipeline(Box::new(
                                    PipelineError::Provider(
                                        "pipeline stuck: 1000 consecutive NotEnoughData without progress".into()
                                    ).temp()
                                )));
                            }
                        }
                        _ => return Err(VerifierError::Pipeline(Box::new(err))),
                    }
                }
                StepResult::OriginAdvanceErr(err) => {
                    match &err {
                        PipelineErrorKind::Temporary(PipelineError::Eof) => {
                            // Traversal exhausted — no more L1 blocks to advance to.
                            break;
                        }
                        _ => return Err(VerifierError::Pipeline(Box::new(err))),
                    }
                }
            }
        }
        Ok(derived)
    }

    /// Execute exactly one derivation step and return the raw [`StepResult`].
    ///
    /// Unlike [`act_l2_pipeline_full`], this does **not** loop. The caller
    /// decides whether and when to step again, making it possible to assert on
    /// intermediate pipeline state between steps or to stop as soon as a
    /// specific outcome is observed.
    ///
    /// When the step returns [`StepResult::PreparedAttributes`] the attributes
    /// are consumed and applied to the safe head automatically, identical to
    /// the behaviour inside [`act_l2_pipeline_full`].
    ///
    /// # Errors
    ///
    /// Returns [`VerifierError::Pipeline`] if the pipeline returns a critical
    /// error. Transient results (`Eof`, `NotEnoughData`) are returned as-is so
    /// the caller can decide how to handle them.
    ///
    /// [`act_l2_pipeline_full`]: L2Verifier::act_l2_pipeline_full
    pub async fn act_l2_pipeline_step(&mut self) -> Result<StepResult, VerifierError> {
        let result = self.pipeline.step(self.safe_head).await;
        match result {
            StepResult::PreparedAttributes => {
                if let Some(attrs) = self.pipeline.next() {
                    self.apply_attributes(attrs);
                }
                Ok(StepResult::PreparedAttributes)
            }
            StepResult::AdvancedOrigin => Ok(StepResult::AdvancedOrigin),
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
    /// or until the pipeline reaches EOF (goes idle), or until `max_steps` is
    /// exhausted.
    ///
    /// This is the Rust equivalent of op-e2e's `ActL2EventsUntil`. It drives
    /// the pipeline forward step-by-step and hands each raw [`StepResult`] to
    /// the caller's predicate. Use it when a test needs to stop at a specific
    /// derivation outcome without knowing in advance how many steps it takes to
    /// get there.
    ///
    /// Attributes are consumed and applied automatically on each
    /// [`StepResult::PreparedAttributes`] step, just as in
    /// [`act_l2_pipeline_full`].
    ///
    /// Returns `(steps_taken, condition_met)`. `condition_met` is `false` when
    /// the pipeline reached EOF or `max_steps` before the predicate fired.
    ///
    /// # Errors
    ///
    /// Returns [`VerifierError::Pipeline`] on any non-transient pipeline error,
    /// or when `NotEnoughData` is returned more than 1 000 consecutive times
    /// without progress (indicating a stuck pipeline).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Drive the pipeline until the L1 origin advances past genesis.
    /// let (steps, hit) = verifier
    ///     .act_l2_pipeline_until(
    ///         |r| matches!(r, StepResult::AdvancedOrigin),
    ///         500,
    ///     )
    ///     .await?;
    /// assert!(hit, "pipeline idled before advancing the L1 origin");
    ///
    /// // Drive until exactly one L2 block is derived, then inspect state.
    /// let (_, hit) = verifier
    ///     .act_l2_pipeline_until(
    ///         |r| matches!(r, StepResult::PreparedAttributes),
    ///         500,
    ///     )
    ///     .await?;
    /// assert!(hit);
    /// assert_eq!(verifier.l2_safe().block_info.number, 1);
    /// ```
    ///
    /// [`act_l2_pipeline_full`]: L2Verifier::act_l2_pipeline_full
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
            let result = self.pipeline.step(self.safe_head).await;
            steps += 1;
            if matches!(result, StepResult::PreparedAttributes)
                && let Some(attrs) = self.pipeline.next()
            {
                self.apply_attributes(attrs);
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
                    PipelineErrorKind::Temporary(PipelineError::NotEnoughData) => {
                        no_progress += 1;
                        if no_progress > 1_000 {
                            return Err(VerifierError::Pipeline(Box::new(
                                PipelineError::Provider(
                                    "pipeline stuck: 1000 consecutive NotEnoughData without progress"
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

    /// Return the current L1 origin the pipeline is positioned at.
    pub fn l1_origin(&self) -> Option<BlockInfo> {
        self.pipeline.origin()
    }

    /// Register the block hash for a given L2 block number.
    ///
    /// Call this after [`L2Sequencer::build_next_block`] to record the real
    /// block hash. When derivation later applies attributes for this block
    /// number, the verifier will use the registered hash instead of a default,
    /// keeping the `parent_hash` chain consistent with the sequencer.
    ///
    /// [`L2Sequencer::build_next_block`]: crate::L2Sequencer::build_next_block
    pub fn register_block_hash(&mut self, number: u64, hash: B256) {
        self.block_hashes.insert(number, hash);
    }

    /// Return the user transaction counts recorded for each derived L2 block.
    ///
    /// Each entry is `(l2_block_number, user_tx_count)`. A count of `0` means
    /// the block is deposit-only — either force-included after a dropped batch
    /// (e.g. `NonEmptyTransitionBlock` or sequencer-drift violation) or
    /// generated at a hardfork upgrade boundary.
    pub fn derived_user_tx_counts(&self) -> &[(u64, usize)] {
        &self.derived_user_tx_counts
    }

    /// Return the decoded L1 info transactions for each derived L2 block.
    ///
    /// Each entry is `(l2_block_number, l1_info_tx)`. Use this to inspect
    /// hardfork-specific fee parameters — for example, `operator_fee_scalar`
    /// and `operator_fee_constant` from Isthmus/Jovian blocks — without
    /// requiring EVM execution.
    pub fn derived_l1_info_txs(&self) -> &[(u64, L1BlockInfoTx)] {
        &self.derived_l1_info_txs
    }

    /// Inject an unsafe L2 block as if received via P2P gossip.
    ///
    /// Equivalent to op-e2e's `ActL2UnsafeGossipReceive`. The block's header is
    /// used to advance `unsafe_head`; the block hash is also registered in
    /// `block_hashes` so that subsequent derivation can build a consistent
    /// `parent_hash` chain without a separate [`register_block_hash`] call.
    ///
    /// Only advances `unsafe_head` if `block.header.number` is exactly
    /// `unsafe_head.number + 1` — gaps and out-of-order gossip are silently dropped.
    ///
    /// # Errors
    ///
    /// Returns [`VerifierError::GossipDecodeFailed`] if the first transaction is
    /// not a valid L1 info deposit (i.e. the block was not produced by a
    /// well-formed sequencer).
    ///
    /// [`register_block_hash`]: L2Verifier::register_block_hash
    pub fn act_l2_unsafe_gossip_receive(&mut self, block: &OpBlock) -> Result<(), VerifierError> {
        // Only accept the strictly next block; gaps and duplicates are dropped silently.
        if block.header.number != self.unsafe_head.block_info.number + 1 {
            return Ok(());
        }
        let hash = block.header.hash_slow();
        // Auto-register so parent_hash chaining works in later derivation.
        self.block_hashes.insert(block.header.number, hash);

        let l1_origin =
            self.l1_origin_from_block(block).ok_or(VerifierError::GossipDecodeFailed)?;
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
        Ok(())
    }

    /// Decode the L1 epoch from the first deposit transaction in an [`OpBlock`].
    ///
    /// Mirrors [`l1_origin_from_attrs`] but operates on a fully-formed block
    /// (received via gossip) rather than on derived [`OpAttributesWithParent`].
    ///
    /// [`l1_origin_from_attrs`]: L2Verifier::l1_origin_from_attrs
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
    ///
    /// Returns `None` if the first transaction is absent or cannot be decoded.
    /// This is the same decoding path as [`l1_origin_from_attrs`] but returns
    /// the full info tx rather than just the epoch identifier.
    ///
    /// [`l1_origin_from_attrs`]: L2Verifier::l1_origin_from_attrs
    fn l1_info_from_attrs(&self, attrs: &OpAttributesWithParent) -> Option<L1BlockInfoTx> {
        let txs = attrs.attributes.transactions.as_ref()?;
        let raw = txs.first()?;
        let rlp_bytes = raw.strip_prefix(&[0x7E])?;
        let deposit = TxDeposit::decode(&mut &*rlp_bytes).ok()?;
        L1BlockInfoTx::decode_calldata(deposit.input.as_ref()).ok()
    }

    /// Apply derived attributes to the in-memory L2 chain, advancing the safe head.
    ///
    /// This is the minimal "engine": no EVM execution, no state root computation.
    /// The safe head advances by number and timestamp so derivation can progress.
    ///
    /// `l1_origin` is decoded from the L1 info deposit (first transaction) so it
    /// tracks the batch's L1 **epoch**, not the L1 inclusion block. Getting this
    /// right is critical: [`BatchQueue`] validates each batch's `epoch_num` against
    /// `safe_head.l1_origin.number`, so using the inclusion block here would cause
    /// subsequent same-epoch batches to be rejected as `EpochTooOld`.
    ///
    /// The block hash is looked up from [`register_block_hash`] entries. When the
    /// [`L2Sequencer`] produces real sealed headers, the test must register each
    /// block's hash so the verifier's `parent_hash` chain stays consistent with
    /// the batches the sequencer submitted.
    ///
    /// [`register_block_hash`]: L2Verifier::register_block_hash
    /// [`L2Sequencer`]: crate::L2Sequencer
    /// [`BatchQueue`]: base_consensus_derive::BatchQueue
    fn apply_attributes(&mut self, attrs: OpAttributesWithParent) {
        let new_number = self.safe_head.block_info.number + 1;
        let new_timestamp = attrs.attributes.payload_attributes.timestamp;
        let l1_origin = self
            .l1_origin_from_attrs(&attrs)
            .or_else(|| attrs.derived_from.map(|b| BlockNumHash { hash: b.hash, number: b.number }))
            .unwrap_or_default();
        // seq_num tracks position within the L1 epoch: 0 for the first L2 block
        // of an epoch, incrementing for each subsequent block in the same epoch.
        // BatchQueue uses this for batch ordering validation.
        let seq_num =
            if l1_origin == self.safe_head.l1_origin { self.safe_head.seq_num + 1 } else { 0 };
        // Record user tx count (non-0x7E-prefixed = non-deposit).
        if let Some(ref txs) = attrs.attributes.transactions {
            let user_count = txs.iter().filter(|tx| !tx.starts_with(&[0x7E])).count();
            self.derived_user_tx_counts.push((new_number, user_count));
        }
        // Record the decoded L1 info tx for this block.
        if let Some(l1_info) = self.l1_info_from_attrs(&attrs) {
            self.derived_l1_info_txs.push((new_number, l1_info));
        }
        let hash = self.block_hashes.get(&new_number).copied().unwrap_or_default();
        self.safe_head = L2BlockInfo {
            block_info: BlockInfo {
                number: new_number,
                timestamp: new_timestamp,
                hash,
                parent_hash: self.safe_head.block_info.hash,
            },
            l1_origin,
            seq_num,
        };
        // Track history for finalization scanning.
        self.safe_head_history.push((self.safe_head, l1_origin.number));
    }

    /// Decode the L1 epoch (`l1_origin`) from the L1 info deposit transaction
    /// that is always the first entry in the derived payload attributes.
    ///
    /// The L1 info deposit is EIP-2718 encoded: `[0x7E] ++ rlp(TxDeposit)`.
    /// Its `input` field is the `setL1BlockValues*` calldata from which
    /// [`L1BlockInfoTx`] recovers the epoch number and hash.
    ///
    /// Returns `None` if the transaction is absent or cannot be decoded, in
    /// which case the caller falls back to `derived_from`.
    pub fn l1_origin_from_attrs(&self, attrs: &OpAttributesWithParent) -> Option<BlockNumHash> {
        let txs = attrs.attributes.transactions.as_ref()?;
        let raw = txs.first()?;
        // EIP-2718 deposit: strip the 0x7E type prefix, then RLP-decode TxDeposit.
        let rlp_bytes = raw.strip_prefix(&[0x7E])?;
        let deposit = TxDeposit::decode(&mut &*rlp_bytes).ok()?;
        let l1_info = L1BlockInfoTx::decode_calldata(deposit.input.as_ref()).ok()?;
        Some(l1_info.id())
    }
}
