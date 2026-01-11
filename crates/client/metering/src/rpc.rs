//! Implementation of the metering RPC API.

use std::sync::Arc;

use alloy_consensus::Header;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, U256};
use base_bundles::{Bundle, MeterBundleResponse, ParsedBundle};
use jsonrpsee::core::{RpcResult, async_trait};
use op_alloy_flz::flz_compress_len;
use reth::providers::BlockReaderIdExt;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::OpBlock;
use reth_provider::{BlockReader, ChainSpecProvider, HeaderProvider, StateProviderFactory};
use tracing::{error, info, warn};

use crate::{
    MeterBlockResponse, MeteredPriorityFeeResponse, PriorityFeeEstimator, ResourceDemand,
    ResourceFeeEstimateResponse, block::meter_block, meter::meter_bundle, traits::MeteringApiServer,
};

/// Implementation of the metering RPC API
#[derive(Debug)]
pub struct MeteringApiImpl<Provider> {
    provider: Provider,
    /// Optional priority fee estimator for `meteredPriorityFeePerGas`.
    priority_fee_estimator: Option<Arc<PriorityFeeEstimator>>,
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
    pub const fn new(provider: Provider) -> Self {
        Self { provider, priority_fee_estimator: None }
    }

    /// Creates a new instance with priority fee estimation enabled.
    pub fn with_estimator(provider: Provider, estimator: Arc<PriorityFeeEstimator>) -> Self {
        Self { provider, priority_fee_estimator: Some(estimator) }
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
        let (results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time) =
            meter_bundle(state_provider, self.provider.chain_spec(), parsed_bundle, &header)
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
            total_gas_fees / U256::from(total_gas_used)
        } else {
            U256::from(0)
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
            coinbase_diff: total_gas_fees,
            eth_sent_to_coinbase: U256::from(0),
            gas_fees: total_gas_fees,
            results,
            state_block_number: header.number,
            state_flashblock_index: None,
            total_gas_used,
            total_execution_time_us: total_execution_time,
        })
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

    async fn metered_priority_fee_per_gas(
        &self,
        bundle: Bundle,
    ) -> RpcResult<MeteredPriorityFeeResponse> {
        info!(
            num_transactions = &bundle.txs.len(),
            block_number = &bundle.block_number,
            "Starting metered priority fee estimation"
        );

        // First, meter the bundle to get resource consumption
        let meter_bundle_response = self.meter_bundle(bundle.clone()).await?;

        // Check if we have an estimator configured
        let Some(estimator) = &self.priority_fee_estimator else {
            warn!("Priority fee estimation requested but no estimator configured");
            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                "Priority fee estimation not configured".to_string(),
                None::<()>,
            ));
        };

        // Compute resource demand from metering results
        let demand = compute_resource_demand(&bundle, &meter_bundle_response);

        // Get rolling estimate across recent blocks
        let rolling_estimate = estimator.estimate_rolling(demand).map_err(|e| {
            error!(error = %e, "Priority fee estimation failed");
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                format!("Priority fee estimation failed: {}", e),
                None::<()>,
            )
        })?;

        let Some(estimate) = rolling_estimate else {
            warn!("No metering data available for priority fee estimation");
            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                "No metering data available for priority fee estimation".to_string(),
                None::<()>,
            ));
        };

        // Build response
        let resource_estimates: Vec<ResourceFeeEstimateResponse> = estimate
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

        info!(
            priority_fee = %estimate.priority_fee,
            blocks_sampled = estimate.blocks_sampled,
            "Metered priority fee estimation completed"
        );

        Ok(MeteredPriorityFeeResponse {
            meter_bundle: meter_bundle_response,
            priority_fee: estimate.priority_fee,
            blocks_sampled: estimate.blocks_sampled as u64,
            resource_estimates,
        })
    }
}

