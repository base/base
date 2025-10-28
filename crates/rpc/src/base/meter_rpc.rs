use alloy_consensus::Header;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::U256;
use jsonrpsee::core::{RpcResult, async_trait};
use reth::providers::BlockReaderIdExt;
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use tips_core::types::{Bundle, MeterBundleResponse, ParsedBundle};
use tracing::{error, info};

use crate::{MeteringApiServer, meter_bundle};

/// Implementation of the metering RPC API
#[derive(Debug)]
pub struct MeteringApiImpl<Provider> {
    provider: Provider,
}

impl<Provider> MeteringApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + Clone,
{
    /// Creates a new instance of MeteringApi
    pub const fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl<Provider> MeteringApiServer for MeteringApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse> {
        info!(
            num_transactions = &bundle.txs.len(),
            block_number = &bundle.block_number,
            "Starting bundle metering"
        );

        // Get the latest header
        let header = self
            .provider
            .sealed_header_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(|e| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get latest header: {}", e),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    "Latest block not found".to_string(),
                    None::<()>,
                )
            })?;

        let parsed_bundle = ParsedBundle::try_from(bundle).map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InvalidParams.code(),
                format!("Failed to parse bundle: {}", e),
                None::<()>,
            )
        })?;

        // Get state provider for the block
        let state_provider = self.provider.state_by_block_hash(header.hash()).map_err(|e| {
            error!(error = %e, "Failed to get state provider");
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                format!("Failed to get state provider: {}", e),
                None::<()>,
            )
        })?;

        // Meter bundle using utility function
        let result = meter_bundle(
            state_provider,
            self.provider.chain_spec(),
            parsed_bundle,
            &header,
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
            total_execution_time_us = result.total_execution_time_us,
            state_root_time_us = result.state_root_time_us,
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
            state_flashblock_index: None,
            total_gas_used: result.total_gas_used,
            total_execution_time_us: result.total_execution_time_us,
        })
    }
}
