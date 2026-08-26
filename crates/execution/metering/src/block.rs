//! Block metering logic.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Header, transaction::SignerRecoverable};
use alloy_primitives::B256;
use base_common_consensus::BaseBlock;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use eyre::{Result as EyreResult, eyre};
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_execution_cache::{CachedStateProvider, ExecutionCache};
use reth_primitives_traits::Block as BlockT;
use reth_provider::{HeaderProvider, StateProvider, StateProviderFactory};
use reth_revm::{database::StateProviderDatabase, db::State};

use crate::{
    MeterBlockResponse, MeterBlockTransactions, MeterStateProviderStats, MeteredStateProvider,
};

impl MeterStateProviderStats {
    /// Returns the saturating difference from an earlier cumulative snapshot.
    pub const fn saturating_sub(self, earlier: Self) -> Self {
        Self {
            account_fetches: self.account_fetches.saturating_sub(earlier.account_fetches),
            account_fetch_time_us: self
                .account_fetch_time_us
                .saturating_sub(earlier.account_fetch_time_us),
            storage_fetches: self.storage_fetches.saturating_sub(earlier.storage_fetches),
            storage_fetch_time_us: self
                .storage_fetch_time_us
                .saturating_sub(earlier.storage_fetch_time_us),
            code_fetches: self.code_fetches.saturating_sub(earlier.code_fetches),
            code_fetch_time_us: self.code_fetch_time_us.saturating_sub(earlier.code_fetch_time_us),
            code_fetched_bytes: self.code_fetched_bytes.saturating_sub(earlier.code_fetched_bytes),
        }
    }
}

/// Re-executes a block and meters execution and timing information.
///
/// Takes a provider, the chain spec, and the block to meter.
///
/// Returns `MeterBlockResponse` containing:
/// - Block hash
/// - Signer recovery time (can be parallelized)
/// - EVM execution time for all transactions
/// - Total time
/// - Per-transaction timing information
///
/// # Note
///
/// If the parent block's state has been pruned, this function will return an error.
///
pub fn meter_block<P>(
    provider: P,
    chain_spec: Arc<BaseChainSpec>,
    block: &BaseBlock,
) -> EyreResult<MeterBlockResponse>
where
    P: StateProviderFactory + HeaderProvider<Header = Header>,
{
    meter_block_with_optional_cache(provider, chain_spec, block, None)
}

/// Re-executes a block using an optional prepopulated parent-state execution cache.
pub fn meter_block_with_optional_cache<P>(
    provider: P,
    chain_spec: Arc<BaseChainSpec>,
    block: &BaseBlock,
    cache: Option<ExecutionCache>,
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
    let state_provider = MeteredStateProvider::new(provider.state_by_block_hash(parent_hash)?);
    let cached_state_provider =
        cache.map(|cache| CachedStateProvider::new(&state_provider, cache, None));
    let execution_state_provider: &dyn StateProvider =
        cached_state_provider.as_ref().map_or(&state_provider, |provider| provider);

    // Create state database from parent state
    let state_db = StateProviderDatabase::new(execution_state_provider);
    let mut db = State::builder().with_database(state_db).with_bundle_update().build();

    // Set up block attributes from the actual block header
    let attributes = BaseNextBlockEnvAttributes {
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
        let evm_config = BaseEvmConfig::base(chain_spec);
        let mut builder = evm_config.builder_for_next_block(&mut db, &parent_header, attributes)?;

        builder.apply_pre_execution_changes()?;
        // Keep pre-execution accesses in block totals, but do not attribute them to the first
        // transaction.
        state_provider.take_accesses();

        for recovered_tx in recovered_transactions {
            let tx_hash = recovered_tx.tx_hash();
            let state_provider_before = state_provider.stats();
            let tx_start = Instant::now();

            let gas_used = builder
                .execute_transaction(recovered_tx)
                .map_err(|e| eyre!("Transaction {} execution failed: {}", tx_hash, e))?
                .tx_gas_used();

            let execution_time = tx_start.elapsed().as_micros();
            let (state_provider_accounts, state_provider_code_hashes) =
                state_provider.take_accesses();

            transaction_times.push(MeterBlockTransactions {
                tx_hash,
                gas_used,
                execution_time_us: execution_time,
                state_provider: state_provider.stats().saturating_sub(state_provider_before),
                state_provider_accounts,
                state_provider_code_hashes,
            });
        }
    }
    let execution_time = evm_start.elapsed().as_micros();

    let total_time = signer_recovery_time + execution_time;

    Ok(MeterBlockResponse {
        block_hash,
        block_number,
        signer_recovery_time_us: signer_recovery_time,
        execution_time_us: execution_time,
        // Retained as a zero-valued compatibility field for older profiling clients.
        state_root_time_us: 0,
        total_time_us: total_time,
        state_provider: state_provider.stats(),
        transactions: transaction_times,
    })
}

