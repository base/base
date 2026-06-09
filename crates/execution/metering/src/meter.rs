//! Bundle metering logic.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{
    Address, B256, U256,
    map::{HashMap, HashSet},
};
use base_bundles::{BundleExtensions, BundleTxs, OpcodeGas, ParsedBundle, TransactionResult};
use base_common_evm::L1BlockInfo;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use eyre::{Result as EyreResult, eyre};
use reth_evm::{ConfigureEvm, Evm as _, execute::BlockBuilder};
use reth_primitives_traits::{Account, SealedHeader};
use reth_revm::{database::StateProviderDatabase, db::State, primitives::KECCAK_EMPTY};
use reth_trie_common::HashedPostState;
use revm_bytecode::opcode::OpCode;
use revm_database::states::bundle_state::BundleRetention;

use crate::{inspector::MeteringInspector, metrics::Metrics, transaction::validate_tx};

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
/// to tries the bundle actually modified, excluding empty-storage deletion markers.
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

/// Opcodes and precompiles to track during bundle metering.
#[derive(Debug, Clone, Default)]
pub struct MeteredOpcodes {
    /// EVM opcodes to track.
    pub opcodes: HashSet<OpCode>,
    /// Precompile addresses to track, keyed by address with display name.
    pub precompiles: HashMap<Address, String>,
}

/// Constructs a precompile address from a `u16` value.
const fn precompile_addr(n: u16) -> Address {
    let be = n.to_be_bytes();
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, be[0], be[1]])
}

/// Standard EVM precompile names and their addresses.
///
/// Names follow EIP-7910 conventions.
const PRECOMPILES: &[(&str, Address)] = &[
    ("ECREC", precompile_addr(0x01)),
    ("SHA256", precompile_addr(0x02)),
    ("RIPEMD160", precompile_addr(0x03)),
    ("ID", precompile_addr(0x04)),
    ("MODEXP", precompile_addr(0x05)),
    ("BN254_ADD", precompile_addr(0x06)),
    ("BN254_MUL", precompile_addr(0x07)),
    ("BN254_PAIRING", precompile_addr(0x08)),
    ("BLAKE2F", precompile_addr(0x09)),
    ("KZG_POINT_EVALUATION", precompile_addr(0x0a)),
    ("BLS12_G1ADD", precompile_addr(0x0b)),
    ("BLS12_G1MSM", precompile_addr(0x0c)),
    ("BLS12_G2ADD", precompile_addr(0x0d)),
    ("BLS12_G2MSM", precompile_addr(0x0e)),
    ("BLS12_PAIRING_CHECK", precompile_addr(0x0f)),
    ("BLS12_MAP_FP_TO_G1", precompile_addr(0x10)),
    ("BLS12_MAP_FP2_TO_G2", precompile_addr(0x11)),
    ("P256VERIFY", precompile_addr(0x100)),
];

impl MeteredOpcodes {
    /// Returns true if no opcodes or precompiles are configured.
    pub fn is_empty(&self) -> bool {
        self.opcodes.is_empty() && self.precompiles.is_empty()
    }

    /// Adds all known precompiles to the metered set.
    pub fn with_all_precompiles(mut self) -> Self {
        for &(name, addr) in PRECOMPILES {
            self.precompiles.insert(addr, name.to_string());
        }
        self
    }

    /// Parses opcode and precompile name strings into a [`MeteredOpcodes`] filter.
    ///
    /// Recognizes EVM opcode names (e.g., `SSTORE`, `CALL`) and precompile names
    /// (e.g., `ECREC`, `BLAKE2F`). Matching is case-insensitive.
    pub fn parse(names: &[String]) -> EyreResult<Self> {
        let opcode_lookup: HashMap<&str, OpCode> =
            (0..=255u8).filter_map(|byte| OpCode::new(byte).map(|op| (op.as_str(), op))).collect();

        let precompile_lookup: HashMap<&str, (Address, &str)> =
            PRECOMPILES.iter().map(|&(name, addr)| (name, (addr, name))).collect();

        let mut result = Self::default();
        for name in names {
            let upper = name.to_uppercase();
            if let Some(&opcode) = opcode_lookup.get(upper.as_str()) {
                result.opcodes.insert(opcode);
            } else if let Some(&(addr, display_name)) = precompile_lookup.get(upper.as_str()) {
                result.precompiles.insert(addr, display_name.to_string());
            } else {
                return Err(eyre!("unknown opcode or precompile: {name}"));
            }
        }
        Ok(result)
    }
}

