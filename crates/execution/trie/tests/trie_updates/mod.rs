//! Trie update storage and replacement behavior tests.

use serial_test::serial;
use test_case::test_case;

use super::*;

/// Test storing and retrieving trie updates
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_trie_updates_operations<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let block_ref = BlockWithParent::new(B256::ZERO, NumHash::new(50, B256::repeat_byte(0x96)));
    let sorted_trie_updates = TrieUpdatesSorted::default();
    let sorted_post_state = HashedPostStateSorted::default();
    let block_state_diff = BlockStateDiff {
        sorted_trie_updates: sorted_trie_updates.clone(),
        sorted_post_state: sorted_post_state.clone(),
    };

    // Store trie updates
    storage.store_trie_updates(block_ref, block_state_diff)?;

    // Retrieve and verify
    let retrieved_diff = storage.fetch_trie_updates(block_ref.block.number)?;
    assert_eq!(retrieved_diff.sorted_trie_updates, sorted_trie_updates);
    assert_eq!(retrieved_diff.sorted_post_state, sorted_post_state);

    Ok(())
}

/// Test wiped storage in [`HashedPostState`]
///
/// When `store_trie_updates` receives a [`HashedPostState`] with wiped=true for a storage entry,
/// it should iterate all existing values for that address and create deletion entries for them.
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_store_trie_updates_with_wiped_storage<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let hashed_address = B256::repeat_byte(0x01);
    let block_ref = BlockWithParent::new(B256::ZERO, NumHash::new(100, B256::repeat_byte(0x96)));

    // First, store some storage values at block 50
    let storage_slots = vec![
        (B256::repeat_byte(0x10), U256::from(100)),
        (B256::repeat_byte(0x20), U256::from(200)),
        (B256::repeat_byte(0x30), U256::from(300)),
        (B256::repeat_byte(0x40), U256::from(400)),
    ];

    storage.store_hashed_storages(hashed_address, storage_slots.clone())?;

    // Verify all values are present at block 75
    let mut cursor75 = storage.storage_hashed_cursor(hashed_address, 75)?;
    let mut found_slots = Vec::new();
    while let Some((key, value)) = cursor75.next()? {
        found_slots.push((key, value));
    }
    assert_eq!(found_slots.len(), 4, "All storage slots should be present before wipe");
    assert_eq!(found_slots[0], (B256::repeat_byte(0x10), U256::from(100)));
    assert_eq!(found_slots[1], (B256::repeat_byte(0x20), U256::from(200)));
    assert_eq!(found_slots[2], (B256::repeat_byte(0x30), U256::from(300)));
    assert_eq!(found_slots[3], (B256::repeat_byte(0x40), U256::from(400)));

    // Now create a HashedPostState with wiped=true for this address at block 100
    let mut post_state = HashedPostState::default();
    let wiped_storage = HashedStorage::new(true); // wiped=true, empty storage map
    post_state.storages.insert(hashed_address, wiped_storage);

    let block_state_diff = BlockStateDiff {
        sorted_trie_updates: TrieUpdatesSorted::default(),
        sorted_post_state: post_state.into_sorted(),
    };

    // Store the wiped state
    storage.store_trie_updates(block_ref, block_state_diff)?;

    // After wiping, cursor at block 150 should see NO storage values
    let mut cursor150 = storage.storage_hashed_cursor(hashed_address, 150)?;
    let mut found_slots_after_wipe = Vec::new();
    while let Some((key, value)) = cursor150.next()? {
        found_slots_after_wipe.push((key, value));
    }

    assert_eq!(
        found_slots_after_wipe.len(),
        0,
        "All storage slots should be deleted after wipe. Found: {found_slots_after_wipe:?}"
    );

    // Verify individual seeks also return None
    for (slot, _) in &storage_slots {
        let mut seek_cursor = storage.storage_hashed_cursor(hashed_address, 150)?;
        let result = seek_cursor.seek(*slot)?;
        assert!(
            result.is_none() || result.unwrap().0 != *slot,
            "Storage slot {slot:?} should be deleted after wipe"
        );
    }

    // Verify cursor at block 75 (before wipe) still sees all values
    let mut cursor75_after = storage.storage_hashed_cursor(hashed_address, 75)?;
    let mut found_slots_before_wipe = Vec::new();
    while let Some((key, value)) = cursor75_after.next()? {
        found_slots_before_wipe.push((key, value));
    }
    assert_eq!(
        found_slots_before_wipe.len(),
        4,
        "All storage slots should still be present when querying before wipe block"
    );

    Ok(())
}

/// When a [`HashedPostState`] entry has `wiped = true` AND non-empty `storage` (the shape revm
/// produces for `AccountStatus::DestroyedChanged` accounts that are destroyed and recreated in
/// the same block), `store_trie_updates` must tombstone every prior storage slot for the address
/// at `block_number` AND persist the new post-recreation slot values from `storage`. Dropping
/// the new slots corrupts `HashedStorageHistory` and makes `BaseProofsStateProviderRef::storage`
/// return `None` for the new slots, producing divergent storage / state / output roots
/// downstream of `LiveTrieCollector`.
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_store_trie_updates_with_wiped_storage_and_new_slots<
    S: BaseProofsStore + BaseProofsInitialStateStore,
