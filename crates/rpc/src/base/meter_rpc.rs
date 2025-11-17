use alloy_consensus::{Header, Sealed};
use alloy_primitives::U256;
use base_reth_flashblocks::FlashblocksAPI;
use jsonrpsee::core::{RpcResult, async_trait};
use reth::providers::BlockReaderIdExt;
use reth_optimism_chainspec::OpChainSpec;
use reth_primitives_traits::SealedHeader;
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use std::sync::Arc;
use tips_core::types::{Bundle, MeterBundleResponse, ParsedBundle};
use tracing::{error, info};

use crate::{FlashblockTrieCache, MeteringApiServer, meter_bundle};

/// Implementation of the metering RPC API
pub struct MeteringApiImpl<Provider, FB> {
    provider: Provider,
    flashblocks_state: Arc<FB>,
    /// Cache for the latest flashblock's trie, ensuring each bundle's state root
    /// calculation only measures the bundle's incremental I/O.
    trie_cache: FlashblockTrieCache,
}

impl<Provider, FB> MeteringApiImpl<Provider, FB>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + Clone,
    FB: FlashblocksAPI,
{
    /// Creates a new instance of MeteringApi
    pub fn new(provider: Provider, flashblocks_state: Arc<FB>) -> Self {
        Self {
            provider,
            flashblocks_state,
            trie_cache: FlashblockTrieCache::new(),
        }
    }
}

#[async_trait]
impl<Provider, FB> MeteringApiServer for MeteringApiImpl<Provider, FB>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse> {
        info!(
            num_transactions = &bundle.txs.len(),
            block_number = &bundle.block_number,
            "Starting bundle metering"
        );

        // Get pending flashblocks state
        let pending_blocks = self.flashblocks_state.get_pending_blocks();

        // Get header and flashblock index from pending blocks
        // If no pending blocks exist, fall back to latest canonical block
        let (header, flashblock_index, canonical_block_number) =
            if let Some(pb) = pending_blocks.as_ref() {
                let latest_header: Sealed<Header> = pb.latest_header();
                let flashblock_index = pb.latest_flashblock_index();
                let canonical_block_number = pb.canonical_block_number();

                info!(
                    latest_block = latest_header.number,
                    canonical_block = %canonical_block_number,
                    flashblock_index = flashblock_index,
                    "Using latest flashblock state for metering"
                );

                // Convert Sealed<Header> to SealedHeader
                let sealed_header =
                    SealedHeader::new(latest_header.inner().clone(), latest_header.hash());
                (sealed_header, flashblock_index, canonical_block_number)
            } else {
                // No pending blocks, use latest canonical block
                let canonical_block_number = pending_blocks.get_canonical_block_number();
                let header = self
                    .provider
                    .sealed_header_by_number_or_tag(canonical_block_number)
                    .map_err(|e| {
                        jsonrpsee::types::ErrorObjectOwned::owned(
                            jsonrpsee::types::ErrorCode::InternalError.code(),
                            format!("Failed to get canonical block header: {}", e),
                            None::<()>,
                        )
                    })?
                    .ok_or_else(|| {
                        jsonrpsee::types::ErrorObjectOwned::owned(
                            jsonrpsee::types::ErrorCode::InternalError.code(),
                            "Canonical block not found".to_string(),
                            None::<()>,
                        )
                    })?;

                info!(
                    canonical_block = header.number,
                    "No flashblocks available, using canonical block state for metering"
                );

                (header, 0, canonical_block_number)
            };

        let parsed_bundle = ParsedBundle::try_from(bundle).map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InvalidParams.code(),
                format!("Failed to parse bundle: {}", e),
                None::<()>,
            )
        })?;

        // Get state provider for the canonical block
        let state_provider = self
            .provider
            .state_by_block_number_or_tag(canonical_block_number)
            .map_err(|e| {
                error!(error = %e, "Failed to get state provider");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get state provider: {}", e),
                    None::<()>,
                )
            })?;

        // If we have pending flashblocks, get the state to apply pending changes
        let flashblocks_state = pending_blocks.as_ref().map(|pb| crate::FlashblocksState {
            cache: pb.get_db_cache(),
            bundle_state: pb.get_bundle_state(),
        });

        // Get the flashblock index if we have pending flashblocks
        let state_flashblock_index = pending_blocks
            .as_ref()
            .map(|pb| pb.latest_flashblock_index());

        // Ensure the flashblock trie is cached for reuse across bundle simulations
        let cached_trie = if let Some(ref fb_state) = flashblocks_state {
            let fb_index = state_flashblock_index.unwrap();
            Some(
                self.trie_cache
                    .ensure_cached(header.hash(), fb_index, fb_state, &*state_provider)
                    .map_err(|e| {
                        error!(error = %e, "Failed to cache flashblock trie");
                        jsonrpsee::types::ErrorObjectOwned::owned(
                            jsonrpsee::types::ErrorCode::InternalError.code(),
                            format!("Failed to cache flashblock trie: {}", e),
                            None::<()>,
                        )
                    })?,
            )
        } else {
            None
        };

        // Meter bundle using utility function
        let result = meter_bundle(
            state_provider,
            self.provider.chain_spec(),
            parsed_bundle,
            &header,
            flashblocks_state,
            cached_trie,
        )
        .map_err(|e| {
            error!(error = %e, "Bundle metering failed");
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                format!("Bundle metering failed: {}", e),
                None::<()>,
            )
        })?;

        // Calculate average gas price
        let bundle_gas_price = if result.total_gas_used > 0 {
            result.total_gas_fees / U256::from(result.total_gas_used)
        } else {
            U256::from(0)
        };

        info!(
            bundle_hash = %result.bundle_hash,
            num_transactions = result.results.len(),
            total_gas_used = result.total_gas_used,
            total_time_us = result.total_time_us,
            state_root_time_us = result.state_root_time_us,
            state_block_number = header.number,
            flashblock_index = flashblock_index,
            "Bundle metering completed successfully"
        );

        Ok(MeterBundleResponse {
            bundle_gas_price,
            bundle_hash: result.bundle_hash,
            coinbase_diff: result.total_gas_fees,
            eth_sent_to_coinbase: U256::from(0),
            gas_fees: result.total_gas_fees,
            results: result.results,
            state_block_number: header.number,
            state_flashblock_index,
            total_gas_used: result.total_gas_used,
            // TODO: Rename to total_time_us in tips-core.
            total_execution_time_us: result.total_time_us,
            state_root_time_us: result.state_root_time_us,
        })
    }
}
