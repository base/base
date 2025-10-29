use alloy_consensus::Header;
use alloy_eips::eip2718::Decodable2718;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::U256;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use reth::providers::BlockReaderIdExt;
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use tips_core::types::{Bundle, BundleWithMetadata, MeterBundleResponse};
use tracing::{error, info};

use crate::meter_bundle;

/// RPC API for transaction metering
#[rpc(server, namespace = "base")]
pub trait MeteringApi {
    /// Simulates and meters a bundle of transactions
    #[method(name = "meterBundle")]
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse>;
}

/// Implementation of the metering RPC API
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
    pub fn new(provider: Provider) -> Self {
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
            num_transactions = bundle.txs.len(),
            block_number = bundle.block_number,
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

        // Manually decode transactions to OpTxEnvelope (op-alloy 0.20) instead of using
        // BundleWithMetadata.transactions() which returns op-alloy 0.21 types incompatible with reth.
        // TODO: Remove this workaround after reth updates to op-alloy 0.21 (already on main, awaiting release)
        let mut decoded_txs = Vec::new();
        for tx_bytes in &bundle.txs {
            let mut reader = tx_bytes.as_ref();
            let tx = op_alloy_consensus::OpTxEnvelope::decode_2718(&mut reader).map_err(|e| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Failed to decode transaction: {}", e),
                    None::<()>,
                )
            })?;
            decoded_txs.push(tx);
        }

        let bundle_with_metadata = BundleWithMetadata::load(bundle.clone()).map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InvalidParams.code(),
                format!("Failed to load bundle metadata: {}", e),
                None::<()>,
            )
        })?;

        // Get state provider for the block
        let state_provider = self
            .provider
            .state_by_block_hash(header.hash())
            .map_err(|e| {
                error!(error = %e, "Failed to get state provider");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get state provider: {}", e),
                    None::<()>,
                )
            })?;

        // Meter bundle using utility function
        let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
            meter_bundle(
                state_provider,
                self.provider.chain_spec().clone(),
                decoded_txs,
                &header,
                &bundle_with_metadata,
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
        let bundle_gas_price = if total_gas_used > 0 {
            (total_gas_fees / U256::from(total_gas_used)).to_string()
        } else {
            "0".to_string()
        };

        info!(
            bundle_hash = %bundle_hash,
            num_transactions = results.len(),
            total_gas_used = total_gas_used,
            total_execution_time_us = total_execution_time,
            "Bundle metering completed successfully"
        );

        Ok(MeterBundleResponse {
            bundle_gas_price,
            bundle_hash,
            coinbase_diff: total_gas_fees.to_string(),
            eth_sent_to_coinbase: "0".to_string(),
            gas_fees: total_gas_fees.to_string(),
            results,
            state_block_number: header.number,
            total_gas_used,
            total_execution_time_us: total_execution_time,
        })
    }
}
