//! State provider for an active [`BaseProofsBatchSession`] enabling reads to observe
//! uncommitted writes performed earlier in the same session.

use std::fmt::Debug;

use alloy_primitives::{
    keccak256,
    map::{B256Map, HashMap},
};
use derive_more::Constructor;
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, ProviderError,
    ProviderResult, StateProofProvider, StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_revm::{
    db::BundleState,
    primitives::{Address, B256, Bytes, StorageValue, alloy_primitives::BlockNumber},
};
use reth_trie::{
    StateRoot, StorageRoot, TrieType,
    hashed_cursor::{HashedCursor, HashedPostStateCursorFactory},
    metrics::TrieRootMetrics,
    proof,
    trie_cursor::InMemoryTrieCursorFactory,
    witness::TrieWitness,
};
use reth_trie_common::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedPostStateSorted, HashedStorage,
    KeccakKeyHasher, MultiProof, MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};

use crate::{
    BaseProofsBatchHashedAccountCursorFactory, BaseProofsBatchTrieCursorFactory,
    api::BaseProofsBatchSession,
};

/// State provider that reads through an active [`BaseProofsBatchSession`]'s transaction.
#[derive(Constructor)]
pub struct BaseProofsBatchStateProviderRef<'a, S: BaseProofsBatchSession> {
    latest: Box<dyn StateProvider + Send + 'a>,
    session: &'a S,
    block_number: BlockNumber,
}

impl<S> Debug for BaseProofsBatchStateProviderRef<'_, S>
where
    S: BaseProofsBatchSession,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseProofsBatchStateProviderRef")
            .field("session", &self.session)
            .field("block_number", &self.block_number)
            .finish()
    }
}

impl<'a, S: BaseProofsBatchSession> BaseProofsBatchStateProviderRef<'a, S> {
    const fn factories(
        &self,
    ) -> (BaseProofsBatchTrieCursorFactory<'a, S>, BaseProofsBatchHashedAccountCursorFactory<'a, S>)
    {
        (
            BaseProofsBatchTrieCursorFactory::new(self.session, self.block_number),
            BaseProofsBatchHashedAccountCursorFactory::new(self.session, self.block_number),
        )
    }
}

impl<S: BaseProofsBatchSession> BlockHashReader for BaseProofsBatchStateProviderRef<'_, S> {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.latest.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.latest.canonical_hashes_range(start, end)
    }
}

impl<S: BaseProofsBatchSession> StateRootProvider for BaseProofsBatchStateProviderRef<'_, S> {
    fn state_root(&self, state: HashedPostState) -> ProviderResult<B256> {
        let prefix_sets = state.construct_prefix_sets().freeze();
        let state_sorted = state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StateRoot::new(
            trie_factory,
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(prefix_sets)
        .root()
        .map_err(ProviderError::from)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        let state_sorted = input.state.into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StateRoot::new(
            InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted),
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root()
        .map_err(ProviderError::from)
    }

    fn state_root_with_updates(
        &self,
        state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let prefix_sets = state.construct_prefix_sets().freeze();
        let state_sorted = state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StateRoot::new(
            trie_factory,
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(prefix_sets)
        .root_with_updates()
        .map_err(ProviderError::from)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let state_sorted = input.state.into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StateRoot::new(
            InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted),
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root_with_updates()
        .map_err(ProviderError::from)
    }
}

impl<S: BaseProofsBatchSession> StorageRootProvider for BaseProofsBatchStateProviderRef<'_, S> {
    fn storage_root(&self, address: Address, storage: HashedStorage) -> ProviderResult<B256> {
        let prefix_set = storage.construct_prefix_set().freeze();
        let state_sorted =
            HashedPostState::from_hashed_storage(keccak256(address), storage).into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        StorageRoot::new(
            trie_factory,
            HashedPostStateCursorFactory::new(hashed_factory, &state_sorted),
            address,
            prefix_set,
            TrieRootMetrics::new(TrieType::Custom("base_historical_proofs_storage_batch")),
        )
        .root()
        .map_err(|err| ProviderError::Database(err.into()))
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        let hashed_address = keccak256(address);
        let prefix_set = hashed_storage.construct_prefix_set();
        let state_sorted = HashedPostStateSorted::new(
            Default::default(),
            HashMap::from_iter([(hashed_address, hashed_storage.into_sorted())]),
        );
        let (trie_factory, hashed_factory) = self.factories();
        proof::StorageProof::new(trie_factory, hashed_factory.clone(), address)
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_set_mut(prefix_set)
            .storage_proof(slot)
            .map_err(ProviderError::from)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        let hashed_address = keccak256(address);
        let targets = slots.iter().map(keccak256).collect();
        let prefix_set = hashed_storage.construct_prefix_set();
        let state_sorted = HashedPostStateSorted::new(
            Default::default(),
            HashMap::from_iter([(hashed_address, hashed_storage.into_sorted())]),
        );
        let (trie_factory, hashed_factory) = self.factories();
        proof::StorageProof::new(trie_factory, hashed_factory.clone(), address)
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_set_mut(prefix_set)
            .storage_multiproof(targets)
            .map_err(ProviderError::from)
    }
}

impl<S: BaseProofsBatchSession> StateProofProvider for BaseProofsBatchStateProviderRef<'_, S> {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        proof::Proof::new(trie_factory.clone(), hashed_factory.clone())
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .account_proof(address, slots)
            .map_err(ProviderError::from)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        proof::Proof::new(trie_factory.clone(), hashed_factory.clone())
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .multiproof(targets)
            .map_err(ProviderError::from)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        _mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let (trie_factory, hashed_factory) = self.factories();
        let result: B256Map<Bytes> = TrieWitness::new(trie_factory.clone(), hashed_factory.clone())
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                hashed_factory,
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .always_include_root_node()
            .compute(target)
            .map_err(ProviderError::from)?;
        Ok(result.into_values().collect())
    }
}

impl<S: BaseProofsBatchSession> HashedPostStateProvider for BaseProofsBatchStateProviderRef<'_, S> {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<S: BaseProofsBatchSession> AccountReader for BaseProofsBatchStateProviderRef<'_, S> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let hashed_key = keccak256(address.0);
        Ok(self
            .session
            .account_hashed_cursor(self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, account)| (key == hashed_key).then_some(account)))
    }
}

impl<S: BaseProofsBatchSession> StateProvider for BaseProofsBatchStateProviderRef<'_, S> {
    fn storage(&self, address: Address, storage_key: B256) -> ProviderResult<Option<StorageValue>> {
        let hashed_key = keccak256(storage_key);
        Ok(self
            .session
            .storage_hashed_cursor(keccak256(address.0), self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, storage)| (key == hashed_key).then_some(storage)))
    }
}

impl<S: BaseProofsBatchSession> BytecodeReader for BaseProofsBatchStateProviderRef<'_, S> {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.latest.bytecode_by_hash(code_hash)
    }
}