>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let hashed_address = B256::repeat_byte(0x01);
    let block_ref = BlockWithParent::new(B256::ZERO, NumHash::new(100, B256::repeat_byte(0x96)));

    let pre_existing_slots = vec![
        (B256::repeat_byte(0x10), U256::from(100)),
        (B256::repeat_byte(0x20), U256::from(200)),
    ];
    storage.store_hashed_storages(hashed_address, pre_existing_slots.clone())?;

    let new_slot_kept = (B256::repeat_byte(0x30), U256::from(0xCAFE));
    let new_slot_overwriting_old = (B256::repeat_byte(0x10), U256::from(0xBEEF));

    let mut post_state = HashedPostState::default();
    let wiped_with_new =
        HashedStorage::from_iter(true, vec![new_slot_kept, new_slot_overwriting_old]);
    post_state.storages.insert(hashed_address, wiped_with_new);

    let block_state_diff = BlockStateDiff {
        sorted_trie_updates: TrieUpdatesSorted::default(),
        sorted_post_state: post_state.into_sorted(),
    };

    storage.store_trie_updates(block_ref, block_state_diff)?;

    let mut cursor150 = storage.storage_hashed_cursor(hashed_address, 150)?;
    let mut found = Vec::new();
    while let Some((key, value)) = cursor150.next()? {
        found.push((key, value));
    }

    assert_eq!(
        found,
        vec![new_slot_overwriting_old, new_slot_kept],
        "After wipe+recreate, only the new post-recreation slots must be visible",
    );

    let mut seek_kept = storage.storage_hashed_cursor(hashed_address, 150)?;
    assert_eq!(
        seek_kept.seek(new_slot_kept.0)?,
        Some(new_slot_kept),
        "New slot 0x30 = 0xCAFE must round-trip through the cursor",
    );

    let mut seek_overwritten = storage.storage_hashed_cursor(hashed_address, 150)?;
    assert_eq!(
        seek_overwritten.seek(new_slot_overwriting_old.0)?,
        Some(new_slot_overwriting_old),
        "Reused slot 0x10 must reflect the new post-recreation value 0xBEEF, not the wiped 100",
    );

    let mut seek_dropped = storage.storage_hashed_cursor(hashed_address, 150)?;
    assert_eq!(
        seek_dropped.seek(B256::repeat_byte(0x20))?,
        Some(new_slot_kept),
        "seek(0x20) must skip the tombstoned old slot and advance to the next visible entry \
         (0x30 = 0xCAFE), proving 0x20 is not visible at block 150",
    );

    let mut cursor75 = storage.storage_hashed_cursor(hashed_address, 75)?;
    let mut pre_wipe_found = Vec::new();
    while let Some((key, value)) = cursor75.next()? {
        pre_wipe_found.push((key, value));
    }
    assert_eq!(
        pre_wipe_found, pre_existing_slots,
        "Cursor positioned before the wipe block must still see the original slot values",
    );

    Ok(())
}