/// Computes resource demand from a metered bundle response.
fn compute_resource_demand(bundle: &Bundle, response: &MeterBundleResponse) -> ResourceDemand {
    // Calculate DA bytes from the bundle's raw transactions
    let da_bytes: u64 = bundle
        .txs
        .iter()
        .map(|tx| {
            // Use flz compression to estimate DA size (same as OP stack)
            flz_compress_len(tx) as u64
        })
        .sum();

    ResourceDemand {
        gas_used: Some(response.total_gas_used),
        execution_time_us: Some(response.total_execution_time_us),
        state_root_time_us: None, // Not tracked in bundle response
        data_availability_bytes: Some(da_bytes),
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

#[cfg(test)]
mod tests {
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Bytes, address};
    use alloy_rpc_client::RpcClient;
    use base_bundles::{Bundle, MeterBundleResponse};
    use base_reth_test_utils::{ALICE, BASE_CHAIN_ID, BOB, TestHarness};
    use op_alloy_consensus::OpTxEnvelope;
    use reth_optimism_primitives::OpTransactionSigned;
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;
    use crate::test_utils::metering_launcher;

    fn create_bundle(txs: Vec<Bytes>, block_number: u64, min_timestamp: Option<u64>) -> Bundle {
        Bundle {
            txs,
            block_number,
            flashblock_number_min: None,
            flashblock_number_max: None,
            min_timestamp,
            max_timestamp: None,
            reverting_tx_hashes: vec![],
            replacement_uuid: None,
            dropping_tx_hashes: vec![],
        }
    }

    #[tokio::test]
    async fn test_meter_bundle_empty() -> eyre::Result<()> {
        let harness = TestHarness::with_launcher(metering_launcher).await?;
        let client = RpcClient::new_http(harness.rpc_url().parse()?);

        let bundle = create_bundle(vec![], 0, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 0);
        assert_eq!(response.total_gas_used, 0);
        assert_eq!(response.gas_fees, U256::from(0));
        assert_eq!(response.state_block_number, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_single_transaction() -> eyre::Result<()> {
        let harness = TestHarness::with_launcher(metering_launcher).await?;
        let client = RpcClient::new_http(harness.rpc_url().parse()?);

        // Build a transaction using the shared ALICE account
        let tx = TransactionBuilder::default()
            .signer(ALICE.signer_b256())
            .chain_id(BASE_CHAIN_ID)
            .nonce(0)
            .to(address!("0x1111111111111111111111111111111111111111"))
            .value(1000)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000) // 1 gwei
            .max_priority_fee_per_gas(1_000_000_000)
            .into_eip1559();

        let signed_tx =
            OpTransactionSigned::Eip1559(tx.as_eip1559().expect("eip1559 transaction").clone());
        let envelope: OpTxEnvelope = signed_tx.into();

        // Encode transaction
        let tx_bytes = Bytes::from(envelope.encoded_2718());

        let bundle = create_bundle(vec![tx_bytes], 0, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.total_gas_used, 21_000);
        assert!(response.total_execution_time_us > 0);

        let result = &response.results[0];
        assert_eq!(result.from_address, ALICE.address);
        assert_eq!(result.to_address, Some(address!("0x1111111111111111111111111111111111111111")));
        assert_eq!(result.gas_used, 21_000);
        assert_eq!(result.gas_price, 1_000_000_000);
        assert!(result.execution_time_us > 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_multiple_transactions() -> eyre::Result<()> {
        let harness = TestHarness::with_launcher(metering_launcher).await?;
        let client = RpcClient::new_http(harness.rpc_url().parse()?);

        let tx1_inner = TransactionBuilder::default()
            .signer(ALICE.signer_b256())
            .chain_id(BASE_CHAIN_ID)
            .nonce(0)
            .to(address!("0x1111111111111111111111111111111111111111"))
            .value(1000)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1_000_000_000)
            .into_eip1559();

        let tx1_signed = OpTransactionSigned::Eip1559(
            tx1_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let tx1_envelope: OpTxEnvelope = tx1_signed.into();
        let tx1_bytes = Bytes::from(tx1_envelope.encoded_2718());

        // Second transaction from Bob
        let tx2_inner = TransactionBuilder::default()
            .signer(BOB.signer_b256())
            .chain_id(BASE_CHAIN_ID)
            .nonce(0)
            .to(address!("0x2222222222222222222222222222222222222222"))
            .value(2000)
            .gas_limit(21_000)
            .max_fee_per_gas(2_000_000_000)
            .max_priority_fee_per_gas(2_000_000_000)
            .into_eip1559();

        let tx2_signed = OpTransactionSigned::Eip1559(
            tx2_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let tx2_envelope: OpTxEnvelope = tx2_signed.into();
        let tx2_bytes = Bytes::from(tx2_envelope.encoded_2718());

        let bundle = create_bundle(vec![tx1_bytes, tx2_bytes], 0, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 2);
        assert_eq!(response.total_gas_used, 42_000);
        assert!(response.total_execution_time_us > 0);

        // Check first transaction
        let result1 = &response.results[0];
        assert_eq!(result1.from_address, ALICE.address);
        assert_eq!(result1.gas_used, 21_000);
        assert_eq!(result1.gas_price, 1_000_000_000);

        // Check second transaction
        let result2 = &response.results[1];
        assert_eq!(result2.from_address, BOB.address);
        assert_eq!(result2.gas_used, 21_000);
        assert_eq!(result2.gas_price, 2_000_000_000);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_invalid_transaction() -> eyre::Result<()> {
        let harness = TestHarness::with_launcher(metering_launcher).await?;
        let client = RpcClient::new_http(harness.rpc_url().parse()?);

        let bundle = create_bundle(
            vec![Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef])], // Invalid transaction data
            0,
            None,
        );

        let result: Result<MeterBundleResponse, _> =
            client.request("base_meterBundle", (bundle,)).await;

        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_uses_latest_block() -> eyre::Result<()> {
        let harness = TestHarness::with_launcher(metering_launcher).await?;
        let client = RpcClient::new_http(harness.rpc_url().parse()?);

        // Metering always uses the latest block state, regardless of bundle.block_number
        let bundle = create_bundle(vec![], 0, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        // Should return the latest block number (genesis block 0)
        assert_eq!(response.state_block_number, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_ignores_bundle_block_number() -> eyre::Result<()> {
        let harness = TestHarness::with_launcher(metering_launcher).await?;
        let client = RpcClient::new_http(harness.rpc_url().parse()?);

        // Even if bundle.block_number is different, it should use the latest block
        // In this test, we specify block_number=0 in the bundle
        let bundle1 = create_bundle(vec![], 0, None);
        let response1: MeterBundleResponse = client.request("base_meterBundle", (bundle1,)).await?;

        // Try with a different bundle.block_number (999 - arbitrary value)
        // Since we can't create future blocks, we use a different value to show it's ignored
        let bundle2 = create_bundle(vec![], 999, None);
        let response2: MeterBundleResponse = client.request("base_meterBundle", (bundle2,)).await?;

        // Both should return the same state_block_number (the latest block)
        // because the implementation always uses Latest, not bundle.block_number
        assert_eq!(response1.state_block_number, response2.state_block_number);
        assert_eq!(response1.state_block_number, 0); // Genesis block

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_custom_timestamp() -> eyre::Result<()> {
        let harness = TestHarness::with_launcher(metering_launcher).await?;
        let client = RpcClient::new_http(harness.rpc_url().parse()?);

        // Test that bundle.min_timestamp is used for simulation.
        // The timestamp affects block.timestamp in the EVM during simulation but is not
        // returned in the response.
        let custom_timestamp = 1234567890;
        let bundle = create_bundle(vec![], 0, Some(custom_timestamp));

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        // Verify the request succeeded with custom timestamp
        assert_eq!(response.results.len(), 0);
        assert_eq!(response.total_gas_used, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_arbitrary_block_number() -> eyre::Result<()> {
        let harness = TestHarness::with_launcher(metering_launcher).await?;
        let client = RpcClient::new_http(harness.rpc_url().parse()?);

        // Since we now ignore bundle.block_number and always use the latest block,
        // any block_number value should work (it's only used for bundle validity in TIPS)
        let bundle = create_bundle(vec![], 999999, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        // Should succeed and use the latest block (genesis block 0)
        assert_eq!(response.state_block_number, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_gas_calculations() -> eyre::Result<()> {
        let harness = TestHarness::with_launcher(metering_launcher).await?;
        let client = RpcClient::new_http(harness.rpc_url().parse()?);

        // First transaction with 3 gwei gas price
        let tx1_inner = TransactionBuilder::default()
            .signer(ALICE.signer_b256())
            .chain_id(BASE_CHAIN_ID)
            .nonce(0)
            .to(address!("0x1111111111111111111111111111111111111111"))
            .value(1000)
            .gas_limit(21_000)
            .max_fee_per_gas(3_000_000_000) // 3 gwei
            .max_priority_fee_per_gas(3_000_000_000)
            .into_eip1559();

        let signed_tx1 = OpTransactionSigned::Eip1559(
            tx1_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let envelope1: OpTxEnvelope = signed_tx1.into();
        let tx1_bytes = Bytes::from(envelope1.encoded_2718());

        // Second transaction with 7 gwei gas price
        let tx2_inner = TransactionBuilder::default()
            .signer(BOB.signer_b256())
            .chain_id(BASE_CHAIN_ID)
            .nonce(0)
            .to(address!("0x2222222222222222222222222222222222222222"))
            .value(2000)
            .gas_limit(21_000)
            .max_fee_per_gas(7_000_000_000) // 7 gwei
            .max_priority_fee_per_gas(7_000_000_000)
            .into_eip1559();

        let signed_tx2 = OpTransactionSigned::Eip1559(
            tx2_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let envelope2: OpTxEnvelope = signed_tx2.into();
        let tx2_bytes = Bytes::from(envelope2.encoded_2718());

        let bundle = create_bundle(vec![tx1_bytes, tx2_bytes], 0, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 2);

        // Check first transaction (3 gwei)
        let result1 = &response.results[0];
        let expected_gas_fees_1 = U256::from(21_000) * U256::from(3_000_000_000u64);
        assert_eq!(result1.gas_fees, expected_gas_fees_1);
        assert_eq!(result1.gas_price, U256::from(3000000000u64));
        assert_eq!(result1.coinbase_diff, expected_gas_fees_1);

        // Check second transaction (7 gwei)
        let result2 = &response.results[1];
        let expected_gas_fees_2 = U256::from(21_000) * U256::from(7_000_000_000u64);
        assert_eq!(result2.gas_fees, expected_gas_fees_2);
        assert_eq!(result2.gas_price, U256::from(7000000000u64));
        assert_eq!(result2.coinbase_diff, expected_gas_fees_2);

        // Check bundle totals
        let total_gas_fees = expected_gas_fees_1 + expected_gas_fees_2;
        assert_eq!(response.gas_fees, total_gas_fees);
        assert_eq!(response.coinbase_diff, total_gas_fees);
        assert_eq!(response.total_gas_used, 42_000);

        // Bundle gas price should be weighted average: (3*21000 + 7*21000) / (21000 + 21000) = 5
        // gwei
        assert_eq!(response.bundle_gas_price, U256::from(5000000000u64));

        Ok(())
    }
}
