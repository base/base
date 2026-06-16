//! Branch trie cursor behavior tests.

use serial_test::serial;
use test_case::test_case;

use super::*;

// =============================================================================
// 1. Basic Cursor Operations
// =============================================================================

/// Test cursor operations on empty trie
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_cursor_empty_trie<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let mut cursor = storage.account_trie_cursor(100)?;

    // All operations should return None on empty trie
    assert!(cursor.seek_exact(Nibbles::default())?.is_none());
    assert!(cursor.seek(Nibbles::default())?.is_none());
    assert!(cursor.next()?.is_none());
    assert!(cursor.current()?.is_none());

    Ok(())
}

/// Test cursor operations with single entry
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_cursor_single_entry<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2, 3]);
    let branch = create_test_branch();

    // Store single entry
    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;

    // Test seek_exact
    let result = cursor.seek_exact(path)?.unwrap();
    assert_eq!(result.0, path);

    // Test current position
    assert_eq!(cursor.current()?.unwrap(), path);

    // Test next from end should return None
    assert!(cursor.next()?.is_none());

    Ok(())
}

/// Test cursor operations with multiple entries
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_cursor_multiple_entries<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let paths = vec![
        nibbles_from(vec![1]),
        nibbles_from(vec![1, 2]),
        nibbles_from(vec![2]),
        nibbles_from(vec![2, 3]),
    ];
    let branch = create_test_branch();

    // Store multiple entries
    for path in &paths {
        storage.store_account_branches(vec![(*path, Some(branch.clone()))])?;
    }

    let mut cursor = storage.account_trie_cursor(100)?;

    // Test that we can iterate through all entries
    let mut found_paths = Vec::new();
    while let Some((path, _)) = cursor.next()? {
        found_paths.push(path);
    }

    assert_eq!(found_paths.len(), 4);
    // Paths should be in lexicographic order
    for i in 0..paths.len() {
        assert_eq!(found_paths[i], paths[i]);
    }

    Ok(())
}

// =============================================================================
// 2. Seek Operations
// =============================================================================

/// Test `seek_exact` with existing path
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_seek_exact_existing_path<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2, 3]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    let result = cursor.seek_exact(path)?.unwrap();
    assert_eq!(result.0, path);

    Ok(())
}

/// Test `seek_exact` with non-existing path
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_seek_exact_non_existing_path<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2, 3]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    let non_existing = nibbles_from(vec![4, 5, 6]);
    assert!(cursor.seek_exact(non_existing)?.is_none());

    Ok(())
}

/// Test `seek_exact` with empty path
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_seek_exact_empty_path<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    let result = cursor.seek_exact(Nibbles::default())?.unwrap();
    assert_eq!(result.0, Nibbles::default());

    Ok(())
}

/// Test seek to existing path
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_seek_to_existing_path<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2, 3]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    let result = cursor.seek(path)?.unwrap();
    assert_eq!(result.0, path);

    Ok(())
}

/// Test seek between existing nodes
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_seek_between_existing_nodes<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path1 = nibbles_from(vec![1]);
    let path2 = nibbles_from(vec![3]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path1, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path2, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    // Seek to path between 1 and 3, should return path 3
    let seek_path = nibbles_from(vec![2]);
    let result = cursor.seek(seek_path)?.unwrap();
    assert_eq!(result.0, path2);

    Ok(())
}

/// Test seek after all nodes
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_seek_after_all_nodes<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    // Seek to path after all nodes
    let seek_path = nibbles_from(vec![9]);
    assert!(cursor.seek(seek_path)?.is_none());

    Ok(())
}

/// Test seek before all nodes
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_seek_before_all_nodes<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![5]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    // Seek to path before all nodes, should return first node
    let seek_path = nibbles_from(vec![1]);
    let result = cursor.seek(seek_path)?.unwrap();
    assert_eq!(result.0, path);

    Ok(())
}

// =============================================================================
// 3. Navigation Tests
// =============================================================================

/// Test next without prior seek
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_next_without_prior_seek<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    // next() without prior seek should start from beginning
    let result = cursor.next()?.unwrap();
    assert_eq!(result.0, path);

    Ok(())
}

/// Test next after seek
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_next_after_seek<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path1 = nibbles_from(vec![1]);
    let path2 = nibbles_from(vec![2]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path1, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path2, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    cursor.seek(path1)?;

    // next() should return second node
    let result = cursor.next()?.unwrap();
    assert_eq!(result.0, path2);

    Ok(())
}

/// Test next at end of trie
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_next_at_end_of_trie<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    cursor.seek(path)?;

    // next() at end should return None
    assert!(cursor.next()?.is_none());

    Ok(())
}

