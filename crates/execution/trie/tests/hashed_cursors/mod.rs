//! Hashed account and storage cursor behavior tests.

use serial_test::serial;
use test_case::test_case;

use super::*;

// =============================================================================
// 7. Leaf Node Tests (Hashed Accounts and Storage)
// =============================================================================

/// Test store and retrieve single account
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_store_and_retrieve_single_account<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let account_key = B256::repeat_byte(0x01);
    let account = create_test_account();

    // Store account
    storage.store_hashed_accounts(vec![(account_key, Some(account))])?;

    // Retrieve via cursor
    let mut cursor = storage.account_hashed_cursor(100)?;
    let result = cursor.seek(account_key)?.unwrap();

    assert_eq!(result.0, account_key);
    assert_eq!(result.1.nonce, account.nonce);
    assert_eq!(result.1.balance, account.balance);
    assert_eq!(result.1.bytecode_hash, account.bytecode_hash);

    Ok(())
}

/// Test account cursor navigation
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_account_cursor_navigation<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let accounts = [
        (B256::repeat_byte(0x01), create_test_account()),
        (B256::repeat_byte(0x03), create_test_account()),
        (B256::repeat_byte(0x05), create_test_account()),
    ];

    // Store accounts
    let accounts_to_store: Vec<_> = accounts.iter().map(|(k, v)| (*k, Some(*v))).collect();
    storage.store_hashed_accounts(accounts_to_store)?;

    let mut cursor = storage.account_hashed_cursor(100)?;

    // Test seeking to exact key
    let result = cursor.seek(accounts[1].0)?.unwrap();
    assert_eq!(result.0, accounts[1].0);

    // Test seeking to key that doesn't exist (should return next greater)
    let seek_key = B256::repeat_byte(0x02);
    let result = cursor.seek(seek_key)?.unwrap();
    assert_eq!(result.0, accounts[1].0); // Should find 0x03

    // Test next() navigation
    let result = cursor.next()?.unwrap();
    assert_eq!(result.0, accounts[2].0); // Should find 0x05

    // Test next() at end
    assert!(cursor.next()?.is_none());

    Ok(())
}

/// Test that a `RocksDB` cursor reads from the snapshot captured when it is created.
#[test]
#[serial]
fn test_rocksdb_account_cursor_uses_creation_snapshot() -> Result<(), BaseProofsStorageError> {
    let storage = create_rocksdb_proofs_storage();
    let key1 = B256::repeat_byte(0x01);
    let key2 = B256::repeat_byte(0x02);
    let account1 = create_test_account_with_values(1, 100, 0xAA);
    let account2 = create_test_account_with_values(2, 200, 0xBB);

    let block1 = BlockWithParent::new(B256::ZERO, NumHash::new(1, B256::repeat_byte(0x11)));
    let mut post_state = HashedPostState::default();
    post_state.accounts.insert(key1, Some(account1));
    post_state.accounts.insert(key2, Some(account2));
    storage.store_trie_updates(
        block1,
        BlockStateDiff {
            sorted_trie_updates: TrieUpdatesSorted::default(),
            sorted_post_state: post_state.into_sorted(),
        },
    )?;

    let mut cursor = storage.account_hashed_cursor(1)?;
    let first = cursor.seek(key1)?.expect("first account exists");
    assert_eq!(first.0, key1);

    let replacement_block1 =
        BlockWithParent::new(B256::ZERO, NumHash::new(1, B256::repeat_byte(0x22)));
    let mut replacement_post_state = HashedPostState::default();
    replacement_post_state.accounts.insert(key1, Some(account1));
    storage.replace_updates(
        BlockNumHash::new(0, B256::ZERO),
        vec![(
            replacement_block1,
            BlockStateDiff {
                sorted_trie_updates: TrieUpdatesSorted::default(),
                sorted_post_state: replacement_post_state.into_sorted(),
            },
        )],
    )?;

    let next = cursor.next()?.expect("existing cursor keeps its original snapshot");
    assert_eq!(next.0, key2);
    assert_eq!(next.1.nonce, account2.nonce);

    let mut fresh_cursor = storage.account_hashed_cursor(1)?;
    let fresh_result = fresh_cursor.seek(key2)?;
    assert!(
        !matches!(fresh_result, Some((found_key, _)) if found_key == key2),
        "fresh cursor should not see the replaced account"
    );

    Ok(())
}

/// Test account block versioning
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_account_block_versioning<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let account_key = B256::repeat_byte(0x01);
    let account_v1 = create_test_account_with_values(1, 100, 0xBB);
    let account_v2 = create_test_account_with_values(2, 200, 0xDD);

    // Store account at different blocks
    storage.store_hashed_accounts(vec![(account_key, Some(account_v1))])?;

    // Cursor with max_block_number=75 should see v1
    let mut cursor75 = storage.account_hashed_cursor(75)?;
    let result75 = cursor75.seek(account_key)?.unwrap();
    assert_eq!(result75.1.nonce, account_v1.nonce);
    assert_eq!(result75.1.balance, account_v1.balance);

    storage.store_hashed_accounts(vec![(account_key, Some(account_v2))])?;

    // After update, Cursor with max_block_number=150 should see v2
    let mut cursor150 = storage.account_hashed_cursor(150)?;
    let result150 = cursor150.seek(account_key)?.unwrap();
    assert_eq!(result150.1.nonce, account_v2.nonce);
    assert_eq!(result150.1.balance, account_v2.balance);

    Ok(())
}

