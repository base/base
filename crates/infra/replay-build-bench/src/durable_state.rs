//! Durable accumulating canonical state for the replay loop.
//!
//! The replay harness advances canonical state in-process: it executes canonical block
//! `i` and must serve block `i + 1` the exact post-state of block `i`. The only writable
//! layer available over a read-only snapshot used to be the process-local
//! [`ExecutionCache`](reth_execution_cache::ExecutionCache), which is a *fixed-capacity,
//! collision-evicting* cache: an insert can drop an unrelated entry, and the hash seed is
//! process-local. Using it as the sole store of cross-block state therefore lost
//! accumulated canonical writes nondeterministically, and the next read fell through to
//! the frozen anchor at `from`, returning pre-replay values (observed as
//! `nonce N too high, expected N-1` aborts and receipts divergence that grew with
//! `--count`).
//!
//! [`DurableStateProvider`] fixes that by keeping every canonical bundle in an unbounded
//! in-process overlay layered directly on the frozen anchor. The `ExecutionCache` is then
//! layered *above* this provider purely as a read-through performance cache, exactly as in
//! production: evicting any cache entry is now harmless because the read falls through to
//! the durable overlay instead of the anchor.
//!
//! The overlay is held behind an [`Arc<RwLock<_>>`](std::sync::RwLock) so the same
//! canonical state can be observed by more than one provider: the replay loop's provider
//! writes it between blocks, and prewarm worker threads read it through their own
//! [`DurableStateProvider`] view built with [`DurableStateProvider::from_parts`] over an
//! independently opened anchor. Readers only ever take read guards, so a view always sees
//! a whole applied bundle, never a half-applied one.
//!
//! Trie-facing methods (state root, storage root, proofs, witnesses) delegate to the
//! anchor without overlay data, so state roots computed over replayed blocks are not
//! canonical. That is the pre-existing, documented harness caveat: built payloads are
//! discarded and correctness is anchored to canonical receipts and gas, which depend only
//! on the account/storage/bytecode reads served above.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, RwLock, RwLockReadGuard},
};

use alloy_primitives::{Address, B256, BlockNumber, Bytes, StorageKey, StorageValue};
use reth_primitives_traits::{Account, Bytecode};
use reth_revm::db::BundleState;
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateProviderBox, StateRootProvider, StorageRootProvider,
    errors::provider::ProviderResult,
};
use reth_trie::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput, updates::TrieUpdates,
};

/// Accumulated storage of one account.
#[derive(Debug, Default)]
struct StorageOverlay {
    /// Set once the account was destroyed: unknown slots read as empty instead of
    /// falling through to the anchor's pre-destruction storage.
    wiped: bool,
    /// Latest values of every slot written by a replayed block.
    slots: HashMap<StorageKey, StorageValue>,
}

/// Canonical post-state accumulated across replayed blocks.
///
/// Reads answer `Some(_)` when the overlay knows the value and `None` when the caller must
/// fall through to the anchor. The inner [`Option`] carries the value's own emptiness
/// (a destroyed account, or a wiped slot).
#[derive(Debug, Default)]
pub struct StateOverlay {
    /// Latest account info per address; `None` marks a destroyed/absent account.
    accounts: HashMap<Address, Option<Account>>,
    /// Latest storage per address.
    storages: HashMap<Address, StorageOverlay>,
    /// Bytecode deployed by replayed blocks, keyed by code hash.
    bytecodes: HashMap<B256, Bytecode>,
}

impl StateOverlay {
    /// Folds one canonical block's bundle into the overlay.
    ///
    /// Mirrors [`ExecutionCache::insert_state`](reth_execution_cache::ExecutionCache::insert_state):
    /// unmodified accounts carry no changes, destroyed accounts wipe their storage, and a
    /// modified account without account info is unrepresentable. Unlike the cache, nothing
    /// is ever evicted.
    ///
    /// Returns the offending address if the bundle is inconsistent.
    pub fn apply(&mut self, bundle: &BundleState) -> Result<(), Address> {
        for (code_hash, bytecode) in &bundle.contracts {
            self.bytecodes.insert(*code_hash, Bytecode(bytecode.clone()));
        }
        for (address, account) in &bundle.state {
            if account.status.is_not_modified() {
                continue;
            }
            if account.was_destroyed() {
                let storage = self.storages.entry(*address).or_default();
                storage.slots.clear();
                storage.wiped = true;
            } else if account.info.is_none() {
                // A modified account with no info must have been destroyed; anything else
                // is an inconsistent bundle we must not silently absorb.
                return Err(*address);
            }
            if !account.storage.is_empty() {
                let storage = self.storages.entry(*address).or_default();
                for (key, slot) in &account.storage {
                    storage.slots.insert((*key).into(), slot.present_value);
                }
            }
            self.accounts.insert(*address, account.info.as_ref().map(Account::from));
        }
        Ok(())
    }

