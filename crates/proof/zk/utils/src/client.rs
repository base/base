//! High-level client helpers for derivation and execution.

use std::{fmt::Debug, num::NonZeroU64};

use alloy_consensus::BlockBody;
use alloy_primitives::{B256, Bytes};
use alloy_rlp::Decodable;
use anyhow::Result;
use base_common_consensus::{BaseBlock, BaseTxEnvelope, OpTxType};
use base_common_genesis::RollupConfig;
use base_consensus_derive::{Pipeline, PipelineError, PipelineErrorKind, Signal, SignalReceiver};
use base_proof::{HintType, OracleProviderError};
use base_proof_driver::{Driver, DriverError, DriverPipeline, DriverResult, Executor, TipCursor};
use base_proof_preimage::{CommsClient, PreimageKey};
use base_protocol::L2BlockInfo;
use tracing::{error, info, warn};

/// Fetches the safe head hash of the L2 chain based on the agreed upon L2 output root in the
/// [`BootInfo`].
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
/// Intermediate output roots are recorded every `intermediate_root_interval` blocks.
///
/// ## Returns
/// - `Ok((l2_safe_head, output_root, intermediate_roots))` - A tuple containing the [`L2BlockInfo`]
///   of the produced block, the output root, and the intermediate output roots at
///   `intermediate_root_interval` blocks.
/// - `Err(e)` - An error if the block could not be produced.
#[allow(clippy::result_large_err)]
pub async fn advance_to_target<E, DP, P>(
    driver: &mut Driver<E, DP, P>,
    cfg: &RollupConfig,
    mut target: Option<u64>,
    intermediate_root_interval: NonZeroU64,
) -> DriverResult<(L2BlockInfo, B256, Vec<B256>), E::Error>
where
    E: Executor + Send + Sync + Debug,
    DP: DriverPipeline<P> + Send + Sync + Debug,
    P: Pipeline + SignalReceiver + Send + Sync + Debug,
{
    let mut blocks_processed: u64 = 0;
    let mut intermediate_roots: Vec<B256> = Vec::new();
    loop {
        // Check if we have reached the target block number.
        let pipeline_cursor = driver.cursor.read();
        let tip_cursor = pipeline_cursor.tip();
        if let Some(tb) = target
            && tip_cursor.l2_safe_head.block_info.number >= tb
        {
            info!(target: "client", "Derivation complete, reached L2 safe head.");
            return Ok((
                tip_cursor.l2_safe_head,
                tip_cursor.l2_safe_head_output_root,
                intermediate_roots,
            ));
        }

        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-start: payload-derivation");
        let mut attributes = match driver.pipeline.produce_payload(tip_cursor.l2_safe_head).await {
            Ok(attrs) => attrs.take_inner(),
            Err(PipelineErrorKind::Critical(PipelineError::EndOfSource)) => {
                warn!(target: "client", "Exhausted data source; Halting derivation and using current safe head.");

                // Adjust the target block number to the current safe head, as no more blocks
                // can be produced.
                if target.is_some() {
                    target = Some(tip_cursor.l2_safe_head.block_info.number);
                };

                // If we are in interop mode, this error must be handled by the caller.
                // Otherwise, we continue the loop to halt derivation on the next iteration.
                if cfg.is_isthmus_active(tip_cursor.l2_safe_head.block_info.timestamp) {
                    return Err(PipelineError::EndOfSource.crit().into());
                }
                continue;
            }
            Err(e) => {
                error!(target: "client", error = ?e, "Failed to produce payload");
                return Err(e.into());
            }
        };
        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-end: payload-derivation");

        driver.executor.update_safe_head(tip_cursor.l2_safe_head_header.clone());

        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-start: block-execution");
        let outcome = match driver.executor.execute_payload(attributes.clone()).await {
            Ok(outcome) => outcome,
            Err(e) => {
                error!(target: "client", error = %e, "Failed to execute L2 block");

                if !cfg.is_holocene_active(attributes.payload_attributes.timestamp) {
                    // Pre-Holocene, discard the block if execution fails.
                    continue;
                }

                if !E::is_deposit_only_retryable(&e) {
                    return Err(DriverError::Executor(e));
                }

                // Retry with a deposit-only block.
                warn!(target: "client", "Flushing current channel and retrying deposit only block");

                // Flush the current batch and channel - if a block was replaced with a
                // deposit-only block due to execution failure, the
                // batch and channel it is contained in is forwards
                // invalidated.
                driver.pipeline.signal(Signal::FlushChannel).await?;

                // Strip out all transactions that are not deposits.
                attributes.transactions = attributes.transactions.map(|txs: Vec<Bytes>| {
                    txs.into_iter()
                        .filter(|tx| !tx.is_empty() && tx[0] == OpTxType::Deposit as u8)
                        .collect::<Vec<_>>()
                });

                // Retry the execution.
                driver.executor.update_safe_head(tip_cursor.l2_safe_head_header.clone());
                match driver.executor.execute_payload(attributes.clone()).await {
                    Ok(header) => header,
                    Err(e) => {
                        error!(
                            target: "client",
                            error = %e,
                            "Critical - Failed to execute deposit-only block",
                        );
                        return Err(DriverError::Executor(e));
                    }
                }
            }
        };
        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-end: block-execution");

        // Construct the block.
        let block = BaseBlock {
            header: outcome.header.inner().clone(),
            body: BlockBody {
                transactions: attributes
                    .transactions
                    .as_ref()
                    .unwrap_or(&Vec::new())
                    .iter()
                    .map(|tx: &Bytes| {
                        BaseTxEnvelope::decode(&mut tx.as_ref()).map_err(DriverError::Rlp)
                    })
                    .collect::<DriverResult<Vec<BaseTxEnvelope>, E::Error>>()?,
                ommers: Vec::new(),
                withdrawals: None,
            },
        };

        // Get the pipeline origin and update the tip cursor.
        let origin = driver.pipeline.origin().ok_or(PipelineError::MissingOrigin.crit())?;
        let l2_info =
            L2BlockInfo::from_block_and_genesis(&block, &driver.pipeline.rollup_config().genesis)?;
        let output_root = driver.executor.compute_output_root().map_err(DriverError::Executor)?;
        blocks_processed += 1;
        if blocks_processed.is_multiple_of(intermediate_root_interval.get()) {
            intermediate_roots.push(output_root);
        }
        let tip_cursor = TipCursor::new(l2_info, outcome.header, output_root);

        // Advance the derivation pipeline cursor
        drop(pipeline_cursor);
        driver.cursor.write().advance(origin, tip_cursor);

        // Add forget calls to save cycles
        #[cfg(target_os = "zkvm")]
        std::mem::forget(block);
    }
}

