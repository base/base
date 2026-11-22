//! Branch trie cursor behavior tests.

use rstest::rstest;
use serial_test::serial;

use super::*;

/// Test cursor operations on empty trie
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_cursor_empty_trie<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let mut cursor = storage.account_trie_cursor(100)?;

    assert!(cursor.seek_exact(Nibbles::default())?.is_none());
    assert!(cursor.seek(Nibbles::default())?.is_none());
    assert!(cursor.next()?.is_none());
    assert!(cursor.current()?.is_none());

    Ok(())
}

/// Test cursor operations with single entry
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_cursor_single_entry<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2, 3]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;

    let result = cursor.seek_exact(path)?.unwrap();
    assert_eq!(result.0, path);

    assert_eq!(cursor.current()?.unwrap(), path);

    assert!(cursor.next()?.is_none());

    Ok(())
}

/// `seek_exact` on an absent key returns `None` and leaves `current()` unset.
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_seek_exact_absent_key_returns_none<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let present = nibbles_from(vec![1, 2, 3]);
    let absent = nibbles_from(vec![4, 5, 6]);

    storage.store_account_branches(vec![(present, Some(create_test_branch()))])?;

    let mut cursor = storage.account_trie_cursor(100)?;

    assert!(cursor.seek_exact(absent)?.is_none());
    assert!(cursor.current()?.is_none());

    assert_eq!(cursor.seek_exact(present)?.map(|(k, _)| k), Some(present));

    Ok(())
}

/// Test cursor operations with multiple entries
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_cursor_multiple_entries<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let paths = vec![
        nibbles_from(vec![1]),
        nibbles_from(vec![1, 2]),
        nibbles_from(vec![2]),
        nibbles_from(vec![2, 3]),
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

    assert_eq!(found_paths.len(), 4);
    for i in 0..paths.len() {
        assert_eq!(found_paths[i], paths[i]);
    }

    Ok(())
}

/// Test `seek_exact` with empty path
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_seek_exact_empty_path<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
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
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_seek_to_existing_path<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
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
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_seek_between_existing_nodes<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path1 = nibbles_from(vec![1]);
    let path2 = nibbles_from(vec![3]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path1, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path2, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    let seek_path = nibbles_from(vec![2]);
    let result = cursor.seek(seek_path)?.unwrap();
    assert_eq!(result.0, path2);

    Ok(())
}

/// Test seek after all nodes
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_seek_after_all_nodes<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    let seek_path = nibbles_from(vec![9]);
    assert!(cursor.seek(seek_path)?.is_none());

    Ok(())
}

/// Test seek before all nodes
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_seek_before_all_nodes<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![5]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    let seek_path = nibbles_from(vec![1]);
    let result = cursor.seek(seek_path)?.unwrap();
    assert_eq!(result.0, path);

    Ok(())
}

/// Test next after seek
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_next_after_seek<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path1 = nibbles_from(vec![1]);
    let path2 = nibbles_from(vec![2]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path1, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path2, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;
    cursor.seek(path1)?;

    let result = cursor.next()?.unwrap();
    assert_eq!(result.0, path2);

    Ok(())
}

/// Test multiple consecutive next calls
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_multiple_consecutive_next<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let paths = vec![nibbles_from(vec![1]), nibbles_from(vec![2]), nibbles_from(vec![3])];
    let branch = create_test_branch();

    for path in &paths {
        storage.store_account_branches(vec![(*path, Some(branch.clone()))])?;
    }

    let mut cursor = storage.account_trie_cursor(100)?;

    for expected_path in &paths {
        let result = cursor.next()?.unwrap();
        assert_eq!(result.0, *expected_path);
    }

    assert!(cursor.next()?.is_none());

    Ok(())
}

/// Test current after operations
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_current_after_operations<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path1 = nibbles_from(vec![1]);
    let path2 = nibbles_from(vec![2]);
    let branch = create_test_branch();

    storage.store_account_branches(vec![(path1, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path2, Some(branch))])?;

    let mut cursor = storage.account_trie_cursor(100)?;

    assert!(cursor.current()?.is_none());

    cursor.seek(path1)?;
    assert_eq!(cursor.current()?.unwrap(), path1);

    cursor.next()?;
    assert_eq!(cursor.current()?.unwrap(), path2);

    Ok(())
}

/// Test that rewriting the same path keeps only the latest node
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_rewriting_same_path_keeps_latest_node<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2]);
    let branch1 = create_test_branch();
    let branch2 = create_test_branch_variant();

    // Initial-state writes share one version, so the later write replaces the earlier one.
    storage.store_account_branches(vec![(path, Some(branch1))])?;
    storage.store_account_branches(vec![(path, Some(branch2.clone()))])?;

    let mut cursor = storage.account_trie_cursor(150)?;
    assert_eq!(cursor.seek_exact(path)?, Some((path, branch2)));

    Ok(())
}

