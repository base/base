//! Bundle metering logic.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _, transaction::SignerRecoverable};
use alloy_primitives::{B256, U256};
use base_bundles::{BundleExtensions, BundleTxs, ParsedBundle, TransactionResult};
use eyre::{Result as EyreResult, eyre};
use reth::revm::db::State;
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_primitives_traits::SealedHeader;
use revm_database::states::bundle_state::BundleRetention;

const BLOCK_TIME: u64 = 2; // 2 seconds per block

/// Output from metering a bundle of transactions
#[derive(Debug)]
pub struct MeterBundleOutput {
    /// Transaction results with individual metrics
    pub results: Vec<TransactionResult>,
    /// Total gas used by all transactions
    pub total_gas_used: u64,
    /// Total gas fees paid by all transactions
    pub total_gas_fees: U256,
    /// Bundle hash
    pub bundle_hash: B256,
    /// Total time in microseconds (includes transaction execution and state root calculation)
    pub total_time_us: u128,
    /// State root calculation time in microseconds
    pub state_root_time_us: u128,
}

/// Simulates and meters a bundle of transactions
///
/// Takes a state provider, chain spec, parsed bundle, and block header,
/// then executes transactions in sequence to measure gas usage and execution time.
///
/// Returns [`MeterBundleOutput`] containing transaction results and aggregated metrics,
/// including separate timing for state root calculation.
pub fn meter_bundle<SP>(
    state_provider: SP,
    chain_spec: Arc<OpChainSpec>,
    bundle: ParsedBundle,
    header: &SealedHeader,
) -> EyreResult<MeterBundleOutput>
where
    SP: reth_provider::StateProvider,
{
    // Get bundle hash
    let bundle_hash = bundle.bundle_hash();

    // Create state database
    let state_db = reth::revm::database::StateProviderDatabase::new(state_provider);
    let mut db = State::builder().with_database(state_db).with_bundle_update().build();

    // Set up next block attributes
    // Use bundle.min_timestamp if provided, otherwise use header timestamp + BLOCK_TIME
    let timestamp = bundle.min_timestamp.unwrap_or_else(|| header.timestamp() + BLOCK_TIME);
    let attributes = OpNextBlockEnvAttributes {
        timestamp,
        suggested_fee_recipient: header.beneficiary(),
        prev_randao: header.mix_hash().unwrap_or(B256::random()),
        gas_limit: header.gas_limit(),
        parent_beacon_block_root: header.parent_beacon_block_root(),
        extra_data: header.extra_data().clone(),
    };

    // Execute transactions
    let mut results = Vec::new();
    let mut total_gas_used = 0u64;
    let mut total_gas_fees = U256::ZERO;

    let total_start = Instant::now();
    {
        let evm_config = OpEvmConfig::optimism(chain_spec);
        let mut builder = evm_config.builder_for_next_block(&mut db, header, attributes)?;

        builder.apply_pre_execution_changes()?;

        for tx in bundle.transactions() {
            let tx_start = Instant::now();
            let tx_hash = tx.tx_hash();
            let from = tx.recover_signer()?;
            let to = tx.to();
            let value = tx.value();
            let gas_price = tx.max_fee_per_gas();

            let gas_used = builder
                .execute_transaction(tx.clone())
                .map_err(|e| eyre!("Transaction {} execution failed: {}", tx_hash, e))?;

            let gas_fees = U256::from(gas_used) * U256::from(gas_price);
            total_gas_used = total_gas_used.saturating_add(gas_used);
            total_gas_fees = total_gas_fees.saturating_add(gas_fees);

            let execution_time = tx_start.elapsed().as_micros();

            results.push(TransactionResult {
                coinbase_diff: gas_fees,
                eth_sent_to_coinbase: U256::ZERO,
                from_address: from,
                gas_fees,
                gas_price: U256::from(gas_price),
                gas_used,
                to_address: to,
                tx_hash,
                value,
                execution_time_us: execution_time,
            });
        }
    }

    // Calculate state root and measure its calculation time
    db.merge_transitions(BundleRetention::Reverts);
    let bundle_update = db.take_bundle();
    let state_provider = db.database.as_ref();

    let state_root_start = Instant::now();
    let hashed_state = state_provider.hashed_post_state(&bundle_update);
    let _ = state_provider.state_root_with_updates(hashed_state)?;
    let state_root_time_us = state_root_start.elapsed().as_micros();
    let total_time_us = total_start.elapsed().as_micros();

    Ok(MeterBundleOutput {
        results,
        total_gas_used,
        total_gas_fees,
        bundle_hash,
        total_time_us,
        state_root_time_us,
    })
}