/// Test that `store_trie_updates` properly stores branch nodes, leaf nodes, and removals
///
/// This test verifies that all data stored via `store_trie_updates` can be read back
/// through the cursor APIs.
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_store_trie_updates_comprehensive<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let block_ref = BlockWithParent::new(B256::ZERO, NumHash::new(100, B256::repeat_byte(0x96)));

    // Create comprehensive trie updates with branches, leaves, and removals
    let mut trie_updates = TrieUpdates::default();

    // Add account branch nodes
    let account_path1 = nibbles_from(vec![1, 2, 3]);
    let account_path2 = nibbles_from(vec![4, 5, 6]);
    let account_branch1 = create_test_branch();
    let account_branch2 = create_test_branch_variant();

    trie_updates.account_nodes.insert(account_path1, account_branch1);
    trie_updates.account_nodes.insert(account_path2, account_branch2);

    // Add removed account nodes
    let removed_account_path = nibbles_from(vec![7, 8, 9]);
    trie_updates.removed_nodes.insert(removed_account_path);

    // Add storage branch nodes for an address
    let hashed_address = B256::repeat_byte(0x42);
    let storage_path1 = nibbles_from(vec![1, 1]);
    let storage_path2 = nibbles_from(vec![2, 2]);
    let storage_branch = create_test_branch();

    let mut storage_trie = StorageTrieUpdates::default();
    storage_trie.storage_nodes.insert(storage_path1, storage_branch.clone());
    storage_trie.storage_nodes.insert(storage_path2, storage_branch);

    // Add removed storage node
    let removed_storage_path = nibbles_from(vec![3, 3]);
    storage_trie.removed_nodes.insert(removed_storage_path);

    trie_updates.insert_storage_updates(hashed_address, storage_trie);

    // Create post state with accounts and storage
    let mut post_state = HashedPostState::default();

    // Add accounts
    let account1_addr = B256::repeat_byte(0x10);
    let account2_addr = B256::repeat_byte(0x20);
    let account1 = create_test_account_with_values(1, 1000, 0xAA);
    let account2 = create_test_account_with_values(2, 2000, 0xBB);

    post_state.accounts.insert(account1_addr, Some(account1));
    post_state.accounts.insert(account2_addr, Some(account2));

    // Add deleted account
    let deleted_account_addr = B256::repeat_byte(0x30);
    post_state.accounts.insert(deleted_account_addr, None);

    // Add storage for an address
    let storage_addr = B256::repeat_byte(0x50);
    let mut hashed_storage = HashedStorage::new(false);
    hashed_storage.storage.insert(B256::repeat_byte(0x01), U256::from(111));
    hashed_storage.storage.insert(B256::repeat_byte(0x02), U256::from(222));
    hashed_storage.storage.insert(B256::repeat_byte(0x03), U256::ZERO); // Deleted storage
    post_state.storages.insert(storage_addr, hashed_storage);

    let block_state_diff = BlockStateDiff {
        sorted_trie_updates: trie_updates.into_sorted(),
        sorted_post_state: post_state.into_sorted(),
    };

    // Store the updates
    storage.store_trie_updates(block_ref, block_state_diff)?;

    // ========== Verify Account Branch Nodes ==========
    let mut account_trie_cursor = storage.account_trie_cursor(block_ref.block.number + 10)?;

    // Should find the added branches
    let result1 = account_trie_cursor.seek_exact(account_path1)?;
    assert!(result1.is_some(), "Account branch node 1 should be found");
    assert_eq!(result1.unwrap().0, account_path1);

    let result2 = account_trie_cursor.seek_exact(account_path2)?;
    assert!(result2.is_some(), "Account branch node 2 should be found");
    assert_eq!(result2.unwrap().0, account_path2);

    // Removed node should not be found
    let removed_result = account_trie_cursor.seek_exact(removed_account_path)?;
    assert!(removed_result.is_none(), "Removed account node should not be found");

    // ========== Verify Storage Branch Nodes ==========
    let mut storage_trie_cursor =
        storage.storage_trie_cursor(hashed_address, block_ref.block.number + 10)?;

    let storage_result1 = storage_trie_cursor.seek_exact(storage_path1)?;
    assert!(storage_result1.is_some(), "Storage branch node 1 should be found");

    let storage_result2 = storage_trie_cursor.seek_exact(storage_path2)?;
    assert!(storage_result2.is_some(), "Storage branch node 2 should be found");

    // Removed storage node should not be found
    let removed_storage_result = storage_trie_cursor.seek_exact(removed_storage_path)?;
    assert!(removed_storage_result.is_none(), "Removed storage node should not be found");

    // ========== Verify Account Leaves ==========
    let mut account_cursor = storage.account_hashed_cursor(block_ref.block.number + 10)?;

    let acc1_result = account_cursor.seek(account1_addr)?;
    assert!(acc1_result.is_some(), "Account 1 should be found");
    assert_eq!(acc1_result.unwrap().0, account1_addr);
    assert_eq!(acc1_result.unwrap().1.nonce, 1);
    assert_eq!(acc1_result.unwrap().1.balance, U256::from(1000));

    let acc2_result = account_cursor.seek(account2_addr)?;
    assert!(acc2_result.is_some(), "Account 2 should be found");
    assert_eq!(acc2_result.unwrap().1.nonce, 2);

    // Deleted account should not be found
    let deleted_acc_result = account_cursor.seek(deleted_account_addr)?;
    assert!(
        deleted_acc_result.is_none() || deleted_acc_result.unwrap().0 != deleted_account_addr,
        "Deleted account should not be found"
    );

    // ========== Verify Storage Leaves ==========
    let mut storage_cursor =
        storage.storage_hashed_cursor(storage_addr, block_ref.block.number + 10)?;

    let slot1_result = storage_cursor.seek(B256::repeat_byte(0x01))?;
    assert!(slot1_result.is_some(), "Storage slot 1 should be found");
    assert_eq!(slot1_result.unwrap().1, U256::from(111));

    let slot2_result = storage_cursor.seek(B256::repeat_byte(0x02))?;
    assert!(slot2_result.is_some(), "Storage slot 2 should be found");
    assert_eq!(slot2_result.unwrap().1, U256::from(222));

    // Zero-valued storage should not be found (deleted)
    let slot3_result = storage_cursor.seek(B256::repeat_byte(0x03))?;
    assert!(
        slot3_result.is_none() || slot3_result.unwrap().0 != B256::repeat_byte(0x03),
        "Zero-valued storage slot should not be found"
    );

    // ========== Verify fetch_trie_updates can retrieve the data ==========
    let fetched_diff = storage.fetch_trie_updates(block_ref.block.number)?;

    // Check that trie updates are stored
    assert_eq!(
        fetched_diff.sorted_trie_updates.account_nodes_ref().len(),
        3,
        "Should have 3 account nodes, including removed"
    );
    assert_eq!(
        fetched_diff.sorted_trie_updates.storage_tries_ref().len(),
        1,
        "Should have 1 storage trie"
    );

    // Check that post state is stored
    assert_eq!(
        fetched_diff.sorted_post_state.accounts.len(),
        3,
        "Should have 3 accounts (including deleted)"
    );
    assert_eq!(fetched_diff.sorted_post_state.storages.len(), 1, "Should have 1 storage entry");

    Ok(())
}