/// Inputs for [`meter_bundle`].
#[derive(Debug)]
pub struct MeterBundleInput<SP> {
    /// State provider used to read pre-execution account and storage state.
    pub state_provider: SP,
    /// Chain spec used to construct the EVM environment.
    pub chain_spec: Arc<BaseChainSpec>,
    /// The bundle to simulate.
    pub bundle: ParsedBundle,
    /// Header used as the parent block for simulation; the EVM env is derived from it.
    pub header: SealedHeader,
    /// Optional parent beacon block root override (e.g., from a segment base payload)
    /// used when the header itself omits it.
    pub parent_beacon_block_root: Option<B256>,
    /// L1 block info used to compute L1 data fees during simulation.
    pub l1_block_info: L1BlockInfo,
    /// Opcodes and precompiles to track gas usage for.
    pub metered_opcodes: Arc<MeteredOpcodes>,
}

/// Simulates and meters a bundle of transactions.
///
/// Executes transactions in sequence to measure gas usage and execution time.
/// When `metered_opcodes` is non-empty, a [`MeteringInspector`] is attached to the EVM
/// to collect per-opcode and precompile gas data. Only items in the filter set appear
/// in the output.
///
/// Returns [`MeterBundleOutput`] containing transaction results and aggregated metrics.
pub fn meter_bundle<SP>(input: MeterBundleInput<SP>) -> EyreResult<MeterBundleOutput>
where
    SP: reth_provider::StateProvider,
{
    let MeterBundleInput {
        state_provider,
        chain_spec,
        bundle,
        header,
        parent_beacon_block_root,
        mut l1_block_info,
        metered_opcodes,
    } = input;
    let header = &header;
    let metered_opcodes = metered_opcodes.as_ref();
    // Get bundle hash
    let bundle_hash = bundle.bundle_hash();

    // Create state database
    let state_db = StateProviderDatabase::new(state_provider);

    // Track only the canonical bundle delta.
    let mut db = State::builder().with_database(state_db).with_bundle_update().build();

    // Override sender nonces to match their first transaction's nonce and collect
    // account info for pre-flight validation.
    let mut first_nonces: HashMap<Address, u64> = HashMap::default();
    for tx in bundle.transactions() {
        first_nonces.entry(tx.signer()).or_insert_with(|| tx.nonce());
    }

    let mut account_infos: HashMap<Address, Option<Account>> = HashMap::default();
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
    // Prefer the explicit root when the caller provides one.
    let attributes = BaseNextBlockEnvAttributes {
        timestamp,
        suggested_fee_recipient: header.beneficiary(),
        prev_randao: header.mix_hash().unwrap_or_else(B256::random),
        gas_limit: header.gas_limit(),
        parent_beacon_block_root: parent_beacon_block_root
            .or_else(|| header.parent_beacon_block_root()),
        extra_data: header.extra_data().clone(),
    };

    // Execute transactions with a MeteringInspector to collect per-opcode and
    // precompile gas data. Precompile gas is always tracked; opcode gas is only
    // tracked for opcodes in the metered set.
    let mut results = Vec::new();
    let mut total_gas_used = 0u64;
    let mut total_gas_fees = U256::ZERO;

    let total_start = Instant::now();
    {
        let evm_config = BaseEvmConfig::base(chain_spec);
        let evm_env = evm_config.next_evm_env(header, &attributes)?;
        let spec = evm_env.cfg_env.spec;
        let precompile_addrs = metered_opcodes.precompiles.keys().copied().collect();
        let inspector = MeteringInspector::new(precompile_addrs, metered_opcodes.opcodes.clone());
        let evm = evm_config.evm_with_env_and_inspector(&mut db, evm_env, inspector);
        let ctx = evm_config.context_for_next_block(header, attributes)?;
        let mut builder = evm_config.create_block_builder(evm, header, ctx);

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
                .ok_or_else(|| eyre!("Account not found for address: {from}"))?
                .ok_or_else(|| eyre!("Account is none for tx: {tx_hash}"))?;

            validate_tx(account, tx, &mut l1_block_info, spec)
                .map_err(|e| eyre!("Transaction {tx_hash} validation failed: {e}"))?;

            let gas_used = builder
                .execute_transaction(tx.clone())
                .map_err(|e| eyre!("Transaction {tx_hash} execution failed: {e}"))?
                .tx_gas_used();

            let gas_fees = U256::from(gas_used) * U256::from(gas_price);
            total_gas_used = total_gas_used.saturating_add(gas_used);
            total_gas_fees = total_gas_fees.saturating_add(gas_fees);

            // Extract per-transaction opcode and precompile gas, then reset for next tx.
            let inspector = builder.evm_mut().inspector_mut();
            let opcode_data = inspector.take_opcode_gas();
            let precompile_data = inspector.take_precompile_gas();

            let mut opcode_gas: Vec<OpcodeGas> = opcode_data
                .into_iter()
                .filter_map(|((contract_address, opcode), usage)| {
                    if usage.count > 0 {
                        Some(OpcodeGas {
                            contract_address,
                            opcode: opcode.as_str().to_string(),
                            count: usage.count,
                            gas_used: usage.gas_used,
                        })
                    } else {
                        None
                    }
                })
                .collect();

            for (addr, usage) in &precompile_data {
                if let Some(name) = metered_opcodes.precompiles.get(addr)
                    && usage.count > 0
                {
                    opcode_gas.push(OpcodeGas {
                        contract_address: *addr,
                        opcode: name.clone(),
                        count: usage.count,
                        gas_used: usage.gas_used,
                    });
                }
            }

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
                execution_time_us: tx_start.elapsed().as_micros(),
                opcode_gas,
            });
        }
    }

    // Calculate state root and measure its calculation time for the canonical bundle delta.
    db.merge_transitions(BundleRetention::Reverts);
    let bundle_update = db.take_bundle();

    // Gets the number of storage slots modified from every account
    let storage_slots_modified: usize =
        bundle_update.state().values().map(|account| account.storage.len()).sum();
    Metrics::storage_slots_modified().record(storage_slots_modified as f64);

    // Gets the number of accounts modified
    let accounts_modified: usize = bundle_update.state().len();
    Metrics::accounts_modified().record(accounts_modified as f64);
    // `state_root_with_updates` can emit root-maintenance bookkeeping even when the bundle made no
    // state changes. In that case we still time the calculation, but intentionally attribute zero
    // trie nodes to the bundle itself.
    let has_bundle_state_changes = accounts_modified > 0;

    let state_provider = db.database.as_ref();

    let state_root_start = Instant::now();
    let hashed_state = state_provider.hashed_post_state(&bundle_update);
    let changed_storage_tries = hashed_state.storages.keys().copied().collect::<HashSet<_>>();
    let mut trie_node_counts = count_state_root_leaf_nodes(&hashed_state);

    let (_, trie_updates) = state_provider.state_root_with_updates(hashed_state)?;
    if has_bundle_state_changes {
        add_state_root_trie_update_counts(
            &mut trie_node_counts,
            &changed_storage_tries,
            &trie_updates,
        );
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
    use base_test_utils::{Account, ContractFactory, SimpleStorage};
    use eyre::Context;
    use reth_provider::StateProviderFactory;
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;

    fn create_parsed_bundle(txs: Vec<BaseTransactionSigned>) -> eyre::Result<ParsedBundle> {
        let txs: Vec<Bytes> = txs.iter().map(|tx| Bytes::from(tx.encoded_2718())).collect();

        let bundle = Bundle {
            txs,
            block_number: 0,
            sub_block_number_min: None,
            sub_block_number_max: None,
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

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

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

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

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

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

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
    async fn meter_bundle_opcode_gas_for_storage_write() -> eyre::Result<()> {
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

        let metered = MeteredOpcodes::parse(&["SSTORE".to_string(), "SLOAD".to_string()]).unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 1);
        let tx_opcodes = &output.results[0].opcode_gas;
        assert!(!tx_opcodes.is_empty(), "storage write should produce opcode gas data");

        let sstore = tx_opcodes.iter().find(|o| o.opcode == "SSTORE");
        assert!(sstore.is_some(), "SSTORE should appear in opcode gas results");
        let sstore = sstore.unwrap();
        assert!(sstore.count > 0, "SSTORE count should be non-zero");
        assert!(sstore.gas_used > 0, "SSTORE gas_used should be non-zero");

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_for_create() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (factory_deployment_tx, factory_address, _) =
            Account::Deployer.create_deployment_tx(ContractFactory::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![factory_deployment_tx]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(factory_address)
            .gas_limit(1_000_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(
                ContractFactory::deployWithCreateCall { bytecode: SimpleStorage::BYTECODE.clone() }
                    .abi_encode(),
            )
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let metered = MeteredOpcodes::parse(&["CREATE".to_string()]).unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 1);
        let create = output.results[0]
            .opcode_gas
            .iter()
            .find(|o| o.opcode == "CREATE")
            .expect("CREATE should appear in opcode gas results for a factory deployment");
        assert!(create.count > 0, "CREATE count should be non-zero");
        assert!(create.gas_used > 0, "CREATE gas_used should be non-zero");

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_empty_when_disabled() -> eyre::Result<()> {
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

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

        assert!(
            output.results[0].opcode_gas.is_empty(),
            "opcode gas should be empty when no metered opcodes are configured"
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_filters_to_requested() -> eyre::Result<()> {
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

        // Only request SSTORE — other opcodes like PUSH, ADD, etc. should be filtered out.
        let metered = MeteredOpcodes::parse(&["SSTORE".to_string()]).unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        let tx_opcodes = &output.results[0].opcode_gas;
        for entry in tx_opcodes {
            assert_eq!(entry.opcode, "SSTORE", "only SSTORE should appear, found {}", entry.opcode);
        }
        assert!(!tx_opcodes.is_empty(), "SSTORE should appear in results");

        Ok(())
    }

    #[test]
    fn metered_opcodes_parse_rejects_unknown() {
        let result = MeteredOpcodes::parse(&["NOTAREALOPCODE".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("NOTAREALOPCODE"));
    }

    #[test]
    fn metered_opcodes_parse_case_insensitive() {
        let result = MeteredOpcodes::parse(&["sstore".to_string(), "Sload".to_string()]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().opcodes.len(), 2);
    }

    #[test]
    fn metered_opcodes_parse_recognizes_precompiles() {
        let result = MeteredOpcodes::parse(&[
            "SSTORE".to_string(),
            "BLAKE2F".to_string(),
            "ECREC".to_string(),
        ]);
        assert!(result.is_ok());
        let metered = result.unwrap();
        assert_eq!(metered.opcodes.len(), 1);
        assert_eq!(metered.precompiles.len(), 2);
        assert!(metered.precompiles.values().any(|n| n == "BLAKE2F"));
        assert!(metered.precompiles.values().any(|n| n == "ECREC"));
    }

    #[test]
    fn metered_opcodes_parse_recognizes_azul_additions() {
        // CLZ opcode (EIP-7939) and P256VERIFY precompile gas-cost change (EIP-7951)
        // are the new metering surfaces introduced by Azul.
        let result = MeteredOpcodes::parse(&["CLZ".to_string(), "P256VERIFY".to_string()]).unwrap();
        assert_eq!(result.opcodes.len(), 1, "CLZ should be recognized as an opcode");
        assert!(result.precompiles.values().any(|n| n == "P256VERIFY"));
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

        // Mimic a caller-provided header that lacks the parent beacon block root.
        let mut header_without_root = header.clone_header();
        header_without_root.parent_beacon_block_root = None;
        let sealed_without_root = SealedHeader::new(header_without_root, header.hash());

        let err = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle.clone(),
            header: sealed_without_root.clone(),
            parent_beacon_block_root: None,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })
        .expect_err("missing parent beacon block root should fail");
        assert!(
            err.to_string().to_lowercase().contains("parent beacon block root"),
            "expected missing parent beacon block root error, got {err:?}"
        );

        let state_provider2 = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let output = meter_bundle(MeterBundleInput {
            state_provider: state_provider2,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: sealed_without_root,
            parent_beacon_block_root: Some(header.parent_beacon_block_root().unwrap_or(B256::ZERO)),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

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

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

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

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

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

        let result = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        });

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

        let result = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        });

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

        let result = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        });

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

        let result = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header: header.clone(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        });

        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Insufficient funds"),
            "Expected insufficient funds error"
        );

        Ok(())
    }
}
