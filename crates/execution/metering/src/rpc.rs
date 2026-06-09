//! Implementation of the metering RPC API.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_consensus::{BlockHeader, Header};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, TxHash, U256};
use base_bundles::{Bundle, MeterBundleResponse, ParsedBundle};
use base_common_consensus::BaseBlock;
use base_common_evm::L1BlockInfo;
use base_common_flz::flz_compress_len;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::extract_l1_info_from_tx;
use jsonrpsee::core::{RpcResult, async_trait};
use reth_provider::{
    BlockReader, BlockReaderIdExt, ChainSpecProvider, HeaderProvider, StateProviderFactory,
};
use tracing::{debug, error, info, warn};

use crate::{
    MeterBlockResponse, MeteredPriorityFeeResponse, PriorityFeeEstimator, ResourceDemand,
    ResourceFeeEstimateResponse,
    block::meter_block,
    meter::{MeterBundleInput, meter_bundle},
    traits::MeteringApiServer,
};

/// Implementation of the metering RPC API.
pub struct MeteringApiImpl<Provider> {
    provider: Provider,
    /// Optional priority fee estimator for `meteredPriorityFeePerGas`.
    priority_fee_estimator: Option<Arc<PriorityFeeEstimator>>,
    /// Whether metering data collection is enabled.
    metering_enabled: Arc<AtomicBool>,
    /// Opcodes and precompiles to track for gas metering. When non-empty, a
    /// `MeteringInspector` is attached during bundle execution.
    metered_opcodes: Arc<crate::MeteredOpcodes>,
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
        + ChainSpecProvider<ChainSpec = BaseChainSpec>
        + BlockReaderIdExt<Header = Header>
        + BlockReader<Block = BaseBlock>
        + HeaderProvider<Header = Header>
        + Clone,
{
    /// Creates a new instance of `MeteringApi` without priority fee estimation.
    pub fn new(provider: Provider, metered_opcodes: Arc<crate::MeteredOpcodes>) -> Self {
        Self {
            provider,
            priority_fee_estimator: None,
            metering_enabled: Arc::new(AtomicBool::new(true)),
            metered_opcodes,
        }
    }

    /// Creates a new instance with priority fee estimation enabled.
    pub fn with_estimator(
        provider: Provider,
        estimator: Arc<PriorityFeeEstimator>,
        metered_opcodes: Arc<crate::MeteredOpcodes>,
    ) -> Self {
        Self {
            provider,
            priority_fee_estimator: Some(estimator),
            metering_enabled: Arc::new(AtomicBool::new(true)),
            metered_opcodes,
        }
    }
}

#[async_trait]
impl<Provider> MeteringApiServer for MeteringApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = BaseChainSpec>
        + BlockReaderIdExt<Header = Header>
        + BlockReader<Block = BaseBlock>
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse> {
        debug!(
            num_transactions = &bundle.txs.len(),
            block_number = &bundle.block_number,
            "Starting bundle metering"
        );

        let canonical_block_number = BlockNumberOrTag::Latest;
        let header = self
            .provider
            .sealed_header_by_number_or_tag(canonical_block_number)
            .map_err(|e| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get canonical block header: {e}"),
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

        debug!(canonical_block = header.number, "Using canonical block state for metering");

        let parsed_bundle = ParsedBundle::try_from(bundle).map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InvalidParams.code(),
                format!("Failed to parse bundle: {e}"),
                None::<()>,
            )
        })?;

        // Get state provider for the canonical block
        let state_provider =
            self.provider.state_by_block_number_or_tag(canonical_block_number).map_err(|e| {
                error!(error = %e, "Failed to get state provider");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get state provider: {e}"),
                    None::<()>,
                )
            })?;

        let parent_beacon_block_root = header.parent_beacon_block_root();

        // Get L1 block info from the canonical block.
        let l1_block_info = self.get_l1_block_info(canonical_block_number)?;

        // Meter bundle using utility function
        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: self.provider.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root,
            l1_block_info,
            metered_opcodes: Arc::clone(&self.metered_opcodes),
        })
        .map_err(|e| {
            // Sample error msg:
            // Transaction $TX_HASH execution failed: EVM reported invalid transaction ($TX_HASH): nonce $EXPECTED_NONCE too high, expected $EXPECTED_NONCE"
            let error_msg = e.to_string();
            if error_msg.contains("nonce") {
                debug!(error = %e, "Bundle metering failed");
            } else {
                info!(error = %e, "Bundle metering failed");
            }
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                format!("Bundle metering failed: {e}"),
                None::<()>,
            )
        })?;

        // Calculate average gas price
        let bundle_gas_price = if output.total_gas_used > 0 {
            output.total_gas_fees / U256::from(output.total_gas_used)
        } else {
            U256::from(0)
        };
        let total_execution_time_us = output
            .results
            .iter()
            .fold(0u128, |acc, result| acc.saturating_add(result.execution_time_us));

        debug!(
            bundle_hash = %output.bundle_hash,
            num_transactions = output.results.len(),
            total_gas_used = output.total_gas_used,
            total_time_us = output.total_time_us,
            state_block_number = header.number,
            "Bundle metering completed successfully"
        );

        Ok(MeterBundleResponse {
            bundle_gas_price,
            bundle_hash: output.bundle_hash,
            coinbase_diff: output.total_gas_fees,
            eth_sent_to_coinbase: U256::from(0),
            gas_fees: output.total_gas_fees,
            results: output.results,
            state_block_number: header.number,
            state_sub_block_index: None,
            total_gas_used: output.total_gas_used,
            total_execution_time_us,
            state_root_time_us: output.state_root_time_us,
            state_root_account_leaf_count: output.state_root_account_leaf_count,
            state_root_account_branch_count: output.state_root_account_branch_count,
            state_root_storage_leaf_count: output.state_root_storage_leaf_count,
            state_root_storage_branch_count: output.state_root_storage_branch_count,
        })
    }

    async fn meter_block_by_hash(&self, hash: B256) -> RpcResult<MeterBlockResponse> {
        debug!(block_hash = %hash, "Starting block metering by hash");

        let block = self
            .provider
            .block_by_hash(hash)
            .map_err(|e| {
                error!(error = %e, "Failed to get block by hash");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get block: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block not found: {hash}"),
                    None::<()>,
                )
            })?;

        let response = self.meter_block_internal(&block)?;

        debug!(
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
        debug!(block_number = ?number, "Starting block metering by number");

        let block = self
            .provider
            .block_by_number_or_tag(number)
            .map_err(|e| {
                error!(error = %e, "Failed to get block by number");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get block: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block not found: {number:?}"),
                    None::<()>,
                )
            })?;

        let response = self.meter_block_internal(&block)?;

        debug!(
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

    async fn metered_priority_fee_per_gas(
        &self,
        bundle: Bundle,
    ) -> RpcResult<MeteredPriorityFeeResponse> {
        let Some(estimator) = &self.priority_fee_estimator else {
            debug!("Priority fee estimation requested but no estimator configured");
            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                "Priority fee estimation not configured".to_string(),
                None::<()>,
            ));
        };

        debug!(
            num_transactions = &bundle.txs.len(),
            block_number = &bundle.block_number,
            "Starting metered priority fee estimation"
        );

        // Meter the bundle to get resource consumption
        let meter_bundle_response = self.meter_bundle(bundle.clone()).await?;

        // Compute resource demand from metering results
        let demand = compute_resource_demand(&bundle, &meter_bundle_response);

        // Get rolling estimate from the estimator
        let rolling_estimate = estimator.estimate_rolling(demand).map_err(|e| {
            debug!(error = %e, "Priority fee estimation failed");
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                format!("Priority fee estimation failed: {e}"),
                None::<()>,
            )
        })?;

        let Some(rolling_estimate) = rolling_estimate else {
            warn!("No metering data available for priority fee estimation");
            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                "No metering data available: cache is empty or not yet populated".to_string(),
                None::<()>,
            ));
        };

        // Build response
        let resource_estimates: Vec<ResourceFeeEstimateResponse> = rolling_estimate
            .estimates
            .iter()
            .map(|(kind, est)| ResourceFeeEstimateResponse {
                resource: kind.as_camel_case().to_string(),
                threshold_priority_fee: est.threshold_priority_fee,
                recommended_priority_fee: est.recommended_priority_fee,
                cumulative_usage: U256::from(est.cumulative_usage),
                threshold_tx_count: est.threshold_tx_count as u64,
                total_transactions: est.total_transactions as u64,
            })
            .collect();

        debug!(
            priority_fee = %rolling_estimate.priority_fee,
            blocks_sampled = rolling_estimate.blocks_sampled,
            "Metered priority fee estimation completed"
        );

        Ok(MeteredPriorityFeeResponse {
            meter_bundle: meter_bundle_response,
            priority_fee: rolling_estimate.priority_fee,
            blocks_sampled: rolling_estimate.blocks_sampled as u64,
            resource_estimates,
        })
    }

    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        meter: MeterBundleResponse,
    ) -> RpcResult<()> {
        // Check if metering is enabled
        if !self.metering_enabled.load(Ordering::Relaxed) {
            debug!(tx_hash = %tx_hash, "Ignoring metering info - metering disabled");
            return Ok(());
        }

        if meter.state_root_time_us > 0 {
            debug!(
                tx_hash = %tx_hash,
                state_root_time_us = meter.state_root_time_us,
                "Received external metering info"
            );
        }
        Ok(())
    }

    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()> {
        self.metering_enabled.store(enabled, Ordering::Relaxed);
        info!(enabled = enabled, "Metering data collection enabled state changed");
        Ok(())
    }

    async fn clear_metering_information(&self) -> RpcResult<()> {
        info!("Cleared metering information");
        Ok(())
    }
}

