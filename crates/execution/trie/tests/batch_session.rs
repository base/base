//! Integration tests for [`MdbxProofsStorage`]'s batch session: cross-block atomicity,
//! transaction-local read visibility, and abort-on-error rollback.

use std::sync::Arc;

use alloy_eips::{NumHash, eip1898::BlockWithParent};
use alloy_primitives::B256;
use base_execution_trie::{
    BaseProofsBatchSession, BaseProofsBatchStore, BaseProofsStore, BlockStateDiff,
    MdbxProofsStorage,
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
