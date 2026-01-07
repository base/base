//! Implementation of the metering RPC API.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_consensus::Header;
use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_primitives::{B256, TxHash, U256};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    types::{ErrorCode, ErrorObjectOwned},
};
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use reth::providers::BlockReaderIdExt;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::OpBlock;
use reth_provider::{BlockReader, ChainSpecProvider, HeaderProvider, StateProviderFactory};
use tips_core::types::{Bundle, MeterBundleResponse, ParsedBundle};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::{
    block::meter_block,
    meter::meter_bundle,
    traits::MeteringApiServer,
    types::{MeterBlockResponse, MeteredPriorityFeeResponse, ResourceFeeEstimateResponse},
};
use crate::{
    AnnotatorCommand, MeteredTransaction, PriorityFeeEstimator, ResourceDemand, ResourceEstimates,
    RollingPriorityEstimate,
};

/// Implementation of the metering RPC API
pub struct MeteringApiImpl<Provider> {
    provider: Provider,
    priority_fee_estimator: Option<Arc<PriorityFeeEstimator>>,
    /// Channel to send metered transactions to the annotator.
    tx_sender: Option<mpsc::UnboundedSender<MeteredTransaction>>,
    /// Channel to send commands to the annotator.
    command_sender: Option<mpsc::UnboundedSender<AnnotatorCommand>>,
    /// Whether metering data collection is enabled.
    metering_enabled: Arc<AtomicBool>,
}

impl<Provider> std::fmt::Debug for MeteringApiImpl<Provider> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeteringApiImpl")
            .field("metering_enabled", &self.metering_enabled.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<Provider> MeteringApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + BlockReader<Block = OpBlock>
        + HeaderProvider<Header = Header>
        + Clone,
{
    /// Creates a new instance of MeteringApi without priority fee estimation.
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            priority_fee_estimator: None,
            tx_sender: None,
            command_sender: None,
            metering_enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Creates a new instance of MeteringApi with priority fee estimation enabled.
    pub fn with_estimator(
        provider: Provider,
        priority_fee_estimator: Arc<PriorityFeeEstimator>,
        tx_sender: mpsc::UnboundedSender<MeteredTransaction>,
        command_sender: mpsc::UnboundedSender<AnnotatorCommand>,
    ) -> Self {
        Self {
            provider,
            priority_fee_estimator: Some(priority_fee_estimator),
            tx_sender: Some(tx_sender),
            command_sender: Some(command_sender),
            metering_enabled: Arc::new(AtomicBool::new(true)),
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

        let header = self
            .provider
            .sealed_header_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("Failed to get latest header: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    "Latest block not found".to_string(),
                    None::<()>,
                )
            })?;

        let parsed_bundle = ParsedBundle::try_from(bundle).map_err(|e| {
            ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                format!("Failed to parse bundle: {e}"),
                None::<()>,
            )
        })?;

        let da_usage: u64 = parsed_bundle
            .txs
            .iter()
            .map(|tx| tx_estimated_size_fjord_bytes(&tx.encoded_2718()))
            .sum();

        let state_provider = self.provider.state_by_block_hash(header.hash()).map_err(|e| {
            error!(error = %e, "Failed to get state provider");
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                format!("Failed to get state provider: {e}"),
                None::<()>,
            )
        })?;

        let chain_spec = self.provider.chain_spec();

        let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
            meter_bundle(state_provider, chain_spec, parsed_bundle, &header).map_err(|e| {
                error!(error = %e, "Bundle metering failed");
                ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("Bundle metering failed: {e}"),
                    None::<()>,
                )
            })?;

        let bundle_gas_price = if total_gas_used > 0 {
            total_gas_fees / U256::from(total_gas_used)
        } else {
            U256::ZERO
        };

        info!(
            bundle_hash = %bundle_hash,
            num_transactions = results.len(),
            total_gas_used = total_gas_used,
            total_execution_time_us = total_execution_time,
            "Bundle metering completed successfully"
        );

        let response = MeterBundleResponse {
            bundle_gas_price,
            bundle_hash,
            coinbase_diff: total_gas_fees,
            eth_sent_to_coinbase: U256::ZERO,
            gas_fees: total_gas_fees,
            results,
            state_block_number: header.number,
            state_flashblock_index: None,
            total_gas_used,
            total_execution_time_us: total_execution_time,
        };

        let resource_demand = ResourceDemand {
            gas_used: Some(total_gas_used),
            execution_time_us: Some(total_execution_time),
            state_root_time_us: None, // Populated when state-root metrics become available.
            data_availability_bytes: Some(da_usage),
        };

        Ok((response, resource_demand))
    }
}