    /// Returns the accumulated account, or `None` to defer to the anchor.
    pub fn account(&self, address: &Address) -> Option<Option<Account>> {
        self.accounts.get(address).copied()
    }

    /// Returns the accumulated storage value, or `None` to defer to the anchor.
    pub fn storage(&self, address: &Address, key: StorageKey) -> Option<Option<StorageValue>> {
        let storage = self.storages.get(address)?;
        storage.slots.get(&key).map_or_else(
            // Storage wiped by a self-destruct: every unknown slot is empty.
            || storage.wiped.then_some(None),
            |value| Some(Some(*value)),
        )
    }

    /// Returns bytecode deployed by a replayed block, or `None` to defer to the anchor.
    pub fn bytecode(&self, code_hash: &B256) -> Option<Bytecode> {
        self.bytecodes.get(code_hash).cloned()
    }

    /// Number of accounts held by the overlay.
    pub fn accounts_len(&self) -> usize {
        self.accounts.len()
    }

    /// Number of storage slots held by the overlay.
    pub fn slots_len(&self) -> usize {
        self.storages.values().map(|storage| storage.slots.len()).sum()
    }
}

/// A handle to the canonical post-state accumulated across replayed blocks.
///
/// Cloning the handle shares one overlay between the build loop and prewarm workers: the
/// loop writes it between blocks and workers only ever take read guards.
pub type SharedOverlay = Arc<RwLock<StateOverlay>>;

/// A [`StateProvider`] serving the canonical post-state of every replayed block from a
/// durable overlay, falling through to the frozen snapshot anchor.
///
/// The overlay is shared ([`SharedOverlay`]); the anchor is owned per provider. Sharing the
/// anchor is not possible: [`StateProviderBox`] is `Box<dyn StateProvider + Send>` and not
/// `Sync`, so an `Arc<StateProviderBox>` would itself be `!Send` and could not be handed to
/// a prewarm worker. Each provider therefore opens its own anchor (its own MDBX read
/// transaction) exactly as production prewarm workers each call
/// `client.state_by_block_hash(parent)`; see [`Self::from_parts`].
pub struct DurableStateProvider {
    /// Frozen historical state at the replay anchor.
    anchor: StateProviderBox,
    /// Canonical post-state accumulated by [`Self::apply`], shared with every handle
    /// built from the same overlay.
    overlay: SharedOverlay,
}

impl fmt::Debug for DurableStateProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let overlay = self.overlay();
        f.debug_struct("DurableStateProvider")
            .field("accounts", &overlay.accounts_len())
            .field("slots", &overlay.slots_len())
            .finish_non_exhaustive()
    }
}

impl DurableStateProvider {
    /// Wraps the frozen anchor state with a fresh, empty overlay.
    pub fn new(anchor: StateProviderBox) -> Self {
        Self::from_parts(anchor, SharedOverlay::default())
    }

    /// Wraps `anchor` with an existing shared overlay.
    ///
    /// Used to build a second, read-only view of the same canonical state over an
    /// independently opened anchor - for example inside a prewarm worker thread, which
    /// needs an owned `'static` [`StateProviderBox`] for the parent state of the block
    /// being built. Such a view must never [`apply`](Self::apply): the replay loop owns
    /// canonical advancement.
    pub fn from_parts(anchor: StateProviderBox, overlay: SharedOverlay) -> Self {
        Self { anchor, overlay }
    }

    /// Returns a handle to the shared overlay, for building further views with
    /// [`Self::from_parts`].
    pub fn overlay_handle(&self) -> SharedOverlay {
        Arc::clone(&self.overlay)
    }

    /// Folds one canonical block's bundle into the durable overlay.
    ///
    /// Returns the offending address if the bundle is inconsistent.
    pub fn apply(&self, bundle: &BundleState) -> Result<(), Address> {
        self.overlay.write().expect("durable overlay poisoned").apply(bundle)
    }

