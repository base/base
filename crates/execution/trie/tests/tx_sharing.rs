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
use alloy_primitives::{Address, B256, U256, keccak256};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStorage, MdbxProofsStorage,
    provider::BaseProofsStateProviderRef,
};
use reth_primitives_traits::Account;
use reth_provider::{
    StateProofProvider, StateRootProvider, StorageRootProvider, noop::NoopProvider,
};
use reth_trie_common::{HashedPostState, HashedStorage, MultiProofTargets, TrieInput};
use tempfile::TempDir;

/// Builds a small populated MDBX-backed storage and a state provider over it.
///
/// Returns the temp dir (must be kept alive for the duration of the test) and
/// the wrapped storage so the test can read its `tx_acquisitions` counter
/// before and after each call.
fn setup() -> (TempDir, BaseProofsStorage<Arc<MdbxProofsStorage>>) {
    let dir = TempDir::new().expect("temp dir");
    let mdbx = Arc::new(MdbxProofsStorage::new(dir.path()).expect("mdbx env"));

    // Seed a tiny base state so cursor walks have something to traverse.
    // The exact contents do not matter; we are counting tx acquisitions, not
    // verifying proof contents.
    let address = Address::repeat_byte(0x11);
    let hashed_address = keccak256(address);
    let account = Account { nonce: 1, balance: U256::from(1_000), bytecode_hash: None };
    mdbx.store_hashed_accounts(vec![(hashed_address, Some(account))])
        .expect("store hashed accounts");
    mdbx.store_hashed_storages(
        hashed_address,
        vec![(keccak256(B256::repeat_byte(0x01)), U256::from(42))],
    )
    .expect("store hashed storages");

    mdbx.set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO)).expect("anchor");
    mdbx.commit_initial_state().expect("commit");

    (dir, BaseProofsStorage::from(mdbx))
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
    let address = Address::repeat_byte(0x11);
    let slots = [B256::repeat_byte(0x01)];

    assert_tx_acquisitions(&storage, 1, "proof", || {
        provider.proof(TrieInput::default(), address, &slots).expect("proof");
    });
}

#[test]
fn multiproof_acquires_one_tx_per_call() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);

    assert_tx_acquisitions(&storage, 1, "multiproof", || {
        provider
            .multiproof(TrieInput::default(), MultiProofTargets::default())
            .expect("multiproof");
    });
}

#[test]
fn witness_acquires_one_tx_per_call() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);

    assert_tx_acquisitions(&storage, 1, "witness", || {
        provider.witness(TrieInput::default(), HashedPostState::default()).expect("witness");
    });
}

#[test]
fn state_root_acquires_one_tx_per_call() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);

    assert_tx_acquisitions(&storage, 1, "state_root", || {
        provider.state_root(HashedPostState::default()).expect("state_root");
    });

    assert_tx_acquisitions(&storage, 1, "state_root_with_updates", || {
        provider
            .state_root_with_updates(HashedPostState::default())
            .expect("state_root_with_updates");
    });

    assert_tx_acquisitions(&storage, 1, "state_root_from_nodes", || {
        provider.state_root_from_nodes(TrieInput::default()).expect("state_root_from_nodes");
    });

    assert_tx_acquisitions(&storage, 1, "state_root_from_nodes_with_updates", || {
        provider
            .state_root_from_nodes_with_updates(TrieInput::default())
            .expect("state_root_from_nodes_with_updates");
    });
}

#[test]
fn storage_root_acquires_one_tx_per_call() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);
    let address = Address::repeat_byte(0x11);

    assert_tx_acquisitions(&storage, 1, "storage_root", || {
        provider.storage_root(address, HashedStorage::new(false)).expect("storage_root");
    });

    assert_tx_acquisitions(&storage, 1, "storage_proof", || {
        provider
            .storage_proof(address, B256::repeat_byte(0x01), HashedStorage::new(false))
            .expect("storage_proof");
    });

    assert_tx_acquisitions(&storage, 1, "storage_multiproof", || {
        provider
            .storage_multiproof(address, &[B256::repeat_byte(0x01)], HashedStorage::new(false))
            .expect("storage_multiproof");
    });
}

/// Two consecutive calls on the same provider acquire exactly two transactions
/// total — confirming each request is independently scoped (one tx in, one tx
/// out, no leak across requests).
#[test]
fn back_to_back_calls_acquire_one_tx_each() {
    let (_dir, storage) = setup();
    let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);
    let address = Address::repeat_byte(0x11);
    let slots = [B256::repeat_byte(0x01)];

    let before = storage.tx_acquisitions();
    provider.proof(TrieInput::default(), address, &slots).expect("proof 1");
    provider.proof(TrieInput::default(), address, &slots).expect("proof 2");
    let delta = storage.tx_acquisitions() - before;
    assert_eq!(delta, 2, "two proof calls should acquire exactly two transactions, got {delta}");
}