#[cfg(test)]
pub mod tests {
    //! The pipeline fake is handwritten because `mockall` cannot represent `peek`'s
    //! nested borrowed return (`Option<&AttributesWithParent>`) without a static lifetime.

    use std::{convert::Infallible, sync::Arc};

    use alloy_consensus::Header;
    use alloy_primitives::{Sealable, Sealed, keccak256};
    use async_trait::async_trait;
    use base_common_consensus::TxDeposit;
    use base_common_genesis::SystemConfig;
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_consensus_derive::{OriginProvider, PipelineResult, StepResult};
    use base_proof_driver::PipelineCursor;
    use base_proof_executor::BlockBuildingOutcome;
    use base_protocol::{AttributesWithParent, BlockInfo, L1BlockInfoTx};
    use mockall::mock;
    use spin::RwLock;

    use super::*;

    /// Pipeline that emits a deposit-only payload for each successive block.
    #[derive(Debug)]
    pub struct TestPipeline {
        /// Rollup configuration used to decode block information.
        pub config: RollupConfig,
        /// Encoded L1-info deposit included in each payload.
        pub transaction: Bytes,
    }

    impl Iterator for TestPipeline {
        type Item = AttributesWithParent;

        fn next(&mut self) -> Option<Self::Item> {
            unreachable!("test produces payloads directly")
        }
    }

    impl OriginProvider for TestPipeline {
        fn origin(&self) -> Option<BlockInfo> {
            Some(BlockInfo::default())
        }
    }

    #[async_trait]
    impl SignalReceiver for TestPipeline {
        async fn signal(&mut self, _: Signal) -> PipelineResult<()> {
            unreachable!("test produces valid payloads")
        }
    }