    /// Read access to the accumulated overlay.
    pub fn overlay(&self) -> RwLockReadGuard<'_, StateOverlay> {
        self.overlay.read().expect("durable overlay poisoned")
    }
}

impl AccountReader for DurableStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        if let Some(account) = self.overlay().account(address) {
            return Ok(account);
        }
        self.anchor.basic_account(address)
    }
}

impl StateProvider for DurableStateProvider {
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        if let Some(value) = self.overlay().storage(&account, storage_key) {
            return Ok(value);
        }
        self.anchor.storage(account, storage_key)
    }
}

impl BytecodeReader for DurableStateProvider {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        if let Some(bytecode) = self.overlay().bytecode(code_hash) {
            return Ok(Some(bytecode));
        }
        self.anchor.bytecode_by_hash(code_hash)
    }
}

impl BlockHashReader for DurableStateProvider {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        // The snapshot holds the canonical hashes of the whole replay range.
        self.anchor.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.anchor.canonical_hashes_range(start, end)
    }
}

// Trie-facing methods carry no overlay data; see the module documentation.
impl StateRootProvider for DurableStateProvider {
    fn state_root(&self, state: HashedPostState) -> ProviderResult<B256> {
        self.anchor.state_root(state)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        self.anchor.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.anchor.state_root_with_updates(state)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.anchor.state_root_from_nodes_with_updates(input)
    }
}

impl StorageRootProvider for DurableStateProvider {
    fn storage_root(&self, address: Address, storage: HashedStorage) -> ProviderResult<B256> {
        self.anchor.storage_root(address, storage)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        self.anchor.storage_proof(address, slot, storage)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        self.anchor.storage_multiproof(address, slots, storage)
    }
}

impl StateProofProvider for DurableStateProvider {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        self.anchor.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        self.anchor.multiproof(input, targets)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        self.anchor.witness(input, target, mode)
    }
}

impl HashedPostStateProvider for DurableStateProvider {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> ProviderResult<HashedPostState> {
        self.anchor.hashed_post_state(bundle_state)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use reth_revm::{
        db::{
            AccountStatus, BundleAccount, BundleState,
            states::{StorageSlot, plain_account::StorageWithOriginalValues},
        },
        state::AccountInfo,
    };
    use reth_storage_api::{AccountReader, StateProvider, noop::NoopProvider};

    use super::{DurableStateProvider, SharedOverlay, StateOverlay};

    /// A single-account bundle: `address` ends the block with `nonce` and one written slot.
    fn bundle(
        address: Address,
        status: AccountStatus,
        info: Option<AccountInfo>,
        slots: &[(U256, U256)],
    ) -> BundleState {
        let mut storage = StorageWithOriginalValues::default();
        for (key, value) in slots {
            storage.insert(*key, StorageSlot::new_changed(U256::ZERO, *value));
        }
        let mut bundle = BundleState::default();
        bundle.state.insert(address, BundleAccount::new(None, info, storage, status));
        bundle
    }

    /// Account info with the given nonce.
    fn info(nonce: u64) -> Option<AccountInfo> {
        Some(AccountInfo { nonce, balance: U256::from(7u64), ..Default::default() })
    }

    #[test]
    fn overlay_answers_before_anchor_and_defers_when_unknown() {
        let address = Address::with_last_byte(1);
        let slot = U256::from(3u64);
        let mut overlay = StateOverlay::default();
        overlay
            .apply(&bundle(address, AccountStatus::Changed, info(5), &[(slot, U256::from(9u64))]))
            .unwrap();

        // Known account and slot are answered by the overlay, so the anchor is bypassed.
        assert_eq!(overlay.account(&address).unwrap().unwrap().nonce, 5);
        assert_eq!(overlay.storage(&address, B256::from(slot)), Some(Some(U256::from(9u64))));
        // Unknown entries defer to the anchor.
        assert_eq!(overlay.account(&Address::with_last_byte(2)), None);
        assert_eq!(overlay.storage(&address, B256::with_last_byte(4)), None);
        assert_eq!(overlay.storage(&Address::with_last_byte(2), B256::from(slot)), None);
        assert_eq!(overlay.accounts_len(), 1);
        assert_eq!(overlay.slots_len(), 1);
    }

