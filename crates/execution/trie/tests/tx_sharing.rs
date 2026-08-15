//! Verifies that the production overlay paths acquire exactly one MDBX
//! read-only transaction per request.
//!
//! Each `Proof::overlay_*`, `TrieWitness::overlay_witness`, `StateRoot::overlay_*`,
//! and `StorageRoot::overlay_root` call must acquire its own transaction at entry
//! and reuse it across every internal cursor open. Acquiring more than one tx
//! per request reintroduces the libmdbx `lck_rdt_lock` mutex contention that
//! reth PR #22631 fixes upstream and that this crate fixes locally for
//! `v1.11.4`.
//!
//! This test runs only with the `metrics` feature because the
//! [`BaseProofsStorage`] alias resolves to [`BaseProofsStorageWithMetrics`]
//! there, which exposes the per-instance `tx_acquisitions` counter we need.

#![cfg(feature = "metrics")]

use std::sync::Arc;

use alloy_eips::BlockNumHash;
use alloy_primitives::{
    Address, B256, U256, keccak256,
    map::{B256Map, B256Set},
};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStorage, MdbxProofsStorage,
    provider::BaseProofsStateProviderRef,
};
use reth_primitives_traits::Account;
use reth_provider::{
    AccountReader, StateProofProvider, StateProvider, StateRootProvider, StorageRootProvider,
    noop::NoopProvider,
};
use reth_trie_common::{
    ExecutionWitnessMode, HashedPostState, HashedStorage, MultiProofTargets, TrieInput,
};
use tempfile::TempDir;

/// Number of accounts we seed and target per test request.
///
/// Picked large enough that the underlying account trie has multiple branch
/// nodes so that `account_trie_cursor` has to open and walk repeatedly within
/// a single request.
const ACCOUNTS: u8 = 8;

/// Number of storage slots seeded per account and targeted per request.
///
/// Combined with [`ACCOUNTS`] this yields `ACCOUNTS * SLOTS = 32` storage
/// entries spread across `ACCOUNTS` distinct storage tries, forcing each
/// overlay path that touches storage to open multiple distinct
/// `storage_hashed_cursor` and `storage_trie_cursor` cursors per request.
const SLOTS: u8 = 4;

/// Returns the seeded `(address, [slot; SLOTS])` pairs in the order they were
/// inserted.
fn seed_layout() -> Vec<(Address, Vec<B256>)> {
    (0..ACCOUNTS)
        .map(|a| {
            let address = Address::repeat_byte(0x10 | a);
            let slots = (0..SLOTS).map(|s| B256::repeat_byte(0x01 | (s << 4))).collect();
            (address, slots)
        })
        .collect()
}

/// Builds an MDBX-backed storage seeded with several accounts and storage
/// slots so cursor walks have non-trivial work to do.
///
/// Returns the temp dir (must be kept alive for the duration of the test) and
/// the wrapped storage so the test can read its `tx_acquisitions` counter
/// before and after each call.
fn setup() -> (TempDir, BaseProofsStorage<Arc<MdbxProofsStorage>>) {
    let dir = TempDir::new().expect("temp dir");
    let mdbx = Arc::new(MdbxProofsStorage::new(dir.path()).expect("mdbx env"));

    let mut hashed_accounts = Vec::with_capacity(ACCOUNTS as usize);
    for (i, (address, slots)) in seed_layout().into_iter().enumerate() {
        let hashed_address = keccak256(address);
        let account = Account {
            nonce: i as u64 + 1,
            balance: U256::from(1_000 * (i as u64 + 1)),
            bytecode_hash: None,
        };
        hashed_accounts.push((hashed_address, Some(account)));

        let hashed_slots: Vec<_> = slots
            .iter()
            .enumerate()
            .map(|(j, slot)| (keccak256(slot), U256::from(42 + j as u64)))
            .collect();
        mdbx.store_hashed_storages(hashed_address, hashed_slots).expect("store hashed storages");
    }
    mdbx.store_hashed_accounts(hashed_accounts).expect("store hashed accounts");

    mdbx.set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO)).expect("anchor");
    mdbx.commit_initial_state().expect("commit");

    (dir, BaseProofsStorage::from(mdbx))
}

/// Builds [`MultiProofTargets`] covering every seeded account and slot so the
/// overlay paths that take targets actually walk both account and storage
/// tries multiple times per request.
fn full_targets() -> MultiProofTargets {
    let mut targets = B256Map::default();
    for (address, slots) in seed_layout() {
        let hashed_slots: B256Set = slots.iter().map(keccak256).collect();
        targets.insert(keccak256(address), hashed_slots);
    }
    MultiProofTargets::from_iter(targets)
}

/// Builds a [`HashedPostState`] referencing every seeded account and slot.
///
/// We do not actually mutate values — we only need the prefix sets to be
/// non-empty so the state-root, witness, and multiproof computations descend
/// into both account and storage tries instead of short-circuiting on an
/// empty input.
fn full_post_state() -> HashedPostState {
    let mut state = HashedPostState::default();
    for (i, (address, slots)) in seed_layout().into_iter().enumerate() {
        let hashed_address = keccak256(address);
        let account = Account {
            nonce: i as u64 + 1,
            balance: U256::from(1_000 * (i as u64 + 1)),
            bytecode_hash: None,
        };
        state.accounts.insert(hashed_address, Some(account));
        let hashed_slots = slots
            .into_iter()
            .enumerate()
            .map(|(i, slot)| (keccak256(slot), U256::from(42 + i as u64)));
        state.storages.insert(hashed_address, HashedStorage::from_iter(false, hashed_slots));
    }
    state
}