#[cfg(test)]
mod tests {
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Bytes, keccak256};
    use base_bundles::{Bundle, ParsedBundle};
    use base_client_node::test_utils::{Account, TestHarness};
    use eyre::Context;
    use reth_optimism_primitives::OpTransactionSigned;
    use reth_provider::StateProviderFactory;
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;

    fn create_parsed_bundle(txs: Vec<OpTransactionSigned>) -> eyre::Result<ParsedBundle> {
        let txs: Vec<Bytes> = txs.iter().map(|tx| Bytes::from(tx.encoded_2718())).collect();

        let bundle = Bundle {
            txs,
            block_number: 0,
            flashblock_number_min: None,
            flashblock_number_max: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: vec![],
            replacement_uuid: None,
            dropping_tx_hashes: vec![],
        };

        ParsedBundle::try_from(bundle).map_err(|e| eyre::eyre!(e))
    }

    #[tokio::test]
    async fn meter_bundle_empty_transactions() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(Vec::new())?;

        let output = meter_bundle(state_provider, harness.chain_spec(), parsed_bundle, &header)?;

        assert!(output.results.is_empty());
        assert_eq!(output.total_gas_used, 0);
        assert_eq!(output.total_gas_fees, U256::ZERO);
        // Even empty bundles have some EVM setup overhead
        assert!(output.total_time_us > 0);
        assert!(output.state_root_time_us > 0);
        assert_eq!(output.bundle_hash, keccak256([]));

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_single_transaction() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx = OpTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let tx_hash = tx.tx_hash();

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let output = meter_bundle(state_provider, harness.chain_spec(), parsed_bundle, &header)?;

        assert_eq!(output.results.len(), 1);
        let result = &output.results[0];
        assert!(output.total_time_us > 0);
        assert!(output.state_root_time_us > 0);

        assert_eq!(result.from_address, Account::Alice.address());
        assert_eq!(result.to_address, Some(to));
        assert_eq!(result.tx_hash, tx_hash);
        assert_eq!(result.gas_price, U256::from(10));
        assert_eq!(result.gas_used, 21_000);
        assert_eq!(result.coinbase_diff, (U256::from(21_000) * U256::from(10)),);

        assert_eq!(output.total_gas_used, 21_000);
        assert_eq!(output.total_gas_fees, U256::from(21_000) * U256::from(10));

        let mut concatenated = Vec::with_capacity(32);
        concatenated.extend_from_slice(tx_hash.as_slice());
        assert_eq!(output.bundle_hash, keccak256(concatenated));

        assert!(result.execution_time_us > 0, "execution_time_us should be greater than zero");

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_multiple_transactions() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to_1 = Address::random();
        let to_2 = Address::random();

        // Create first transaction
        let signed_tx_1 = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to_1)
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx_1 = OpTransactionSigned::Eip1559(
            signed_tx_1.as_eip1559().expect("eip1559 transaction").clone(),
        );

        // Create second transaction
        let signed_tx_2 = TransactionBuilder::default()
            .signer(Account::Bob.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to_2)
            .value(2_000)
            .gas_limit(21_000)
            .max_fee_per_gas(15)
            .max_priority_fee_per_gas(2)
            .into_eip1559();

        let tx_2 = OpTransactionSigned::Eip1559(
            signed_tx_2.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let tx_hash_1 = tx_1.tx_hash();
        let tx_hash_2 = tx_2.tx_hash();

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx_1, tx_2])?;

        let output = meter_bundle(state_provider, harness.chain_spec(), parsed_bundle, &header)?;

        assert_eq!(output.results.len(), 2);
        assert!(output.total_time_us > 0);
        assert!(output.state_root_time_us > 0);

        // Check first transaction
        let result_1 = &output.results[0];
        assert_eq!(result_1.from_address, Account::Alice.address());
        assert_eq!(result_1.to_address, Some(to_1));
        assert_eq!(result_1.tx_hash, tx_hash_1);
        assert_eq!(result_1.gas_price, U256::from(10));
        assert_eq!(result_1.gas_used, 21_000);
        assert_eq!(result_1.coinbase_diff, (U256::from(21_000) * U256::from(10)),);

        // Check second transaction
        let result_2 = &output.results[1];
        assert_eq!(result_2.from_address, Account::Bob.address());
        assert_eq!(result_2.to_address, Some(to_2));
        assert_eq!(result_2.tx_hash, tx_hash_2);
        assert_eq!(result_2.gas_price, U256::from(15));
        assert_eq!(result_2.gas_used, 21_000);
        assert_eq!(result_2.coinbase_diff, U256::from(21_000) * U256::from(15),);

        // Check aggregated values
        assert_eq!(output.total_gas_used, 42_000);
        let expected_total_fees =
            U256::from(21_000) * U256::from(10) + U256::from(21_000) * U256::from(15);
        assert_eq!(output.total_gas_fees, expected_total_fees);

        // Check bundle hash includes both transactions
        let mut concatenated = Vec::with_capacity(64);
        concatenated.extend_from_slice(tx_hash_1.as_slice());
        concatenated.extend_from_slice(tx_hash_2.as_slice());
        assert_eq!(output.bundle_hash, keccak256(concatenated));

        assert!(result_1.execution_time_us > 0, "execution_time_us should be greater than zero");
        assert!(result_2.execution_time_us > 0, "execution_time_us should be greater than zero");

        Ok(())
    }

    /// Test that state_root_time_us is always <= total_time_us
    #[tokio::test]
    async fn meter_bundle_state_root_time_invariant() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx = OpTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let output = meter_bundle(state_provider, harness.chain_spec(), parsed_bundle, &header)?;

        // Verify invariant: total time must include state root time
        assert!(
            output.total_time_us >= output.state_root_time_us,
            "total_time_us ({}) should be >= state_root_time_us ({})",
            output.total_time_us,
            output.state_root_time_us
        );

        // State root time should be non-zero
        assert!(output.state_root_time_us > 0, "state_root_time_us should be greater than zero");

        Ok(())
    }
}
