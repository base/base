//! Storage destruction and recreation through hashing, trie updates, persistence, and proofs.

use std::sync::Arc;

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{Address, B256, U256};
use base_execution_trie::{
    BaseProofsBatchSession, BaseProofsBatchStateProviderRef, BaseProofsBatchStore,
    BaseProofsInitialStateStore, BaseProofsStorage, BlockStateDiff, MdbxProofsStorage,
    RocksdbProofsStorage, provider::BaseProofsStateProviderRef,
};
use reth_primitives_traits::Account;
use reth_provider::{
    HashedPostStateProvider, StateProvider, StateRootProvider, noop::NoopProvider,
};
use reth_revm::{
    db::{AccountStatus, BundleAccount, BundleState, states::StorageSlot},
    state::AccountInfo,
};
use reth_trie_common::{
    EMPTY_ROOT_HASH, TrieInput,
    root::{state_root_unhashed, storage_root_unhashed},
};
use tempfile::TempDir;

/// A populated parent, destruction (optionally followed by same-block recreation), and a
/// subsequent write. Enough parent slots are seeded to exercise removal of persisted branches.
#[derive(Debug)]
pub struct StorageLifecycle;

impl StorageLifecycle {
    /// Contract whose storage is destroyed and recreated.
    pub const ADDRESS: Address = Address::repeat_byte(0x42);
    /// Number of parent slots, large enough to populate storage branches.
    pub const PARENT_SLOTS: u64 = 32;
    /// New slot outside the parent's key set.
    pub const RECREATED_SLOT: u64 = 64;

    /// Linked block identity for the three-block history.
    pub fn block(number: u64) -> BlockWithParent {
        BlockWithParent::new(
            B256::from(U256::from(number - 1)),
            NumHash::new(number, B256::from(U256::from(number))),
        )
    }

    /// Expected persisted account whenever the contract exists.
    pub fn account() -> Account {
        Account { nonce: 1, balance: U256::from(100), bytecode_hash: None }
    }

    /// Execution-layer account information used in the bundle transitions.
    pub fn info() -> AccountInfo {
        AccountInfo { nonce: 1, balance: U256::from(100), ..Default::default() }
    }

    /// Complete expected storage, independent of the sparse production updates.
    pub fn slots(number: u64, recreated: bool) -> Vec<(B256, U256)> {
        if number == 1 {
            return (1..=Self::PARENT_SLOTS)
                .map(|slot| (B256::from(U256::from(slot)), U256::from(slot * 10)))
                .collect();
        }
        let mut slots = if recreated {
            vec![
                (B256::from(U256::from(2)), U256::from(33)),
                (B256::from(U256::from(Self::RECREATED_SLOT)), U256::from(44)),
            ]
        } else {
            Vec::new()
        };
        if number == 3 {
            slots.push((B256::from(U256::from(3)), U256::from(55)));
        }
        slots
    }

    /// Reference root rebuilt from leaves without any persisted trie nodes.
    pub fn root(number: u64, recreated: bool) -> B256 {
        if number == 2 && !recreated {
            return EMPTY_ROOT_HASH;
        }
        state_root_unhashed([(
            Self::ADDRESS,
            Self::account()
                .into_trie_account(storage_root_unhashed(Self::slots(number, recreated))),
        )])
    }

    /// Sparse execution output, deliberately omitting unread slots during destruction.
    pub fn bundle(number: u64, recreated: bool) -> BundleState {
        let exists = number != 2 || recreated;
        let original = number != 1 && (number != 3 || recreated);
        let status = match number {
            2 if recreated => AccountStatus::DestroyedChanged,
            2 => AccountStatus::Destroyed,
            3 if recreated => AccountStatus::Changed,
            _ => AccountStatus::InMemoryChange,
        };
        let slots = if number == 3 {
            vec![(B256::from(U256::from(3)), U256::from(55))]
        } else {
            Self::slots(number, recreated)
        };
        let mut bundle = BundleState::default();
        bundle.state.insert(
            Self::ADDRESS,
            BundleAccount::new(
                original.then(Self::info),
                exists.then(Self::info),
                slots
                    .into_iter()
                    .map(|(slot, value)| {
                        (U256::from_be_bytes(slot.0), StorageSlot::new_changed(U256::ZERO, value))
                    })
                    .collect(),
                status,
            ),
        );
        bundle
    }

