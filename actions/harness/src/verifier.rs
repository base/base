use std::{collections::HashMap, sync::Arc};

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use base_alloy_consensus::TxDeposit;
use base_consensus_derive::{
    ActivationSignal, IndexedAttributesQueueStage, OriginProvider, Pipeline, PipelineBuilder,
    PipelineError, PipelineErrorKind, ResetSignal, Signal, SignalReceiver,
    StatefulAttributesBuilder, StepResult,
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
    Pipeline(PipelineErrorKind),
    /// A pipeline signal failed.
    #[error("signal error: {0}")]
    Signal(PipelineErrorKind),
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
#[derive(Debug)]
pub struct L2Verifier {
    /// The real derivation pipeline wired to in-memory providers.
    pipeline: VerifierPipeline,
    /// The current L2 safe head (advances as attributes are consumed).
    safe_head: L2BlockInfo,
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
}

impl L2Verifier {
    /// Construct an [`L2Verifier`] from the given providers and config.
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

        Self {
            pipeline,
            safe_head,
            finalized_head: safe_head,
            finalized_l1_number: 0,
            safe_head_history: Vec::new(),
            block_hashes: HashMap::new(),
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
            .map_err(VerifierError::Signal)?;

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
    /// since no sequencer is operating. When paired with an [`L2Sequencer`]
    /// actor this will diverge.
    ///
    /// [`L2Sequencer`]: crate::L2Sequencer
    pub const fn l2_unsafe(&self) -> L2BlockInfo {
        self.safe_head
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
        self.pipeline.signal(Signal::ProvideBlock(head)).await.map_err(VerifierError::Signal)
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

        if let Some(l2) = candidate {
            if l2.block_info.number > self.finalized_head.block_info.number {
                self.finalized_head = l2;
            }
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
            .map_err(VerifierError::Signal)?;
        self.safe_head = l2_safe_head;
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
                                return Err(VerifierError::Pipeline(
                                    PipelineError::Provider(
                                        "pipeline stuck: 1000 consecutive NotEnoughData without progress".into()
                                    ).temp()
                                ));
                            }
                        }
                        _ => return Err(VerifierError::Pipeline(err)),
                    }
                }
                StepResult::OriginAdvanceErr(err) => {
                    match &err {
                        PipelineErrorKind::Temporary(PipelineError::Eof) => {
                            // Traversal exhausted — no more L1 blocks to advance to.
                            break;
                        }
                        _ => return Err(VerifierError::Pipeline(err)),
                    }
                }
            }
        }
        Ok(derived)
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
