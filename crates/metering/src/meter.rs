use alloy_consensus::{transaction::SignerRecoverable, BlockHeader, Transaction as _};
use alloy_primitives::{B256, U256};
use eyre::{eyre, Result as EyreResult};
use reth_primitives_traits::SealedHeader;
use reth::revm::db::State;
use reth_evm::execute::BlockBuilder;
use reth_evm::ConfigureEvm;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use std::sync::Arc;
use std::time::Instant;

use crate::TransactionResult;

const BLOCK_TIME: u64 = 2; // 2 seconds per block

/// Simulates and meters a bundle of transactions
///
/// Takes a state provider, chain spec, decoded transactions, block header, and optional timestamp,
/// and executes them in sequence to measure gas usage and execution time.
///
/// Returns a tuple of:
/// - Vector of transaction results
/// - Total gas used
/// - Total gas fees paid
/// - Bundle hash
pub fn meter_bundle<SP>(
    state_provider: SP,
    chain_spec: Arc<OpChainSpec>,
    decoded_txs: Vec<op_alloy_consensus::OpTxEnvelope>,
    header: &SealedHeader,
    timestamp: Option<u64>,
) -> EyreResult<(Vec<TransactionResult>, u64, U256, B256)>
where
    SP: reth_provider::StateProvider,
{
    // Calculate bundle hash
    let tx_hashes: Vec<B256> = decoded_txs.iter().map(|tx| tx.tx_hash()).collect();
    let bundle_hash = calculate_bundle_hash(&tx_hashes);

    // Create state database
    let state_db = reth::revm::database::StateProviderDatabase::new(state_provider);
    let mut db = State::builder()
        .with_database(state_db)
        .with_bundle_update()
        .build();

    // Set up next block attributes
    let timestamp = timestamp.unwrap_or_else(|| header.timestamp() + BLOCK_TIME);
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

    {
        let evm_config = OpEvmConfig::optimism(chain_spec);
        let mut builder = evm_config.builder_for_next_block(&mut db, header, attributes)?;

        builder.apply_pre_execution_changes()?;

        for tx in decoded_txs {
            let tx_start = Instant::now();
            let tx_hash = tx.tx_hash();
            let from = tx.recover_signer()?;
            let to = tx.to();
            let value = tx.value();
            let gas_price = tx.max_fee_per_gas();

            let recovered_tx =
                alloy_consensus::transaction::Recovered::new_unchecked(tx.clone(), from);

            let gas_used = builder
                .execute_transaction(recovered_tx)
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

    Ok((results, total_gas_used, total_gas_fees, bundle_hash))
}

/// Calculate bundle hash using Flashbots methodology
///
/// Concatenates all transaction hashes and computes a single keccak256 hash:
/// `keccak256(concat(tx_hashes))`
///
/// Reference: <https://github.com/flashbots/ethers-provider-flashbots-bundle/blob/0d404bb041b82c12789bd62b18e218304a095b6f/src/index.ts#L266-L269>
fn calculate_bundle_hash(tx_hashes: &[B256]) -> B256 {
    use alloy_primitives::keccak256;
    let mut combined = Vec::with_capacity(tx_hashes.len() * 32);
    for hash in tx_hashes {
        combined.extend_from_slice(hash.as_slice());
    }
    keccak256(&combined)
}
