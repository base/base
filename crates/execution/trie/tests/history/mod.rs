//! History fetch, prune, and unwind behavior tests.

use serial_test::serial;
use test_case::test_case;

use super::*;

#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_fetch_trie_updates_basic<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let block_ref = test_block(B256::ZERO, 1, 0x11);
    let account_address = B256::repeat_byte(0x11);
    let deleted_account = B256::repeat_byte(0x22);
    let storage_address = B256::repeat_byte(0x33);
    let trie_storage_address = B256::repeat_byte(0x44);
    let slot = B256::repeat_byte(0xA1);
    let account = Account { nonce: 1, balance: U256::from(100), ..Default::default() };

    let mut trie_updates = TrieUpdates::default();
    trie_updates.account_nodes.insert(nibbles_from(vec![0, 1, 2, 3]), BranchNodeCompact::default());
    let mut storage_trie_updates = StorageTrieUpdates::default();
    storage_trie_updates
        .storage_nodes
        .insert(nibbles_from(vec![1, 2, 3, 4]), BranchNodeCompact::default());
    trie_updates.storage_tries.insert(trie_storage_address, storage_trie_updates);

    let mut post_state = HashedPostState::default();
    post_state.accounts.insert(account_address, Some(account));
    post_state.accounts.insert(deleted_account, None);
    let mut hashed_storage = HashedStorage::default();
    hashed_storage.storage.insert(slot, U256::from(1234));
    post_state.storages.insert(storage_address, hashed_storage);

    let expected = BlockStateDiff {
        sorted_trie_updates: trie_updates.into_sorted(),
        sorted_post_state: post_state.into_sorted(),
    };
    storage.store_trie_updates(block_ref, expected.clone())?;

    let actual = storage.fetch_trie_updates(block_ref.block.number)?;
    assert_eq!(
        actual.sorted_trie_updates.account_nodes_ref(),
        expected.sorted_trie_updates.account_nodes_ref()
    );
    assert_eq!(
        actual.sorted_trie_updates.storage_tries_ref(),
        expected.sorted_trie_updates.storage_tries_ref()
    );
    assert_eq!(actual.sorted_post_state.accounts, expected.sorted_post_state.accounts);
    assert_eq!(actual.sorted_post_state.storages, expected.sorted_post_state.storages);
    Ok(())
}

#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_store_trie_updates_out_of_order_rejects<
    S: BaseProofsStore + BaseProofsInitialStateStore,
>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    storage.set_earliest_block_number(0, B256::ZERO)?;
    let block_42 = test_block(B256::ZERO, 42, 0x42);
    storage.store_trie_updates(block_42, BlockStateDiff::default())?;

    let bad_block = test_block(B256::repeat_byte(0xFF), 99, 0x99);
    let error = storage.store_trie_updates(bad_block, BlockStateDiff::default()).unwrap_err();
    assert!(matches!(error, BaseProofsStorageError::OutOfOrder { .. }));
    Ok(())
}

#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_prune_earliest_state_comprehensive<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    storage.set_earliest_block_number(0, B256::ZERO)?;
    let account_address = B256::repeat_byte(1);
    let storage_slot = B256::repeat_byte(2);
    let account_path = nibbles_from(vec![1]);
    let storage_path = nibbles_from(vec![3]);
    let account_v1 = Account { nonce: 1, balance: U256::from(100), ..Default::default() };
    let account_v2 = Account { nonce: 2, balance: U256::from(200), ..Default::default() };
    let block_1 = test_block(B256::ZERO, 1, 1);
    let block_2 = test_block(block_1.block.hash, 2, 2);
    let block_3 = test_block(block_2.block.hash, 3, 3);

    let mut trie_updates = TrieUpdates::default();
    trie_updates.account_nodes.insert(account_path, BranchNodeCompact::default());
    let mut storage_trie_updates = StorageTrieUpdates::default();
    storage_trie_updates.storage_nodes.insert(storage_path, BranchNodeCompact::default());
    trie_updates.storage_tries.insert(account_address, storage_trie_updates);

    let mut post_state_1 = HashedPostState::default();
    post_state_1.accounts.insert(account_address, Some(account_v1));
    let mut hashed_storage = HashedStorage::default();
    hashed_storage.storage.insert(storage_slot, U256::from(1234));
    post_state_1.storages.insert(account_address, hashed_storage);

    let mut post_state_2 = HashedPostState::default();
    post_state_2.accounts.insert(account_address, Some(account_v2));

    storage.store_trie_updates(
        block_1,
        BlockStateDiff {
            sorted_trie_updates: trie_updates.into_sorted(),
            sorted_post_state: post_state_1.into_sorted(),
        },
    )?;
    storage.store_trie_updates(
        block_2,
        BlockStateDiff {
            sorted_trie_updates: TrieUpdatesSorted::default(),
            sorted_post_state: post_state_2.into_sorted(),
        },
    )?;
    storage.prune_earliest_state(block_3)?;

    assert_account_at(&storage, 3, account_address, account_v2)?;
    assert_storage_at(&storage, account_address, 3, storage_slot, U256::from(1234))?;
    assert_account_branch_present(&storage, 3, account_path)?;

    let mut storage_cursor = storage.storage_trie_cursor(account_address, 3)?;
    assert!(storage_cursor.seek_exact(storage_path)?.is_some());
    Ok(())
}

#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_prune_earliest_state_returns_correct_counts<
    S: BaseProofsStore + BaseProofsInitialStateStore,