/// Computes resource demand from bundle metering results.
fn compute_resource_demand(bundle: &Bundle, meter_result: &MeterBundleResponse) -> ResourceDemand {
    // Calculate DA bytes from bundle transactions
    let da_bytes: u64 =
        bundle.txs.iter().fold(0u64, |acc, tx| acc.saturating_add(flz_compress_len(tx) as u64));

    ResourceDemand {
        gas_used: Some(meter_result.total_gas_used),
        execution_time_us: Some(meter_result.total_execution_time_us),
        state_root_time_us: Some(meter_result.state_root_time_us),
        data_availability_bytes: Some(da_bytes),
    }
}

impl<Provider> MeteringApiImpl<Provider>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = BaseChainSpec>
        + BlockReaderIdExt<Header = Header>
        + BlockReader<Block = BaseBlock>
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Get L1 block info from the first transaction of a block.
    ///
    /// Uses the block number/tag to look up the block.
    fn get_l1_block_info(&self, block_id: BlockNumberOrTag) -> RpcResult<L1BlockInfo> {
        let first_tx = self
            .provider
            .block_by_number_or_tag(block_id)
            .map_err(|e| {
                error!(error = %e, block = ?block_id, "Failed to get block");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get block: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block not found: {block_id:?}"),
                    None::<()>,
                )
            })?
            .body
            .transactions
            .first()
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block has no transactions: {block_id:?}"),
                    None::<()>,
                )
            })?
            .clone();

        extract_l1_info_from_tx(&first_tx).map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InvalidParams.code(),
                format!("Failed to extract L1 block info from transaction: {e}"),
                None::<()>,
            )
        })
    }

    /// Internal helper to meter a block's execution
    fn meter_block_internal(&self, block: &BaseBlock) -> RpcResult<MeterBlockResponse> {
        meter_block(self.provider.clone(), self.provider.chain_spec(), block).map_err(|e| {
            error!(error = %e, "Block metering failed");
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                format!("Block metering failed: {e}"),
                None::<()>,
            )
        })
    }
}