/// Asserts that `body` causes exactly `expected` increments on `storage.tx_acquisitions()`.
fn assert_tx_acquisitions<F>(
    storage: &BaseProofsStorage<Arc<MdbxProofsStorage>>,
    expected: u64,
    label: &str,
    body: F,
) where
    F: FnOnce(),
{
    let before = storage.tx_acquisitions();
    body();
    let delta = storage.tx_acquisitions() - before;
    assert_eq!(
        delta, expected,
        "{label}: expected exactly {expected} tx acquisition(s), got {delta}"
    );
}

#[test]
fn proof_acquires_one_tx_per_call() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);
    let (address, slots) = seed_layout().into_iter().next().expect("seed");

    assert_tx_acquisitions(&storage, 1, "proof", || {
        provider.proof(TrieInput::default(), address, &slots).expect("proof");
    });
}

#[test]
fn multiproof_acquires_one_tx_per_call() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);

    assert_tx_acquisitions(&storage, 1, "multiproof", || {
        provider.multiproof(TrieInput::default(), full_targets()).expect("multiproof");
    });
}

#[test]
fn witness_acquires_one_tx_per_call() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);

    assert_tx_acquisitions(&storage, 1, "witness", || {
        provider
            .witness(
                TrieInput::from_state(full_post_state()),
                full_post_state(),
                ExecutionWitnessMode::default(),
            )
            .expect("witness");
    });
}

#[test]
fn state_root_acquires_one_tx_per_call() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);

    assert_tx_acquisitions(&storage, 1, "state_root", || {
        provider.state_root(full_post_state()).expect("state_root");
    });

    assert_tx_acquisitions(&storage, 1, "state_root_with_updates", || {
        provider.state_root_with_updates(full_post_state()).expect("state_root_with_updates");
    });

    assert_tx_acquisitions(&storage, 1, "state_root_from_nodes", || {
        provider
            .state_root_from_nodes(TrieInput::from_state(full_post_state()))
            .expect("state_root_from_nodes");
    });

    assert_tx_acquisitions(&storage, 1, "state_root_from_nodes_with_updates", || {
        provider
            .state_root_from_nodes_with_updates(TrieInput::from_state(full_post_state()))
            .expect("state_root_from_nodes_with_updates");
    });
}

#[test]
fn storage_root_acquires_one_tx_per_call() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);
    let (address, slots) = seed_layout().into_iter().next().expect("seed");
    let hashed_storage = HashedStorage::from_iter(
        false,
        slots.iter().enumerate().map(|(i, slot)| (keccak256(slot), U256::from(42 + i as u64))),
    );

    assert_tx_acquisitions(&storage, 1, "storage_root", || {
        provider.storage_root(address, hashed_storage.clone()).expect("storage_root");
    });

    assert_tx_acquisitions(&storage, 1, "storage_proof", || {
        provider.storage_proof(address, slots[0], hashed_storage.clone()).expect("storage_proof");
    });

    assert_tx_acquisitions(&storage, 1, "storage_multiproof", || {
        provider
            .storage_multiproof(address, &slots, hashed_storage.clone())
            .expect("storage_multiproof");
    });
}

#[test]
fn evm_state_reads_share_one_lazy_tx_per_provider() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);
    let (address, slots) = seed_layout().into_iter().next().expect("seed");

    assert_tx_acquisitions(&storage, 1, "evm_state_reads", || {
        provider.basic_account(&address).expect("account");
        provider.storage(address, slots[0]).expect("storage 0");
        provider.storage(address, slots[1]).expect("storage 1");
    });
}

/// Two consecutive calls on the same provider acquire exactly two transactions
/// total — confirming each request is independently scoped (one tx in, one tx
/// out, no leak across requests).
#[test]
fn back_to_back_calls_acquire_one_tx_each() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);
    let (address, slots) = seed_layout().into_iter().next().expect("seed");

    let before = storage.tx_acquisitions();
    provider.proof(TrieInput::default(), address, &slots).expect("proof 1");
    provider.proof(TrieInput::default(), address, &slots).expect("proof 2");
    let delta = storage.tx_acquisitions() - before;
    assert_eq!(delta, 2, "two proof calls should acquire exactly two transactions, got {delta}");
}

/// `N` concurrent `proof` calls acquire exactly `N` transactions and return
/// byte-identical proofs — guarding against both tx-sharing regressions
/// (delta != N) and concurrent state leaks (proofs diverge).
#[test]
fn concurrent_calls_acquire_one_tx_per_request() {
    const N: u64 = 8;

    let (_dir, storage) = setup();
    let (address, slots) = seed_layout().into_iter().next().expect("seed");

    let before = storage.tx_acquisitions();

    let proofs: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..N)
            .map(|_| {
                s.spawn(|| {
                    let provider = BaseProofsStateProviderRef::new(
                        Box::<NoopProvider>::default(),
                        &storage,
                        0,
                    );
                    provider.proof(TrieInput::default(), address, &slots).expect("proof")
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("thread join")).collect()
    });

    let delta = storage.tx_acquisitions() - before;
    assert_eq!(
        delta, N,
        "{N} concurrent proof calls should acquire exactly {N} transactions, got {delta}"
    );

    let baseline = &proofs[0];
    for (i, p) in proofs.iter().enumerate().skip(1) {
        assert_eq!(
            p, baseline,
            "proof {i} diverges from proof 0 — concurrent state leak suspected"
        );
    }
}
