use std::{sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _, transaction::SignerRecoverable};
use alloy_primitives::{B256, U256};
use eyre::{Result as EyreResult, eyre};
use reth::revm::db::State;
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_primitives_traits::SealedHeader;
use tips_core::types::{BundleExtensions, BundleTxs, ParsedBundle};

use crate::TransactionResult;

const BLOCK_TIME: u64 = 2; // 2 seconds per block

/// Simulates and meters a bundle of transactions
///
/// Takes a state provider, chain spec, decoded transactions, block header, and bundle metadata,
/// and executes transactions in sequence to measure gas usage and execution time.
///
/// Returns a tuple of:
/// - Vector of transaction results
/// - Total gas used
/// - Total gas fees paid
/// - Bundle hash
/// - Total execution time in microseconds
pub fn meter_bundle<SP>(
    state_provider: SP,
    chain_spec: Arc<OpChainSpec>,
    bundle: ParsedBundle,
    header: &SealedHeader,
) -> EyreResult<(Vec<TransactionResult>, u64, U256, B256, u128)>
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

    let execution_start = Instant::now();
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
                coinbase_diff: gas_fees.to_string(),
                eth_sent_to_coinbase: "0".to_string(),
                from_address: from,
                gas_fees: gas_fees.to_string(),
                gas_price: gas_price.to_string(),
                gas_used,
                to_address: to,
                tx_hash,
                value: value.to_string(),
                execution_time_us: execution_time,
            });
        }
    }
    let total_execution_time = execution_start.elapsed().as_micros();

    Ok((results, total_gas_used, total_gas_fees, bundle_hash, total_execution_time))
}
