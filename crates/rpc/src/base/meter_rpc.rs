//! Implementation of the metering RPC API.

use std::sync::Arc;

use alloy_consensus::{Header, Sealed};
use alloy_eips::Encodable2718;
use alloy_primitives::U256;
use base_reth_flashblocks::{FlashblocksAPI, PendingBlocksAPI};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    types::{ErrorCode, ErrorObjectOwned},
};
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use reth::providers::BlockReaderIdExt;
use reth_optimism_chainspec::OpChainSpec;
use reth_primitives_traits::SealedHeader;
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use tips_core::types::{Bundle, MeterBundleResponse, ParsedBundle};
use tracing::{debug, error, info};

use super::types::{MeteredPriorityFeeResponse, ResourceFeeEstimateResponse};
use crate::{
    FlashblockTrieCache, MeteringApiServer, PriorityFeeEstimator, ResourceDemand,
    ResourceEstimates, RollingPriorityEstimate, meter_bundle,
};

/// Implementation of the metering RPC API.
#[derive(Debug)]
pub struct MeteringApiImpl<Provider, FB> {
    provider: Provider,
    flashblocks_state: Arc<FB>,
    /// Cache for the latest flashblock's trie, ensuring each bundle's state root
    /// calculation only measures the bundle's incremental I/O.
    trie_cache: FlashblockTrieCache,
    /// Optional priority fee estimator for the `metered_priority_fee_per_gas` RPC.
    priority_fee_estimator: Option<Arc<PriorityFeeEstimator>>,
}

impl<Provider, FB> MeteringApiImpl<Provider, FB>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + Clone,
    FB: FlashblocksAPI,
{
    /// Creates a new instance of MeteringApi without priority fee estimation.
    pub fn new(provider: Provider, flashblocks_state: Arc<FB>) -> Self {
        Self {
            provider,
            flashblocks_state,
            trie_cache: FlashblockTrieCache::new(),
            priority_fee_estimator: None,
        }
    }

    /// Creates a new instance of MeteringApi with priority fee estimation enabled.
    pub fn with_estimator(
        provider: Provider,
        flashblocks_state: Arc<FB>,
        priority_fee_estimator: Arc<PriorityFeeEstimator>,
    ) -> Self {
        Self {
            provider,
            flashblocks_state,
            trie_cache: FlashblockTrieCache::new(),
            priority_fee_estimator: Some(priority_fee_estimator),
        }
    }

    fn run_metering(
        &self,
        bundle: Bundle,
    ) -> Result<(MeterBundleResponse, ResourceDemand), ErrorObjectOwned> {
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
                        ErrorObjectOwned::owned(
                            ErrorCode::InternalError.code(),
                            format!("Failed to get canonical block header: {}", e),
                            None::<()>,
                        )
                    })?
                    .ok_or_else(|| {
                        ErrorObjectOwned::owned(
                            ErrorCode::InternalError.code(),
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
            ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                format!("Failed to parse bundle: {}", e),
                None::<()>,
            )
        })?;

        // Calculate DA usage for priority fee estimation
        let da_usage: u64 = parsed_bundle
            .txs
            .iter()
            .map(|tx| tx_estimated_size_fjord_bytes(&tx.encoded_2718()))
            .sum();

        // Get state provider for the canonical block
        let state_provider =
            self.provider.state_by_block_number_or_tag(canonical_block_number).map_err(|e| {
                error!(error = %e, "Failed to get state provider");
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
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
        let state_flashblock_index = pending_blocks.as_ref().map(|pb| pb.latest_flashblock_index());

        // Ensure the flashblock trie is cached for reuse across bundle simulations
        let cached_trie = if let Some(ref fb_state) = flashblocks_state {
            let fb_index = state_flashblock_index.unwrap();
            Some(
                self.trie_cache
                    .ensure_cached(header.hash(), fb_index, fb_state, &*state_provider)
                    .map_err(|e| {
                        error!(error = %e, "Failed to cache flashblock trie");
                        ErrorObjectOwned::owned(
                            ErrorCode::InternalError.code(),
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
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                format!("Bundle metering failed: {}", e),
                None::<()>,
            )
        })?;

        // Calculate average gas price
        let bundle_gas_price = if result.total_gas_used > 0 {
            result.total_gas_fees / U256::from(result.total_gas_used)
        } else {
            U256::ZERO
        };

        info!(
            bundle_hash = %result.bundle_hash,
            num_transactions = result.results.len(),
            total_gas_used = result.total_gas_used,
            total_time_us = result.total_time_us,
            state_block_number = header.number,
            flashblock_index = flashblock_index,
            "Bundle metering completed successfully"
        );

        let response = MeterBundleResponse {
            bundle_gas_price,
            bundle_hash: result.bundle_hash,
            coinbase_diff: result.total_gas_fees,
            eth_sent_to_coinbase: U256::ZERO,
            gas_fees: result.total_gas_fees,
            results: result.results,
            state_block_number: header.number,
            state_flashblock_index,
            total_gas_used: result.total_gas_used,
            // TODO: reintroduce state_root_time_us once tips-core exposes it again.
            // TODO: rename total_execution_time_us to total_time_us since it includes state root time
            total_execution_time_us: result.total_time_us,
        };

        let resource_demand = ResourceDemand {
            gas_used: Some(result.total_gas_used),
            execution_time_us: Some(result.total_time_us),
            state_root_time_us: None, // Populated when state-root metrics become available.
            data_availability_bytes: Some(da_usage),
        };

        Ok((response, resource_demand))
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
        let (response, _) = self.run_metering(bundle)?;
        Ok(response)
    }

    async fn metered_priority_fee_per_gas(
        &self,
        bundle: Bundle,
    ) -> RpcResult<MeteredPriorityFeeResponse> {
        let (meter_bundle, resource_demand) = self.run_metering(bundle)?;

        let estimator = self.priority_fee_estimator.as_ref().ok_or_else(|| {
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "Priority fee estimation not enabled".to_string(),
                None::<()>,
            )
        })?;

        debug!(?resource_demand, "Computing priority fee estimates");

        let estimates = estimator
            .estimate_rolling(resource_demand)
            .map_err(|e| {
                ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), e.to_string(), None::<()>)
            })?
            .ok_or_else(|| {
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    "Priority fee data unavailable".to_string(),
                    None::<()>,
                )
            })?;

        let response = build_priority_fee_response(meter_bundle, estimates);
        Ok(response)
    }
}

/// Converts a rolling estimate to the response format.
fn build_priority_fee_response(
    meter_bundle: MeterBundleResponse,
    estimate: RollingPriorityEstimate,
) -> MeteredPriorityFeeResponse {
    let resource_estimates = build_resource_estimate_responses(&estimate.estimates);

    MeteredPriorityFeeResponse {
        meter_bundle,
        priority_fee: estimate.priority_fee,
        blocks_sampled: estimate.blocks_sampled as u64,
        resource_estimates,
    }
}

fn build_resource_estimate_responses(
    estimates: &ResourceEstimates,
) -> Vec<ResourceFeeEstimateResponse> {
    estimates
        .iter()
        .map(|(kind, est)| ResourceFeeEstimateResponse {
            resource: kind.as_camel_case().to_string(),
            threshold_priority_fee: est.threshold_priority_fee,
            recommended_priority_fee: est.recommended_priority_fee,
            cumulative_usage: U256::from(est.cumulative_usage),
            threshold_tx_count: est.threshold_tx_count.try_into().unwrap_or(u64::MAX),
            total_transactions: est.total_transactions.try_into().unwrap_or(u64::MAX),
        })
        .collect()
}
