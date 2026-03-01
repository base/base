//! Sync Start

use alloc::sync::Arc;
use core::fmt::Debug;

use alloy_consensus::{Header, Sealed};
use alloy_primitives::B256;
use base_consensus_derive::ChainProvider;
use base_consensus_genesis::RollupConfig;
use base_proof_driver::{PipelineCursor, TipCursor};
use base_proof_preimage::{PreimageKey, PreimageOracleClient};
use base_protocol::{BatchValidationProvider, OutputRoot};
use spin::RwLock;

use crate::errors::OracleProviderError;

/// Constructs a [`PipelineCursor`] from the caching oracle, boot info, and providers.
pub async fn new_oracle_pipeline_cursor<L1, L2>(
    rollup_config: &RollupConfig,
    safe_header: Sealed<Header>,
    chain_provider: &mut L1,
    l2_chain_provider: &mut L2,
) -> Result<Arc<RwLock<PipelineCursor>>, OracleProviderError>
where
    L1: ChainProvider + Send + Sync + Debug + Clone,
    L2: BatchValidationProvider + Send + Sync + Debug + Clone,
    OracleProviderError:
        From<<L1 as ChainProvider>::Error> + From<<L2 as BatchValidationProvider>::Error>,
{
    let safe_head_info = l2_chain_provider.l2_block_info_by_number(safe_header.number).await?;
    let l1_origin = chain_provider.block_info_by_number(safe_head_info.l1_origin.number).await?;

    // Walk back the starting L1 block by `channel_timeout` to ensure that the full channel is
    // captured.
    let channel_timeout = rollup_config.channel_timeout(safe_head_info.block_info.timestamp);
    let mut l1_origin_number = l1_origin.number.saturating_sub(channel_timeout);
    if l1_origin_number < rollup_config.genesis.l1.number {
        l1_origin_number = rollup_config.genesis.l1.number;
    }
    let origin = chain_provider.block_info_by_number(l1_origin_number).await?;

    // Construct the cursor.
    let mut cursor = PipelineCursor::new(channel_timeout, origin);
    let tip = TipCursor::new(safe_head_info, safe_header, B256::ZERO);
    cursor.advance(origin, tip);

    // Wrap the cursor in a shared read-write lock
    Ok(Arc::new(RwLock::new(cursor)))
}

/// Fetches the L2 safe head block hash from the agreed L2 output root preimage.
///
/// Retrieves the output root preimage for `agreed_l2_output_root` from the oracle,
/// decodes it as a V0 [`OutputRoot`], and returns the embedded block hash.
/// This is the starting point for pipeline setup: the returned hash seeds both the
/// L2 chain provider and the derivation cursor.
#[derive(Debug, Clone, Copy)]
pub struct SafeHeadFetcher;

impl SafeHeadFetcher {
    /// Fetches the safe head hash from the oracle.
    pub async fn fetch<O>(
        oracle: &O,
        agreed_l2_output_root: B256,
    ) -> Result<B256, OracleProviderError>
    where
        O: PreimageOracleClient + Send,
    {
        let preimage = oracle
            .get(PreimageKey::new_keccak256(*agreed_l2_output_root))
            .await
            .map_err(OracleProviderError::Preimage)?;
        OutputRoot::decode(&preimage)
            .map(|root| root.block_hash)
            .ok_or(OracleProviderError::InvalidOutputRootPreimage)
    }
}