/// Test that `replace_updates` properly applies hashed/trie storage updates to the DB
///
/// This test verifies the bug fix where `replace_updates` was only storing `trie_updates`
/// and `post_states` directly without populating the internal data structures
/// (`hashed_accounts`, `hashed_storages`, `account_branches`, `storage_branches`).
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_replace_updates_applies_all_updates<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let block_ref_50 = BlockWithParent::new(B256::ZERO, NumHash::new(50, B256::repeat_byte(0x96)));

    // ========== Setup: Store initial state at blocks 50, 100, 101 ==========
    let initial_account_addr = B256::repeat_byte(0x10);
    let initial_account = create_test_account_with_values(1, 1000, 0xAA);

    let initial_storage_addr = B256::repeat_byte(0x20);
    let initial_storage_slot = B256::repeat_byte(0x01);
    let initial_storage_value = U256::from(100);

    let initial_branch_path = nibbles_from(vec![1, 2, 3]);
    let initial_branch = create_test_branch();

    // Store initial data at block 50
    let mut initial_trie_updates_50 = TrieUpdates::default();
    initial_trie_updates_50.account_nodes.insert(initial_branch_path, initial_branch.clone());

    let mut initial_post_state_50 = HashedPostState::default();
    initial_post_state_50.accounts.insert(initial_account_addr, Some(initial_account));

    let initial_diff_50 = BlockStateDiff {
        sorted_trie_updates: initial_trie_updates_50.into_sorted(),
        sorted_post_state: initial_post_state_50.into_sorted(),
    };
    storage.store_trie_updates(block_ref_50, initial_diff_50)?;

    // Store data at block 100 (common block)
    let mut initial_trie_updates_100 = TrieUpdates::default();
    let common_branch_path = nibbles_from(vec![4, 5, 6]);
    initial_trie_updates_100.account_nodes.insert(common_branch_path, initial_branch.clone());

    let mut initial_post_state_100 = HashedPostState::default();
    let mut initial_storage_100 = HashedStorage::new(false);
    initial_storage_100.storage.insert(initial_storage_slot, initial_storage_value);
    initial_post_state_100.storages.insert(initial_storage_addr, initial_storage_100);

    let initial_diff_100 = BlockStateDiff {
        sorted_trie_updates: initial_trie_updates_100.into_sorted(),
        sorted_post_state: initial_post_state_100.into_sorted(),
    };

    let block_ref_100 =
        BlockWithParent::new(block_ref_50.block.hash, NumHash::new(100, B256::repeat_byte(0x97)));

    storage.store_trie_updates(block_ref_100, initial_diff_100)?;

    // Store data at block 101 (will be replaced)
    let mut initial_trie_updates_101 = TrieUpdates::default();
    let old_branch_path = nibbles_from(vec![7, 8, 9]);
    initial_trie_updates_101.account_nodes.insert(old_branch_path, initial_branch);

    let mut initial_post_state_101 = HashedPostState::default();
    let old_account_addr = B256::repeat_byte(0x30);
    let old_account = create_test_account_with_values(99, 9999, 0xFF);
    initial_post_state_101.accounts.insert(old_account_addr, Some(old_account));

    let initial_diff_101 = BlockStateDiff {
        sorted_trie_updates: initial_trie_updates_101.into_sorted(),
        sorted_post_state: initial_post_state_101.into_sorted(),
    };
    let block_ref_101 =
        BlockWithParent::new(block_ref_100.block.hash, NumHash::new(101, B256::repeat_byte(0x98)));
    storage.store_trie_updates(block_ref_101, initial_diff_101)?;

    let block_ref_102 =
        BlockWithParent::new(block_ref_101.block.hash, NumHash::new(102, B256::repeat_byte(0x99)));

    // ========== Verify initial state exists ==========
    // Verify block 50 data exists
    let mut cursor_initial = storage.account_trie_cursor(75)?;
    assert!(
        cursor_initial.seek_exact(initial_branch_path)?.is_some(),
        "Initial branch should exist before replace"
    );

    // Verify block 101 old data exists
    let mut cursor_old = storage.account_trie_cursor(150)?;
    assert!(
        cursor_old.seek_exact(old_branch_path)?.is_some(),
        "Old branch at block 101 should exist before replace"
    );

    let mut account_cursor_old = storage.account_hashed_cursor(150)?;
    assert!(
        account_cursor_old.seek(old_account_addr)?.is_some(),
        "Old account at block 101 should exist before replace"
    );

    // ========== Call replace_updates to replace blocks after 100 ==========
    let mut blocks_to_add: Vec<(BlockWithParent, BlockStateDiff)> = Vec::default();

    // New data for block 101
    let new_account_addr = B256::repeat_byte(0x40);
    let new_account = create_test_account_with_values(5, 5000, 0xCC);

    let new_storage_addr = B256::repeat_byte(0x50);
    let new_storage_slot = B256::repeat_byte(0x02);
    let new_storage_value = U256::from(999);

    let new_branch_path = nibbles_from(vec![10, 11, 12]);
    let new_branch = create_test_branch_variant();

    let storage_branch_path = nibbles_from(vec![5, 5]);
    let storage_hashed_addr = B256::repeat_byte(0x60);

    let mut new_trie_updates = TrieUpdates::default();
    new_trie_updates.account_nodes.insert(new_branch_path, new_branch.clone());

    // Add storage trie updates
    let mut storage_trie = StorageTrieUpdates::default();
    storage_trie.storage_nodes.insert(storage_branch_path, new_branch.clone());
    new_trie_updates.insert_storage_updates(storage_hashed_addr, storage_trie);

    let mut new_post_state = HashedPostState::default();
    new_post_state.accounts.insert(new_account_addr, Some(new_account));

    let mut new_storage = HashedStorage::new(false);
    new_storage.storage.insert(new_storage_slot, new_storage_value);
    new_post_state.storages.insert(new_storage_addr, new_storage);

    blocks_to_add.push((
        block_ref_101,
        BlockStateDiff {
            sorted_trie_updates: new_trie_updates.into_sorted(),
            sorted_post_state: new_post_state.into_sorted(),
        },
    ));

    // New data for block 102
    let block_102_account_addr = B256::repeat_byte(0x70);
    let block_102_account = create_test_account_with_values(10, 10000, 0xDD);

    let mut trie_updates_102 = TrieUpdates::default();
    let block_102_branch_path = nibbles_from(vec![15, 14, 13]);
    trie_updates_102.account_nodes.insert(block_102_branch_path, new_branch);

    let mut post_state_102 = HashedPostState::default();
    post_state_102.accounts.insert(block_102_account_addr, Some(block_102_account));

    blocks_to_add.push((
        block_ref_102,
        BlockStateDiff {
            sorted_trie_updates: trie_updates_102.into_sorted(),
            sorted_post_state: post_state_102.into_sorted(),
        },
    ));

    // Execute replace_updates
    storage.replace_updates(BlockNumHash::new(100, block_ref_100.block.hash), blocks_to_add)?;
    // ========== Verify that data up to block 100 still exists ==========
    let mut cursor_50 = storage.account_trie_cursor(75)?;
    assert!(
        cursor_50.seek_exact(initial_branch_path)?.is_some(),
        "Block 50 branch should still exist after replace"
    );

    let mut cursor_100 = storage.account_trie_cursor(100)?;
    assert!(
        cursor_100.seek_exact(common_branch_path)?.is_some(),
        "Block 100 branch should still exist after replace"
    );

    let mut storage_cursor_100 = storage.storage_hashed_cursor(initial_storage_addr, 100)?;
    let result_100 = storage_cursor_100.seek(initial_storage_slot)?;
    assert!(result_100.is_some(), "Block 100 storage should still exist after replace");
    assert_eq!(
        result_100.unwrap().1,
        initial_storage_value,
        "Block 100 storage value should be unchanged"
    );

    // ========== Verify that old data after block 100 is gone ==========
    let mut cursor_old_gone = storage.account_trie_cursor(150)?;
    assert!(
        cursor_old_gone.seek_exact(old_branch_path)?.is_none(),
        "Old branch at block 101 should be removed after replace"
    );

    let mut account_cursor_old_gone = storage.account_hashed_cursor(150)?;
    let old_acc_result = account_cursor_old_gone.seek(old_account_addr)?;
    assert!(
        old_acc_result.is_none() || old_acc_result.unwrap().0 != old_account_addr,
        "Old account at block 101 should be removed after replace"
    );

    // ========== Verify new data is properly accessible via cursors ==========

    // Verify new account branch nodes
    let mut trie_cursor = storage.account_trie_cursor(150)?;
    let branch_result = trie_cursor.seek_exact(new_branch_path)?;
    assert!(branch_result.is_some(), "New account branch should be accessible via cursor");
    assert_eq!(branch_result.unwrap().0, new_branch_path);

    // Verify new storage branch nodes
    let mut storage_trie_cursor = storage.storage_trie_cursor(storage_hashed_addr, 150)?;
    let storage_branch_result = storage_trie_cursor.seek_exact(storage_branch_path)?;
    assert!(storage_branch_result.is_some(), "New storage branch should be accessible via cursor");
    assert_eq!(storage_branch_result.unwrap().0, storage_branch_path);

    // Verify new hashed accounts
    let mut account_cursor = storage.account_hashed_cursor(150)?;
    let account_result = account_cursor.seek(new_account_addr)?;
    assert!(account_result.is_some(), "New account should be accessible via cursor");
    assert_eq!(account_result.as_ref().unwrap().0, new_account_addr);
    assert_eq!(account_result.as_ref().unwrap().1.nonce, new_account.nonce);
    assert_eq!(account_result.as_ref().unwrap().1.balance, new_account.balance);
    assert_eq!(account_result.as_ref().unwrap().1.bytecode_hash, new_account.bytecode_hash);

    // Verify new hashed storages
    let mut storage_cursor = storage.storage_hashed_cursor(new_storage_addr, 150)?;
    let storage_result = storage_cursor.seek(new_storage_slot)?;
    assert!(storage_result.is_some(), "New storage should be accessible via cursor");
    assert_eq!(storage_result.as_ref().unwrap().0, new_storage_slot);
    assert_eq!(storage_result.as_ref().unwrap().1, new_storage_value);

    // Verify block 102 data
    let mut trie_cursor_102 = storage.account_trie_cursor(150)?;
    let branch_result_102 = trie_cursor_102.seek_exact(block_102_branch_path)?;
    assert!(branch_result_102.is_some(), "Block 102 branch should be accessible");
    assert_eq!(branch_result_102.unwrap().0, block_102_branch_path);

    let mut account_cursor_102 = storage.account_hashed_cursor(150)?;
    let account_result_102 = account_cursor_102.seek(block_102_account_addr)?;
    assert!(account_result_102.is_some(), "Block 102 account should be accessible");
    assert_eq!(account_result_102.as_ref().unwrap().0, block_102_account_addr);
    assert_eq!(account_result_102.as_ref().unwrap().1.nonce, block_102_account.nonce);

    // Verify fetch_trie_updates returns the new data
    let fetched_101 = storage.fetch_trie_updates(101)?;
    assert_eq!(
        fetched_101.sorted_trie_updates.account_nodes_ref().len(),
        1,
        "Should have 1 account branch node at block 101"
    );
    assert!(
        fetched_101
            .sorted_trie_updates
            .account_nodes_ref()
            .iter()
            .any(|(addr, _)| *addr == new_branch_path),
        "New branch path should be in trie_updates"
    );
    assert_eq!(
        fetched_101.sorted_post_state.accounts.len(),
        1,
        "Should have 1 account at block 101"
    );
    assert!(
        fetched_101.sorted_post_state.accounts.iter().any(|(addr, _)| *addr == new_account_addr),
        "New account should be in post_state"
    );

    Ok(())
}

