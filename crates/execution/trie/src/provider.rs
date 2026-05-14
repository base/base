//! Provider for external proofs storage

use std::fmt::Debug;

use alloy_primitives::keccak256;
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
    StateRoot, StorageRoot,
    hashed_cursor::{HashedCursor, HashedPostStateCursorFactory},
    proof::{self, Proof},
    trie_cursor::InMemoryTrieCursorFactory,
    witness::TrieWitness,
};
use reth_trie_common::{
    AccountProof, HashedPostState, HashedStorage, KeccakKeyHasher, MultiProof, MultiProofTargets,
    StorageMultiProof, StorageProof, TrieInput, updates::TrieUpdates,
};

use crate::{
    BaseProofsHashedAccountCursorFactory, BaseProofsStorage, BaseProofsStorageError,
    BaseProofsStore, BaseProofsTrieCursorFactory, ProofsBatchOverlay,
    proof::{
        DatabaseProof, DatabaseStateRoot, DatabaseStorageProof, DatabaseStorageRoot,
        DatabaseTrieWitness,
    },
};

/// State provider for external proofs storage.
#[derive(Constructor)]
pub struct BaseProofsStateProviderRef<'a, Storage: BaseProofsStore> {
    /// Historical state provider for non-state related tasks.
    latest: Box<dyn StateProvider + Send + 'a>,

    /// Storage provider for state lookups.
    storage: &'a BaseProofsStorage<Storage>,

    /// Max block number that can be used for state lookups.
    block_number: BlockNumber,
}

/// Overlay-aware state provider for batched proofs sync execution.
#[derive(Constructor)]
pub struct OverlayedBaseProofsStateProviderRef<'a, Storage: BaseProofsStore> {
    /// Historical state provider for non-state related tasks.
    latest: Box<dyn StateProvider + Send + 'a>,

    /// Storage provider for persisted state lookups.
    storage: &'a BaseProofsStorage<Storage>,

    /// Max block number that can be used for persisted state lookups.
    block_number: BlockNumber,

    /// Batch-local parent-state read overlay.
    overlay: &'a ProofsBatchOverlay,
}

impl<'a, Storage> Debug for BaseProofsStateProviderRef<'a, Storage>
where
    Storage: BaseProofsStore + 'a + Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseProofsStateProviderRef")
            .field("storage", &self.storage)
            .field("block_number", &self.block_number)
            .finish()
    }
}

impl<'a, Storage> Debug for OverlayedBaseProofsStateProviderRef<'a, Storage>
where
    Storage: BaseProofsStore + 'a + Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayedBaseProofsStateProviderRef")
            .field("storage", &self.storage)
            .field("block_number", &self.block_number)
            .field("overlay_len", &self.overlay.len())
            .finish()
    }
}

impl From<BaseProofsStorageError> for ProviderError {
    fn from(error: BaseProofsStorageError) -> Self {
        Self::other(error)
    }
}

impl<'a, Storage: BaseProofsStore> BlockHashReader for BaseProofsStateProviderRef<'a, Storage> {
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

impl<'a, Storage: BaseProofsStore + Clone> StateRootProvider
    for BaseProofsStateProviderRef<'a, Storage>
{
    fn state_root(&self, state: HashedPostState) -> ProviderResult<B256> {
        Ok(StateRoot::overlay_root(self.storage, self.block_number, state)?)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        Ok(StateRoot::overlay_root_from_nodes(self.storage, self.block_number, input)?)
    }

    fn state_root_with_updates(
        &self,
        state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok(StateRoot::overlay_root_with_updates(self.storage, self.block_number, state)?)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok(StateRoot::overlay_root_from_nodes_with_updates(self.storage, self.block_number, input)?)
    }
}

impl<'a, Storage: BaseProofsStore + Clone> StorageRootProvider
    for BaseProofsStateProviderRef<'a, Storage>
{
    fn storage_root(&self, address: Address, storage: HashedStorage) -> ProviderResult<B256> {
        StorageRoot::overlay_root(self.storage, self.block_number, address, storage)
            .map_err(|err| ProviderError::Database(err.into()))
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        proof::StorageProof::overlay_storage_proof(
            self.storage,
            self.block_number,
            address,
            slot,
            storage,
        )
        .map_err(ProviderError::from)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        proof::StorageProof::overlay_storage_multiproof(
            self.storage,
            self.block_number,
            address,
            slots,
            storage,
        )
        .map_err(ProviderError::from)
    }
}

impl<'a, Storage: BaseProofsStore + Clone> StateProofProvider
    for BaseProofsStateProviderRef<'a, Storage>
{
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Proof::overlay_account_proof(self.storage, self.block_number, input, address, slots)
            .map_err(ProviderError::from)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Proof::overlay_multiproof(self.storage, self.block_number, input, targets)
            .map_err(ProviderError::from)
    }

    fn witness(&self, input: TrieInput, target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        TrieWitness::overlay_witness(self.storage, self.block_number, input, target)
            .map_err(ProviderError::from)
            .map(|hm| hm.into_values().collect())
    }
}

