//! Deterministic instruction-count benchmark for assembling a Base mainnet-shaped block.
//!
//! Counts [`BaseBlockAssembler::assemble_block`], which turns an executed block into its header
//! and body: the transactions root, the receipts root (`calculate_receipt_root_no_memo`), the
//! logs bloom, and the Isthmus withdrawals root (the `L2ToL1MessagePasser` storage root). The
//! native payload builder runs it once per built block, about 4×10^5 blocks per day after the
//! Denim upgrade.
//!
//! The block is the one `block_execution_iai.rs` executes (`fixtures/mainnet_block.rs`,
//! `MAINNET_MIX`), 387 transactions. Executing it
//! and collecting its receipts, transactions, and bundle state happen in iai's unmeasured setup,
//! and the inputs and the assembled block are handed back so they drop outside the counted
//! region. The state root is computed before assembly in production, so it is passed in.
//!
//! The withdrawals root reads the predeploy's storage root from the state provider. Production
//! walks the database storage trie there; this bench uses [`MessagePasserStorage`], an in-memory
//! stand-in that hashes the pre-block storage with the block's updates. Its cost is part of the
//! count but is not production cost: it is small next to the receipts root.
//!
//! The teardown pins the header roots, bloom, and gas fields. They are fixed by the workload, so
//! a mismatch means assembly now produces a different header, and the instruction count is not
//! comparable with the base commit's.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

#[path = "fixtures/mainnet_block.rs"]
mod fixture;

use std::hint::black_box;

use alloy_consensus::{Block, Header};
use alloy_primitives::{
    Address, B256, BlockNumber, Bytes, StorageKey, StorageValue, U256, b256, keccak256,
    map::B256Map,
};
use base_common_consensus::{BaseReceipt, BaseTransactionSigned, Predeploys};
use base_common_evm::{BaseBlockExecutionCtx, BaseSpecId};
use base_execution_evm::BaseEvmConfig;
use fixture::{MAINNET_MIX, MainnetBlock};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use reth_evm::{ConfigureEvm, EvmEnv, execute::BlockAssemblerInput};
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{Account, Bytecode, SealedHeader};
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateRootProvider, StorageRootProvider, errors::ProviderResult,
};
use reth_trie_common::{
    AccountProof, DecodedMultiProofV2, ExecutionWitnessMode, HashedPostState, HashedStorage,
    MultiProof, MultiProofTargets, MultiProofTargetsV2, StorageMultiProof, StorageProof, TrieInput,
    root::storage_root_unsorted, updates::TrieUpdates,
};
use revm::database::{BundleState, InMemoryDB, states::bundle_state::BundleRetention};

/// The block executor factory whose assembly input [`BaseEvmConfig`] builds.
type Factory = <BaseEvmConfig as ConfigureEvm>::BlockExecutorFactory;

/// In-memory stand-in for the builder's state provider, answering only the
/// `L2ToL1MessagePasser` storage root the Isthmus withdrawals root asks for.
///
/// Hand-rolled because `StateProvider` is an external reth trait whose test double,
/// `StateProviderTest`, does not implement storage roots, and a database-backed provider would
/// bring I/O into the count. Every other method is unreachable from block assembly.
#[derive(Debug)]
struct MessagePasserStorage {
    /// Pre-block storage of the predeploy, keyed by hashed slot.
    hashed_storage: B256Map<U256>,
}

impl MessagePasserStorage {
    /// Captures the predeploy's pre-block storage from the block's pre-state.
    fn new(pre_state: &InMemoryDB) -> Self {
        let hashed_storage = pre_state.cache.accounts[&Predeploys::L2_TO_L1_MESSAGE_PASSER]
            .storage
            .iter()
            .map(|(slot, value)| (keccak256(B256::from(*slot)), *value))
            .collect();
        Self { hashed_storage }
    }
}

impl StorageRootProvider for MessagePasserStorage {
    fn storage_root(&self, address: Address, updates: HashedStorage) -> ProviderResult<B256> {
        assert_eq!(address, Predeploys::L2_TO_L1_MESSAGE_PASSER, "only the withdrawals root");
        let mut storage = self.hashed_storage.clone();
        storage.extend(updates.storage);
        Ok(storage_root_unsorted(storage.into_iter().filter(|(_, value)| !value.is_zero())))
    }

    fn storage_proof(&self, _: Address, _: B256, _: HashedStorage) -> ProviderResult<StorageProof> {
        unreachable!("block assembly reads no storage proofs")
    }

    fn storage_multiproof(
        &self,
        _: Address,
        _: &[B256],
        _: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        unreachable!("block assembly reads no storage proofs")
    }
}

impl AccountReader for MessagePasserStorage {
    fn basic_account(&self, _: &Address) -> ProviderResult<Option<Account>> {
        unreachable!("block assembly reads no accounts")
    }
}

impl BlockHashReader for MessagePasserStorage {
    fn block_hash(&self, _: u64) -> ProviderResult<Option<B256>> {
        unreachable!("block assembly reads no block hashes")
    }

    fn canonical_hashes_range(&self, _: BlockNumber, _: BlockNumber) -> ProviderResult<Vec<B256>> {
        unreachable!("block assembly reads no block hashes")
    }
}

impl BytecodeReader for MessagePasserStorage {
    fn bytecode_by_hash(&self, _: &B256) -> ProviderResult<Option<Bytecode>> {
        unreachable!("block assembly reads no bytecode")
    }
}

impl StateRootProvider for MessagePasserStorage {
    fn state_root(&self, _: HashedPostState) -> ProviderResult<B256> {
        unreachable!("the state root is computed before assembly")
    }