    #[test]
    fn later_bundles_supersede_earlier_ones() {
        let address = Address::with_last_byte(1);
        let slot = U256::from(3u64);
        let mut overlay = StateOverlay::default();
        overlay
            .apply(&bundle(address, AccountStatus::Changed, info(5), &[(slot, U256::from(9u64))]))
            .unwrap();
        overlay
            .apply(&bundle(address, AccountStatus::Changed, info(6), &[(slot, U256::from(10u64))]))
            .unwrap();

        assert_eq!(overlay.account(&address).unwrap().unwrap().nonce, 6);
        assert_eq!(overlay.storage(&address, B256::from(slot)), Some(Some(U256::from(10u64))));
    }

    #[test]
    fn unmodified_accounts_are_ignored() {
        let address = Address::with_last_byte(1);
        let mut overlay = StateOverlay::default();
        overlay.apply(&bundle(address, AccountStatus::Loaded, info(5), &[])).unwrap();
        assert_eq!(overlay.account(&address), None);
        assert_eq!(overlay.accounts_len(), 0);
    }

    #[test]
    fn destroyed_account_wipes_storage_instead_of_falling_through() {
        let address = Address::with_last_byte(1);
        let slot = U256::from(3u64);
        let mut overlay = StateOverlay::default();
        overlay
            .apply(&bundle(address, AccountStatus::Changed, info(5), &[(slot, U256::from(9u64))]))
            .unwrap();
        overlay.apply(&bundle(address, AccountStatus::Destroyed, None, &[])).unwrap();

        assert_eq!(overlay.account(&address), Some(None));
        // Wiped storage answers empty rather than deferring to the anchor.
        assert_eq!(overlay.storage(&address, B256::from(slot)), Some(None));
        assert_eq!(overlay.storage(&address, B256::with_last_byte(4)), Some(None));
    }

    #[test]
    fn modified_account_without_info_is_rejected() {
        let address = Address::with_last_byte(1);
        assert_eq!(
            StateOverlay::default().apply(&bundle(address, AccountStatus::Changed, None, &[])),
            Err(address)
        );
    }

    /// A `StateProviderBox` handed to a prewarm worker must be `Send + 'static`; the
    /// overlay handle must additionally be `Send + Sync` to be captured by the worker
    /// factory closure.
    #[test]
    fn provider_and_overlay_handle_are_thread_safe() {
        const fn assert_send_static<T: Send + 'static>() {}
        const fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_static::<DurableStateProvider>();
        assert_send_sync::<SharedOverlay>();
    }

    #[test]
    fn provider_serves_overlay_over_anchor() {
        let address = Address::with_last_byte(1);
        let slot = U256::from(3u64);
        // The anchor knows nothing, so any answer must come from the overlay.
        let provider = DurableStateProvider::new(Box::new(NoopProvider::default()));
        assert_eq!(provider.basic_account(&address).unwrap(), None);

        provider
            .apply(&bundle(address, AccountStatus::Changed, info(5), &[(slot, U256::from(9u64))]))
            .unwrap();
        assert_eq!(provider.basic_account(&address).unwrap().unwrap().nonce, 5);
        assert_eq!(provider.storage(address, B256::from(slot)).unwrap(), Some(U256::from(9u64)));
        assert_eq!(provider.storage(address, B256::with_last_byte(4)).unwrap(), None);
    }

    #[test]
    fn views_sharing_an_overlay_observe_the_same_canonical_state() {
        let address = Address::with_last_byte(1);
        let slot = U256::from(3u64);
        let provider = DurableStateProvider::new(Box::new(NoopProvider::default()));
        // A worker-side view over its own anchor, sharing the loop's overlay.
        let view = DurableStateProvider::from_parts(
            Box::new(NoopProvider::default()),
            provider.overlay_handle(),
        );
        assert_eq!(view.basic_account(&address).unwrap(), None);

        // Applying through the loop's provider is visible to the view immediately.
        provider
            .apply(&bundle(address, AccountStatus::Changed, info(5), &[(slot, U256::from(9u64))]))
            .unwrap();
        assert_eq!(view.basic_account(&address).unwrap().unwrap().nonce, 5);
        assert_eq!(view.storage(address, B256::from(slot)).unwrap(), Some(U256::from(9u64)));
        assert_eq!(view.overlay().accounts_len(), 1);

        // A view over an unrelated overlay is unaffected.
        let separate = DurableStateProvider::from_parts(
            Box::new(NoopProvider::default()),
            SharedOverlay::default(),
        );
        assert_eq!(separate.basic_account(&address).unwrap(), None);
    }
}