impl<'a, Storage: BaseProofsStore> HashedPostStateProvider
    for BaseProofsStateProviderRef<'a, Storage>
{
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<'a, Storage: BaseProofsStore> AccountReader for BaseProofsStateProviderRef<'a, Storage> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let hashed_key = keccak256(address.0);
        Ok(self
            .storage
            .account_hashed_cursor(self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, account)| (key == hashed_key).then_some(account)))
    }
}

impl<'a, Storage> StateProvider for BaseProofsStateProviderRef<'a, Storage>
where
    Storage: BaseProofsStore + Clone,
{
    fn storage(&self, address: Address, storage_key: B256) -> ProviderResult<Option<StorageValue>> {
        let hashed_key = keccak256(storage_key);
        self.storage_by_hashed_key(address, hashed_key)
    }

    fn storage_by_hashed_key(
        &self,
        address: Address,
        hashed_key: B256,
    ) -> ProviderResult<Option<StorageValue>> {
        Ok(self
            .storage
            .storage_hashed_cursor(keccak256(address.0), self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, storage)| (key == hashed_key).then_some(storage)))
    }
}

impl<'a, Storage: BaseProofsStore> BytecodeReader for BaseProofsStateProviderRef<'a, Storage> {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.latest.bytecode_by_hash(code_hash)
    }
}