/// Test that multi-block replacements make wiped storage see storage added earlier in the
/// replacement chain.
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_replace_updates_wipes_storage_added_by_prior_replacement_block<
    S: BaseProofsStore + BaseProofsInitialStateStore,
>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let common_block = BlockWithParent::new(B256::ZERO, NumHash::new(1, B256::repeat_byte(0xA1)));
    storage.store_trie_updates(common_block, BlockStateDiff::default())?;

    let old_block_2 =
        BlockWithParent::new(common_block.block.hash, NumHash::new(2, B256::repeat_byte(0xB2)));
    let old_block_3 =
        BlockWithParent::new(old_block_2.block.hash, NumHash::new(3, B256::repeat_byte(0xB3)));
    storage.store_trie_updates(old_block_2, BlockStateDiff::default())?;
    storage.store_trie_updates(old_block_3, BlockStateDiff::default())?;

    let storage_address = B256::repeat_byte(0x44);
    let storage_slot = B256::repeat_byte(0x55);
    let storage_value = U256::from(0x1234);

    let replacement_block_2 =
        BlockWithParent::new(common_block.block.hash, NumHash::new(2, B256::repeat_byte(0xC2)));
    let replacement_block_3 = BlockWithParent::new(
        replacement_block_2.block.hash,
        NumHash::new(3, B256::repeat_byte(0xC3)),
    );

    let mut replacement_post_state_2 = HashedPostState::default();
    let mut replacement_storage_2 = HashedStorage::new(false);
    replacement_storage_2.storage.insert(storage_slot, storage_value);
    replacement_post_state_2.storages.insert(storage_address, replacement_storage_2);

    let mut replacement_post_state_3 = HashedPostState::default();
    replacement_post_state_3.storages.insert(storage_address, HashedStorage::new(true));

    storage.replace_updates(
        BlockNumHash::new(common_block.block.number, common_block.block.hash),
        vec![
            (
                replacement_block_2,
                BlockStateDiff {
                    sorted_trie_updates: TrieUpdatesSorted::default(),
                    sorted_post_state: replacement_post_state_2.into_sorted(),
                },
            ),
            (
                replacement_block_3,
                BlockStateDiff {
                    sorted_trie_updates: TrieUpdatesSorted::default(),
                    sorted_post_state: replacement_post_state_3.into_sorted(),
                },
            ),
        ],
    )?;

    let mut storage_cursor = storage.storage_hashed_cursor(storage_address, 3)?;
    let storage_result = storage_cursor.seek(storage_slot)?;
    assert!(
        !matches!(storage_result, Some((found_slot, _)) if found_slot == storage_slot),
        "storage added by replacement block 2 should be wiped by replacement block 3"
    );

    Ok(())
}

