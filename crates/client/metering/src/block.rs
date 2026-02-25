//! Block metering logic.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Header, transaction::SignerRecoverable};
use alloy_primitives::B256;
use eyre::{Result as EyreResult, eyre};
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_primitives::OpBlock;
use reth_primitives_traits::Block as BlockT;
use reth_provider::{HeaderProvider, StateProviderFactory};
use reth_revm::{database::StateProviderDatabase, db::State};

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
    let transactions = block.body().transactions();

    // Get parent header
    let parent_hash = block.header().parent_hash();
    let parent_header = provider
        .sealed_header_by_hash(parent_hash)?
        .ok_or_else(|| eyre!("Parent header not found: {}", parent_hash))?;

    // Get state provider at parent block
    let state_provider = provider.state_by_block_hash(parent_hash)?;

    // Create state database from parent state
    let state_db = StateProviderDatabase::new(&state_provider);
    let mut db = State::builder().with_database(state_db).with_bundle_update().build();

    // Set up block attributes from the actual block header
    let attributes = OpNextBlockEnvAttributes {
        timestamp: block.header().timestamp(),
        suggested_fee_recipient: block.header().beneficiary(),
        prev_randao: block.header().mix_hash().unwrap_or_else(B256::random),
        gas_limit: block.header().gas_limit(),
        parent_beacon_block_root: block.header().parent_beacon_block_root(),
        extra_data: block.header().extra_data().clone(),
    };

    // Recover signers first (this can be parallelized in production)
    let signer_recovery_start = Instant::now();
    let recovered_transactions: Vec<_> = transactions
        .map(|tx| {
            let tx_hash = tx.tx_hash();
            let signer = tx
                .recover_signer()
                .map_err(|e| eyre!("Failed to recover signer for tx {}: {}", tx_hash, e))?;
            Ok(alloy_consensus::transaction::Recovered::new_unchecked(tx.clone(), signer))
        })
        .collect::<EyreResult<Vec<_>>>()?;
    let tx_count = recovered_transactions.len();
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
    let hashed_state = state_provider.hashed_post_state(&db.bundle_state);
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

#[cfg(test)]
mod tests {
    use alloy_consensus::TxEip1559;
    use alloy_primitives::{Address, Signature};
    use base_client_node::test_utils::{Account, TestHarness};
    use reth_optimism_primitives::{OpBlockBody, OpTransactionSigned};
    use reth_primitives_traits::Block as _;
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::BlockMeter;

    fn create_block_with_transactions(
        harness: &TestHarness,
        transactions: Vec<OpTransactionSigned>,
    ) -> OpBlock {
        let latest = harness.latest_block();
        let header = Header {
            parent_hash: latest.hash(),
            number: latest.number() + 1,
            timestamp: latest.timestamp() + 2,
            gas_limit: 30_000_000,
            beneficiary: Address::random(),
            base_fee_per_gas: Some(1),
            // Required for post-Cancun blocks (EIP-4788)
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        };

        let body = OpBlockBody { transactions, ommers: vec![], withdrawals: None };

        OpBlock::new(header, body)
    }

    #[tokio::test]
    async fn meter_block_empty_transactions() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let block = create_block_with_transactions(&harness, vec![]);

        let response = meter_block(harness.blockchain_provider(), harness.chain_spec(), &block)?;

