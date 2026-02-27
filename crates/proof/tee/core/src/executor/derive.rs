use std::sync::Arc;

use alloy_primitives::{B256, Sealable};
use base_consensus_derive::EthereumDataSource;
use base_proof::{
    OracleBlobProvider, OracleL1ChainProvider, OracleL2ChainProvider, OraclePipeline,
    new_oracle_pipeline_cursor,
};
use base_proof_driver::DriverPipeline;
use base_proof_executor::TrieDBProvider;
use base_proof_preimage::{PreimageKey, PreimageOracleClient};
use base_protocol::OpAttributesWithParent;

use crate::{error::ExecutorError, executor::Oracle};

/// Derives L2 block attributes from L1 data using the kona derivation pipeline.
///
/// Wraps a pre-populated [`Oracle`] and runs the derivation pipeline to produce
/// [`OpAttributesWithParent`] for a target block, replacing trust in the proposer
/// with cryptographic verification of L1-derived data.
#[derive(Debug)]
pub struct BlockDeriver {
    oracle: Arc<Oracle>,
}

impl BlockDeriver {
    /// Creates a new [`BlockDeriver`] backed by the given oracle.
    pub const fn new(oracle: Arc<Oracle>) -> Self {
        Self { oracle }
    }

    /// Derives the payload attributes for the target block by running the kona
    /// derivation pipeline from the agreed L2 safe head.
    ///
    /// The safe head is resolved from `boot_info.agreed_l2_output_root` by
    /// fetching the output root preimage and extracting the block hash.
    pub async fn derive_block_attributes(
        &self,
        boot_info: &base_proof::BootInfo,
        target_block_number: u64,
    ) -> Result<OpAttributesWithParent, ExecutorError> {
        let cfg = Arc::new(boot_info.rollup_config.clone());
        let l1_cfg = Arc::new(boot_info.l1_config.clone());

        // Resolve the L2 safe head block hash from the agreed output root.
        // Output root v0 preimage layout:
        //   version(32) ++ state_root(32) ++ msg_passer_root(32) ++ block_hash(32)
        let output_preimage = self
            .oracle
            .get(PreimageKey::new_keccak256(*boot_info.agreed_l2_output_root))
            .await
            .map_err(|e| {
                ExecutorError::DerivationFailed(format!("failed to get output root preimage: {e}"))
            })?;
        if output_preimage.len() != 128 {
            return Err(ExecutorError::DerivationFailed(format!(
                "output root preimage too short: expected 128 bytes, got {}",
                output_preimage.len()
            )));
        }
        let l2_head_hash = B256::from_slice(&output_preimage[96..128]);

        // Create providers backed by the oracle.
        let mut l1_provider = OracleL1ChainProvider::new(boot_info.l1_head, Arc::clone(&self.oracle));
        let mut l2_provider =
            OracleL2ChainProvider::new(l2_head_hash, Arc::clone(&cfg), Arc::clone(&self.oracle));
        let blob_provider = OracleBlobProvider::new(Arc::clone(&self.oracle));
        let da_provider =
            EthereumDataSource::new_from_parts(l1_provider.clone(), blob_provider, &cfg);

        // Get the safe head header and seal it.
        let safe_header = l2_provider
            .header_by_hash(l2_head_hash)
            .map_err(|e| {
                ExecutorError::DerivationFailed(format!("failed to get safe head header: {e}"))
            })?
            .seal_slow();

        // Create the pipeline cursor at the safe head.
        let cursor = new_oracle_pipeline_cursor(
            &cfg,
            safe_header,
            &mut l1_provider,
            &mut l2_provider,
        )
        .await
        .map_err(|e| {
            ExecutorError::DerivationFailed(format!("failed to create pipeline cursor: {e}"))
        })?;

        // Set cursor on L2 provider so it tracks derivation progress.
        l2_provider.set_cursor(Arc::clone(&cursor));

        // Build the derivation pipeline.
        let mut pipeline = OraclePipeline::new(
            cfg,
            l1_cfg,
            Arc::clone(&cursor),
            Arc::clone(&self.oracle),
            da_provider,
            l1_provider,
            l2_provider,
        )
        .await
        .map_err(|e| ExecutorError::DerivationFailed(format!("failed to build pipeline: {e}")))?;

        // Derive the next block's payload attributes.
        let l2_safe_head = *cursor.read().l2_safe_head();
        let attributes = pipeline
            .produce_payload(l2_safe_head)
            .await
            .map_err(|e| ExecutorError::DerivationFailed(format!("derivation failed: {e}")))?;

        // Verify the derived block number matches the target.
        let derived_number = attributes.block_number();
        if derived_number != target_block_number {
            return Err(ExecutorError::DerivationFailed(format!(
                "derived block {derived_number} but target is {target_block_number}"
            )));
        }

        Ok(attributes)
    }
}
