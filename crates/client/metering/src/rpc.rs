//! Implementation of the metering RPC API.

use std::sync::Arc;

use alloy_consensus::{BlockHeader, Header, Sealed};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, U256};
use base_bundles::{Bundle, MeterBundleResponse, ParsedBundle};
use base_flashblocks::{FlashblocksAPI, PendingBlocksAPI};
use jsonrpsee::core::{RpcResult, async_trait};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::OpBlock;
use reth_primitives_traits::SealedHeader;
use reth_provider::{
    BlockReader, BlockReaderIdExt, ChainSpecProvider, HeaderProvider, StateProviderFactory,
};
use reth_optimism_evm::extract_l1_info_from_tx;
use tracing::{error, info};

use crate::{
    MeterBlockResponse, PendingState, PendingTrieCache, block::meter_block, meter::meter_bundle,
    traits::MeteringApiServer,
};

/// Implementation of the metering RPC API.
#[derive(Debug)]
pub struct MeteringApiImpl<Provider, FB> {
    provider: Provider,
    flashblocks_api: Arc<FB>,
    /// Cache for pending trie input, ensuring each bundle's state root
    /// calculation only measures the bundle's incremental I/O.
    pending_trie_cache: PendingTrieCache,
}

impl<Provider, FB> MeteringApiImpl<Provider, FB>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + BlockReader<Block = OpBlock>
        + HeaderProvider<Header = Header>
        + Clone,
    FB: FlashblocksAPI,
{
    /// Creates a new instance of MeteringApi.
    pub fn new(provider: Provider, flashblocks_api: Arc<FB>) -> Self {
        Self { provider, flashblocks_api, pending_trie_cache: PendingTrieCache::new() }
    }
}