/// Test that pure deletions (nodes only in `removed_nodes`) are properly stored
///
/// This test verifies that when a node appears only in `removed_nodes` (not in updates),
/// it is properly stored as a deletion and subsequent queries return None for that path.
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_pure_deletions_stored_correctly<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    // ========== Setup: Store initial branch nodes at block 50 ==========
    let account_path1 = nibbles_from(vec![1, 2, 3]);
    let account_path2 = nibbles_from(vec![4, 5, 6]);
    let storage_path1 = nibbles_from(vec![7, 8, 9]);
    let storage_path2 = nibbles_from(vec![10, 11, 12]);
    let storage_address = B256::repeat_byte(0x42);

    let initial_branch = create_test_branch();

    let mut initial_trie_updates = TrieUpdates::default();
    initial_trie_updates.account_nodes.insert(account_path1, initial_branch.clone());
    initial_trie_updates.account_nodes.insert(account_path2, initial_branch.clone());

    let mut storage_trie = StorageTrieUpdates::default();
    storage_trie.storage_nodes.insert(storage_path1, initial_branch.clone());
    storage_trie.storage_nodes.insert(storage_path2, initial_branch);
    initial_trie_updates.insert_storage_updates(storage_address, storage_trie);

    let initial_diff = BlockStateDiff {
        sorted_trie_updates: initial_trie_updates.into_sorted(),
        sorted_post_state: HashedPostStateSorted::default(),
    };

    let block_ref_50 = BlockWithParent::new(B256::ZERO, NumHash::new(50, B256::repeat_byte(0x96)));

    storage.store_trie_updates(block_ref_50, initial_diff)?;

    // Verify initial state exists at block 75
    let mut cursor_75 = storage.account_trie_cursor(75)?;
    assert!(
        cursor_75.seek_exact(account_path1)?.is_some(),
        "Initial account branch 1 should exist at block 75"
    );
    assert!(
        cursor_75.seek_exact(account_path2)?.is_some(),
        "Initial account branch 2 should exist at block 75"
    );

    let mut storage_cursor_75 = storage.storage_trie_cursor(storage_address, 75)?;
    assert!(
        storage_cursor_75.seek_exact(storage_path1)?.is_some(),
        "Initial storage branch 1 should exist at block 75"
    );
    assert!(
        storage_cursor_75.seek_exact(storage_path2)?.is_some(),
        "Initial storage branch 2 should exist at block 75"
    );

    // ========== At block 100: Mark paths as deleted (ONLY in removed_nodes) ==========
    let mut deletion_trie_updates = TrieUpdates::default();

    // Add to removed_nodes ONLY (no updates)
    deletion_trie_updates.removed_nodes.insert(account_path1);

    // Do the same for storage branch
    let mut deletion_storage_trie = StorageTrieUpdates::default();
    deletion_storage_trie.removed_nodes.insert(storage_path1);
    deletion_trie_updates.insert_storage_updates(storage_address, deletion_storage_trie);

    let deletion_diff = BlockStateDiff {
        sorted_trie_updates: deletion_trie_updates.into_sorted(),
        sorted_post_state: HashedPostStateSorted::default(),
    };

    let block_ref_100 =
        BlockWithParent::new(B256::repeat_byte(0x96), NumHash::new(100, B256::repeat_byte(0x97)));

    storage.store_trie_updates(block_ref_100, deletion_diff)?;

    // ========== Verify that deleted nodes return None at block 150 ==========

    // Deleted account branch should not be found
    let mut cursor_150 = storage.account_trie_cursor(150)?;
    let account_result = cursor_150.seek_exact(account_path1)?;
    assert!(account_result.is_none(), "Deleted account branch should return None at block 150");

    // Non-deleted account branch should still exist
    let account_result2 = cursor_150.seek_exact(account_path2)?;
    assert!(
        account_result2.is_some(),
        "Non-deleted account branch should still exist at block 150"
    );

    // Deleted storage branch should not be found
    let mut storage_cursor_150 = storage.storage_trie_cursor(storage_address, 150)?;
    let storage_result = storage_cursor_150.seek_exact(storage_path1)?;
    assert!(storage_result.is_none(), "Deleted storage branch should return None at block 150");

    // Non-deleted storage branch should still exist
    let storage_result2 = storage_cursor_150.seek_exact(storage_path2)?;
    assert!(
        storage_result2.is_some(),
        "Non-deleted storage branch should still exist at block 150"
    );

    // ========== Verify that the nodes still exist at block 75 (before deletion) ==========
    let mut cursor_75_after = storage.account_trie_cursor(75)?;
    assert!(
        cursor_75_after.seek_exact(account_path1)?.is_some(),
        "Deleted node should still exist at block 75 (before deletion)"
    );

    let mut storage_cursor_75_after = storage.storage_trie_cursor(storage_address, 75)?;
    assert!(
        storage_cursor_75_after.seek_exact(storage_path1)?.is_some(),
        "Deleted storage node should still exist at block 75 (before deletion)"
    );

    // ========== Verify iteration skips deleted nodes ==========
    let mut cursor_iter = storage.account_trie_cursor(150)?;
    let mut found_paths = Vec::new();
    while let Some((path, _)) = cursor_iter.next()? {
        found_paths.push(path);
    }

    assert!(!found_paths.contains(&account_path1), "Iteration should skip deleted node");
    assert!(found_paths.contains(&account_path2), "Iteration should include non-deleted node");

    Ok(())
}