    /// Generate the persisted diff through the production hashing and root APIs.
    pub fn diff(
        provider: &(impl HashedPostStateProvider + StateRootProvider),
        number: u64,
        recreated: bool,
    ) -> BlockStateDiff {
        let state = provider.hashed_post_state(&Self::bundle(number, recreated)).unwrap();
        let (root, updates) = provider.state_root_with_updates(state.clone()).unwrap();
        assert_eq!(root, Self::root(number, recreated), "overlay root at block {number}");
        BlockStateDiff {
            sorted_post_state: state.into_sorted(),
            sorted_trie_updates: updates.into_sorted(),
        }
    }

    /// Check reads and cryptographic proofs against the independently rebuilt root.
    pub fn assert_state(provider: &impl StateProvider, number: u64, recreated: bool) {
        let expected = Self::slots(number, recreated);
        let root = Self::root(number, recreated);
        assert_eq!(provider.state_root(Default::default()).unwrap(), root);
        assert_eq!(
            provider.basic_account(&Self::ADDRESS).unwrap(),
            (number != 2 || recreated).then(Self::account),
        );
        let slots: Vec<_> = (1..=Self::PARENT_SLOTS)
            .chain([Self::RECREATED_SLOT])
            .map(|slot| B256::from(U256::from(slot)))
            .collect();
        for slot in &slots {
            let value = expected
                .iter()
                .find(|(key, _)| key == slot)
                .map_or(U256::ZERO, |(_, value)| *value);
            assert_eq!(provider.storage(Self::ADDRESS, *slot).unwrap().unwrap_or_default(), value);
        }
        let proof = provider.proof(TrieInput::default(), Self::ADDRESS, &slots).unwrap();
        proof
            .verify(root)
            .expect("persisted account and storage proofs must match the reference root");
        for storage_proof in &proof.storage_proofs {
            let value = expected
                .iter()
                .find(|(slot, _)| *slot == storage_proof.key)
                .map_or(U256::ZERO, |(_, value)| *value);
            assert_eq!(storage_proof.value, value);
        }
    }

    /// Exercise committed or session-local parents and then recheck every historical state.
    pub fn run<S: BaseProofsBatchStore + BaseProofsInitialStateStore + 'static>(
        store: S,
        batched: bool,
        recreated: bool,
    ) {
        let store = Arc::new(store);
        store.set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO)).unwrap();
        store.commit_initial_state().unwrap();
        let storage: BaseProofsStorage<Arc<S>> = BaseProofsStorage::from(Arc::clone(&store));
        if batched {
            store
                .with_batch_session(|session| {
                    for number in 1..=3 {
                        let provider = BaseProofsBatchStateProviderRef::new(
                            Box::<NoopProvider>::default(),
                            session,
                            number - 1,
                        );
                        let diff = Self::diff(&provider, number, recreated);
                        drop(provider);
                        session.store_trie_updates(Self::block(number), diff)?;
                        let provider = BaseProofsBatchStateProviderRef::new(
                            Box::<NoopProvider>::default(),
                            session,
                            number,
                        );
                        Self::assert_state(&provider, number, recreated);
                    }
                    Ok(())
                })
                .unwrap();
        } else {
            for number in 1..=3 {
                let provider = BaseProofsStateProviderRef::new(
                    Box::<NoopProvider>::default(),
                    &storage,
                    number - 1,
                );
                let diff = Self::diff(&provider, number, recreated);
                drop(provider);
                store.store_trie_updates(Self::block(number), diff).unwrap();
            }
        }
        // Reopen providers after all writes: deletion and recreation must not alter old proofs.
        for number in 1..=3 {
            let provider =
                BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, number);
            Self::assert_state(&provider, number, recreated);
        }
    }
}

/// MDBX must retain historical proofs across committed and transaction-local destruction.
#[test]
pub fn storage_lifecycle_mdbx() {
    for batched in [false, true] {
        for recreated in [false, true] {
            let dir = TempDir::new().unwrap();
            StorageLifecycle::run(MdbxProofsStorage::new(dir.path()).unwrap(), batched, recreated);
        }
    }
}

/// `RocksDB` must retain historical proofs across both provider paths.
#[test]
pub fn storage_lifecycle_rocksdb() {
    for batched in [false, true] {
        for recreated in [false, true] {
            let dir = TempDir::new().unwrap();
            StorageLifecycle::run(
                RocksdbProofsStorage::new(dir.path()).unwrap(),
                batched,
                recreated,
            );
        }
    }
}