/// Test multiple consecutive next calls
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_multiple_consecutive_next<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let paths = vec![nibbles_from(vec![1]), nibbles_from(vec![2]), nibbles_from(vec![3])];
    let branch = create_test_branch();

    for path in &paths {
        storage.store_account_branches(vec![(*path, Some(branch.clone()))])?;
    }

    let mut cursor = storage.account_trie_cursor(100)?;

    // Iterate through all with consecutive next() calls
    for expected_path in &paths {
        let result = cursor.next()?.unwrap();
        assert_eq!(result.0, *expected_path);
    }

    // Final next() should return None
    assert!(cursor.next()?.is_none());

    Ok(())
}

/// Test current after operations
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_current_after_operations<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path1 = nibbles_from(vec![1]);
    let path2 = nibbles_from(vec![2]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path1, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path2, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;

    // Current should be None initially
    assert!(cursor.current()?.is_none());

    // After seek, current should track position
    cursor.seek(path1)?;
    assert_eq!(cursor.current()?.unwrap(), path1);

    // After next, current should update
    cursor.next()?;
    assert_eq!(cursor.current()?.unwrap(), path2);

    Ok(())
}

/// Test current with no prior operations
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_current_no_prior_operations<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let mut cursor = storage.account_trie_cursor(100)?;

    // Current should be None when no operations performed
    assert!(cursor.current()?.is_none());

    Ok(())
}

// =============================================================================
// 4. Block Number Filtering
// =============================================================================

/// Test same path with different blocks
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_same_path_different_blocks<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2]);
    let branch1 = create_test_branch();
    let branch2 = create_test_branch_variant();

    // Store same path at different blocks
    storage.store_account_branches(vec![(path, Some(branch1))])?;
    storage.store_account_branches(vec![(path, Some(branch2))])?;

    // Cursor with max_block_number=75 should see only block 50 data
    let mut cursor75 = storage.account_trie_cursor(75)?;
    let result75 = cursor75.seek_exact(path)?.unwrap();
    assert_eq!(result75.0, path);

    // Cursor with max_block_number=150 should see block 100 data (latest)
    let mut cursor150 = storage.account_trie_cursor(150)?;
    let result150 = cursor150.seek_exact(path)?.unwrap();
    assert_eq!(result150.0, path);

    Ok(())
}

/// Test deleted branch nodes
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_deleted_branch_nodes<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2]);
    let branch = create_test_branch();
    let block_ref = BlockWithParent::new(B256::ZERO, NumHash::new(100, B256::repeat_byte(0x96)));

    // Store branch node, then delete it (store None)
    storage.store_account_branches(vec![(path, Some(branch))])?;

    // Cursor before deletion should see the node
    let mut cursor75 = storage.account_trie_cursor(75)?;
    assert!(cursor75.seek_exact(path)?.is_some());

    let mut block_state_diff_trie_updates = TrieUpdates::default();
    block_state_diff_trie_updates.removed_nodes.insert(path);
    let block_state_diff = BlockStateDiff {
        sorted_trie_updates: block_state_diff_trie_updates.into_sorted(),
        sorted_post_state: HashedPostStateSorted::default(),
    };
    storage.store_trie_updates(block_ref, block_state_diff)?;

    // Cursor after deletion should not see the node
    let mut cursor150 = storage.account_trie_cursor(150)?;
    assert!(cursor150.seek_exact(path)?.is_none());

    Ok(())
}

// =============================================================================
// 5. Hashed Address Filtering
// =============================================================================

/// Test account-specific cursor
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_account_specific_cursor<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2]);
    let addr1 = B256::repeat_byte(0x01);
    let addr2 = B256::repeat_byte(0x02);
    let branch = create_test_branch();

    // Store same path for different accounts (using storage branches)
    storage.store_storage_branches(addr1, vec![(path, Some(branch.clone()))])?;
    storage.store_storage_branches(addr2, vec![(path, Some(branch))])?;

    // Cursor for addr1 should only see addr1 data
    let mut cursor1 = storage.storage_trie_cursor(addr1, 100)?;
    let result1 = cursor1.seek_exact(path)?.unwrap();
    assert_eq!(result1.0, path);

    // Cursor for addr2 should only see addr2 data
    let mut cursor2 = storage.storage_trie_cursor(addr2, 100)?;
    let result2 = cursor2.seek_exact(path)?.unwrap();
    assert_eq!(result2.0, path);

    // Cursor for addr1 should not see addr2 data when iterating
    let mut cursor1_iter = storage.storage_trie_cursor(addr1, 100)?;
    let mut found_count = 0;
    while cursor1_iter.next()?.is_some() {
        found_count += 1;
    }
    assert_eq!(found_count, 1); // Should only see one entry (for addr1)

    Ok(())
}

