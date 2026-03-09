use std::sync::Arc;

use alloy_eips::BlockNumHash;
use alloy_rlp::Decodable;
use base_alloy_consensus::TxDeposit;
use base_consensus_derive::{
    ActivationSignal, IndexedAttributesQueueStage, OriginProvider, Pipeline, PipelineBuilder,
    PipelineError, PipelineErrorKind, ResetSignal, Signal, SignalReceiver,
    StatefulAttributesBuilder, StepResult,
};
use base_consensus_genesis::{L1ChainConfig, RollupConfig, SystemConfig};
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo, OpAttributesWithParent};

use crate::{ActionDataSource, ActionL1ChainProvider, ActionL2ChainProvider};

/// The concrete pipeline type used by [`L2Verifier`].
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
///
/// # Unsafe head
///
/// The unsafe head is not tracked; action tests that only verify derivation
/// correctness do not need it. A `L2Sequencer` actor (building on this type)
/// can be added later.
#[derive(Debug)]
pub struct L2Verifier {
    /// The real derivation pipeline wired to in-memory providers.
    pipeline: VerifierPipeline,
    /// The current L2 safe head (advances as attributes are consumed).
    safe_head: L2BlockInfo,
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

        Self { pipeline, safe_head }
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
        self.safe_head = L2BlockInfo {
            block_info: BlockInfo {
                number: new_number,
                timestamp: new_timestamp,
                // Hash is left as default — action tests assert on number/origin,
                // not on the exact L2 block hash.
                ..Default::default()
            },
            l1_origin,
            seq_num,
        };
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
