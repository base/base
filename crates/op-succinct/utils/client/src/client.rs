use alloy_consensus::BlockBody;
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use anyhow::Result;
use kona_derive::{
    errors::{PipelineError, PipelineErrorKind},
    traits::{Pipeline, SignalReceiver},
    types::Signal,
};
use kona_driver::{Driver, DriverError, DriverPipeline, DriverResult, Executor, TipCursor};
use kona_genesis::RollupConfig;
use kona_preimage::{CommsClient, PreimageKey};
use kona_proof::{errors::OracleProviderError, HintType};
use kona_protocol::{L2BlockInfo, OpAttributesWithParent};
use op_alloy_consensus::{OpBlock, OpTxEnvelope, OpTxType};
use std::fmt::Debug;
use tracing::{error, info, warn};

/// Fetches the safe head hash of the L2 chain based on the agreed upon L2 output root in the
/// [BootInfo].
pub(crate) async fn fetch_safe_head_hash<O>(
    caching_oracle: &O,
    agreed_l2_output_root: B256,
) -> Result<B256, OracleProviderError>
where
    O: CommsClient,
{
    let mut output_preimage = [0u8; 128];
    HintType::StartingL2Output
        .with_data(&[agreed_l2_output_root.as_ref()])
        .send(caching_oracle)
        .await?;
    caching_oracle
        .get_exact(PreimageKey::new_keccak256(*agreed_l2_output_root), output_preimage.as_mut())
        .await?;

    output_preimage[96..128].try_into().map_err(OracleProviderError::SliceConversion)
}

// Sourced from kona/crates/driver/src/core.rs with modifications to use the L2 provider's caching
// system. After each block execution, we update the L2 provider's caches (header_by_number,
// block_by_number, system_config_by_number, l2_block_info_by_number) with the new block data. This
// ensures subsequent lookups for this block number can be served directly from cache rather than
// requiring oracle queries.
/// Advances the derivation pipeline to the target block number.
///
/// ## Takes
/// - `cfg`: The rollup configuration.
/// - `target`: The target block number.
///
/// ## Returns
/// - `Ok((number, output_root))` - A tuple containing the number of the produced block and the
///   output root.
/// - `Err(e)` - An error if the block could not be produced.
pub async fn advance_to_target<E, DP, P>(
    driver: &mut Driver<E, DP, P>,
    cfg: &RollupConfig,
    mut target: Option<u64>,
) -> DriverResult<(L2BlockInfo, B256), E::Error>
where
    E: Executor + Send + Sync + Debug,
    DP: DriverPipeline<P> + Send + Sync + Debug,
    P: Pipeline + SignalReceiver + Send + Sync + Debug,
{
    loop {
        // Check if we have reached the target block number.
        let pipeline_cursor = driver.cursor.read();
        let tip_cursor = pipeline_cursor.tip();
        if let Some(tb) = target {
            if tip_cursor.l2_safe_head.block_info.number >= tb {
                info!(target: "client", "Derivation complete, reached L2 safe head.");
                return Ok((tip_cursor.l2_safe_head, tip_cursor.l2_safe_head_output_root));
            }
        }

        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-start: payload-derivation");
        let OpAttributesWithParent { mut attributes, .. } = match driver
            .pipeline
            .produce_payload(tip_cursor.l2_safe_head)
            .await
        {
            Ok(attrs) => attrs,
            Err(PipelineErrorKind::Critical(PipelineError::EndOfSource)) => {
                warn!(target: "client", "Exhausted data source; Halting derivation and using current safe head.");

                // Adjust the target block number to the current safe head, as no more blocks
                // can be produced.
                if target.is_some() {
                    target = Some(tip_cursor.l2_safe_head.block_info.number);
                };

                // If we are in interop mode, this error must be handled by the caller.
                // Otherwise, we continue the loop to halt derivation on the next iteration.
                if cfg.is_interop_active(driver.cursor.read().l2_safe_head().block_info.number) {
                    return Err(PipelineError::EndOfSource.crit().into());
                } else {
                    continue;
                }
            }
            Err(e) => {
                error!(target: "client", "Failed to produce payload: {:?}", e);
                return Err(DriverError::Pipeline(e));
            }
        };
        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-end: payload-derivation");

        driver.executor.update_safe_head(tip_cursor.l2_safe_head_header.clone());

        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-start: block-execution");
        let execution_result = match driver.executor.execute_payload(attributes.clone()).await {
            Ok(header) => header,
            Err(e) => {
                error!(target: "client", "Failed to execute L2 block: {}", e);

                if cfg.is_holocene_active(attributes.payload_attributes.timestamp) {
                    // Retry with a deposit-only block.
                    warn!(target: "client", "Flushing current channel and retrying deposit only block");

                    // Flush the current batch and channel - if a block was replaced with a
                    // deposit-only block due to execution failure, the
                    // batch and channel it is contained in is forwards
                    // invalidated.
                    driver.pipeline.signal(Signal::FlushChannel).await?;

                    // Strip out all transactions that are not deposits.
                    attributes.transactions = attributes.transactions.map(|txs| {
                        txs.into_iter()
                            .filter(|tx| (!tx.is_empty() && tx[0] == OpTxType::Deposit as u8))
                            .collect::<Vec<_>>()
                    });

                    // Retry the execution.
                    driver.executor.update_safe_head(tip_cursor.l2_safe_head_header.clone());
                    match driver.executor.execute_payload(attributes.clone()).await {
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
        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-end: block-execution");

        // Construct the block.
        let block = OpBlock {
            header: execution_result.header.inner().clone(),
            body: BlockBody {
                transactions: attributes
                    .transactions
                    .unwrap_or_default()
                    .into_iter()
                    .map(|tx| OpTxEnvelope::decode(&mut tx.as_ref()).map_err(DriverError::Rlp))
                    .collect::<DriverResult<Vec<OpTxEnvelope>, E::Error>>()?,
                ommers: Vec::new(),
                withdrawals: None,
            },
        };

        // Get the pipeline origin and update the tip cursor.
        let origin = driver.pipeline.origin().ok_or(PipelineError::MissingOrigin.crit())?;
        let l2_info =
            L2BlockInfo::from_block_and_genesis(&block, &driver.pipeline.rollup_config().genesis)?;
        let tip_cursor = TipCursor::new(
            l2_info,
            execution_result.header,
            driver.executor.compute_output_root().map_err(DriverError::Executor)?,
        );

        // Advance the derivation pipeline cursor
        drop(pipeline_cursor);
        driver.cursor.write().advance(origin, tip_cursor);

        // Add forget calls to save cycles
        #[cfg(target_os = "zkvm")]
        std::mem::forget(block);
    }
}
