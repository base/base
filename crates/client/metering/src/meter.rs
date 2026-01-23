//! Bundle metering logic.

use std::{collections::HashMap, sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _, transaction::SignerRecoverable};
use alloy_primitives::{Address, B256, U256};
use base_bundles::{BundleExtensions, BundleTxs, ParsedBundle, TransactionResult};
use eyre::{Result as EyreResult, eyre};
use op_revm::l1block::L1BlockInfo;
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_primitives_traits::{Account, SealedHeader};
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_trie_common::TrieInput;
use revm_database::states::{BundleState, bundle_state::BundleRetention};

use crate::{metrics::Metrics, transaction::validate_tx};

/// Computes the pending trie input from the bundle state.
///
/// This function records metrics for cache misses and compute duration.
pub(crate) fn compute_pending_trie_input<SP>(
    state_provider: &SP,
    bundle_state: &BundleState,
    metrics: &Metrics,
) -> EyreResult<PendingTrieInput>
where
    SP: reth_provider::StateProvider + ?Sized,
{
    metrics.pending_trie_cache_misses.increment(1);
    let start = Instant::now();

    let hashed = state_provider.hashed_post_state(bundle_state);
    let (_state_root, trie_updates) = state_provider.state_root_with_updates(hashed.clone())?;

    let elapsed = start.elapsed();
    metrics.pending_trie_compute_duration.record(elapsed.as_secs_f64());

    Ok(PendingTrieInput { trie_updates, hashed_state: hashed })
}

/// Pre-computed trie input from pending state for efficient state root calculation.
///
/// When metering bundles on top of pending flashblocks, we first compute the trie updates
/// and hashed state for the pending state. This can then be prepended to the bundle's
/// trie input, so state root calculation only performs I/O for the bundle's changes.
#[derive(Debug, Clone)]
pub struct PendingTrieInput {
    /// Trie updates from computing pending state root.
    pub trie_updates: reth_trie_common::updates::TrieUpdates,
    /// Hashed state from pending flashblocks.
    pub hashed_state: reth_trie_common::HashedPostState,
}

/// Pending state from flashblocks used as the base for bundle metering.
///
/// This contains the accumulated state changes from pending flashblocks,
/// allowing bundle simulation to build on top of not-yet-canonical state.
#[derive(Debug, Clone)]
pub struct PendingState {
    /// The accumulated bundle of state changes from pending flashblocks.
    pub bundle_state: BundleState,
    /// Optional pre-computed trie input for faster state root calculation.
    /// If provided, state root calculation skips recomputing the pending state's trie.
    pub trie_input: Option<PendingTrieInput>,
}

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