#[async_trait]
impl<Provider, FB> MeteringApiServer for MeteringApiImpl<Provider, FB>
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
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse> {
        info!(
            num_transactions = &bundle.txs.len(),
            block_number = &bundle.block_number,
            "Starting bundle metering"
        );

        // Get pending blocks from flashblocks API
        let pending_blocks = self.flashblocks_api.get_pending_blocks();

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
        let state_provider =
            self.provider.state_by_block_number_or_tag(canonical_block_number).map_err(|e| {
                error!(error = %e, "Failed to get state provider");
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InternalError.code(),
                    format!("Failed to get state provider: {}", e),
                    None::<()>,
                )
            })?;

        // Get the flashblock index if we have pending blocks
        let state_flashblock_index = pending_blocks.as_ref().map(|pb| pb.latest_flashblock_index());

        // If we have pending blocks, extract the pending state for metering
        let pending_state = if let Some(pb) = pending_blocks.as_ref() {
            let bundle_state = pb.get_bundle_state();

            // Build a temporary PendingState without trie_input to get the cached trie
            let temp_state = PendingState { bundle_state: bundle_state.clone(), trie_input: None };

            // Ensure the pending trie input is cached for reuse across bundle simulations
            let fb_index = state_flashblock_index.unwrap();
            let trie_input = self
                .pending_trie_cache
                .ensure_cached(header.hash(), fb_index, &temp_state, &*state_provider)
                .map_err(|e| {
                    error!(error = %e, "Failed to cache pending trie input");
                    jsonrpsee::types::ErrorObjectOwned::owned(
                        jsonrpsee::types::ErrorCode::InternalError.code(),
                        format!("Failed to cache pending trie input: {}", e),
                        None::<()>,
                    )
                })?;

            Some(PendingState { bundle_state, trie_input: Some(trie_input) })
        } else {
            None
        };

        // Pending flashblock headers can omit parent_beacon_block_root; prefer the CL-provided
        // value from the flashblock base payload when available, otherwise fall back to the header.
        let parent_beacon_block_root = header.parent_beacon_block_root().or_else(|| {
            pending_blocks.as_ref().and_then(|pb| {
                pb.get_flashblocks()
                    .first()
                    .and_then(|fb| fb.base.as_ref().map(|base| base.parent_beacon_block_root))
            })
        });

        let first_tx = self.provider
            .block_by_hash(header.hash())
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
                    format!("Block not found: {}", header.hash()),
                    None::<()>,
                )
            })?
            .body
            .transactions
            .first()
            .ok_or_else(|| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::ErrorCode::InvalidParams.code(),
                    format!("Block has no transactions: {}", header.hash()),
                    None::<()>,
                )
            })?
            .clone();
        let l1_block_info = extract_l1_info_from_tx(&first_tx).map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::ErrorCode::InvalidParams.code(),
                format!("Failed to extract L1 block info from transaction: {}", e),
                None::<()>,
            )
        })?;

        // Meter bundle using utility function
        let output = meter_bundle(
            state_provider,
            self.provider.chain_spec(),
            parsed_bundle,
            &header,
            parent_beacon_block_root,
            pending_state,
            l1_block_info,
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
        let bundle_gas_price = if output.total_gas_used > 0 {
            output.total_gas_fees / U256::from(output.total_gas_used)
        } else {
            U256::from(0)
        };

        info!(
            bundle_hash = %output.bundle_hash,
            num_transactions = output.results.len(),
            total_gas_used = output.total_gas_used,
            total_time_us = output.total_time_us,
            state_block_number = header.number,
            flashblock_index = flashblock_index,
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
            state_flashblock_index,
            total_gas_used: output.total_gas_used,
            total_execution_time_us: output.total_time_us,
            state_root_time_us: output.state_root_time_us,
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
}

impl<Provider, FB> MeteringApiImpl<Provider, FB>
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
    FB: FlashblocksAPI + Send + Sync + 'static,
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
    use base_client_node::test_utils::{Account, TestHarness};
    use op_alloy_consensus::OpTxEnvelope;
    use reth_optimism_primitives::OpTransactionSigned;
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;
    use crate::{MeteringConfig, MeteringExtension};

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
                .clone()
                .into_encoded_bytes()
        ]
    }

    #[tokio::test]
    async fn test_meter_bundle_empty() -> eyre::Result<()> {
        let (harness, client) = setup().await?;

        // Build a block with a tx so that we don't get an error about missing L1 block info
        harness.build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await).await?;

        let bundle = create_bundle(vec![], 0, None);

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

        harness.build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await).await?;

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
            OpTransactionSigned::Eip1559(tx.as_eip1559().expect("eip1559 transaction").clone());
        let envelope: OpTxEnvelope = signed_tx;

        let tx_bytes = Bytes::from(envelope.encoded_2718());

        let bundle = create_bundle(vec![tx_bytes], 0, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.total_gas_used, 21_000);
        assert!(response.total_execution_time_us > 0);

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

        harness.build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await).await?;

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

        let tx1_signed = OpTransactionSigned::Eip1559(
            tx1_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let tx1_envelope: OpTxEnvelope = tx1_signed;
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

        let tx2_signed = OpTransactionSigned::Eip1559(
            tx2_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let tx2_envelope: OpTxEnvelope = tx2_signed;
        let tx2_bytes = Bytes::from(tx2_envelope.encoded_2718());

        let bundle = create_bundle(vec![tx1_bytes, tx2_bytes], 0, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 2);
        assert_eq!(response.total_gas_used, 42_000);
        assert!(response.total_execution_time_us > 0);

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
        let (harness, client) = setup().await?;
        harness.build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await).await?;

        let bundle = create_bundle(vec![], 1, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.state_block_number, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_ignores_bundle_block_number() -> eyre::Result<()> {
        let (harness, client) = setup().await?;
        harness.build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await).await?;

        let bundle1 = create_bundle(vec![], 1, None);
        let response1: MeterBundleResponse = client.request("base_meterBundle", (bundle1,)).await?;

        let bundle2 = create_bundle(vec![], 999, None);
        let response2: MeterBundleResponse = client.request("base_meterBundle", (bundle2,)).await?;

        assert_eq!(response1.state_block_number, response2.state_block_number);
        assert_eq!(response1.state_block_number, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_custom_timestamp() -> eyre::Result<()> {
        let (harness, client) = setup().await?;
        harness.build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await).await?;

        let custom_timestamp = 1234567890;
        let bundle = create_bundle(vec![], 0, Some(custom_timestamp));

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.results.len(), 0);
        assert_eq!(response.total_gas_used, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_arbitrary_block_number() -> eyre::Result<()> {
        let (harness, client) = setup().await?;
        harness.build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await).await?;

        let bundle = create_bundle(vec![], 999999, None);

        let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;

        assert_eq!(response.state_block_number, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_meter_bundle_gas_calculations() -> eyre::Result<()> {
        let (harness, client) = setup().await?;
        harness.build_block_from_transactions(generate_txs_for_block(harness.chain_id()).await).await?;

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

        let signed_tx1 = OpTransactionSigned::Eip1559(
            tx1_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let envelope1: OpTxEnvelope = signed_tx1;
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

        let signed_tx2 = OpTransactionSigned::Eip1559(
            tx2_inner.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let envelope2: OpTxEnvelope = signed_tx2;
        let tx2_bytes = Bytes::from(envelope2.encoded_2718());

        let bundle = create_bundle(vec![tx1_bytes, tx2_bytes], 0, None);

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
}
