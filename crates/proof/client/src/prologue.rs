//! The prologue phase - loading boot information and initializing providers.

use alloc::sync::Arc;

use alloy_consensus::Sealed;
use alloy_evm::{EvmFactory, FromRecoveredTx, FromTxWithEncoded, revm::context::BlockEnv};
use alloy_primitives::B256;
use base_alloy_consensus::OpTxEnvelope;
use base_alloy_evm::OpTxEnv;
use base_consensus_derive::EthereumDataSource;
use base_proof_executor::TrieDBProvider;
use base_proof_preimage::{CommsClient, HintWriterClient, PreimageKey, PreimageOracleClient};
use base_revm::OpSpecId;
use tracing::{error, info};

use base_proof::{OracleL1ChainProvider, OracleL2ChainProvider, OracleBlobProvider, OraclePipeline, new_oracle_pipeline_cursor};

use crate::{driver::FaultProofDriver, error::FaultProofProgramError};

// Re-export types from the proof crate
pub use base_proof::{BootInfo, CachingOracle, HintType, OracleProviderError};

/// The prologue phase - loading boot information and initializing the derivation pipeline.
#[derive(Debug)]
pub struct Prologue<P, H, F> {
    oracle_client: P,
    hint_writer: H,
    evm_factory: F,
}

impl<P, H, F> Prologue<P, H, F>
where
    P: PreimageOracleClient + Send + Sync + Clone + core::fmt::Debug + 'static,
    H: HintWriterClient + Send + Sync + Clone + core::fmt::Debug + 'static,
    F: EvmFactory<Spec = OpSpecId, BlockEnv = BlockEnv> + Send + Sync + Clone + core::fmt::Debug + 'static,
    F::Tx: FromTxWithEncoded<OpTxEnvelope> + FromRecoveredTx<OpTxEnvelope> + OpTxEnv,
{
    /// Creates a new prologue with the given oracle client, hint writer, and EVM factory.
    pub const fn new(oracle_client: P, hint_writer: H, evm_factory: F) -> Self {
        Self { oracle_client, hint_writer, evm_factory }
    }

    /// Loads boot information and initializes the derivation pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if boot information cannot be loaded or pipeline initialization fails.
    pub async fn load(self) -> Result<FaultProofDriver<P, H, F>, FaultProofProgramError> {
        const ORACLE_LRU_SIZE: usize = 1024;

        let oracle = Arc::new(CachingOracle::new(
            ORACLE_LRU_SIZE,
            self.oracle_client.clone(),
            self.hint_writer.clone(),
        ));
        let boot = BootInfo::load(oracle.as_ref()).await?;
        let l1_config = boot.l1_config;
        let rollup_config = Arc::new(boot.rollup_config);

        // Check for trace extension: if agreed and claimed output roots match, skip derivation
        if boot.agreed_l2_output_root == boot.claimed_l2_output_root {
            info!("trace extension detected");
            return Err(FaultProofProgramError::TraceExtension);
        }

        // Fetch the safe head hash from the oracle
        let safe_head_hash = fetch_safe_head_hash(oracle.as_ref(), boot.agreed_l2_output_root).await?;

        let mut l1_provider = OracleL1ChainProvider::new(boot.l1_head, Arc::clone(&oracle));
        let mut l2_provider = OracleL2ChainProvider::new(
            safe_head_hash,
            Arc::clone(&rollup_config),
            Arc::clone(&oracle),
        );
        let beacon = OracleBlobProvider::new(Arc::clone(&oracle));

        // Fetch the safe head block header
        let safe_head = l2_provider
            .header_by_hash(safe_head_hash)
            .map(|header| Sealed::new_unchecked(header, safe_head_hash))?;

        // Validate: claimed block number must be >= safe head block number
        if boot.claimed_l2_block_number < safe_head.number {
            error!(
                claimed = boot.claimed_l2_block_number,
                safe = safe_head.number,
                "claimed L2 block number is less than the safe head"
            );
            return Err(FaultProofProgramError::InvalidClaim {
                computed: boot.agreed_l2_output_root,
                claimed: boot.claimed_l2_output_root,
            });
        }

        // Create the pipeline cursor
        let cursor = new_oracle_pipeline_cursor(
            rollup_config.as_ref(),
            safe_head.clone(),
            &mut l1_provider,
            &mut l2_provider,
        )
        .await
        .map_err(|e| {
            error!(error = ?e, "failed to create pipeline cursor");
            e
        })?;
        l2_provider.set_cursor(Arc::clone(&cursor));

        // Create the data availability provider
        let da_provider = EthereumDataSource::new_from_parts(
            l1_provider.clone(),
            beacon.clone(),
            &rollup_config,
        );

        // Create the derivation pipeline
        let pipeline = OraclePipeline::new(
            Arc::clone(&rollup_config),
            l1_config.into(),
            Arc::clone(&cursor),
            Arc::clone(&oracle),
            da_provider,
            l1_provider.clone(),
            l2_provider.clone(),
        )
        .await?;

        // At this point, prologue is complete
        // Return a driver with all necessary state
        Ok(FaultProofDriver::new(
            rollup_config,
            boot.claimed_l2_block_number,
            boot.claimed_l2_output_root,
            cursor,
            pipeline,
            l2_provider,
            self.evm_factory,
        ))
    }
}

/// Fetches the safe head hash from the oracle using the agreed upon output root.
async fn fetch_safe_head_hash<O>(
    caching_oracle: &O,
    agreed_l2_output_root: B256,
) -> Result<B256, FaultProofProgramError>
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

    B256::try_from(&output_preimage[96..128]).map_err(|_| {
        FaultProofProgramError::OraclePreimage(
            base_proof_preimage::errors::PreimageOracleError::BufferLengthMismatch(32, output_preimage[96..128].len()),
        )
    })
}