impl<'a, Storage: BaseProofsStore> BlockHashReader
    for OverlayedBaseProofsStateProviderRef<'a, Storage>
{
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

impl<'a, Storage: BaseProofsStore + Clone> StateRootProvider
    for OverlayedBaseProofsStateProviderRef<'a, Storage>
{
    fn state_root(&self, state: HashedPostState) -> ProviderResult<B256> {
        let prefix_sets = state.construct_prefix_sets().freeze();
        let overlay_nodes_sorted = self.overlay.parent_trie().clone_into_sorted();
        let overlay_state_sorted = self.overlay.read_state().clone_into_sorted();
        let state_sorted = state.into_sorted();
        let tx = self.storage.ro_tx().map_err(Into::<ProviderError>::into)?;

        Ok(StateRoot::new(
            InMemoryTrieCursorFactory::new(
                BaseProofsTrieCursorFactory::new(self.storage, &tx, self.block_number),
                &overlay_nodes_sorted,
            ),
            HashedPostStateCursorFactory::new(
                HashedPostStateCursorFactory::new(
                    BaseProofsHashedAccountCursorFactory::new(self.storage, &tx, self.block_number),
                    &overlay_state_sorted,
                ),
                &state_sorted,
            ),
        )
        .with_prefix_sets(prefix_sets)
        .root()?)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        let overlay_nodes_sorted = self.overlay.parent_trie().clone_into_sorted();
        let overlay_state_sorted = self.overlay.read_state().clone_into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let tx = self.storage.ro_tx().map_err(Into::<ProviderError>::into)?;

        Ok(StateRoot::new(
            InMemoryTrieCursorFactory::new(
                InMemoryTrieCursorFactory::new(
                    BaseProofsTrieCursorFactory::new(self.storage, &tx, self.block_number),
                    &overlay_nodes_sorted,
                ),
                &nodes_sorted,
            ),
            HashedPostStateCursorFactory::new(
                HashedPostStateCursorFactory::new(
                    BaseProofsHashedAccountCursorFactory::new(self.storage, &tx, self.block_number),
                    &overlay_state_sorted,
                ),
                &state_sorted,
            ),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root()?)
    }

    fn state_root_with_updates(
        &self,
        state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let prefix_sets = state.construct_prefix_sets().freeze();
        let overlay_nodes_sorted = self.overlay.parent_trie().clone_into_sorted();
        let overlay_state_sorted = self.overlay.read_state().clone_into_sorted();
        let state_sorted = state.into_sorted();
        let tx = self.storage.ro_tx().map_err(Into::<ProviderError>::into)?;

        Ok(StateRoot::new(
            InMemoryTrieCursorFactory::new(
                BaseProofsTrieCursorFactory::new(self.storage, &tx, self.block_number),
                &overlay_nodes_sorted,
            ),
            HashedPostStateCursorFactory::new(
                HashedPostStateCursorFactory::new(
                    BaseProofsHashedAccountCursorFactory::new(self.storage, &tx, self.block_number),
                    &overlay_state_sorted,
                ),
                &state_sorted,
            ),
        )
        .with_prefix_sets(prefix_sets)
        .root_with_updates()?)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let overlay_nodes_sorted = self.overlay.parent_trie().clone_into_sorted();
        let overlay_state_sorted = self.overlay.read_state().clone_into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        let tx = self.storage.ro_tx().map_err(Into::<ProviderError>::into)?;

        Ok(StateRoot::new(
            InMemoryTrieCursorFactory::new(
                InMemoryTrieCursorFactory::new(
                    BaseProofsTrieCursorFactory::new(self.storage, &tx, self.block_number),
                    &overlay_nodes_sorted,
                ),
                &nodes_sorted,
            ),
            HashedPostStateCursorFactory::new(
                HashedPostStateCursorFactory::new(
                    BaseProofsHashedAccountCursorFactory::new(self.storage, &tx, self.block_number),
                    &overlay_state_sorted,
                ),
                &state_sorted,
            ),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root_with_updates()?)
    }
}

impl<'a, Storage: BaseProofsStore + Clone> StorageRootProvider
    for OverlayedBaseProofsStateProviderRef<'a, Storage>
{
    fn storage_root(&self, _address: Address, _storage: HashedStorage) -> ProviderResult<B256> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl<'a, Storage: BaseProofsStore + Clone> StateProofProvider
    for OverlayedBaseProofsStateProviderRef<'a, Storage>
{
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn witness(&self, _input: TrieInput, _target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl<'a, Storage: BaseProofsStore> HashedPostStateProvider
    for OverlayedBaseProofsStateProviderRef<'a, Storage>
{
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<'a, Storage: BaseProofsStore> AccountReader
    for OverlayedBaseProofsStateProviderRef<'a, Storage>
{
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let hashed_key = keccak256(address.0);
        if let Some(account) = self.overlay.read_state().accounts.get(&hashed_key) {
            return Ok(*account);
        }

        Ok(self
            .storage
            .account_hashed_cursor(self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, account)| (key == hashed_key).then_some(account)))
    }
}

impl<'a, Storage> StateProvider for OverlayedBaseProofsStateProviderRef<'a, Storage>
where
    Storage: BaseProofsStore + Clone,
{
    fn storage(&self, address: Address, storage_key: B256) -> ProviderResult<Option<StorageValue>> {
        let hashed_key = keccak256(storage_key);
        self.storage_by_hashed_key(address, hashed_key)
    }

    fn storage_by_hashed_key(
        &self,
        address: Address,
        hashed_key: B256,
    ) -> ProviderResult<Option<StorageValue>> {
        let hashed_address = keccak256(address.0);
        if let Some(storage) = self.overlay.read_state().storages.get(&hashed_address) {
            if let Some(value) = storage.storage.get(&hashed_key) {
                return Ok(Some(*value));
            }
            if storage.wiped {
                return Ok(Some(StorageValue::ZERO));
            }
        }

        Ok(self
            .storage
            .storage_hashed_cursor(hashed_address, self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, storage)| (key == hashed_key).then_some(storage)))
    }
}

impl<'a, Storage: BaseProofsStore> BytecodeReader
    for OverlayedBaseProofsStateProviderRef<'a, Storage>
{
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.latest.bytecode_by_hash(code_hash)
    }
}

#[cfg(all(test, not(feature = "metrics")))]
mod tests {
    use alloy_eips::eip1898::BlockWithParent;
    use alloy_primitives::U256;
    use reth_provider::noop::NoopProvider;

    use super::*;
    use crate::InMemoryProofsStorage;

    #[test]
    fn test_base_proofs_state_provider_ref_debug() {
        let latest: Box<dyn StateProvider + Send> = Box::<NoopProvider>::default();
        let storage: crate::BaseProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::new().into();
        let block_number = 42u64;

        let provider = BaseProofsStateProviderRef::new(latest, &storage, block_number);

        assert_eq!(
            format!("{:?}", provider),
            "BaseProofsStateProviderRef { storage: InMemoryProofsStorage { inner: RwLock { data: InMemoryStorageInner { account_branches: {}, storage_branches: {}, hashed_accounts: {}, hashed_storages: {}, trie_updates: {}, post_states: {}, earliest_block: None, anchor_block: None } } }, block_number: 42 }"
        );
    }

    #[test]
    fn test_overlayed_provider_returns_destroyed_account_from_overlay() {
        let latest: Box<dyn StateProvider + Send> = Box::<NoopProvider>::default();
        let storage: crate::BaseProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::new().into();
        let address = Address::repeat_byte(0x11);
        let mut state = HashedPostState::default();
        state.accounts.insert(keccak256(address.0), None);
        let mut overlay = ProofsBatchOverlay::new();
        overlay.append_executed(
            BlockWithParent::new(B256::ZERO, alloy_eips::NumHash::new(1, B256::repeat_byte(1))),
            state,
            TrieUpdates::default(),
        );
        let provider = OverlayedBaseProofsStateProviderRef::new(latest, &storage, 0, &overlay);

        assert_eq!(provider.basic_account(&address).unwrap(), None);
    }

    #[test]
    fn test_overlayed_provider_returns_zero_for_unwritten_slot_after_wipe() {
        let latest: Box<dyn StateProvider + Send> = Box::<NoopProvider>::default();
        let storage: crate::BaseProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::new().into();
        let address = Address::repeat_byte(0x22);
        let mut state = HashedPostState::default();
        state.storages.insert(keccak256(address.0), HashedStorage::new(true));
        let mut overlay = ProofsBatchOverlay::new();
        overlay.append_executed(
            BlockWithParent::new(B256::ZERO, alloy_eips::NumHash::new(1, B256::repeat_byte(1))),
            state,
            TrieUpdates::default(),
        );
        let provider = OverlayedBaseProofsStateProviderRef::new(latest, &storage, 0, &overlay);

        assert_eq!(
            provider.storage_by_hashed_key(address, B256::repeat_byte(0x33)).unwrap(),
            Some(U256::ZERO)
        );
    }
}