/// Test store and retrieve storage
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]

fn test_store_and_retrieve_storage<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let hashed_address = B256::repeat_byte(0x01);
    let storage_slots = vec![
        (B256::repeat_byte(0x10), U256::from(100)),
        (B256::repeat_byte(0x20), U256::from(200)),
        (B256::repeat_byte(0x30), U256::from(300)),
    ];

    // Store storage slots
    storage.store_hashed_storages(hashed_address, storage_slots.clone())?;

    // Retrieve via cursor
    let mut cursor = storage.storage_hashed_cursor(hashed_address, 100)?;

    // Test seeking to each slot
    for (key, expected_value) in &storage_slots {
        let result = cursor.seek(*key)?.unwrap();
        assert_eq!(result.0, *key);
        assert_eq!(result.1, *expected_value);
    }

    Ok(())
}

/// Test storage cursor navigation
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_storage_cursor_navigation<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let hashed_address = B256::repeat_byte(0x01);
    let storage_slots = vec![
        (B256::repeat_byte(0x10), U256::from(100)),
        (B256::repeat_byte(0x30), U256::from(300)),
        (B256::repeat_byte(0x50), U256::from(500)),
    ];

    storage.store_hashed_storages(hashed_address, storage_slots.clone())?;

    let mut cursor = storage.storage_hashed_cursor(hashed_address, 100)?;

    // Start from beginning with next()
    let mut found_slots = Vec::new();
    while let Some((key, value)) = cursor.next()? {
        found_slots.push((key, value));
    }

    assert_eq!(found_slots.len(), 3);
    assert_eq!(found_slots[0], storage_slots[0]);
    assert_eq!(found_slots[1], storage_slots[1]);
    assert_eq!(found_slots[2], storage_slots[2]);

    Ok(())
}

/// Test storage account isolation
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_storage_account_isolation<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let address1 = B256::repeat_byte(0x01);
    let address2 = B256::repeat_byte(0x02);
    let storage_key = B256::repeat_byte(0x10);

    // Store same storage key for different accounts
    storage.store_hashed_storages(address1, vec![(storage_key, U256::from(100))])?;
    storage.store_hashed_storages(address2, vec![(storage_key, U256::from(200))])?;

    // Verify each account sees only its own storage
    let mut cursor1 = storage.storage_hashed_cursor(address1, 100)?;
    let result1 = cursor1.seek(storage_key)?.unwrap();
    assert_eq!(result1.1, U256::from(100));

    let mut cursor2 = storage.storage_hashed_cursor(address2, 100)?;
    let result2 = cursor2.seek(storage_key)?.unwrap();
    assert_eq!(result2.1, U256::from(200));

    // Verify cursor1 doesn't see address2's storage
    let mut cursor1_iter = storage.storage_hashed_cursor(address1, 100)?;
    let mut count = 0;
    while cursor1_iter.next()?.is_some() {
        count += 1;
    }
    assert_eq!(count, 1); // Should only see one entry

    Ok(())
}

/// Test storage block versioning
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_storage_block_versioning<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let hashed_address = B256::repeat_byte(0x01);
    let storage_key = B256::repeat_byte(0x10);

    // Store storage at different blocks
    storage.store_hashed_storages(hashed_address, vec![(storage_key, U256::from(100))])?;

    // Cursor with max_block_number=75 should see old value
    let mut cursor75 = storage.storage_hashed_cursor(hashed_address, 75)?;
    let result75 = cursor75.seek(storage_key)?.unwrap();
    assert_eq!(result75.1, U256::from(100));

    storage.store_hashed_storages(hashed_address, vec![(storage_key, U256::from(200))])?;
    // Cursor with max_block_number=150 should see new value
    let mut cursor150 = storage.storage_hashed_cursor(hashed_address, 150)?;
    let result150 = cursor150.seek(storage_key)?.unwrap();
    assert_eq!(result150.1, U256::from(200));

    Ok(())
}

/// Test storage zero value deletion
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_storage_zero_value_deletion<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let hashed_address = B256::repeat_byte(0x01);
    let storage_key = B256::repeat_byte(0x10);

    // Store non-zero value
    storage.store_hashed_storages(hashed_address, vec![(storage_key, U256::from(100))])?;

    // Cursor before deletion should see the value
    let mut cursor75 = storage.storage_hashed_cursor(hashed_address, 75)?;
    let result75 = cursor75.seek(storage_key)?.unwrap();
    assert_eq!(result75.1, U256::from(100));

    // "Delete" by storing zero value at block 100
    let mut block_state_diff_post_state = HashedPostState::default();
    let mut hashed_storage = HashedStorage::default();
    hashed_storage.storage.insert(storage_key, U256::ZERO);
    block_state_diff_post_state.storages.insert(hashed_address, hashed_storage);

    let block_ref = BlockWithParent::new(B256::ZERO, NumHash::new(100, B256::repeat_byte(0x96)));
    let block_state_diff = BlockStateDiff {
        sorted_trie_updates: TrieUpdatesSorted::default(),
        sorted_post_state: block_state_diff_post_state.into_sorted(),
    };
    storage.store_trie_updates(block_ref, block_state_diff)?;

    // Cursor after deletion should NOT see the entry (zero values are skipped)
    let mut cursor150 = storage.storage_hashed_cursor(hashed_address, 150)?;
    let result150 = cursor150.seek(storage_key)?;
    assert!(result150.is_none(), "Zero values should be skipped/deleted");

    Ok(())
}