/// Simulates and meters a bundle of transactions.
///
/// Takes a state provider, chain spec, parsed bundle, block header, and optional pending state,
/// then executes transactions in sequence to measure gas usage and execution time.
///
/// Returns [`MeterBundleOutput`] containing transaction results and aggregated metrics.
pub fn meter_bundle<SP>(
    state_provider: SP,
    chain_spec: Arc<OpChainSpec>,
    bundle: ParsedBundle,
    header: &SealedHeader,
    parent_beacon_block_root: Option<B256>,
    pending_state: Option<PendingState>,
    mut l1_block_info: L1BlockInfo,
) -> EyreResult<MeterBundleOutput>
where
    SP: reth_provider::StateProvider,
{
    // Get bundle hash
    let bundle_hash = bundle.bundle_hash();

    // Get pending trie input before starting timers. This ensures we only measure
    // the bundle's incremental I/O cost, not I/O from pending flashblocks.
    let metrics = Metrics::default();
    let pending_trie = pending_state
        .as_ref()
        .map(|ps| -> EyreResult<PendingTrieInput> {
            // Use cached trie input if available, otherwise compute it
            if let Some(ref cached) = ps.trie_input {
                metrics.pending_trie_cache_hits.increment(1);
                Ok(cached.clone())
            } else {
                compute_pending_trie_input(&state_provider, &ps.bundle_state, &metrics)
            }
        })
        .transpose()?;

    // Create state database
    let state_db = StateProviderDatabase::new(state_provider);

    // Track bundle state changes. If metering with pending state, include it as bundle prestate.
    let mut db = if let Some(ref ps) = pending_state {
        State::builder()
            .with_database(state_db)
            .with_bundle_update()
            .with_bundle_prestate(ps.bundle_state.clone())
            .build()
    } else {
        State::builder().with_database(state_db).with_bundle_update().build()
    };

    // Set up next block attributes
    // Use bundle.min_timestamp if provided, otherwise use header timestamp + BLOCK_TIME
    let timestamp = bundle.min_timestamp.unwrap_or_else(|| header.timestamp() + BLOCK_TIME);
    // Pending flashblock headers may omit parent_beacon_block_root; prefer the explicit value
    // provided by the caller (e.g., flashblock base payload) to keep EIP-4788 happy.
    let attributes = OpNextBlockEnvAttributes {
        timestamp,
        suggested_fee_recipient: header.beneficiary(),
        prev_randao: header.mix_hash().unwrap_or(B256::random()),
        gas_limit: header.gas_limit(),
        parent_beacon_block_root: parent_beacon_block_root
            .or_else(|| header.parent_beacon_block_root()),
        extra_data: header.extra_data().clone(),
    };

    // Pre-fetch account information for all transactions before creating builder. The
    // account information is used to validate the transaction.
    let mut accounts: HashMap<Address, Option<Account>> = HashMap::new();
    for tx in bundle.transactions() {
        let from = tx.recover_signer()?;
        let account = db.database.basic_account(&from)?;
        accounts.insert(from, account);
    }

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
            let account = accounts
                .get(&from)
                .ok_or_else(|| eyre!("Account not found in HashMap for address: {}", from))?
                .ok_or_else(|| eyre!("Account is none for tx: {}", tx_hash))?;

            // Don't waste resources metering invalid transactions
            validate_tx(account, tx, &mut l1_block_info)
                .map_err(|e| eyre!("Transaction {} validation failed: {}", tx_hash, e))?;

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

    // Calculate state root and measure its calculation time. The bundle already includes
    // pending state if it was provided via with_bundle_prestate.
    db.merge_transitions(BundleRetention::Reverts);
    let bundle_update = db.take_bundle();
    let state_provider = db.database.as_ref();

    let state_root_start = Instant::now();
    let hashed_state = state_provider.hashed_post_state(&bundle_update);

    if let Some(cached_trie) = pending_trie {
        // Prepend cached pending trie so state root calculation only performs I/O
        // for this bundle's changes, not for pending flashblocks.
        let mut trie_input = TrieInput::from_state(hashed_state);
        trie_input.prepend_cached(cached_trie.trie_updates, cached_trie.hashed_state);
        let _ = state_provider.state_root_from_nodes_with_updates(trie_input)?;
    } else {
        // No pending state, just calculate bundle state root
        let _ = state_provider.state_root_with_updates(hashed_state)?;
    }

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
    use alloy_primitives::{Address, Bytes, keccak256, utils::Unit};
    use base_bundles::{Bundle, ParsedBundle};
    use base_client_node::test_utils::{Account, TestHarness};
    use eyre::Context;
    use reth_optimism_primitives::OpTransactionSigned;
    use reth_provider::StateProviderFactory;
    use reth_revm::{bytecode::Bytecode, primitives::KECCAK_EMPTY, state::AccountInfo};
    use reth_transaction_pool::test_utils::TransactionBuilder;
    use revm_context_interface::transaction::{AccessList, AccessListItem};

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

        let output = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            None,
            L1BlockInfo::default(),
        )?;

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

        let output = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            None,
            L1BlockInfo::default(),
        )?;

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
    async fn meter_bundle_requires_parent_beacon_block_root() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let parsed_bundle = create_parsed_bundle(Vec::new())?;

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        // Mimic a pending flashblock header that lacks the parent beacon block root.
        let mut header_without_root = header.clone_header();
        header_without_root.parent_beacon_block_root = None;
        let sealed_without_root = SealedHeader::new(header_without_root, header.hash());

        let err = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle.clone(),
            &sealed_without_root,
            None,
            None,
            L1BlockInfo::default(),
        )
        .expect_err("missing parent beacon block root should fail");
        assert!(
            err.to_string().to_lowercase().contains("parent beacon block root"),
            "expected missing parent beacon block root error, got {err:?}"
        );

        let state_provider2 = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let output = meter_bundle(
            state_provider2,
            harness.chain_spec(),
            parsed_bundle,
            &sealed_without_root,
            Some(header.parent_beacon_block_root().unwrap_or(B256::ZERO)),
            None,
            L1BlockInfo::default(),
        )?;

        assert!(output.total_time_us > 0);
        assert!(output.state_root_time_us > 0);

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

        let output = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            None,
            L1BlockInfo::default(),
        )?;

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

        let output = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            None,
            L1BlockInfo::default(),
        )?;

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

    /// Integration test: verifies meter_bundle uses flashblocks state correctly.
    ///
    /// A transaction using nonce=1 should fail without flashblocks state (since
    /// canonical nonce is 0), but succeed when flashblocks state indicates nonce=1.
    #[tokio::test]
    async fn meter_bundle_requires_correct_layering_for_pending_nonce() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        // Create a transaction that requires nonce=1 (assuming canonical nonce is 0)
        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(1) // Requires pending state to have nonce=1
            .to(to)
            .value(100)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx = OpTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        // Without flashblocks state, transaction should fail (nonce mismatch)
        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let result_without_flashblocks = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle.clone(),
            &header,
            header.parent_beacon_block_root(),
            None, // No pending state
            L1BlockInfo::default(),
        );

        assert!(
            result_without_flashblocks.is_err(),
            "Transaction with nonce=1 should fail without pending state (canonical nonce is 0)"
        );

        // Now create pending state with nonce=1 for Alice
        // Use BundleState::new() to properly calculate state_size
        let bundle_state = BundleState::new(
            [(
                Account::Alice.address(),
                Some(AccountInfo {
                    balance: U256::from(1_000_000_000u64),
                    nonce: 0, // original
                    code_hash: KECCAK_EMPTY,
                    code: None,
                }),
                Some(AccountInfo {
                    balance: U256::from(1_000_000_000u64),
                    nonce: 1, // pending (after first flashblock tx)
                    code_hash: KECCAK_EMPTY,
                    code: None,
                }),
                Default::default(), // no storage changes
            )],
            Vec::<Vec<(Address, Option<Option<AccountInfo>>, Vec<(U256, U256)>)>>::new(),
            Vec::<(B256, Bytecode)>::new(),
        );

        let pending_state = PendingState { bundle_state, trie_input: None };

        // With correct pending state, transaction should succeed
        let state_provider2 = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let result_with_pending = meter_bundle(
            state_provider2,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            Some(pending_state),
            L1BlockInfo::default(),
        );

        assert!(
            result_with_pending.is_ok(),
            "Transaction with nonce=1 should succeed with pending state showing nonce=1: {:?}",
            result_with_pending.err()
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_err_interop_tx() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        // Create a transaction with cross-L2 interop address in access list
        let to = Address::random();
        let access_list = AccessList::from(vec![AccessListItem {
            address: op_alloy_consensus::interop::CROSS_L2_INBOX_ADDRESS,
            storage_keys: vec![],
        }]);

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .access_list(access_list)
            .into_eip1559();

        let tx = OpTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let result = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            None,
            L1BlockInfo::default(),
        );

        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Interop transactions are not supported"),
            "Expected interop error"
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_err_insufficient_funds() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        // TestHarness uses build_test_genesis() which gives accounts 1 million ETH.
        // Transaction cost = value + (gas_limit * max_fee_per_gas)
        // We set value to 2 million ETH which exceeds the 1 million ETH balance
        let value_eth = 2_000_000u128;
        let value_in_wei = value_eth.saturating_mul(Unit::ETHER.wei().to::<u128>());

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .value(value_in_wei)
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

        let result = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            None,
            L1BlockInfo::default(),
        );

        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Insufficient funds"),
            "Expected insufficient funds error"
        );

        Ok(())
    }
}