    #[async_trait]
    impl Pipeline for TestPipeline {
        fn peek(&self) -> Option<&AttributesWithParent> {
            unreachable!("test produces payloads directly")
        }

        async fn step(&mut self, _: L2BlockInfo) -> StepResult {
            unreachable!("test produces payloads directly")
        }

        fn rollup_config(&self) -> &RollupConfig {
            &self.config
        }

        async fn system_config_by_number(&mut self, _: u64) -> PipelineResult<SystemConfig> {
            unreachable!("test does not derive attributes")
        }
    }

    #[async_trait]
    impl DriverPipeline<Self> for TestPipeline {
        fn flush(&mut self) {
            unreachable!("test produces valid payloads")
        }

        async fn produce_payload(
            &mut self,
            parent: L2BlockInfo,
        ) -> PipelineResult<AttributesWithParent> {
            let mut attributes = BasePayloadAttributes {
                transactions: Some(vec![self.transaction.clone()]),
                ..Default::default()
            };
            attributes.payload_attributes.timestamp = parent.block_info.number + 1;
            Ok(AttributesWithParent::new(attributes, parent, None, false))
        }
    }

    mock! {
        #[derive(Debug)]
        pub TestExecutor {}

        #[async_trait]
        impl Executor for TestExecutor {
            type Error = Infallible;
            async fn wait_until_ready(&mut self);
            fn update_safe_head(&mut self, header: Sealed<Header>);
            async fn execute_payload(&mut self, attributes: BasePayloadAttributes)
                -> Result<BlockBuildingOutcome, Infallible>;
            fn compute_output_root(&mut self) -> Result<B256, Infallible>;
        }
    }

    fn checkpoints_for(interval: u64) -> Vec<B256> {
        // An unaligned start distinguishes range-relative checkpoints from absolute heights.
        let start = 37;
        let end = 337;
        let config = RollupConfig::default();
        let header = Header { number: start, ..Default::default() }.seal_slow();
        let head = L2BlockInfo {
            block_info: BlockInfo { number: start, hash: header.hash(), ..Default::default() },
            ..Default::default()
        };
        let mut cursor = PipelineCursor::new(10, BlockInfo::default());
        cursor.advance(BlockInfo::default(), TipCursor::new(head, header, B256::ZERO));

        let deposit = BaseTxEnvelope::from(TxDeposit {
            input: L1BlockInfoTx::Bedrock(Default::default()).encode_calldata(),
            ..Default::default()
        });
        let transaction = Bytes::from(alloy_rlp::encode(deposit));
        let pipeline = TestPipeline { config: config.clone(), transaction };

        let mut executor = MockTestExecutor::new();
        executor.expect_update_safe_head().return_const(());
        executor.expect_execute_payload().returning(|attributes| {
            Ok(BlockBuildingOutcome {
                header: Header {
                    number: attributes.payload_attributes.timestamp,
                    ..Default::default()
                }
                .seal_slow(),
                execution_result: Default::default(),
            })
        });
        let mut numbers = start + 1..=end;
        executor
            .expect_compute_output_root()
            .returning(move || Ok(keccak256(numbers.next().unwrap().to_be_bytes())));

        let mut driver = Driver::new(Arc::new(RwLock::new(cursor)), executor, pipeline);
        let (head, root, checkpoints) = base_proof::block_on(advance_to_target(
            &mut driver,
            &config,
            Some(end),
            NonZeroU64::new(interval).unwrap(),
        ))
        .unwrap();

        assert_eq!(head.block_info.number, end);
        assert_eq!(root, keccak256(337_u64.to_be_bytes()));
        checkpoints
    }

    #[test]
    fn three_hundred_block_checkpoint_commits_only_endpoint() {
        assert_eq!(checkpoints_for(300), vec![keccak256(337_u64.to_be_bytes())]);
    }

    #[test]
    fn legacy_thirty_block_checkpoints_are_preserved() {
        assert_eq!(
            checkpoints_for(30),
            (67_u64..=337).step_by(30).map(|n| keccak256(n.to_be_bytes())).collect::<Vec<_>>()
        );
    }

    #[test]
    fn custom_non_thirty_interval_is_not_special_cased() {
        assert_eq!(
            checkpoints_for(75),
            [112_u64, 187, 262, 337].map(|n| keccak256(n.to_be_bytes()))
        );
    }
}
