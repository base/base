use alloc::{sync::Arc, vec::Vec};
use core::fmt::Debug;

use alloy_evm::{EvmFactory, FromRecoveredTx, FromTxWithEncoded, revm::context::BlockEnv};
use alloy_primitives::{B256, Bytes};
use base_common_consensus::BaseReceiptEnvelope;
use base_common_evm::{BaseSpecId, BaseTxEnv};
use base_consensus_derive::EthereumDataSource;
use base_proof::{
    BaseExecutor, CachingOracle, OracleBlobProvider, OracleL1ChainProvider, OracleL2ChainProvider,
    OraclePipeline,
};
use base_proof_driver::Driver;
use base_proof_preimage::{HintWriterClient, PreimageOracleClient};
use spin::RwLock;

use crate::{Epilogue, FaultProofProgramError};

type OracleL1Provider<P, H> = OracleL1ChainProvider<CachingOracle<P, H>>;
type OracleL2Provider<P, H> = OracleL2ChainProvider<CachingOracle<P, H>>;
type OracleBeacon<P, H> = OracleBlobProvider<CachingOracle<P, H>>;
type OracleDA<P, H> = EthereumDataSource<OracleL1Provider<P, H>, OracleBeacon<P, H>>;
type ConcreteOraclePipeline<P, H> = OraclePipeline<
    CachingOracle<P, H>,
    OracleL1Provider<P, H>,
    OracleL2Provider<P, H>,
    OracleDA<P, H>,
>;

/// Derivation and execution artifacts for one block produced by a fault-proof run.
#[derive(Debug, Clone)]
pub struct FaultProofBlock {
    /// The derived L2 block information.
    pub l2_info: base_protocol::L2BlockInfo,
    /// The sealed block header.
    pub header: alloy_consensus::Sealed<alloy_consensus::Header>,
    /// The encoded transactions supplied to execution.
    pub transactions: Vec<Bytes>,
    /// The receipts produced by execution.
    pub receipts: Vec<BaseReceiptEnvelope>,
    /// The output root after executing this block.
    pub output_root: B256,
}

/// The driver for the proof program — holds pipeline state and executes derivation.
#[derive(Debug)]
pub struct FaultProofDriver<P, H, F>
where
    P: PreimageOracleClient + Send + Sync + Clone + Debug + 'static,
    H: HintWriterClient + Send + Sync + Clone + Debug + 'static,
    F: EvmFactory<Spec = BaseSpecId, BlockEnv = BlockEnv> + Send + Sync + Clone + 'static,
{
    rollup_config: Arc<base_common_genesis::RollupConfig>,
    claimed_l2_block_number: u64,
    claimed_l2_output_root: B256,
    cursor: Arc<RwLock<base_proof_driver::PipelineCursor>>,
    pipeline: ConcreteOraclePipeline<P, H>,
    l2_provider: OracleL2Provider<P, H>,
    evm_factory: F,
}

impl<P, H, F> FaultProofDriver<P, H, F>
where
    P: PreimageOracleClient + Send + Sync + Clone + Debug + 'static,
    H: HintWriterClient + Send + Sync + Clone + Debug + 'static,
    F: EvmFactory<Spec = BaseSpecId, BlockEnv = BlockEnv> + Send + Sync + Clone + Debug + 'static,
    F::Tx: FromTxWithEncoded<base_common_consensus::BaseTxEnvelope>
        + FromRecoveredTx<base_common_consensus::BaseTxEnvelope>
        + BaseTxEnv,
{
    /// Creates a new driver.
    pub const fn new(
        rollup_config: Arc<base_common_genesis::RollupConfig>,
        claimed_l2_block_number: u64,
        claimed_l2_output_root: B256,
        cursor: Arc<RwLock<base_proof_driver::PipelineCursor>>,
        pipeline: ConcreteOraclePipeline<P, H>,
        l2_provider: OracleL2Provider<P, H>,
        evm_factory: F,
    ) -> Self {
        Self {
            rollup_config,
            claimed_l2_block_number,
            claimed_l2_output_root,
            cursor,
            pipeline,
            l2_provider,
            evm_factory,
        }
    }

    /// Executes the derivation pipeline to the claimed block.
    ///
    /// # Errors
    ///
    /// Returns an error if derivation fails.
    pub async fn execute(self) -> Result<Epilogue, FaultProofProgramError> {
        self.run_pipeline(|_, _, _, _| {}).await
    }

    /// Like [`execute`](Self::execute), but also collects per-block `(L2BlockInfo, output_root)`
    /// pairs for all intermediate blocks.
    pub async fn execute_with_intermediates(
        self,
    ) -> Result<(Epilogue, Vec<(base_protocol::L2BlockInfo, B256)>), FaultProofProgramError> {
        let mut intermediates = Vec::new();
        let epilogue = self
            .run_pipeline(|l2_info, output_root, _, _| {
                intermediates.push((l2_info, output_root));
            })
            .await?;
        Ok((epilogue, intermediates))
    }

    /// Like [`execute`](Self::execute), but returns transactions, receipts, and headers for every
    /// derived block.
    pub async fn execute_with_artifacts(
        self,
    ) -> Result<(Epilogue, Vec<FaultProofBlock>), FaultProofProgramError> {
        let mut blocks = Vec::new();
        let epilogue = self
            .run_pipeline(|l2_info, output_root, outcome, transactions| {
                blocks.push(FaultProofBlock {
                    l2_info,
                    header: outcome.header.clone(),
                    transactions: transactions.to_vec(),
                    receipts: outcome.execution_result.receipts.clone(),
                    output_root,
                });
            })
            .await?;
        Ok((epilogue, blocks))
    }

    async fn run_pipeline(
        self,
        on_block: impl FnMut(
            base_protocol::L2BlockInfo,
            B256,
            &base_proof_executor::BlockBuildingOutcome,
            &[Bytes],
        ),
    ) -> Result<Epilogue, FaultProofProgramError> {
        let executor = BaseExecutor::new(
            self.rollup_config.as_ref(),
            self.l2_provider.clone(),
            self.l2_provider.clone(),
            self.evm_factory,
            None,
        );
        let mut driver = Driver::new(Arc::clone(&self.cursor), executor, self.pipeline);
        let (safe_head, output_root) = driver
            .advance_to_target_with_artifacts(
                self.rollup_config.as_ref(),
                Some(self.claimed_l2_block_number),
                on_block,
            )
            .await
            .map_err(|e| {
                error!(error = ?e, "driver failed");
                FaultProofProgramError::Driver(e)
            })?;

        if safe_head.block_info.number != self.claimed_l2_block_number {
            return Err(FaultProofProgramError::InvalidClaimBlock {
                derived: safe_head.block_info.number,
                claimed: self.claimed_l2_block_number,
            });
        }

        Ok(Epilogue { safe_head, output_root, claimed_output_root: self.claimed_l2_output_root })
    }
}