/// Test state trie cursor
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_state_trie_cursor<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2]);
    let addr = B256::repeat_byte(0x01);
    let branch = create_test_branch();

    // Store data for account trie and state trie
    storage.store_storage_branches(addr, vec![(path, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path, Some(branch))])?;

    // State trie cursor (None address) should only see state trie data
    let mut state_cursor = storage.account_trie_cursor(100)?;
    let result = state_cursor.seek_exact(path)?.unwrap();
    assert_eq!(result.0, path);

    // Verify state cursor doesn't see account data when iterating
    let mut state_cursor_iter = storage.account_trie_cursor(100)?;
    let mut found_count = 0;
    while state_cursor_iter.next()?.is_some() {
        found_count += 1;
    }

    assert_eq!(found_count, 1); // Should only see state trie entry

    Ok(())
}

/// Test mixed account and state data
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_mixed_account_state_data<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path1 = nibbles_from(vec![1]);
    let path2 = nibbles_from(vec![2]);
    let addr = B256::repeat_byte(0x01);
    let branch = create_test_branch();

    // Store mixed account and state trie data
    storage.store_storage_branches(addr, vec![(path1, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path2, Some(branch))])?;

    // Account cursor should only see account data
    let mut account_cursor = storage.storage_trie_cursor(addr, 100)?;
    let mut account_paths = Vec::new();
    while let Some((path, _)) = account_cursor.next()? {
        account_paths.push(path);
    }
    assert_eq!(account_paths.len(), 1);
    assert_eq!(account_paths[0], path1);

    // State cursor should only see state data
    let mut state_cursor = storage.account_trie_cursor(100)?;
    let mut state_paths = Vec::new();
    while let Some((path, _)) = state_cursor.next()? {
        state_paths.push(path);
    }
    assert_eq!(state_paths.len(), 1);
    assert_eq!(state_paths[0], path2);

    Ok(())
}

// =============================================================================
// 6. Path Ordering Tests
// =============================================================================

/// Test lexicographic ordering
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_lexicographic_ordering<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let paths = vec![
        nibbles_from(vec![3, 1]),
        nibbles_from(vec![1, 2]),
        nibbles_from(vec![2]),
        nibbles_from(vec![1]),
    ];
    let branch = create_test_branch();

    // Store paths in random order
    for path in &paths {
        storage.store_account_branches(vec![(*path, Some(branch.clone()))])?;
    }

    let mut cursor = storage.account_trie_cursor(100)?;
    let mut found_paths = Vec::new();
    while let Some((path, _)) = cursor.next()? {
        found_paths.push(path);
    }

    // Should be returned in lexicographic order: [1], [1,2], [2], [3,1]
    let expected_order = vec![
        nibbles_from(vec![1]),
        nibbles_from(vec![1, 2]),
        nibbles_from(vec![2]),
        nibbles_from(vec![3, 1]),
    ];

    assert_eq!(found_paths, expected_order);

    Ok(())
}

/// Test path prefix scenarios
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_path_prefix_scenarios<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    let paths = vec![
        nibbles_from(vec![1]),       // Prefix of next
        nibbles_from(vec![1, 2]),    // Extends first
        nibbles_from(vec![1, 2, 3]), // Extends second
    ];
    let branch = create_test_branch();

    for path in &paths {
        storage.store_account_branches(vec![(*path, Some(branch.clone()))])?;
    }

    let mut cursor = storage.account_trie_cursor(100)?;

    // Seek to prefix should find exact match
    let result = cursor.seek_exact(paths[0])?.unwrap();
    assert_eq!(result.0, paths[0]);

    // Next should go to next path, not skip prefixed paths
    let result = cursor.next()?.unwrap();
    assert_eq!(result.0, paths[1]);

    let result = cursor.next()?.unwrap();
    assert_eq!(result.0, paths[2]);

    Ok(())
}

/// Test complex nibble combinations
#[test_case(InMemoryProofsStorage::new(); "InMemory")]
#[test_case(create_mdbx_proofs_storage(); "Mdbx")]
#[test_case(create_rocksdb_proofs_storage() => ignore; "Rocksdb")]
#[serial]
fn test_complex_nibble_combinations<S: BaseProofsStore + BaseProofsInitialStateStore>(
    storage: S,
) -> Result<(), BaseProofsStorageError> {
    // Test various nibble patterns including edge values
    let paths = vec![
        nibbles_from(vec![0]),
        nibbles_from(vec![0, 15]),
        nibbles_from(vec![15]),
        nibbles_from(vec![15, 0]),
        nibbles_from(vec![7, 8, 9]),
    ];
    let branch = create_test_branch();

    for path in &paths {
        storage.store_account_branches(vec![(*path, Some(branch.clone()))])?;
    }

    let mut cursor = storage.account_trie_cursor(100)?;
    let mut found_paths = Vec::new();
    while let Some((path, _)) = cursor.next()? {
        found_paths.push(path);
    }

    // All paths should be found and in correct order
    assert_eq!(found_paths.len(), 5);

    // Verify specific ordering for edge cases
    assert_eq!(found_paths[0], nibbles_from(vec![0]));
    assert_eq!(found_paths[1], nibbles_from(vec![0, 15]));
    assert_eq!(found_paths[4], nibbles_from(vec![15, 0]));

    Ok(())
}
