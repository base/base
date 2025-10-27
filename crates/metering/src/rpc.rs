use alloy_consensus::Header;
use alloy_eips::eip2718::Decodable2718;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, Bytes, TxHash, B256, U256};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use reth::providers::BlockReaderIdExt;
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::meter_bundle;

/// Request payload for base_meterBundle
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterBundleRequest {
    /// Array of signed transactions (hex encoded)
    pub txs: Vec<Bytes>,
    /// Block number for which this bundle is valid
    pub block_number: BlockNumberOrTag,
    /// State block number or tag to base simulation on
    pub state_block_number: BlockNumberOrTag,
    /// Optional timestamp for simulation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

/// Per-transaction result
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionResult {
    pub coinbase_diff: String,
    pub eth_sent_to_coinbase: String,
    pub from_address: Address,
    pub gas_fees: String,
    pub gas_price: String,
    pub gas_used: u64,
    pub to_address: Option<Address>,
    pub tx_hash: TxHash,
    pub value: String,
    /// Resource metering: execution time for this tx in microseconds
    pub execution_time_us: u128,
}

/// Response for base_meterBundle
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterBundleResponse {
    pub bundle_gas_price: String,
    pub bundle_hash: B256,
    pub coinbase_diff: String,
    pub eth_sent_to_coinbase: String,
    pub gas_fees: String,
    pub results: Vec<TransactionResult>,
    pub state_block_number: u64,
    pub total_gas_used: u64,
    /// Resource metering: total execution time in microseconds
    pub total_execution_time_us: u128,
}

/// RPC API for transaction metering
#[rpc(server, namespace = "base")]
pub trait MeteringApi {
    /// Simulates and meters a bundle of transactions
    #[method(name = "meterBundle")]
    async fn meter_bundle(&self, request: MeterBundleRequest) -> RpcResult<MeterBundleResponse>;
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
    async fn meter_bundle(&self, request: MeterBundleRequest) -> RpcResult<MeterBundleResponse> {
        info!(
            num_transactions = request.txs.len(),
            block_number = ?request.block_number,
            state_block_number = ?request.state_block_number,
            "Starting bundle metering"
        );

        // Resolve state block number
        let state_block_number = match request.state_block_number {
            BlockNumberOrTag::Number(n) => n,
            BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                self.provider.best_block_number().map_err(|e| {
                    jsonrpsee::types::ErrorObjectOwned::owned(
                        jsonrpsee::types::ErrorCode::InternalError.code(),
                        format!("Failed to get latest block: {}", e),
                        None::<()>,
                    )
                })?
            }
            _ => {
                return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    "Unsupported block number tag".to_string(),
                    None::<()>,
                ));
            }
        };

        // Get the header
        let header = self
            .provider
            .sealed_header(state_block_number)
            .map_err(|e| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get header: {}", e),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block {} not found", state_block_number),
                    None::<()>,
                )
            })?;

        // Decode all transactions
        let mut decoded_txs = Vec::new();
        for tx_bytes in &request.txs {
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
        let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) = meter_bundle(
            state_provider,
            self.provider.chain_spec().clone(),
            decoded_txs,
            &header,
            request.timestamp,
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
            state_block_number,
            total_gas_used,
            total_execution_time_us: total_execution_time,
        })
    }
}
