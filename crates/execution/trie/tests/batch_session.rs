//! Integration tests for [`MdbxProofsStorage`]'s batch session: cross-block atomicity,
//! transaction-local read visibility, and abort-on-error rollback.

use std::sync::Arc;

use alloy_eips::{NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
use base_execution_trie::{
    BaseProofsBatchSession, BaseProofsBatchStore, BaseProofsStore, BlockStateDiff,
    MdbxProofsStorage,
};
use reth_trie::{
    BranchNodeCompact, HashedPostState, HashedStorage, Nibbles,
    hashed_cursor::HashedCursor,
    trie_cursor::TrieCursor,
    updates::{StorageTrieUpdates, TrieUpdates, TrieUpdatesSorted},
};
use tempfile::TempDir;

const fn b256(byte: u8) -> B256 {
    B256::new([byte; 32])
}

const fn block(num: u64) -> BlockWithParent {
    let parent = if num == 0 { B256::ZERO } else { b256((num - 1) as u8) };
    BlockWithParent::new(parent, NumHash::new(num, b256(num as u8)))
}

fn setup() -> (TempDir, Arc<MdbxProofsStorage>) {
    let dir = TempDir::new().expect("tmp dir");
    let store = Arc::new(MdbxProofsStorage::new(dir.path()).expect("mdbx env"));
    store.set_earliest_block_number(0, b256(0)).expect("set earliest");
    store.store_trie_updates(block(0), BlockStateDiff::default()).expect("seed block 0");
    (dir, store)
}

#[test]
fn batch_session_commits_all_blocks_atomically() {
    let (_dir, store) = setup();

    store
        .with_batch_session(|session| {
            for n in 1..=5 {
                session.store_trie_updates(block(n), BlockStateDiff::default())?;
            }
            Ok(())
        })
        .expect("batch commit");

    let (latest, _) = store.get_latest_block_number().expect("latest").expect("some");
    assert_eq!(latest, 5);
}

#[test]
fn batch_session_aborts_on_error() {
    let (_dir, store) = setup();

    let result: Result<(), _> = store.with_batch_session(|session| {
        session.store_trie_updates(block(1), BlockStateDiff::default())?;
        session.store_trie_updates(block(2), BlockStateDiff::default())?;
        Err(base_execution_trie::BaseProofsStorageError::NoBlocksFound)
    });
    assert!(result.is_err());

    let (latest, _) = store.get_latest_block_number().expect("latest").expect("some");
    assert_eq!(latest, 0, "writes from aborted batch must not be visible");
}

#[test]
fn batch_session_reads_see_uncommitted_writes() {
    let (_dir, store) = setup();

    store
        .with_batch_session(|session| {
            session.store_trie_updates(block(1), BlockStateDiff::default())?;
            let (mid, _) = session.get_latest_block_number()?.expect("latest in session");
            assert_eq!(mid, 1, "session read must see uncommitted block 1");

            session.store_trie_updates(block(2), BlockStateDiff::default())?;
            let (end, _) = session.get_latest_block_number()?.expect("latest in session");
            assert_eq!(end, 2, "session read must see uncommitted block 2");
            Ok(())
        })
        .expect("batch commit");

    let (latest, _) = store.get_latest_block_number().expect("latest").expect("some");
    assert_eq!(latest, 2);
}

/// Regression: when a wipe block sits inside a batch session and its parent (also inside the
/// same batch) staged new storage slots, the wipe lookback must enumerate the parent's staged
/// slots so they get tombstoned at the wipe block. A fresh RO tx misses those staged writes and
/// silently produces incomplete tombstones.
#[test]
fn batch_session_wipe_sees_uncommitted_parent_storage_slots() {
    let (_dir, store) = setup();

    let addr = B256::repeat_byte(0xAB);
    let s1 = B256::repeat_byte(0x01);
    let s2 = B256::repeat_byte(0x02);
    let v1 = U256::from(111u64);
    let v2 = U256::from(222u64);

    store
        .with_batch_session(|session| {
            let mut post_state = HashedPostState::default();
            let mut storage = HashedStorage::default();
            storage.storage.insert(s1, v1);
            storage.storage.insert(s2, v2);
            post_state.storages.insert(addr, storage);
            session.store_trie_updates(
                block(1),
                BlockStateDiff {
                    sorted_trie_updates: TrieUpdatesSorted::default(),
                    sorted_post_state: post_state.into_sorted(),
                },
            )?;

            let mut wipe_state = HashedPostState::default();
            wipe_state.storages.insert(addr, HashedStorage::new(true));
            session.store_trie_updates(
                block(2),
                BlockStateDiff {
                    sorted_trie_updates: TrieUpdatesSorted::default(),
                    sorted_post_state: wipe_state.into_sorted(),
                },
            )?;
            Ok(())
        })
        .expect("batch commit");

    let mut at_block_1 = store.storage_hashed_cursor(addr, 1).expect("cursor at 1");
    let mut seen = Vec::new();
    while let Some(entry) = at_block_1.next().expect("next") {
        seen.push(entry);
    }
    assert_eq!(seen, vec![(s1, v1), (s2, v2)], "block 1 must observe its own writes");

    let mut at_block_2 = store.storage_hashed_cursor(addr, 2).expect("cursor at 2");
    let after_wipe: Vec<_> = std::iter::from_fn(|| at_block_2.next().expect("next")).collect();
    assert!(
        after_wipe.is_empty(),
        "wipe at block 2 must tombstone slots staged at block 1 inside the same batch; \
         leaked entries: {after_wipe:?}",
    );
}

/// Regression mirror of the hashed-storage case for the storage-trie path: `is_deleted = true`
/// on a block whose parent staged trie nodes for the same address inside the same batch must
/// enumerate those staged paths during the wipe lookback.
#[test]
fn batch_session_wipe_sees_uncommitted_parent_storage_trie_nodes() {
    let (_dir, store) = setup();

    let addr = B256::repeat_byte(0xCD);
    let p1 = Nibbles::from_nibbles_unchecked([0x01, 0x02]);
    let p2 = Nibbles::from_nibbles_unchecked([0x0A, 0x0B, 0x0C]);

    store
        .with_batch_session(|session| {
            let mut trie_updates = TrieUpdates::default();
            let mut storage_nodes = StorageTrieUpdates::default();
            storage_nodes.storage_nodes.insert(p1, BranchNodeCompact::default());
            storage_nodes.storage_nodes.insert(p2, BranchNodeCompact::default());
            trie_updates.storage_tries.insert(addr, storage_nodes);
            session.store_trie_updates(
                block(1),
                BlockStateDiff {
                    sorted_trie_updates: trie_updates.into_sorted(),
                    sorted_post_state: HashedPostState::default().into_sorted(),
                },
            )?;

            let mut wipe = TrieUpdates::default();
            let mut deleted = StorageTrieUpdates::default();
            deleted.set_deleted(true);
            wipe.storage_tries.insert(addr, deleted);
            session.store_trie_updates(
                block(2),
                BlockStateDiff {
                    sorted_trie_updates: wipe.into_sorted(),
                    sorted_post_state: HashedPostState::default().into_sorted(),
                },
            )?;
            Ok(())
        })
        .expect("batch commit");

    let mut at_block_2 = store.storage_trie_cursor(addr, 2).expect("cursor at 2");
    let mut leaked = Vec::new();
    while let Some(entry) = at_block_2.next().expect("next") {
        leaked.push(entry);
    }
    assert!(
        leaked.is_empty(),
        "is_deleted at block 2 must tombstone trie paths staged at block 1 inside the same batch; \
         leaked: {leaked:?}",
    );
}

#[test]
fn batch_session_rejects_out_of_order_block() {
    let (_dir, store) = setup();

    let result: Result<(), _> = store.with_batch_session(|session| {
        session.store_trie_updates(block(1), BlockStateDiff::default())?;
        let bad = BlockWithParent::new(b256(99), NumHash::new(2, b256(2)));
        session.store_trie_updates(bad, BlockStateDiff::default())?;
        Ok(())
    });
    assert!(matches!(
        result,
        Err(base_execution_trie::BaseProofsStorageError::OutOfOrder { block_number: 2, .. })
    ));

    let (latest, _) = store.get_latest_block_number().expect("latest").expect("some");
    assert_eq!(latest, 0, "any error in batch must abort the entire transaction");
}
