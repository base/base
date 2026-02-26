//! The driver of the derivation pipeline.

use alloc::{sync::Arc, vec::Vec};
use core::fmt::Debug;

use alloy_consensus::BlockBody;
use alloy_primitives::{B256, Bytes};
use alloy_rlp::Decodable;
use base_alloy_consensus::{OpBlock, OpTxEnvelope, OpTxType};
use base_consensus_derive::{Pipeline, PipelineError, PipelineErrorKind, Signal, SignalReceiver};
use base_consensus_genesis::RollupConfig;
use base_proof_executor::BlockBuildingOutcome;
use base_protocol::L2BlockInfo;
use spin::RwLock;

use crate::{DriverError, DriverPipeline, DriverResult, Executor, PipelineCursor, TipCursor};

/// The Rollup Driver entrypoint.
#[derive(Debug)]
pub struct Driver<E, DP, P>
where
    E: Executor + Send + Sync + Debug,
    DP: DriverPipeline<P> + Send + Sync + Debug,
    P: Pipeline + SignalReceiver + Send + Sync + Debug,
{
    /// Marker for the pipeline type parameter.
    _marker: core::marker::PhantomData<P>,
    /// Cursor tracking the current L2 derivation state and safe head.
    pub cursor: Arc<RwLock<PipelineCursor>>,
    /// The block executor responsible for building and executing L2 blocks.
    pub executor: E,
    /// The derivation pipeline that produces block attributes from L1 data.
    pub pipeline: DP,
    /// Cached execution artifacts and transactions from the most recent safe head.
    pub safe_head_artifacts: Option<(BlockBuildingOutcome, Vec<Bytes>)>,
}

impl<E, DP, P> Driver<E, DP, P>
where
    E: Executor + Send + Sync + Debug,
    DP: DriverPipeline<P> + Send + Sync + Debug,
    P: Pipeline + SignalReceiver + Send + Sync + Debug,
{
    /// Creates a new [`Driver`] instance.
    pub const fn new(cursor: Arc<RwLock<PipelineCursor>>, executor: E, pipeline: DP) -> Self {
        Self {
            _marker: core::marker::PhantomData,
            cursor,
            executor,
            pipeline,
            safe_head_artifacts: None,
        }
    }

    /// Waits until the executor is ready for block processing.
    pub async fn wait_for_executor(&mut self) {
        self.executor.wait_until_ready().await;
    }

    /// Advances the derivation pipeline to the target block number.
    pub async fn advance_to_target(
        &mut self,
        cfg: &RollupConfig,
        mut target: Option<u64>,
    ) -> DriverResult<(L2BlockInfo, B256), E::Error> {
        loop {
            // Check if we have reached the target block number.
            let pipeline_cursor = self.cursor.read();
            let tip_cursor = pipeline_cursor.tip();
            if let Some(tb) = target
                && tip_cursor.l2_safe_head.block_info.number >= tb
            {
                info!(target: "client", "Derivation complete, reached L2 safe head.");
                return Ok((tip_cursor.l2_safe_head, tip_cursor.l2_safe_head_output_root));
            }

            let mut attributes = match self.pipeline.produce_payload(tip_cursor.l2_safe_head).await
            {
                Ok(attrs) => attrs.take_inner(),
                Err(PipelineErrorKind::Critical(PipelineError::EndOfSource)) => {
                    warn!(target: "client", "Exhausted data source; Halting derivation and using current safe head.");

                    // Adjust the target block number to the current safe head, as no more blocks
                    // can be produced.
                    if target.is_some() {
                        target = Some(tip_cursor.l2_safe_head.block_info.number);
                    };

                    continue;
                }
                Err(e) => {
                    error!(target: "client", error = ?e, "Failed to produce payload");
                    return Err(DriverError::Pipeline(e));
                }
            };

            self.executor.update_safe_head(tip_cursor.l2_safe_head_header.clone());
            let outcome = match self.executor.execute_payload(attributes.clone()).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    error!(target: "client", error = %e, "Failed to execute L2 block");

                    if cfg.is_holocene_active(attributes.payload_attributes.timestamp) {
                        // Retry with a deposit-only block.
                        warn!(target: "client", "Flushing current channel and retrying deposit only block");

                        // Flush the current batch and channel - if a block was replaced with a
                        // deposit-only block due to execution failure, the
                        // batch and channel it is contained in is forwards
                        // invalidated.
                        self.pipeline.signal(Signal::FlushChannel).await?;

                        // Strip out all transactions that are not deposits.
                        attributes.transactions = attributes.transactions.map(|txs| {
                            txs.into_iter()
                                .filter(|tx| !tx.is_empty() && tx[0] == OpTxType::Deposit as u8)
                                .collect::<Vec<_>>()
                        });

                        // Retry the execution.
                        self.executor.update_safe_head(tip_cursor.l2_safe_head_header.clone());
                        match self.executor.execute_payload(attributes.clone()).await {
                            Ok(header) => header,
                            Err(e) => {
                                error!(
                                    target: "client",
                                    "Critical - Failed to execute deposit-only block: {e}",
                                );
                                return Err(DriverError::Executor(e));
                            }
                        }
                    } else {
                        // Pre-Holocene, discard the block if execution fails.
                        continue;
                    }
                }
            };

            // Construct the block.
            let block = OpBlock {
                header: outcome.header.inner().clone(),
                body: BlockBody {
                    transactions: attributes
                        .transactions
                        .as_ref()
                        .unwrap_or(&Vec::new())
                        .iter()
                        .map(|tx| OpTxEnvelope::decode(&mut tx.as_ref()).map_err(DriverError::Rlp))
                        .collect::<DriverResult<Vec<OpTxEnvelope>, E::Error>>()?,
                    ommers: Vec::new(),
                    withdrawals: None,
                },
            };

            // Get the pipeline origin and update the tip cursor.
            let origin = self.pipeline.origin().ok_or(PipelineError::MissingOrigin.crit())?;
            let l2_info = L2BlockInfo::from_block_and_genesis(
                &block,
                &self.pipeline.rollup_config().genesis,
            )?;
            let tip_cursor = TipCursor::new(
                l2_info,
                outcome.header.clone(),
                self.executor.compute_output_root().map_err(DriverError::Executor)?,
            );

            // Advance the derivation pipeline cursor
            drop(pipeline_cursor);
            self.cursor.write().advance(origin, tip_cursor);

            // Update the latest safe head artifacts.
            self.safe_head_artifacts = Some((outcome, attributes.transactions.unwrap_or_default()));
        }
    }
}