/// Test that zero values are skipped during iteration
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_storage_cursor_skips_zero_values<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let hashed_address = B256::repeat_byte(0x01);

    // Create a mix of non-zero and zero value storage slots
    let storage_slots = vec![
        (B256::repeat_byte(0x10), U256::from(100)), // Non-zero
        (B256::repeat_byte(0x20), U256::ZERO),      // Zero value - should be skipped
        (B256::repeat_byte(0x30), U256::from(300)), // Non-zero
        (B256::repeat_byte(0x40), U256::ZERO),      // Zero value - should be skipped
        (B256::repeat_byte(0x50), U256::from(500)), // Non-zero
    ];

    // Store all slots
    storage.store_hashed_storages(hashed_address, storage_slots)?;

    // Create cursor and iterate through all entries
    let mut cursor = storage.storage_hashed_cursor(hashed_address, 100)?;
    let mut found_slots = Vec::new();
    while let Some((key, value)) = cursor.next()? {
        found_slots.push((key, value));
    }

    // Should only find 3 non-zero values
    assert_eq!(found_slots.len(), 3, "Zero values should be skipped during iteration");

    // Verify the non-zero values are the ones we stored
    assert_eq!(found_slots[0], (B256::repeat_byte(0x10), U256::from(100)));
    assert_eq!(found_slots[1], (B256::repeat_byte(0x30), U256::from(300)));
    assert_eq!(found_slots[2], (B256::repeat_byte(0x50), U256::from(500)));

    // Verify seeking to a zero-value slot returns None or skips to next non-zero
    let mut seek_cursor = storage.storage_hashed_cursor(hashed_address, 100)?;
    let seek_result = seek_cursor.seek(B256::repeat_byte(0x20))?;

    // Should either return None or skip to the next non-zero value (0x30)
    if let Some((key, value)) = seek_result {
        assert_eq!(key, B256::repeat_byte(0x30), "Should skip zero value and find next non-zero");
        assert_eq!(value, U256::from(300));
    }

    Ok(())
}

/// Test empty cursors
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_empty_cursors<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    // Test empty account cursor
    let mut account_cursor = storage.account_hashed_cursor(100)?;
    assert!(account_cursor.seek(B256::repeat_byte(0x01))?.is_none());
    assert!(account_cursor.next()?.is_none());

    // Test empty storage cursor
    let mut storage_cursor = storage.storage_hashed_cursor(B256::repeat_byte(0x01), 100)?;
    assert!(storage_cursor.seek(B256::repeat_byte(0x10))?.is_none());
    assert!(storage_cursor.next()?.is_none());

    Ok(())
}

/// Test cursor boundary conditions
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_cursor_boundary_conditions<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let account_key = B256::repeat_byte(0x80); // Middle value
    let account = create_test_account();

    storage.store_hashed_accounts(vec![(account_key, Some(account))])?;

    let mut cursor = storage.account_hashed_cursor(100)?;

    // Seek to minimum key should find our account
    let result = cursor.seek(B256::ZERO)?.unwrap();
    assert_eq!(result.0, account_key);

    // Seek to maximum key should find nothing
    assert!(cursor.seek(B256::repeat_byte(0xFF))?.is_none());

    // Seek to key just before our account should find our account
    let just_before = B256::repeat_byte(0x7F);
    let result = cursor.seek(just_before)?.unwrap();
    assert_eq!(result.0, account_key);

    Ok(())
}

/// Test large batch operations
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage(); "Rocksdb")]
#[serial]
fn test_large_batch_operations<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    // Create large batch of accounts
    let mut accounts = Vec::new();
    for i in 0..100 {
        let key = B256::from([i as u8; 32]);
        let account = create_test_account_with_values(i, i * 1000, (i + 1) as u8);
        accounts.push((key, Some(account)));
    }

    // Store in batch
    storage.store_hashed_accounts(accounts.clone())?;

    // Verify all accounts can be retrieved
    let mut cursor = storage.account_hashed_cursor(100)?;
    let mut found_count = 0;
    while cursor.next()?.is_some() {
        found_count += 1;
    }
    assert_eq!(found_count, 100);

    // Test specific account retrieval
    let test_key = B256::from([42u8; 32]);
    let result = cursor.seek(test_key)?.unwrap();
    assert_eq!(result.0, test_key);
    assert_eq!(result.1.nonce, 42);

    Ok(())
}