/// Test that updates take precedence over removals when both are present
///
/// This test verifies that when a path appears in both `removed_nodes` and `account_nodes`,
/// the update from `account_nodes` takes precedence. This is critical for correctness
/// when processing trie updates that both remove and update the same node.
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_updates_take_precedence_over_removals<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    // ========== Setup: Store initial branch nodes at block 50 ==========
    let account_path = nibbles_from(vec![1, 2, 3]);
    let storage_path = nibbles_from(vec![4, 5, 6]);
    let storage_address = B256::repeat_byte(0x42);

    let initial_branch = create_test_branch();

    let mut initial_trie_updates = TrieUpdates::default();
    initial_trie_updates.account_nodes.insert(account_path, initial_branch.clone());

    let mut storage_trie = StorageTrieUpdates::default();
    storage_trie.storage_nodes.insert(storage_path, initial_branch.clone());
    initial_trie_updates.insert_storage_updates(storage_address, storage_trie);

    let initial_diff = BlockStateDiff {
        sorted_trie_updates: initial_trie_updates.into_sorted(),
        sorted_post_state: HashedPostStateSorted::default(),
    };

    let block_ref_50 = BlockWithParent::new(B256::ZERO, NumHash::new(50, B256::repeat_byte(0x96)));

    storage.store_trie_updates(block_ref_50, initial_diff)?;

    // Verify initial state exists at block 75
    let mut cursor_75 = storage.account_trie_cursor(75)?;
    assert!(
        cursor_75.seek_exact(account_path)?.is_some(),
        "Initial account branch should exist at block 75"
    );

    let mut storage_cursor_75 = storage.storage_trie_cursor(storage_address, 75)?;
    assert!(
        storage_cursor_75.seek_exact(storage_path)?.is_some(),
        "Initial storage branch should exist at block 75"
    );

    // ========== At block 100: Add paths to BOTH removed_nodes AND account_nodes ==========
    // This simulates a scenario where a node is both removed and updated
    // The update should take precedence
    let updated_branch = create_test_branch_variant();

    let mut conflicting_trie_updates = TrieUpdates::default();

    // Add to removed_nodes
    conflicting_trie_updates.removed_nodes.insert(account_path);

    // Also add to account_nodes (this should take precedence)
    conflicting_trie_updates.account_nodes.insert(account_path, updated_branch.clone());

    // Do the same for storage branch
    let mut conflicting_storage_trie = StorageTrieUpdates::default();
    conflicting_storage_trie.removed_nodes.insert(storage_path);
    conflicting_storage_trie.storage_nodes.insert(storage_path, updated_branch.clone());
    conflicting_trie_updates.insert_storage_updates(storage_address, conflicting_storage_trie);

    let conflicting_diff = BlockStateDiff {
        sorted_trie_updates: conflicting_trie_updates.into_sorted(),
        sorted_post_state: HashedPostStateSorted::default(),
    };

    let block_ref_100 =
        BlockWithParent::new(B256::repeat_byte(0x96), NumHash::new(100, B256::repeat_byte(0x97)));

    storage.store_trie_updates(block_ref_100, conflicting_diff)?;

    // ========== Verify that updates took precedence at block 150 ==========

    // Account branch should exist (not deleted) with the updated value
    let mut cursor_150 = storage.account_trie_cursor(150)?;
    let account_result = cursor_150.seek_exact(account_path)?;
    assert!(
        account_result.is_some(),
        "Account branch should exist at block 150 (update should take precedence over removal)"
    );
    let (found_path, found_branch) = account_result.unwrap();
    assert_eq!(found_path, account_path);
    // Verify it's the updated branch, not the initial one
    assert_eq!(
        found_branch.state_mask, updated_branch.state_mask,
        "Account branch should be the updated version, not the initial one"
    );

    // Storage branch should exist (not deleted) with the updated value
    let mut storage_cursor_150 = storage.storage_trie_cursor(storage_address, 150)?;
    let storage_result = storage_cursor_150.seek_exact(storage_path)?;
    assert!(
        storage_result.is_some(),
        "Storage branch should exist at block 150 (update should take precedence over removal)"
    );
    let (found_storage_path, found_storage_branch) = storage_result.unwrap();
    assert_eq!(found_storage_path, storage_path);
    // Verify it's the updated branch
    assert_eq!(
        found_storage_branch.state_mask, updated_branch.state_mask,
        "Storage branch should be the updated version, not the initial one"
    );

    // ========== Verify that the old version still exists at block 75 ==========
    let mut cursor_75_after = storage.account_trie_cursor(75)?;
    let result_75 = cursor_75_after.seek_exact(account_path)?;
    assert!(result_75.is_some(), "Initial version should still exist at block 75");
    let (_, branch_75) = result_75.unwrap();
    assert_eq!(
        branch_75.state_mask, initial_branch.state_mask,
        "Block 75 should see the initial branch, not the updated one"
    );

    Ok(())
}
