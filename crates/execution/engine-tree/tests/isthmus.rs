//! Regression tests for Isthmus-specific engine-tree validation.

use std::sync::Arc;

use alloy_chains::Chain;
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, U256, keccak256};
use base_common_consensus::{
    BaseBlock, BasePrimitives, BaseReceipt, BaseTransactionSigned, Predeploys,
};
use base_engine_tree::PayloadValidatorWithState;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_node_core::{BaseEngineTypes, BaseNode, engine::BaseEngineValidator};
use reth_chain_state::{ComputedTrieData, ExecutedBlock};
use reth_db_common::init::init_genesis;
use reth_engine_primitives::PayloadValidator;
use reth_engine_tree::tree::StateProviderBuilder;
use reth_primitives_traits::{RecoveredBlock, SealedBlock};
use reth_provider::{
    BlockExecutionOutput, StateProviderFactory, providers::BlockchainProvider,
    test_utils::create_test_provider_factory_with_node_types,
};
use reth_revm::{bytecode::Bytecode, state::AccountInfo};
use reth_trie::{
    HashedPostState, HashedStorage, KeccakKeyHasher, test_utils::storage_root_prehashed,
};
use revm::database::BundleState;

#[test]
fn isthmus_withdrawals_root_validation_uses_in_memory_parent_overlay() {
    let chain_spec = Arc::new(
        BaseChainSpecBuilder::default()
            .chain(Chain::dev())
            .genesis(Default::default())
            .isthmus_activated()
            .build(),
    );
    let provider_factory =
        create_test_provider_factory_with_node_types::<BaseNode>(Arc::clone(&chain_spec));
    let genesis_hash = init_genesis(&provider_factory).expect("initialize genesis");
    let provider = BlockchainProvider::new(provider_factory).expect("create blockchain provider");

    let parent_block = recovered_empty_block(1, genesis_hash, None);
    let parent_hash = parent_block.hash();
    let (parent_execution_output, parent_hashed_state, parent_withdrawals_root) =
        parent_message_passer_update();
    let parent_executed_block = ExecutedBlock::<BasePrimitives>::new(
        Arc::new(parent_block),
        Arc::new(parent_execution_output),
        ComputedTrieData {
            hashed_state: Arc::new(parent_hashed_state.into_sorted()),
            ..Default::default()
        },
    );

    let child_block = recovered_empty_block(2, parent_hash, Some(parent_withdrawals_root));
    let child_state_updates = HashedPostState::default();
    let validator = BaseEngineValidator::<_, BaseTransactionSigned, BaseChainSpec>::new::<
        KeccakKeyHasher,
    >(Arc::clone(&chain_spec), provider.clone());

    let legacy_result =
        PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
            &validator,
            &child_state_updates,
            &child_block,
        );
    assert!(
        legacy_result.is_err(),
        "legacy validation should not find the unpersisted parent by hash"
    );
    assert!(
        provider.state_by_block_hash(parent_hash).is_err(),
        "test setup requires the parent to exist only in memory"
    );

    let parent_state_provider_builder =
        StateProviderBuilder::new(provider, genesis_hash, Some(vec![parent_executed_block]));
    let parent_state_provider =
        parent_state_provider_builder.build().expect("build overlay parent state provider");

    PayloadValidatorWithState::<BaseEngineTypes>::validate_block_post_execution_with_state(
        &validator,
        &child_state_updates,
        parent_state_provider,
        &child_block,
    )
    .expect("state-aware validation should use the in-memory parent overlay");
}

fn parent_message_passer_update() -> (BlockExecutionOutput<BaseReceipt>, HashedPostState, B256) {
    let storage_slots = [(U256::from(1), U256::from(11)), (U256::from(2), U256::from(22))];
    let parent_storage = HashedStorage::from_iter(
        false,
        storage_slots.iter().copied().map(|(slot, value)| (keccak256(B256::from(slot)), value)),
    );

    let mut parent_hashed_state = HashedPostState::default();
    parent_hashed_state
        .storages
        .insert(keccak256(Predeploys::L2_TO_L1_MESSAGE_PASSER), parent_storage.clone());

    let parent_bundle_state = BundleState::new(
        [(
            Predeploys::L2_TO_L1_MESSAGE_PASSER,
            None::<AccountInfo>,
            None::<AccountInfo>,
            storage_slots
                .iter()
                .copied()
                .map(|(slot, value)| (slot, (U256::ZERO, value)))
                .collect(),
        )],
        Vec::<Vec<(Address, Option<Option<AccountInfo>>, Vec<(U256, U256)>)>>::new(),
        Vec::<(B256, Bytecode)>::new(),
    );

    (
        BlockExecutionOutput { state: parent_bundle_state, ..Default::default() },
        parent_hashed_state,
        storage_root_prehashed(parent_storage.storage),
    )
}

fn recovered_empty_block(
    number: u64,
    parent_hash: B256,
    withdrawals_root: Option<B256>,
) -> RecoveredBlock<BaseBlock> {
    let header =
        Header { parent_hash, number, timestamp: number, withdrawals_root, ..Default::default() };
    let block = BaseBlock { header, body: Default::default() };

    RecoveredBlock::new_sealed(SealedBlock::seal_slow(block), Vec::new())
}