/// Test deleted branch nodes
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_deleted_branch_nodes<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2]);
    let branch = create_test_branch();
    let block_ref = BlockWithParent::new(B256::ZERO, NumHash::new(100, B256::repeat_byte(0x96)));

    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut cursor75 = storage.account_trie_cursor(75)?;
    assert!(cursor75.seek_exact(path)?.is_some());

    let mut block_state_diff_trie_updates = TrieUpdates::default();
    block_state_diff_trie_updates.removed_nodes.insert(path);
    let block_state_diff = BlockStateDiff {
        sorted_trie_updates: block_state_diff_trie_updates.into_sorted(),
        sorted_post_state: HashedPostStateSorted::default(),
    };
    storage.store_trie_updates(block_ref, block_state_diff)?;

    let mut cursor150 = storage.account_trie_cursor(150)?;
    assert!(cursor150.seek_exact(path)?.is_none());

    Ok(())
}

/// Test account-specific cursor
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_account_specific_cursor<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2]);
    let addr1 = B256::repeat_byte(0x01);
    let addr2 = B256::repeat_byte(0x02);
    let branch = create_test_branch();

    storage.store_storage_branches(addr1, vec![(path, Some(branch.clone()))])?;
    storage.store_storage_branches(addr2, vec![(path, Some(branch))])?;

    let mut cursor1 = storage.storage_trie_cursor(addr1, 100)?;
    let result1 = cursor1.seek_exact(path)?.unwrap();
    assert_eq!(result1.0, path);

    let mut cursor2 = storage.storage_trie_cursor(addr2, 100)?;
    let result2 = cursor2.seek_exact(path)?.unwrap();
    assert_eq!(result2.0, path);

    let mut cursor1_iter = storage.storage_trie_cursor(addr1, 100)?;
    let mut found_count = 0;
    while cursor1_iter.next()?.is_some() {
        found_count += 1;
    }
    assert_eq!(found_count, 1);

    Ok(())
}

/// Test state trie cursor
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_state_trie_cursor<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path = nibbles_from(vec![1, 2]);
    let addr = B256::repeat_byte(0x01);
    let branch = create_test_branch();

    storage.store_storage_branches(addr, vec![(path, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path, Some(branch))])?;

    let mut state_cursor = storage.account_trie_cursor(100)?;
    let result = state_cursor.seek_exact(path)?.unwrap();
    assert_eq!(result.0, path);

    let mut state_cursor_iter = storage.account_trie_cursor(100)?;
    let mut found_count = 0;
    while state_cursor_iter.next()?.is_some() {
        found_count += 1;
    }

    assert_eq!(found_count, 1);

    Ok(())
}

/// Test mixed account and state data
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_mixed_account_state_data<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let path1 = nibbles_from(vec![1]);
    let path2 = nibbles_from(vec![2]);
    let addr = B256::repeat_byte(0x01);
    let branch = create_test_branch();

    storage.store_storage_branches(addr, vec![(path1, Some(branch.clone()))])?;
    storage.store_account_branches(vec![(path2, Some(branch))])?;

    let mut account_cursor = storage.storage_trie_cursor(addr, 100)?;
    let mut account_paths = Vec::new();
    while let Some((path, _)) = account_cursor.next()? {
        account_paths.push(path);
    }
    assert_eq!(account_paths.len(), 1);
    assert_eq!(account_paths[0], path1);

    let mut state_cursor = storage.account_trie_cursor(100)?;
    let mut state_paths = Vec::new();
    while let Some((path, _)) = state_cursor.next()? {
        state_paths.push(path);
    }
    assert_eq!(state_paths.len(), 1);
    assert_eq!(state_paths[0], path2);

    Ok(())
}

/// Test lexicographic ordering
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_lexicographic_ordering<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let paths = vec![
        nibbles_from(vec![3, 1]),
        nibbles_from(vec![1, 2]),
        nibbles_from(vec![2]),
        nibbles_from(vec![1]),
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
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_path_prefix_scenarios<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
    let paths = vec![nibbles_from(vec![1]), nibbles_from(vec![1, 2]), nibbles_from(vec![1, 2, 3])];
    let branch = create_test_branch();

    for path in &paths {
        storage.store_account_branches(vec![(*path, Some(branch.clone()))])?;
    }

    let mut cursor = storage.account_trie_cursor(100)?;

    let result = cursor.seek_exact(paths[0])?.unwrap();
    assert_eq!(result.0, paths[0]);

    let result = cursor.next()?.unwrap();
    assert_eq!(result.0, paths[1]);

    let result = cursor.next()?.unwrap();
    assert_eq!(result.0, paths[2]);

    Ok(())
}

/// Test complex nibble combinations
#[rstest]
#[case::in_memory(InMemoryProofsStorage::new())]
#[case::mdbx(create_mdbx_proofs_storage())]
#[case::rocksdb(create_rocksdb_proofs_storage())]
#[serial]
fn test_complex_nibble_combinations<S: BaseProofsStore + BaseProofsInitialStateStore>(
    #[case] storage: S,
) -> Result<(), BaseProofsStorageError> {
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

    assert_eq!(found_paths.len(), 5);

    assert_eq!(found_paths[0], nibbles_from(vec![0]));
    assert_eq!(found_paths[1], nibbles_from(vec![0, 15]));
    assert_eq!(found_paths[4], nibbles_from(vec![15, 0]));

    Ok(())
}