#[async_trait]
impl<Provider> MeteringApiServer for MeteringApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + BlockReader<Block = OpBlock>
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
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

    async fn meter_block_by_hash(&self, hash: B256) -> RpcResult<MeterBlockResponse> {
        info!(block_hash = %hash, "Starting block metering by hash");

        let block = self
            .provider
            .block_by_hash(hash)
            .map_err(|e| {
                error!(error = %e, "Failed to get block by hash");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get block: {}", e),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block not found: {}", hash),
                    None::<()>,
                )
            })?;

        let response = self.meter_block_internal(&block)?;

        info!(
            block_hash = %hash,
            signer_recovery_time_us = response.signer_recovery_time_us,
            execution_time_us = response.execution_time_us,
            state_root_time_us = response.state_root_time_us,
            total_time_us = response.total_time_us,
            "Block metering completed successfully"
        );

        Ok(response)
    }

    async fn meter_block_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<MeterBlockResponse> {
        info!(block_number = ?number, "Starting block metering by number");

        let block = self
            .provider
            .block_by_number_or_tag(number)
            .map_err(|e| {
                error!(error = %e, "Failed to get block by number");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get block: {}", e),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block not found: {:?}", number),
                    None::<()>,
                )
            })?;

        let response = self.meter_block_internal(&block)?;

        info!(
            block_number = ?number,
            block_hash = %response.block_hash,
            signer_recovery_time_us = response.signer_recovery_time_us,
            execution_time_us = response.execution_time_us,
            state_root_time_us = response.state_root_time_us,
            total_time_us = response.total_time_us,
            "Block metering completed successfully"
        );

        Ok(response)
    }

    async fn set_metering_info(
        &self,
        tx_hash: TxHash,
        meter: MeterBundleResponse,
    ) -> RpcResult<()> {
        if !self.metering_enabled.load(Ordering::Relaxed) {
            debug!(tx_hash = %tx_hash, "Metering disabled, ignoring set_metering_info");
            return Ok(());
        }

        let tx_sender = self.tx_sender.as_ref().ok_or_else(|| {
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "Metering pipeline not configured".to_string(),
                None::<()>,
            )
        })?;

        // Extract data from the first transaction result (single tx metering)
        let result = meter.results.first().ok_or_else(|| {
            ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                "MeterBundleResponse must contain at least one result".to_string(),
                None::<()>,
            )
        })?;

        let metered_tx = MeteredTransaction {
            tx_hash,
            priority_fee_per_gas: result.gas_price,
            gas_used: result.gas_used,
            execution_time_us: result.execution_time_us,
            state_root_time_us: 0,      // Not available in MeterBundleResponse
            data_availability_bytes: 0, // Not available in MeterBundleResponse, will be set by annotator if needed
        };

        debug!(
            tx_hash = %tx_hash,
            gas_used = result.gas_used,
            execution_time_us = result.execution_time_us,
            "Received metering info via RPC"
        );

        if tx_sender.send(metered_tx).is_err() {
            warn!(tx_hash = %tx_hash, "Failed to send metered transaction to annotator");
        }

        Ok(())
    }

    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()> {
        self.metering_enabled.store(enabled, Ordering::Relaxed);
        info!(enabled, "Metering data collection enabled state changed");
        Ok(())
    }

    async fn clear_metering_info(&self) -> RpcResult<()> {
        let command_sender = self.command_sender.as_ref().ok_or_else(|| {
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "Metering pipeline not configured".to_string(),
                None::<()>,
            )
        })?;

        if command_sender.send(AnnotatorCommand::ClearPending).is_err() {
            warn!("Failed to send clear command to annotator");
        }

        info!("Cleared pending metering information");
        Ok(())
    }
}

impl<Provider> MeteringApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + BlockReader<Block = OpBlock>
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Internal helper to meter a block's execution
    fn meter_block_internal(&self, block: &OpBlock) -> RpcResult<MeterBlockResponse> {
        meter_block(self.provider.clone(), self.provider.chain_spec(), block).map_err(|e| {
            error!(error = %e, "Block metering failed");
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                format!("Block metering failed: {}", e),
                None::<()>,
            )
        })
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