#[cfg(test)]
mod tests {
    use alloy_consensus::TxEip1559;
    use alloy_primitives::{Address, Signature, U256};
    use alloy_sol_types::SolCall;
    use base_common_consensus::{BaseBlockBody, BaseTransactionSigned};
    use base_node_runner::test_utils::TestHarness;
    use base_test_utils::{Account, SimpleStorage};
    use reth_primitives_traits::Block as _;
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;

    fn create_block_with_transactions(
        harness: &TestHarness,
        transactions: Vec<BaseTransactionSigned>,
    ) -> BaseBlock {
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

        let body = BaseBlockBody { transactions, ommers: vec![], withdrawals: None };

        BaseBlock::new(header, body)
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
        assert_eq!(response.state_root_time_us, 0);
        assert_eq!(
            response.total_time_us,
            response.signer_recovery_time_us + response.execution_time_us
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

        let tx = BaseTransactionSigned::Eip1559(
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
        assert!(
            metered_tx.state_provider.account_fetches > 0,
            "transaction should fetch accounts from the parent state"
        );
        assert!(
            !metered_tx.state_provider_accounts.is_empty(),
            "transaction should report fetched account addresses"
        );
        assert_eq!(
            metered_tx
                .state_provider_accounts
                .iter()
                .map(|access| access.account_fetches)
                .sum::<u64>(),
            metered_tx.state_provider.account_fetches
        );
        assert_eq!(
            metered_tx
                .state_provider_accounts
                .iter()
                .map(|access| access.account_fetch_time_us)
                .sum::<u128>(),
            metered_tx.state_provider.account_fetch_time_us
        );
        assert_eq!(
            metered_tx
                .state_provider_accounts
                .iter()
                .map(|access| access.storage_fetches)
                .sum::<u64>(),
            metered_tx.state_provider.storage_fetches
        );
        assert_eq!(
            metered_tx
                .state_provider_accounts
                .iter()
                .map(|access| access.storage_fetch_time_us)
                .sum::<u128>(),
            metered_tx.state_provider.storage_fetch_time_us
        );
        assert_eq!(
            metered_tx.state_provider_code_hashes.iter().map(|access| access.fetches).sum::<u64>(),
            metered_tx.state_provider.code_fetches
        );
        assert_eq!(
            metered_tx
                .state_provider_code_hashes
                .iter()
                .map(|access| access.fetch_time_us)
                .sum::<u128>(),
            metered_tx.state_provider.code_fetch_time_us
        );
        assert_eq!(
            metered_tx
                .state_provider_code_hashes
                .iter()
                .map(|access| access.fetched_bytes)
                .sum::<u64>(),
            metered_tx.state_provider.code_fetched_bytes
        );
        assert!(
            response.state_provider.account_fetches >= metered_tx.state_provider.account_fetches,
            "block fetches should include transaction fetches"
        );

        assert!(response.signer_recovery_time_us > 0, "signer recovery should take time");
        assert!(response.execution_time_us > 0);
        assert_eq!(
            response.total_time_us,
            response.signer_recovery_time_us + response.execution_time_us
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

        let tx_1 = BaseTransactionSigned::Eip1559(
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

        let tx_2 = BaseTransactionSigned::Eip1559(
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
        assert_eq!(
            response.total_time_us,
            response.signer_recovery_time_us + response.execution_time_us
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
    async fn meter_block_reports_contract_storage_and_bytecode_identity() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let (deployment_tx, contract_address, _) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![deployment_tx]).await?;

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(contract_address)
            .gas_limit(100_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1)
            .input(SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode())
            .into_eip1559();
        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let block = create_block_with_transactions(&harness, vec![tx]);

        let response = meter_block(harness.blockchain_provider(), harness.chain_spec(), &block)?;
        let metered_tx = &response.transactions[0];
        let contract_access = metered_tx
            .state_provider_accounts
            .iter()
            .find(|access| access.address == contract_address)
            .expect("contract access should be reported");
        let bytecode_hash =
            contract_access.bytecode_hash.expect("contract bytecode hash should be reported");

        assert!(contract_access.storage_keys.contains(&B256::ZERO));
        assert!(
            metered_tx
                .state_provider_code_hashes
                .iter()
                .any(|access| access.code_hash == bytecode_hash),
            "fetched bytecode should be associated with the contract account"
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

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let block = create_block_with_transactions(&harness, vec![tx]);

        let response = meter_block(harness.blockchain_provider(), harness.chain_spec(), &block)?;

        // Verify timing invariants
        assert!(response.signer_recovery_time_us > 0, "signer recovery time must be positive");
        assert!(response.execution_time_us > 0, "execution time must be positive");
        assert_eq!(
            response.total_time_us,
            response.signer_recovery_time_us + response.execution_time_us,
            "total time must equal signer recovery + execution time"
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

        let body = BaseBlockBody { transactions: vec![], ommers: vec![], withdrawals: None };
        let block = BaseBlock::new(header, body);

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
        let base_tx = BaseTransactionSigned::Eip1559(signed_tx);

        let block = create_block_with_transactions(&harness, vec![base_tx]);

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
