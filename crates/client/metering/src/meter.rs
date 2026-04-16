//! Bundle metering logic.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{Address, B256, U256};
use base_bundles::{BundleExtensions, BundleTxs, ParsedBundle, TransactionResult};
use base_common_evm::L1BlockInfo;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseEvmConfig, OpNextBlockEnvAttributes};
use eyre::{Result as EyreResult, eyre};
use reth_evm::{ConfigureEvm, execute::BlockBuilder};
use reth_primitives_traits::{Account, SealedHeader};
use reth_revm::{database::StateProviderDatabase, db::State, primitives::KECCAK_EMPTY};
use reth_trie_common::{HashedPostState, TrieInput};
use revm_database::states::{BundleState, CacheState, bundle_state::BundleRetention};

use crate::{metrics::Metrics, transaction::validate_tx};

/// Computes the pending trie input from a pre-built [`HashedPostState`].
///
/// This function records metrics for cache misses and compute duration.
pub(crate) fn compute_pending_trie_input<SP>(
    state_provider: &SP,
    hashed_state: HashedPostState,
) -> EyreResult<PendingTrieInput>
where
    SP: reth_provider::StateProvider + ?Sized,
{
    Metrics::pending_trie_cache_misses().increment(1);
    let start = Instant::now();

    let (_state_root, trie_updates) =
        state_provider.state_root_with_updates(hashed_state.clone())?;

    let elapsed = start.elapsed();
    Metrics::pending_trie_compute_duration().record(elapsed.as_secs_f64());

    Ok(PendingTrieInput { trie_updates, hashed_state })
}

