use alloc::{sync::Arc, vec::Vec};
use core::fmt::Debug;

use alloy_evm::{EvmFactory, FromRecoveredTx, FromTxWithEncoded, revm::context::BlockEnv};
use alloy_primitives::B256;
use base_alloy_evm::OpTxEnv;
use base_consensus_derive::EthereumDataSource;
use base_proof::{
    BaseExecutor, CachingOracle, OracleBlobProvider, OracleL1ChainProvider, OracleL2ChainProvider,
    OraclePipeline,
};
use base_proof_driver::Driver;
use base_proof_preimage::{HintWriterClient, PreimageOracleClient};
use base_revm::OpSpecId;
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

/// The driver for the proof program — holds pipeline state and executes derivation.
#[derive(Debug)]
pub struct FaultProofDriver<P, H, F>
where
    P: PreimageOracleClient + Send + Sync + Clone + Debug + 'static,
    H: HintWriterClient + Send + Sync + Clone + Debug + 'static,
    F: EvmFactory<Spec = OpSpecId, BlockEnv = BlockEnv> + Send + Sync + Clone + 'static,
{
    rollup_config: Arc<base_consensus_genesis::RollupConfig>,
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
    F: EvmFactory<Spec = OpSpecId, BlockEnv = BlockEnv> + Send + Sync + Clone + Debug + 'static,
    F::Tx: FromTxWithEncoded<base_alloy_consensus::OpTxEnvelope>
        + FromRecoveredTx<base_alloy_consensus::OpTxEnvelope>
        + OpTxEnv,
{
    /// Creates a new driver.
    pub const fn new(
        rollup_config: Arc<base_consensus_genesis::RollupConfig>,
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
        let (results, claimed_l2_output_root): (Vec<_>, _) =
            self.execute_with_intermediates().await?;

        let (safe_head, output_root) = results.last().copied().ok_or_else(|| {
            FaultProofProgramError::Driver(base_proof_driver::DriverError::Pipeline(
                base_consensus_derive::PipelineErrorKind::Critical(
                    base_consensus_derive::PipelineError::EndOfSource,
                ),
            ))
        })?;

        Ok(Epilogue { safe_head, output_root, claimed_output_root: claimed_l2_output_root })
    }

    /// Executes the derivation pipeline, returning per-block `(L2BlockInfo, output_root)` pairs
    /// and the claimed output root for validation.
    pub async fn execute_with_intermediates(
        self,
    ) -> Result<
        (Vec<(base_protocol::L2BlockInfo, B256)>, B256),
        FaultProofProgramError,
    > {
        let executor = BaseExecutor::new(
            self.rollup_config.as_ref(),
            self.l2_provider.clone(),
            self.l2_provider.clone(),
            self.evm_factory,
            None,
        );
        let mut driver = Driver::new(Arc::clone(&self.cursor), executor, self.pipeline);
        let results = driver
            .advance_to_target(self.rollup_config.as_ref(), Some(self.claimed_l2_block_number))
            .await
            .map_err(|e| {
                error!(error = ?e, "driver failed");
                FaultProofProgramError::Driver(e)
            })?;

        Ok((results, self.claimed_l2_output_root))
    }
}