    fn state_root_from_nodes(&self, _: TrieInput) -> ProviderResult<B256> {
        unreachable!("the state root is computed before assembly")
    }

    fn state_root_with_updates(&self, _: HashedPostState) -> ProviderResult<(B256, TrieUpdates)> {
        unreachable!("the state root is computed before assembly")
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        unreachable!("the state root is computed before assembly")
    }
}

impl StateProofProvider for MessagePasserStorage {
    fn proof(&self, _: TrieInput, _: Address, _: &[B256]) -> ProviderResult<AccountProof> {
        unreachable!("block assembly reads no proofs")
    }

    fn multiproof(&self, _: TrieInput, _: MultiProofTargets) -> ProviderResult<MultiProof> {
        unreachable!("block assembly reads no proofs")
    }

    fn multiproof_v2(
        &self,
        _: TrieInput,
        _: MultiProofTargetsV2,
    ) -> ProviderResult<DecodedMultiProofV2> {
        unreachable!("block assembly reads no proofs")
    }

    fn witness(
        &self,
        _: TrieInput,
        _: HashedPostState,
        _: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        unreachable!("block assembly reads no witnesses")
    }
}

impl HashedPostStateProvider for MessagePasserStorage {
    fn hashed_post_state(&self, _: &BundleState) -> ProviderResult<HashedPostState> {
        unreachable!("block assembly hashes no post state")
    }
}

impl StateProvider for MessagePasserStorage {
    fn storage(&self, _: Address, _: StorageKey) -> ProviderResult<Option<StorageValue>> {
        unreachable!("block assembly reads no storage")
    }
}

/// An executed block ready to assemble.
struct Input {
    /// The fixture, whose EVM configuration holds the production assembler.
    block: MainnetBlock,
    evm_env: EvmEnv<BaseSpecId>,
    ctx: BaseBlockExecutionCtx,
    parent: SealedHeader,
    transactions: Vec<BaseTransactionSigned>,
    result: BlockExecutionResult<BaseReceipt>,
    bundle_state: BundleState,
    provider: MessagePasserStorage,
}

/// The inputs assembly borrows, handed back so they drop outside the counted region.
type Leftovers = (
    MainnetBlock,
    SealedHeader,
    BlockExecutionResult<BaseReceipt>,
    BundleState,
    MessagePasserStorage,
);

/// Header fields the workload fixes.
#[derive(Debug, PartialEq, Eq)]
struct HeaderPins {
    transactions: usize,
    transactions_root: B256,
    receipts_root: B256,
    logs_bloom_hash: B256,
    withdrawals_root: Option<B256>,
    gas_used: u64,
    blob_gas_used: Option<u64>,
}

/// Executes the block and collects what assembly consumes outside the counted region.
fn setup() -> Input {
    let block = MainnetBlock::new(MAINNET_MIX);
    let mut state = block.fresh_state();
    let result = block.execute(&mut state, block.block_txs());
    state.merge_transitions(BundleRetention::Reverts);
    Input {
        evm_env: block.evm_env.clone(),
        ctx: block.ctx.clone(),
        parent: SealedHeader::default(),
        transactions: block.transactions.iter().map(|tx| tx.1.inner().clone()).collect(),
        result,
        bundle_state: state.take_bundle(),
        provider: MessagePasserStorage::new(&block.pre_state),
        block,
    }
}

/// Checks the assembled header against the workload's documented roots.
fn check_header((assembled, _): (Block<BaseTransactionSigned>, Leftovers)) {
    let header: &Header = &assembled.header;
    let pins = HeaderPins {
        transactions: assembled.body.transactions.len(),
        transactions_root: header.transactions_root,
        receipts_root: header.receipts_root,
        logs_bloom_hash: keccak256(header.logs_bloom),
        withdrawals_root: header.withdrawals_root,
        gas_used: header.gas_used,
        blob_gas_used: header.blob_gas_used,
    };
    let expected = HeaderPins {
        transactions: 387,
        transactions_root: b256!(
            "0x92bc15d75f5a2731b5452ea2156b0de679e8f4e1cfeafbdf194170707bb7b3e9"
        ),
        receipts_root: b256!("0x40a4dd5ecbfab5a9b81b745aab89f4df251f18a58bb5fdc3edd5de6e3bd79edf"),
        logs_bloom_hash: b256!(
            "0x33a6c9c78a372a275deffa0ec74297b61f0e0eda0c7885dcdee7ad0f04a046ef"
        ),
        withdrawals_root: Some(b256!(
            "0xe3fedf7b70d2b6e2e9e58084eae481130a433995b50a912c9b4efed076e52948"
        )),
        gas_used: 13_228_002,
        blob_gas_used: Some(45_874_000),
    };
    assert_eq!(pins, expected, "block_assembly/mainnet_block header changed");
}

#[library_benchmark]
#[bench::mainnet_block(setup = setup, teardown = check_header)]
fn assemble_block(input: Input) -> (Block<BaseTransactionSigned>, Leftovers) {
    let Input { block, evm_env, ctx, parent, transactions, result, bundle_state, provider } = input;
    let assembled = block
        .evm_config
        .block_assembler()
        .assemble_block(BlockAssemblerInput::<Factory, Header>::new(
            evm_env,
            ctx,
            &parent,
            transactions,
            &result,
            &bundle_state,
            &provider,
            B256::ZERO,
            None,
        ))
        .expect("the executed block assembles");
    (black_box(assembled), (block, parent, result, bundle_state, provider))
}

library_benchmark_group!(
    name = block_assembly;
    benchmarks = assemble_block
);

main!(library_benchmark_groups = block_assembly);
