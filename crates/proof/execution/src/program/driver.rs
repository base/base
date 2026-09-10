use alloc::{sync::Arc, vec::Vec};
use core::fmt::Debug;

use alloy_primitives::B256;
use base_consensus_derive::EthereumDataSource;
use base_proof_witness_preimage::{HintWriterClient, PreimageOracleClient};
use spin::RwLock;

use crate::{
    BaseExecutor, CachingOracle, Driver, OracleBlobProvider, OracleL1ChainProvider,
    OracleL2ChainProvider, OraclePipeline,
    program::{Epilogue, FaultProofProgramError},
};

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
pub struct FaultProofDriver<P, H>
where
    P: PreimageOracleClient + Send + Sync + Clone + Debug + 'static,
    H: HintWriterClient + Send + Sync + Clone + Debug + 'static,
{
    rollup_config: Arc<base_common_chain_config::RollupConfig>,
    claimed_l2_block_number: u64,
    claimed_l2_output_root: B256,
    cursor: Arc<RwLock<crate::PipelineCursor>>,
    pipeline: ConcreteOraclePipeline<P, H>,
    l2_provider: OracleL2Provider<P, H>,
    evm_factory: base_execution_evm_runtime::BaseEvmFactory,
}

impl<P, H> FaultProofDriver<P, H>
where
    P: PreimageOracleClient + Send + Sync + Clone + Debug + 'static,
    H: HintWriterClient + Send + Sync + Clone + Debug + 'static,
{
    /// Creates a new driver.
    pub const fn new(
        rollup_config: Arc<base_common_chain_config::RollupConfig>,
        claimed_l2_block_number: u64,
        claimed_l2_output_root: B256,
        cursor: Arc<RwLock<crate::PipelineCursor>>,
        pipeline: ConcreteOraclePipeline<P, H>,
        l2_provider: OracleL2Provider<P, H>,
        evm_factory: base_execution_evm_runtime::BaseEvmFactory,
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
        self.run_pipeline(|_, _| {}).await
    }

    /// Like [`execute`](Self::execute), but also collects per-block `(L2BlockInfo, output_root)`
    /// pairs for all intermediate blocks.
    pub async fn execute_with_intermediates(
        self,
    ) -> Result<(Epilogue, Vec<(base_consensus_batch::L2BlockInfo, B256)>), FaultProofProgramError>
    {
        let mut intermediates = Vec::new();
        let epilogue = self
            .run_pipeline(|l2_info, output_root| intermediates.push((l2_info, output_root)))
            .await?;
        Ok((epilogue, intermediates))
    }

    async fn run_pipeline(
        self,
        on_block: impl FnMut(base_consensus_batch::L2BlockInfo, B256),
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
            .advance_to_target(
                self.rollup_config.as_ref(),
                Some(self.claimed_l2_block_number),
                on_block,
            )
            .await
            .map_err(|e| {
                error!(error = ?e, "driver failed");
                FaultProofProgramError::Driver(e)
            })?;

        Ok(Epilogue {
            safe_head,
            claimed_l2_block_number: self.claimed_l2_block_number,
            output_root,
            claimed_output_root: self.claimed_l2_output_root,
        })
    }
}