        assert_eq!(response.block_hash, block.header().hash_slow());
        assert_eq!(response.block_number, block.header().number());
        assert!(response.transactions.is_empty());
        // No transactions means minimal signer recovery time (just timing overhead)
        assert!(
            response.execution_time_us > 0,
            "execution time should be non-zero due to EVM setup"
        );
        assert!(response.state_root_time_us > 0, "state root time should be non-zero");
        assert_eq!(
            response.total_time_us,
            response.signer_recovery_time_us
                + response.execution_time_us
                + response.state_root_time_us
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_block_single_transaction() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

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

        let block = create_block_with_transactions(&harness, vec![tx]);

        let response = meter_block(harness.blockchain_provider(), harness.chain_spec(), &block)?;

        assert_eq!(response.block_hash, block.header().hash_slow());
        assert_eq!(response.block_number, block.header().number());
        assert_eq!(response.transactions.len(), 1);

        let metered_tx = &response.transactions[0];
        assert_eq!(metered_tx.tx_hash, tx_hash);
        assert_eq!(metered_tx.gas_used, 21_000);
        assert!(metered_tx.execution_time_us > 0, "transaction execution time should be non-zero");

        assert!(response.signer_recovery_time_us > 0, "signer recovery should take time");
        assert!(response.execution_time_us > 0);
        assert!(response.state_root_time_us > 0);
        assert_eq!(
            response.total_time_us,
            response.signer_recovery_time_us
                + response.execution_time_us
                + response.state_root_time_us
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_block_multiple_transactions() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let to_1 = Address::random();
        let to_2 = Address::random();

        // Create first transaction from Alice
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
        let tx_hash_1 = tx_1.tx_hash();

        // Create second transaction from Bob
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
        let tx_hash_2 = tx_2.tx_hash();

        let block = create_block_with_transactions(&harness, vec![tx_1, tx_2]);

        let response = meter_block(harness.blockchain_provider(), harness.chain_spec(), &block)?;

        assert_eq!(response.block_hash, block.header().hash_slow());
        assert_eq!(response.block_number, block.header().number());
        assert_eq!(response.transactions.len(), 2);

        // Check first transaction
        let metered_tx_1 = &response.transactions[0];
        assert_eq!(metered_tx_1.tx_hash, tx_hash_1);
        assert_eq!(metered_tx_1.gas_used, 21_000);
        assert!(metered_tx_1.execution_time_us > 0);

        // Check second transaction
        let metered_tx_2 = &response.transactions[1];
        assert_eq!(metered_tx_2.tx_hash, tx_hash_2);
        assert_eq!(metered_tx_2.gas_used, 21_000);
        assert!(metered_tx_2.execution_time_us > 0);

        // Check aggregate times
        assert!(response.signer_recovery_time_us > 0, "signer recovery should take time");
        assert!(response.execution_time_us > 0);
        assert!(response.state_root_time_us > 0);
        assert_eq!(
            response.total_time_us,
            response.signer_recovery_time_us
                + response.execution_time_us
                + response.state_root_time_us
        );

        // Ensure individual transaction times are consistent with total
        let individual_times: u128 =
            response.transactions.iter().map(|t| t.execution_time_us).sum();
        assert!(
            individual_times <= response.execution_time_us,
            "sum of individual times should not exceed total (due to EVM overhead)"
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_block_timing_consistency() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        // Create a block with one transaction
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(Address::random())
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx = OpTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let block = create_block_with_transactions(&harness, vec![tx]);

        let response = meter_block(harness.blockchain_provider(), harness.chain_spec(), &block)?;

        // Verify timing invariants
        assert!(response.signer_recovery_time_us > 0, "signer recovery time must be positive");
        assert!(response.execution_time_us > 0, "execution time must be positive");
        assert!(response.state_root_time_us > 0, "state root time must be positive");
        assert_eq!(
            response.total_time_us,
            response.signer_recovery_time_us
                + response.execution_time_us
                + response.state_root_time_us,
            "total time must equal signer recovery + execution + state root times"
        );

        Ok(())
    }

    // ============================================================================
    // Error Path Tests
    // ============================================================================

    #[tokio::test]
    async fn meter_block_parent_header_not_found() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();

        // Create a block that references a non-existent parent
        let fake_parent_hash = B256::random();
        let header = Header {
            parent_hash: fake_parent_hash, // This parent doesn't exist
            number: 999,
            timestamp: latest.timestamp() + 2,
            gas_limit: 30_000_000,
            beneficiary: Address::random(),
            base_fee_per_gas: Some(1),
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        };

        let body = OpBlockBody { transactions: vec![], ommers: vec![], withdrawals: None };
        let block = OpBlock::new(header, body);

        let result = meter_block(harness.blockchain_provider(), harness.chain_spec(), &block);

        assert!(result.is_err(), "should fail when parent header is not found");
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("Parent header not found") || err_str.contains("not found"),
            "error should indicate parent header not found: {err_str}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_block_invalid_transaction_signature() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        // Create a transaction with an invalid signature
        let tx = TxEip1559 {
            chain_id: harness.chain_id(),
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 10,
            max_priority_fee_per_gas: 1,
            to: alloy_primitives::TxKind::Call(Address::random()),
            value: alloy_primitives::U256::from(1000),
            access_list: Default::default(),
            input: Default::default(),
        };

        // Create a signature with invalid values (all zeros is invalid for secp256k1)
        let invalid_signature =
            Signature::new(alloy_primitives::U256::ZERO, alloy_primitives::U256::ZERO, false);

        let signed_tx =
            alloy_consensus::Signed::new_unchecked(tx, invalid_signature, B256::random());
        let op_tx = OpTransactionSigned::Eip1559(signed_tx);

        let block = create_block_with_transactions(&harness, vec![op_tx]);

        let result = meter_block(harness.blockchain_provider(), harness.chain_spec(), &block);

        assert!(result.is_err(), "should fail when transaction has invalid signature");
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("recover signer") || err_str.contains("signature"),
            "error should indicate signer recovery failure: {err_str}"
        );

        Ok(())
    }
}