>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    storage.set_earliest_block_number(0, B256::ZERO)?;
    let address = B256::repeat_byte(4);
    let block_1 = test_block(B256::ZERO, 1, 1);
    let block_2 = test_block(block_1.block.hash, 2, 2);

    let mut post_state_1 = HashedPostState::default();
    post_state_1.accounts.insert(address, Some(Account { nonce: 1, ..Default::default() }));
    let mut post_state_2 = HashedPostState::default();
    post_state_2.accounts.insert(address, Some(Account { nonce: 2, ..Default::default() }));

    storage.store_trie_updates(
        block_1,
        BlockStateDiff {
            sorted_trie_updates: TrieUpdatesSorted::default(),
            sorted_post_state: post_state_1.into_sorted(),
        },
    )?;
    storage.store_trie_updates(
        block_2,
        BlockStateDiff {
            sorted_trie_updates: TrieUpdatesSorted::default(),
            sorted_post_state: post_state_2.into_sorted(),
        },
    )?;

    let counts = storage.prune_earliest_state(block_2)?;
    assert_eq!(counts.hashed_accounts_written_total, 1);
    assert_eq!(counts.account_trie_updates_written_total, 0);
    Ok(())
}

#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_unwind_history_with_trie_nodes<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    storage.set_earliest_block_number(0, B256::ZERO)?;
    let path_1 = nibbles_from(vec![1]);
    let path_2 = nibbles_from(vec![2]);
    let block_1 = test_block(B256::ZERO, 1, 1);
    let block_2 = test_block(block_1.block.hash, 2, 2);
    let block_3 = test_block(block_2.block.hash, 3, 3);

    for (block, path) in [(block_1, path_1), (block_2, path_2), (block_3, path_1)] {
        let mut trie_updates = TrieUpdates::default();
        trie_updates.account_nodes.insert(path, BranchNodeCompact::default());
        storage.store_trie_updates(
            block,
            BlockStateDiff {
                sorted_trie_updates: trie_updates.into_sorted(),
                sorted_post_state: HashedPostStateSorted::default(),
            },
        )?;
    }

    storage.unwind_history(block_2)?;
    assert_account_branch_present(&storage, 10, path_1)?;
    assert_account_branch_missing(&storage, 10, path_2)
}

#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_unwind_history_comprehensive<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    storage.set_earliest_block_number(0, B256::ZERO)?;
    let account_1 = B256::repeat_byte(1);
    let account_2 = B256::repeat_byte(2);
    let slot_1 = B256::repeat_byte(3);
    let slot_2 = B256::repeat_byte(4);
    let block_1 = test_block(B256::ZERO, 1, 1);
    let block_2 = test_block(block_1.block.hash, 2, 2);
    let block_3 = test_block(block_2.block.hash, 3, 3);

    let mut post_state_1 = HashedPostState::default();
    post_state_1.accounts.insert(account_1, Some(Account::default()));
    let mut storage_1 = HashedStorage::default();
    storage_1.storage.insert(slot_1, U256::from(1111));
    post_state_1.storages.insert(account_1, storage_1);

    let mut post_state_2 = HashedPostState::default();
    post_state_2.accounts.insert(account_2, Some(Account::default()));
    let mut storage_2 = HashedStorage::default();
    storage_2.storage.insert(slot_2, U256::from(2222));
    post_state_2.storages.insert(account_2, storage_2);

    let mut post_state_3 = HashedPostState::default();
    post_state_3.accounts.insert(account_1, Some(Account { nonce: 9, ..Default::default() }));

    storage.store_trie_updates(
        block_1,
        BlockStateDiff {
            sorted_trie_updates: TrieUpdatesSorted::default(),
            sorted_post_state: post_state_1.into_sorted(),
        },
    )?;
    storage.store_trie_updates(
        block_2,
        BlockStateDiff {
            sorted_trie_updates: TrieUpdatesSorted::default(),
            sorted_post_state: post_state_2.into_sorted(),
        },
    )?;
    storage.store_trie_updates(
        block_3,
        BlockStateDiff {
            sorted_trie_updates: TrieUpdatesSorted::default(),
            sorted_post_state: post_state_3.into_sorted(),
        },
    )?;

    storage.unwind_history(block_2)?;
    assert!(storage.fetch_trie_updates(1).is_ok());
    assert!(storage.fetch_trie_updates(2).is_err());
    assert!(storage.fetch_trie_updates(3).is_err());
    assert_eq!(storage.get_latest_block_number()?, Some((1, block_1.block.hash)));
    Ok(())
}

#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_unwind_history_idempotent<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    storage.set_earliest_block_number(0, B256::ZERO)?;
    let address = B256::repeat_byte(1);
    let make_diff = |nonce| {
        let mut post_state = HashedPostState::default();
        post_state.accounts.insert(address, Some(Account { nonce, ..Default::default() }));
        BlockStateDiff {
            sorted_trie_updates: TrieUpdatesSorted::default(),
            sorted_post_state: post_state.into_sorted(),
        }
    };
    let block_1 = test_block(B256::ZERO, 1, 1);
    let block_2 = test_block(block_1.block.hash, 2, 2);
    let block_3 = test_block(block_2.block.hash, 3, 3);

    storage.store_trie_updates(block_1, make_diff(10))?;
    storage.store_trie_updates(block_2, make_diff(20))?;
    storage.store_trie_updates(block_3, make_diff(30))?;

    storage.unwind_history(block_2)?;
    storage.unwind_history(block_2)?;
    assert!(storage.fetch_trie_updates(1).is_ok());
    assert!(storage.fetch_trie_updates(2).is_err());
    assert!(storage.fetch_trie_updates(3).is_err());
    Ok(())
}