/// Converts a pending [`BundleState`] into a [`CacheState`] for use with
/// `with_cached_prestate()`.
fn cache_state_from_bundle_state(bundle_state: &BundleState) -> CacheState {
    CacheState {
        accounts: bundle_state
            .state
            .iter()
            .map(|(&address, account)| (address, account.into()))
            .collect(),
        contracts: bundle_state
            .contracts
            .iter()
            .map(|(&hash, code)| (hash, code.clone()))
            .collect(),
        ..Default::default()
    }
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
// Static floor from the current minimum base fee for metering simulation.
// The protocol has a dynamic min_base_fee via system config, but for metering
// we use a static floor to reject transactions that will never make it onchain.
const MIN_BASEFEE: u64 = 5_000_000;
const MAX_NONCE_AHEAD: u64 = 10_000; // max nonce distance from on-chain state

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
    /// Count of account leaves in the bundle's `HashedPostState`: one per modified account that
    /// survives in the post-state trie. Proportional to gas (each account touch costs gas) and
    /// does not reflect trie depth.
    pub state_root_account_leaf_count: u64,
    /// Count of account branch/removal nodes emitted by `TrieUpdates` during state root
    /// calculation. These are intermediate trie nodes that were rebuilt or removed, and their
    /// count scales with trie depth — the structural cost that gas does not price.
    pub state_root_account_branch_count: u64,
    /// Count of storage slot leaves in the bundle's `HashedPostState`: one per modified non-zero
    /// storage slot. Like account leaves, proportional to gas and does not reflect trie depth.
    pub state_root_storage_leaf_count: u64,
    /// Count of storage branch/removal nodes emitted by `TrieUpdates` during state root
    /// calculation, restricted to tries the bundle actually modified. Like account branches,
    /// these scale with trie depth.
    pub state_root_storage_branch_count: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StateRootTrieNodeCounts {
    account_leaves: u64,
    account_branches: u64,
    storage_leaves: u64,
    storage_branches: u64,
}

/// Counts surviving leaves in the bundle's `HashedPostState`.
///
/// These are the values that changed, not intermediate trie structure. Account leaves are one per
/// modified surviving account; storage leaves are one per modified non-zero storage slot. Deleted
/// accounts and zero-valued storage removals are represented through trie updates, not here.
fn count_state_root_leaf_nodes(hashed_state: &HashedPostState) -> StateRootTrieNodeCounts {
    let account_leaves =
        hashed_state.accounts.values().filter(|account| account.is_some()).count() as u64;
    let storage_leaves = hashed_state
        .storages
        .values()
        .map(|storage| storage.storage.values().filter(|value| !value.is_zero()).count())
        .sum::<usize>() as u64;

    StateRootTrieNodeCounts { account_leaves, storage_leaves, ..Default::default() }
}

/// Adds branch/removal counts from `TrieUpdates` emitted during state root calculation.
///
/// These are intermediate trie nodes that were rebuilt or removed — the structural work whose cost
/// scales with trie depth. The `changed_storage_tries` filter restricts storage-side attribution
/// to tries the bundle actually modified, excluding cached pending-state tries and empty-storage
/// deletion markers.
fn add_state_root_trie_update_counts(
    counts: &mut StateRootTrieNodeCounts,
    changed_storage_tries: &HashSet<B256>,
    trie_updates: &reth_trie_common::updates::TrieUpdates,
) {
    counts.account_branches = counts.account_branches.saturating_add(
        trie_updates
            .account_nodes_ref()
            .len()
            .saturating_add(trie_updates.removed_nodes_ref().len()) as u64,
    );
    counts.storage_branches = counts.storage_branches.saturating_add(
        trie_updates
            .storage_tries_ref()
            .iter()
            .filter(|(hashed_address, _)| changed_storage_tries.contains(*hashed_address))
            .map(|(_, updates)| updates.len())
            .sum::<usize>() as u64,
    );
}

/// Simulates and meters a bundle of transactions.
///
/// Takes a state provider, chain spec, parsed bundle, block header, and optional pending state,
/// then executes transactions in sequence to measure gas usage and execution time.
///
/// Returns [`MeterBundleOutput`] containing transaction results and aggregated metrics.
pub fn meter_bundle<SP>(
    state_provider: SP,
    chain_spec: Arc<BaseChainSpec>,
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
    let pending_trie = pending_state
        .as_ref()
        .map(|ps| -> EyreResult<PendingTrieInput> {
            // Use cached trie input if available, otherwise compute it
            ps.trie_input.as_ref().map_or_else(
                || {
                    let hashed = state_provider.hashed_post_state(&ps.bundle_state);
                    compute_pending_trie_input(&state_provider, hashed)
                },
                |cached| {
                    Metrics::pending_trie_cache_hits().increment(1);
                    Ok(cached.clone())
                },
            )
        })
        .transpose()?;

    // Create state database
    let state_db = StateProviderDatabase::new(state_provider);

    // Track bundle state changes. When metering on top of pending flashblocks, seed execution
    // from a cache prestate instead of `with_bundle_prestate()`. The two approaches produce
    // identical execution results, but differ in what `take_bundle()` returns:
    //
    // - `with_bundle_prestate()`: `take_bundle()` includes the pending prestate in its output,
    //   so `hashed_post_state()` generates prefix sets for every pending path. The trie walker
    //   then rebuilds all of them — even though `prepend_cached` already provides those nodes —
    //   making state root time proportional to pending state size.
    //
    // - `with_cached_prestate()`: `take_bundle()` returns only the bundle's delta, so prefix
    //   sets cover only bundle-changed paths. The trie walker skips pending paths (reusing
    //   cached nodes) and state root time is proportional to bundle size alone.
    let mut db = if let Some(ref ps) = pending_state {
        State::builder()
            .with_database(state_db)
            .with_bundle_update()
            .with_cached_prestate(cache_state_from_bundle_state(&ps.bundle_state))
            .build()
    } else {
        State::builder().with_database(state_db).with_bundle_update().build()
    };

    // Override sender nonces to match their first transaction's nonce and collect
    // account info for pre-flight validation. `load_cache_account` reads from the
    // cached pending prestate when available, so balances reflect pending state.
    let mut first_nonces: HashMap<Address, u64> = HashMap::new();
    for tx in bundle.transactions() {
        first_nonces.entry(tx.signer()).or_insert_with(|| tx.nonce());
    }

    let mut account_infos: HashMap<Address, Option<Account>> = HashMap::new();
    for (&addr, &nonce) in &first_nonces {
        let cache_account = db.load_cache_account(addr)?;
        if let Some(ref mut account) = cache_account.account {
            let max_nonce = account.info.nonce.saturating_add(MAX_NONCE_AHEAD);
            if nonce > max_nonce {
                return Err(eyre!(
                    "transaction nonce {} for {} exceeds max allowed (on-chain {} + {})",
                    nonce,
                    addr,
                    account.info.nonce,
                    MAX_NONCE_AHEAD,
                ));
            }
            account.info.nonce = nonce;

            account_infos.insert(
                addr,
                Some(Account {
                    nonce: account.info.nonce,
                    balance: account.info.balance,
                    bytecode_hash: (account.info.code_hash != KECCAK_EMPTY)
                        .then_some(account.info.code_hash),
                }),
            );
        } else {
            account_infos.insert(addr, None);
        }
    }

    // Set up next block attributes
    // Use bundle.min_timestamp if provided, otherwise use header timestamp + BLOCK_TIME
    let timestamp = bundle.min_timestamp.unwrap_or_else(|| header.timestamp() + BLOCK_TIME);
    // Pending flashblock headers may omit parent_beacon_block_root; prefer the explicit value
    // provided by the caller (e.g., flashblock base payload) to keep EIP-4788 happy.
    let attributes = OpNextBlockEnvAttributes {
        timestamp,
        suggested_fee_recipient: header.beneficiary(),
        prev_randao: header.mix_hash().unwrap_or_else(B256::random),
        gas_limit: header.gas_limit(),
        parent_beacon_block_root: parent_beacon_block_root
            .or_else(|| header.parent_beacon_block_root()),
        extra_data: header.extra_data().clone(),
    };

    // Execute transactions
    let mut results = Vec::new();
    let mut total_gas_used = 0u64;
    let mut total_gas_fees = U256::ZERO;

    let total_start = Instant::now();
    {
        let evm_config = BaseEvmConfig::optimism(chain_spec);
        let mut builder = evm_config.builder_for_next_block(&mut db, header, attributes)?;

        // Cap the base fee at MIN_BASEFEE so transactions aren't rejected for
        // max_fee_per_gas < basefee. We're simulating for gas measurement, not fee
        // accounting. Balance checks in validate_tx still catch underfunded senders
        // intentionally.
        let block = &mut builder.evm_mut().block;
        block.basefee = block.basefee.min(MIN_BASEFEE);

        builder.apply_pre_execution_changes()?;

        for tx in bundle.transactions() {
            let tx_start = Instant::now();
            let tx_hash = tx.tx_hash();
            let from = tx.signer();
            let to = tx.to();
            let value = tx.value();
            let gas_price = tx.max_fee_per_gas();
            let account = account_infos
                .get(&from)
                .ok_or_else(|| eyre!("Account not found in HashMap for address: {}", from))?
                .ok_or_else(|| eyre!("Account is none for tx: {}", tx_hash))?;

            // Don't waste resources metering invalid transactions.
            // Note: balance checks (InsufficientFunds*) are intentionally kept — an underfunded
            // sender is a meaningful validation failure. Nonce and base fee are overridden above.
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

    // Calculate state root and measure its calculation time. If pending flashblocks were present,
    // `bundle_update` now contains only this bundle's delta; the cached pending trie is prepended
    // below so state-root work stays incremental.
    db.merge_transitions(BundleRetention::Reverts);
    let bundle_update = db.take_bundle();

    // Gets the number of storage slots modified from every account
    let storage_slots_modified: usize =
        bundle_update.state().values().map(|account| account.storage.len()).sum();
    Metrics::storage_slots_modified().record(storage_slots_modified as f64);

    // Gets the number of accounts modified
    let accounts_modified: usize = bundle_update.state().len();
    Metrics::accounts_modified().record(accounts_modified as f64);
    // `state_root_*_with_updates` reports structural trie updates for the entire overlay we hand
    // to `reth`, not just the bundle delta. When the bundle made no state changes, those updates
    // can come entirely from cached pending trie nodes or root-maintenance bookkeeping. In that
    // case we still time the calculation, but we intentionally attribute zero trie nodes to the
    // bundle itself.
    let has_bundle_state_changes = accounts_modified > 0;

    let state_provider = db.database.as_ref();

    let state_root_start = Instant::now();
    let hashed_state = state_provider.hashed_post_state(&bundle_update);
    let changed_storage_tries = hashed_state.storages.keys().copied().collect::<HashSet<_>>();
    let mut trie_node_counts = count_state_root_leaf_nodes(&hashed_state);

    if let Some(cached_trie) = pending_trie {
        // Build the trie input so the state root reflects canonical + pending + bundle.
        //
        // `from_state` generates prefix sets only for bundle-changed paths.
        // `prepend_cached` merges the pending state's trie nodes and hashed values
        // WITHOUT adding prefix sets — so the trie walker reuses cached nodes for
        // pending-only paths and only rebuilds paths the bundle actually changed.
        //
        // Note: `prepend_cached` (not `prepend_self`) is essential here.
        // `prepend_self` would merge prefix sets from the pending state, causing the
        // walker to redundantly rebuild every pending path and defeating the
        // optimization.
        let mut trie_input = TrieInput::from_state(hashed_state);
        trie_input.prepend_cached(cached_trie.trie_updates, cached_trie.hashed_state);
        let (_, trie_updates) = state_provider.state_root_from_nodes_with_updates(trie_input)?;
        if has_bundle_state_changes {
            add_state_root_trie_update_counts(
                &mut trie_node_counts,
                &changed_storage_tries,
                &trie_updates,
            );
        }
    } else {
        // No pending state, just calculate bundle state root
        let (_, trie_updates) = state_provider.state_root_with_updates(hashed_state)?;
        if has_bundle_state_changes {
            add_state_root_trie_update_counts(
                &mut trie_node_counts,
                &changed_storage_tries,
                &trie_updates,
            );
        }
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
        state_root_account_leaf_count: trie_node_counts.account_leaves,
        state_root_account_branch_count: trie_node_counts.account_branches,
        state_root_storage_leaf_count: trie_node_counts.storage_leaves,
        state_root_storage_branch_count: trie_node_counts.storage_branches,
    })
}

#[cfg(test)]
mod tests {
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Bytes, keccak256, utils::Unit};
    use alloy_sol_types::SolCall;
    use base_bundles::{Bundle, ParsedBundle};
    use base_common_consensus::BaseTransactionSigned;
    use base_node_runner::test_utils::TestHarness;
    use base_test_utils::{Account, SimpleStorage};
    use eyre::Context;
    use reth_provider::StateProviderFactory;
    use reth_revm::{bytecode::Bytecode, state::AccountInfo};
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;

    fn create_parsed_bundle(txs: Vec<BaseTransactionSigned>) -> eyre::Result<ParsedBundle> {
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
        assert_eq!(output.state_root_account_leaf_count, 0);
        assert_eq!(output.state_root_account_branch_count, 0);
        assert_eq!(output.state_root_storage_leaf_count, 0);
        assert_eq!(output.state_root_storage_branch_count, 0);
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

        let tx = BaseTransactionSigned::Eip1559(
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
        assert!(
            output.state_root_account_leaf_count > 0 || output.state_root_account_branch_count > 0
        );
        assert_eq!(output.state_root_storage_leaf_count, 0);
        assert_eq!(output.state_root_storage_branch_count, 0);

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
    async fn meter_bundle_storage_write_transaction() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (deployment_tx, contract_address, _deployment_hash) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![deployment_tx]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(contract_address)
            .gas_limit(100_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode())
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
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

        assert_eq!(output.results.len(), 1);
        assert!(
            output.state_root_account_leaf_count > 0 || output.state_root_account_branch_count > 0
        );
        assert!(
            output.state_root_storage_leaf_count > 0 || output.state_root_storage_branch_count > 0,
            "storage-writing transactions should attribute storage trie work"
        );

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

        let tx_1 = BaseTransactionSigned::Eip1559(
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

        let tx_2 = BaseTransactionSigned::Eip1559(
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

    /// Test that `state_root_time_us` is always <= `total_time_us`
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

        let tx = BaseTransactionSigned::Eip1559(
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

    /// Verifies that a nonce ahead of on-chain state succeeds via override.
    ///
    /// Canonical nonce is 0, but the transaction uses nonce=1. The nonce override
    /// sets the account nonce to match, so simulation succeeds.
    #[tokio::test]
    async fn meter_bundle_overrides_nonce_too_high() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(1) // Ahead of canonical nonce (0)
            .to(to)
            .value(100)
            .gas_limit(21_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let result = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            None,
            L1BlockInfo::default(),
        );

        assert!(
            result.is_ok(),
            "Nonce ahead of on-chain state should succeed via override: {:?}",
            result.err()
        );

        let output = result.unwrap();
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.total_gas_used, 21_000);

        Ok(())
    }

    /// Verifies that a nonce behind on-chain state succeeds via override.
    ///
    /// Uses pending state to advance Alice's nonce to 5, then submits a transaction
    /// with nonce=0. The nonce override sets the account nonce to match the
    /// transaction, so simulation succeeds despite the nonce being "too low".
    #[tokio::test]
    async fn meter_bundle_overrides_nonce_too_low() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        // Build pending state where Alice's nonce has advanced to 5
        let bundle_state = BundleState::new(
            [(
                Account::Alice.address(),
                Some(AccountInfo {
                    balance: U256::from(1_000_000_000_000_000_000u128),
                    nonce: 0, // original
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    account_id: None,
                }),
                Some(AccountInfo {
                    balance: U256::from(1_000_000_000_000_000_000u128),
                    nonce: 5, // pending
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    account_id: None,
                }),
                Default::default(),
            )],
            Vec::<Vec<(Address, Option<Option<AccountInfo>>, Vec<(U256, U256)>)>>::new(),
            Vec::<(B256, Bytecode)>::new(),
        );
        let pending_state = PendingState { bundle_state, trie_input: None };

        // Transaction with nonce=0 — "too low" relative to pending nonce of 5
        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .value(100)
            .gas_limit(21_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let result = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            Some(pending_state),
            L1BlockInfo::default(),
        );

        assert!(
            result.is_ok(),
            "Nonce behind on-chain state should succeed via override: {:?}",
            result.err()
        );

        let output = result.unwrap();
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.total_gas_used, 21_000);

        Ok(())
    }

    /// Verifies pending flashblock prestate is loaded into the execution cache, not the output
    /// bundle. This keeps later trie prefix invalidation scoped to the simulated bundle delta.
    #[test]
    fn cached_prestate_does_not_leak_into_bundle_output() -> eyre::Result<()> {
        let pending_bundle = BundleState::new(
            [(
                Account::Alice.address(),
                Some(AccountInfo {
                    balance: U256::from(1_000_000_000_000_000_000u128),
                    nonce: 0,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    account_id: None,
                }),
                Some(AccountInfo {
                    balance: U256::from(1_000_000_000_000_000_000u128),
                    nonce: 5,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    account_id: None,
                }),
                Default::default(),
            )],
            Vec::<Vec<(Address, Option<Option<AccountInfo>>, Vec<(U256, U256)>)>>::new(),
            Vec::<(B256, Bytecode)>::new(),
        );

        let mut db = State::builder()
            .with_bundle_update()
            .with_cached_prestate(cache_state_from_bundle_state(&pending_bundle))
            .build();

        let pending_account = db.load_cache_account(Account::Alice.address())?;
        assert_eq!(
            pending_account.account.as_ref().expect("pending account").info.nonce,
            5,
            "execution must read the pending prestate nonce from the cache"
        );

        db.merge_transitions(BundleRetention::Reverts);
        let bundle_update = db.take_bundle();

        assert!(
            bundle_update.state().is_empty(),
            "cached prestate must not be included in the simulated bundle output"
        );

        Ok(())
    }

    /// Verifies that nonce overrides are rejected when too far ahead of on-chain state.
    #[tokio::test]
    async fn meter_bundle_err_nonce_too_far_ahead() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        let nonce = MAX_NONCE_AHEAD + 1; // Just over the limit (on-chain nonce is 0)
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(nonce)
            .to(to)
            .value(100)
            .gas_limit(21_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
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

        assert!(result.is_err(), "Nonce exceeding MAX_NONCE_AHEAD should fail");
        assert!(
            result.unwrap_err().to_string().contains("exceeds max allowed"),
            "Expected max nonce error"
        );

        Ok(())
    }

    /// Verifies that the base fee is capped at `MIN_BASEFEE` for simulation.
    ///
    /// The test genesis produces a next-block base fee of ~980M wei. A transaction with
    /// `max_fee_per_gas` at the `MIN_BASEFEE` floor (5M wei) would normally be rejected,
    /// but `meter_bundle` caps the base fee so simulation succeeds.
    #[tokio::test]
    async fn meter_bundle_caps_basefee_at_minimum() -> eyre::Result<()> {
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
            .max_fee_per_gas(MIN_BASEFEE as u128) // At the floor, below the ~980M on-chain base fee
            .max_priority_fee_per_gas(0)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
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

        assert!(
            result.is_ok(),
            "Transaction with max_fee_per_gas below base fee but at least MIN_BASEFEE should succeed: {:?}",
            result.err()
        );

        let output = result.unwrap();
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.total_gas_used, 21_000);

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

        let tx = BaseTransactionSigned::Eip1559(
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

    /// Exercises the full optimized path: pending state with a cached trie input.
    ///
    /// Computes a real [`PendingTrieInput`] from pending state, then meters a bundle
    /// on top of it. This covers the `prepend_cached` code path with the
    /// `with_cached_prestate` change, verifying the two work correctly together.
    #[tokio::test]
    async fn meter_bundle_with_pending_state_and_cached_trie() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        // Build pending state: Alice sent a tx (nonce advanced to 1, balance decreased)
        let pending_balance = U256::from(999_999_999_999_000_000_000_000u128);
        let bundle_state = BundleState::new(
            [(
                Account::Alice.address(),
                Some(AccountInfo {
                    balance: U256::from(1_000_000u128) * U256::from(10u128).pow(U256::from(18)),
                    nonce: 0,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    account_id: None,
                }),
                Some(AccountInfo {
                    balance: pending_balance,
                    nonce: 1,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    account_id: None,
                }),
                Default::default(),
            )],
            Vec::<Vec<(Address, Option<Option<AccountInfo>>, Vec<(U256, U256)>)>>::new(),
            Vec::<(B256, Bytecode)>::new(),
        );

        // Compute the pending trie input
        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider for trie")?;
        let hashed = state_provider.hashed_post_state(&bundle_state);
        let trie_input = compute_pending_trie_input(&state_provider, hashed)?;
        drop(state_provider);

        let pending_state = PendingState { bundle_state, trie_input: Some(trie_input) };

        // Create a bundle tx: Alice (nonce=1 from pending) sends to a random address
        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(1)
            .to(to)
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let output = meter_bundle(
            state_provider,
            harness.chain_spec(),
            parsed_bundle,
            &header,
            header.parent_beacon_block_root(),
            Some(pending_state),
            L1BlockInfo::default(),
        )?;

        assert_eq!(output.results.len(), 1);
        assert_eq!(output.total_gas_used, 21_000);
        assert!(output.total_time_us > 0);
        assert!(output.state_root_time_us > 0);
        assert!(
            output.total_time_us >= output.state_root_time_us,
            "total_time_us ({}) should be >= state_root_time_us ({})",
            output.total_time_us,
            output.state_root_time_us
        );

        Ok(())
    }
}
