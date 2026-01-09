//! Block metering logic.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Header, transaction::SignerRecoverable};
use alloy_primitives::B256;
use eyre::{Result as EyreResult, eyre};
use reth::revm::db::State;
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_primitives::OpBlock;
use reth_primitives_traits::Block as BlockT;
use reth_provider::{HeaderProvider, StateProviderFactory};

use crate::types::{MeterBlockResponse, MeterBlockTransactions};

/// Re-executes a block and meters execution time, state root calculation time, and total time.
///
/// Takes a provider, the chain spec, and the block to meter.
///
/// Returns `MeterBlockResponse` containing:
/// - Block hash
/// - Signer recovery time (can be parallelized)
/// - EVM execution time for all transactions
/// - State root calculation time
/// - Total time
/// - Per-transaction timing information
///
/// # Note
///
/// If the parent block's state has been pruned, this function will return an error.
///
/// State root calculation timing is most accurate for recent blocks where state tries are
/// cached. For older blocks, trie nodes may not be cached, which can significantly inflate
/// the `state_root_time_us` value.
pub fn meter_block<P>(
    provider: P,
    chain_spec: Arc<OpChainSpec>,
    block: &OpBlock,
) -> EyreResult<MeterBlockResponse>
where
    P: StateProviderFactory + HeaderProvider<Header = Header>,
{
    let block_hash = block.header().hash_slow();
    let block_number = block.header().number();
    let transactions: Vec<_> = block.body().transactions().cloned().collect();
    let tx_count = transactions.len();

    // Get parent header
    let parent_hash = block.header().parent_hash();
    let parent_header = provider
        .sealed_header_by_hash(parent_hash)?
        .ok_or_else(|| eyre!("Parent header not found: {}", parent_hash))?;

    // Get state provider at parent block
    let state_provider = provider.state_by_block_hash(parent_hash)?;

    // Create state database from parent state
    let state_db = reth::revm::database::StateProviderDatabase::new(&state_provider);
    let mut db = State::builder().with_database(state_db).with_bundle_update().build();

    // Set up block attributes from the actual block header
    let attributes = OpNextBlockEnvAttributes {
        timestamp: block.header().timestamp(),
        suggested_fee_recipient: block.header().beneficiary(),
        prev_randao: block.header().mix_hash().unwrap_or(B256::random()),
        gas_limit: block.header().gas_limit(),
        parent_beacon_block_root: block.header().parent_beacon_block_root(),
        extra_data: block.header().extra_data().clone(),
    };

    // Recover signers first (this can be parallelized in production)
    let signer_recovery_start = Instant::now();
    let recovered_transactions: Vec<_> = transactions
        .iter()
        .map(|tx| {
            let tx_hash = tx.tx_hash();
            let signer = tx
                .recover_signer()
                .map_err(|e| eyre!("Failed to recover signer for tx {}: {}", tx_hash, e))?;
            Ok(alloy_consensus::transaction::Recovered::new_unchecked(tx.clone(), signer))
        })
        .collect::<EyreResult<Vec<_>>>()?;
    let signer_recovery_time = signer_recovery_start.elapsed().as_micros();

    // Execute transactions and measure time
    let mut transaction_times = Vec::with_capacity(tx_count);

    let evm_start = Instant::now();
    {
        let evm_config = OpEvmConfig::optimism(chain_spec);
        let mut builder = evm_config.builder_for_next_block(&mut db, &parent_header, attributes)?;

        builder.apply_pre_execution_changes()?;

        for recovered_tx in recovered_transactions {
            let tx_start = Instant::now();
            let tx_hash = recovered_tx.tx_hash();

            let gas_used = builder
                .execute_transaction(recovered_tx)
                .map_err(|e| eyre!("Transaction {} execution failed: {}", tx_hash, e))?;

            let execution_time = tx_start.elapsed().as_micros();

            transaction_times.push(MeterBlockTransactions {
                tx_hash,
                gas_used,
                execution_time_us: execution_time,
            });
        }
    }
    let execution_time = evm_start.elapsed().as_micros();

    // Calculate state root and measure time
    let state_root_start = Instant::now();
    let bundle_state = db.bundle_state.clone();
    let hashed_state = state_provider.hashed_post_state(&bundle_state);
    let _state_root = state_provider
        .state_root(hashed_state)
        .map_err(|e| eyre!("Failed to calculate state root: {}", e))?;
    let state_root_time = state_root_start.elapsed().as_micros();

    let total_time = signer_recovery_time + execution_time + state_root_time;

    Ok(MeterBlockResponse {
        block_hash,
        block_number,
        signer_recovery_time_us: signer_recovery_time,
        execution_time_us: execution_time,
        state_root_time_us: state_root_time,
        total_time_us: total_time,
        transactions: transaction_times,
    })
}
