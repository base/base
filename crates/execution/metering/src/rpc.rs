//! Implementation of the metering RPC API.

use std::sync::Arc;

use alloy_consensus::{BlockHeader, Header};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, U256};
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
    /// Opcodes and precompiles to track for gas metering. When non-empty, a
    /// `MeteringInspector` is attached during bundle execution.
    metered_opcodes: Arc<crate::MeteredOpcodes>,
}

impl<Provider> std::fmt::Debug for MeteringApiImpl<Provider> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeteringApiImpl").finish_non_exhaustive()
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
    pub const fn new(provider: Provider, metered_opcodes: Arc<crate::MeteredOpcodes>) -> Self {
        Self { provider, priority_fee_estimator: None, metered_opcodes }
    }

    /// Creates a new instance with priority fee estimation enabled.
    pub const fn with_estimator(
        provider: Provider,
        estimator: Arc<PriorityFeeEstimator>,
        metered_opcodes: Arc<crate::MeteredOpcodes>,
    ) -> Self {
        Self { provider, priority_fee_estimator: Some(estimator), metered_opcodes }
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
        debug!(num_transactions = &bundle.txs.len(), "Starting bundle metering");

        // Simulate against the latest canonical header and state for that
        // header's hash. Do not pin state to `Number(n)`: reth routes numbered
        // lookups through historical/DB state instead of the in-memory tip.
        let header = self
            .provider
            .sealed_header_by_number_or_tag(BlockNumberOrTag::Latest)
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

        let state_provider = self.provider.state_by_block_hash(header.hash()).map_err(|e| {
            error!(error = %e, block_hash = %header.hash(), "Failed to get state provider");
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InternalError.code(),
                format!("Failed to get state provider: {e}"),
                None::<()>,
            )
        })?;

        let l1_block_info = self.get_l1_block_info(header.hash())?;
        let state_block_number = header.number;

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: self.provider.chain_spec(),
            bundle: parsed_bundle,
            header,
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
            state_block_number,
            "Bundle metering completed successfully"
        );

        Ok(MeterBundleResponse {
            bundle_gas_price,
            bundle_hash: output.bundle_hash,
            coinbase_diff: output.total_gas_fees,
            eth_sent_to_coinbase: U256::from(0),
            gas_fees: output.total_gas_fees,
            results: output.results,
            state_block_number,
            total_gas_used: output.total_gas_used,
            total_execution_time_us,
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

        debug!(num_transactions = &bundle.txs.len(), "Starting metered priority fee estimation");

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
}

/// Computes resource demand from bundle metering results.
fn compute_resource_demand(bundle: &Bundle, meter_result: &MeterBundleResponse) -> ResourceDemand {
    // Calculate DA bytes from bundle transactions
    let da_bytes: u64 =
        bundle.txs.iter().fold(0u64, |acc, tx| acc.saturating_add(flz_compress_len(tx) as u64));

    ResourceDemand {
        gas_used: Some(meter_result.total_gas_used),
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
    /// Get L1 block info from the first transaction of a canonical block.
    fn get_l1_block_info(&self, block_hash: B256) -> RpcResult<L1BlockInfo> {
        let first_tx = self
            .provider
            .block_by_hash(block_hash)
            .map_err(|e| {
                error!(error = %e, block_hash = %block_hash, "Failed to get block");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get block: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block not found: {block_hash}"),
                    None::<()>,
                )
            })?
            .body
            .transactions
            .first()
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block has no transactions: {block_hash}"),
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Header;
    use alloy_eips::Encodable2718;
    use alloy_primitives::{B256, Bloom, Bytes, address};
    use alloy_rpc_client::RpcClient;
    use base_bundles::{Bundle, MeterBundleResponse};
    use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_flashblocks::{FlashblocksConfig, PendingBlocksBuilder};
    use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, TestHarness};
    use base_test_utils::Account;
    use reth_transaction_pool::test_utils::TransactionBuilder;
    use url::Url;

    use super::*;
    use crate::{MeteringConfig, MeteringExtension, MeteringResourceLimits};

    fn create_bundle(txs: Vec<Bytes>) -> Bundle {
        Bundle { txs }
    }

    async fn setup() -> eyre::Result<(TestHarness, RpcClient)> {
        let harness = TestHarness::builder()
            .with_ext::<MeteringExtension>(MeteringConfig::enabled())
            .build()
            .await?;
        let client = harness.rpc_client()?;
        Ok((harness, client))
    }

    async fn generate_txs_for_block(chain_id: u64) -> Vec<Bytes> {
        vec![
            L1_BLOCK_INFO_DEPOSIT_TX,
            TransactionBuilder::default()
                .signer(Account::Charlie.signer_b256())
                .chain_id(chain_id)
                .nonce(0)
                .to(address!("0x1111111111111111111111111111111111111111"))
                .value(1000)
                .gas_limit(21_000)
                .max_fee_per_gas(1_000_000_000)
                .max_priority_fee_per_gas(1_000_000_000)
                .into_eip1559()
                .into_encoded()
                .into_encoded_bytes(),
        ]
    }

    #[tokio::test]
    async fn test_meter_bundle_empty() -> eyre::Result<()> {
        let (harness, client) = setup().await?;

        // Build a block with a tx so that we don't get an error about missing L1 block info
        harness
            .build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await)
            .await?;

        let bundle = create_bundle(vec![]);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 0);
        assert_eq!(response.total_gas_used, 0);
        assert_eq!(response.gas_fees, U256::from(0));
        assert_eq!(response.state_block_number, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_single_transaction() -> eyre::Result<()> {
        let (harness, client) = setup().await?;

        harness
            .build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await)
            .await?;

        let sender_address = Account::Alice.address();
        let sender_secret = Account::Alice.signer_b256();

        let tx = TransactionBuilder::default()
            .signer(sender_secret)
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(address!("0x1111111111111111111111111111111111111111"))
            .value(1000)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000) // 1 gwei
            .max_priority_fee_per_gas(1_000_000_000)
            .into_eip1559();

        let signed_tx =
            BaseTransactionSigned::Eip1559(tx.as_eip1559().expect("eip1559 transaction").clone());
        let envelope: BaseTxEnvelope = signed_tx;

        let tx_bytes = Bytes::from(envelope.encoded_2718());

        let bundle = create_bundle(vec![tx_bytes]);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.total_gas_used, 21_000);
        assert!(response.total_execution_time_us > 0);
        assert_eq!(
            response.total_execution_time_us,
            response.results.iter().map(|result| result.execution_time_us).sum::<u128>()
        );

        let result = &response.results[0];
        assert_eq!(result.from_address, sender_address);
        assert_eq!(result.to_address, Some(address!("0x1111111111111111111111111111111111111111")));
        assert_eq!(result.gas_used, 21_000);
        assert_eq!(result.gas_price, 1_000_000_000);
        assert!(result.execution_time_us > 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_multiple_transactions() -> eyre::Result<()> {
        let (harness, client) = setup().await?;

        harness
            .build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await)
            .await?;

        let address1 = Account::Alice.address();
        let secret1 = Account::Alice.signer_b256();

        let tx1_inner = TransactionBuilder::default()
            .signer(secret1)
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(address!("0x1111111111111111111111111111111111111111"))
            .value(1000)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1_000_000_000)
            .into_eip1559();

        let tx1_signed = BaseTransactionSigned::Eip1559(
            tx1_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let tx1_envelope: BaseTxEnvelope = tx1_signed;
        let tx1_bytes = Bytes::from(tx1_envelope.encoded_2718());

        let address2 = Account::Bob.address();
        let secret2 = Account::Bob.signer_b256();

        let tx2_inner = TransactionBuilder::default()
            .signer(secret2)
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(address!("0x2222222222222222222222222222222222222222"))
            .value(2000)
            .gas_limit(21_000)
            .max_fee_per_gas(2_000_000_000)
            .max_priority_fee_per_gas(2_000_000_000)
            .into_eip1559();

        let tx2_signed = BaseTransactionSigned::Eip1559(
            tx2_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let tx2_envelope: BaseTxEnvelope = tx2_signed;
        let tx2_bytes = Bytes::from(tx2_envelope.encoded_2718());

        let bundle = create_bundle(vec![tx1_bytes, tx2_bytes]);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 2);
        assert_eq!(response.total_gas_used, 42_000);
        assert!(response.total_execution_time_us > 0);
        assert_eq!(
            response.total_execution_time_us,
            response.results.iter().map(|result| result.execution_time_us).sum::<u128>()
        );

        let result1 = &response.results[0];
        assert_eq!(result1.from_address, address1);
        assert_eq!(result1.gas_used, 21_000);
        assert_eq!(result1.gas_price, 1_000_000_000);

        let result2 = &response.results[1];
        assert_eq!(result2.from_address, address2);
        assert_eq!(result2.gas_used, 21_000);
        assert_eq!(result2.gas_price, 2_000_000_000);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_invalid_transaction() -> eyre::Result<()> {
        let (_harness, client) = setup().await?;

        let bundle = create_bundle(vec![Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef])]);

        let result: Result<MeterBundleResponse, _> =
            client.request("base_meterBundle", (bundle,)).await;

        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_uses_latest_block() -> eyre::Result<()> {
        let (harness, client) = setup().await?;
        harness
            .build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await)
            .await?;

        let bundle = create_bundle(vec![]);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.state_block_number, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_gas_calculations() -> eyre::Result<()> {
        let (harness, client) = setup().await?;
        harness
            .build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await)
            .await?;

        let secret1 = Account::Alice.signer_b256();
        let secret2 = Account::Bob.signer_b256();

        let tx1_inner = TransactionBuilder::default()
            .signer(secret1)
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(address!("0x1111111111111111111111111111111111111111"))
            .value(1000)
            .gas_limit(21_000)
            .max_fee_per_gas(3_000_000_000) // 3 gwei
            .max_priority_fee_per_gas(3_000_000_000)
            .into_eip1559();

        let signed_tx1 = BaseTransactionSigned::Eip1559(
            tx1_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let envelope1: BaseTxEnvelope = signed_tx1;
        let tx1_bytes = Bytes::from(envelope1.encoded_2718());

        let tx2_inner = TransactionBuilder::default()
            .signer(secret2)
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(address!("0x2222222222222222222222222222222222222222"))
            .value(2000)
            .gas_limit(21_000)
            .max_fee_per_gas(7_000_000_000) // 7 gwei
            .max_priority_fee_per_gas(7_000_000_000)
            .into_eip1559();

        let signed_tx2 = BaseTransactionSigned::Eip1559(
            tx2_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let envelope2: BaseTxEnvelope = signed_tx2;
        let tx2_bytes = Bytes::from(envelope2.encoded_2718());

        let bundle = create_bundle(vec![tx1_bytes, tx2_bytes]);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 2);

        let result1 = &response.results[0];
        let expected_gas_fees_1 = U256::from(21_000) * U256::from(3_000_000_000u64);
        assert_eq!(result1.gas_fees, expected_gas_fees_1);
        assert_eq!(result1.gas_price, U256::from(3000000000u64));
        assert_eq!(result1.coinbase_diff, expected_gas_fees_1);

        let result2 = &response.results[1];
        let expected_gas_fees_2 = U256::from(21_000) * U256::from(7_000_000_000u64);
        assert_eq!(result2.gas_fees, expected_gas_fees_2);
        assert_eq!(result2.gas_price, U256::from(7000000000u64));
        assert_eq!(result2.coinbase_diff, expected_gas_fees_2);

        let total_gas_fees = expected_gas_fees_1 + expected_gas_fees_2;
        assert_eq!(response.gas_fees, total_gas_fees);
        assert_eq!(response.coinbase_diff, total_gas_fees);
        assert_eq!(response.total_gas_used, 42_000);

        // Bundle gas price should be weighted average: (3*21000 + 7*21000) / (21000 + 21000) = 5 gwei
        assert_eq!(response.bundle_gas_price, U256::from(5000000000u64));

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_no_l1_block_info() -> eyre::Result<()> {
        let (_harness, client) = setup().await?;

        let bundle = create_bundle(vec![]);
        let response: Result<MeterBundleResponse, _> =
            client.request("base_meterBundle", (bundle,)).await;

        assert!(response.is_err());

        Ok(())
    }

    /// Flashblock pending state must not be used for `meter_bundle`.
    ///
    /// Overlaying lagged pending state caused expensive database reads that
    /// stalled RPC nodes. Canonical state is the source of truth.
    #[tokio::test]
    async fn test_meter_bundle_ignores_flashblock_pending_state() -> eyre::Result<()> {
        let flashblocks_config =
            FlashblocksConfig::new(Url::parse("ws://localhost:12345").unwrap(), 10);
        let flashblocks_state = Arc::clone(&flashblocks_config.state);

        let harness = TestHarness::builder()
            .with_ext::<MeteringExtension>(MeteringConfig::with_flashblocks(flashblocks_config))
            .build()
            .await?;
        let client = harness.rpc_client()?;

        harness
            .build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await)
            .await?;

        let flashblock_header = Header {
            number: 2,
            timestamp: 1_700_000_001,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let sealed_header = flashblock_header.seal(B256::ZERO);

        let flashblock = Flashblock {
            payload_id: Default::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Default::default(),
                prev_randao: B256::ZERO,
                block_number: 2,
                gas_limit: 30_000_000,
                timestamp: 1_700_000_001,
                extra_data: Default::default(),
                base_fee_per_gas: alloy_primitives::U256::from(1_000_000_000u64),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 0,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: Some(0),
            },
            metadata: Metadata::new(2),
        };

        let mut builder = PendingBlocksBuilder::new();
        builder.with_header(sealed_header);
        builder.with_flashblocks([flashblock]);
        flashblocks_state.set_pending_blocks_for_testing(Some(builder.build()?));

        let bundle = create_bundle(vec![]);
        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.state_block_number, 1);

        Ok(())
    }

    // === Priority Fee Estimation RPC Tests (PR 1a) ===

    async fn setup_with_estimator() -> eyre::Result<(TestHarness, RpcClient)> {
        let config = MeteringConfig::enabled()
            .with_resource_limits(MeteringResourceLimits {
                gas_limit: Some(30_000_000),
                da_bytes: Some(1_000_000),
            })
            .with_target_flashblocks_per_block(4);
        let harness = TestHarness::builder().with_ext::<MeteringExtension>(config).build().await?;
        let client = harness.rpc_client()?;
        Ok((harness, client))
    }

    #[test]
    fn compute_resource_demand_preserves_gas_and_da_dimensions() {
        let tx = Bytes::from_static(&[0x02, 0x01, 0x02, 0x03]);
        let bundle = create_bundle(vec![tx.clone()]);
        let meter_result = MeterBundleResponse { total_gas_used: 21_000, ..Default::default() };

        let demand = compute_resource_demand(&bundle, &meter_result);

        assert_eq!(demand.gas_used, Some(21_000));
        assert_eq!(demand.data_availability_bytes, Some(flz_compress_len(&tx) as u64));
    }

    #[tokio::test]
    async fn test_metered_priority_fee_per_gas_empty_cache_returns_error() -> eyre::Result<()> {
        let (harness, client) = setup_with_estimator().await?;

        harness
            .build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await)
            .await?;

        let bundle = create_bundle(vec![]);

        let result: Result<serde_json::Value, _> =
            client.request("base_meteredPriorityFeePerGas", (bundle,)).await;

        // Should error because the metering cache is empty
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("No metering data available"));

        Ok(())
    }

    #[tokio::test]
    async fn test_metered_priority_fee_per_gas_empty_cache_with_tx_returns_error()
    -> eyre::Result<()> {
        let (harness, client) = setup_with_estimator().await?;

        harness
            .build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await)
            .await?;

        let sender_secret = Account::Alice.signer_b256();

        let tx = TransactionBuilder::default()
            .signer(sender_secret)
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(address!("0x1111111111111111111111111111111111111111"))
            .value(1000)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1_000_000_000)
            .into_eip1559();

        let signed_tx =
            BaseTransactionSigned::Eip1559(tx.as_eip1559().expect("eip1559 transaction").clone());
        let envelope: BaseTxEnvelope = signed_tx;
        let tx_bytes = Bytes::from(envelope.encoded_2718());

        let bundle = create_bundle(vec![tx_bytes]);

        let result: Result<serde_json::Value, _> =
            client.request("base_meteredPriorityFeePerGas", (bundle,)).await;

        // Should error because the metering cache is empty
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("No metering data available"));

        Ok(())
    }

    #[tokio::test]
    async fn test_metered_priority_fee_per_gas_no_estimator_returns_error() -> eyre::Result<()> {
        // Use setup() which doesn't configure resource limits (no estimator)
        let (_harness, client) = setup().await?;

        let bundle = create_bundle(vec![]);

        let result: Result<serde_json::Value, _> =
            client.request("base_meteredPriorityFeePerGas", (bundle,)).await;

        // Should error because no estimator is configured
        assert!(result.is_err());

        Ok(())
    }
}
