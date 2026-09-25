//! Compares an exex account miss against a deleted neighbor.
//!
//! `seek_plus_filter` is the old batch read: `seek()` walks forward to the next live key, then the
//! caller drops it. `seek_exact` returns nothing for that miss.

use std::{hint::black_box, sync::Arc};

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
use base_execution_trie::{
    BaseProofsBatchSession, BaseProofsBatchStore, BaseProofsInitialStateStore, BaseProofsStore,
    BlockStateDiff, RocksdbProofsStorage,
};
use criterion::{Criterion, criterion_group, criterion_main};
use reth_primitives_traits::Account;
use reth_trie::hashed_cursor::HashedCursor;
use reth_trie_common::{HashedPostState, updates::TrieUpdates};
use tempfile::TempDir;

const DEAD_VERSIONS: u64 = 64;

fn account_at(nonce: u64) -> Account {
    Account { nonce, balance: U256::from(nonce), bytecode_hash: None }
}

fn block_for(number: u64) -> BlockWithParent {
    let hash = if number == 0 { B256::ZERO } else { B256::from(U256::from(number)) };
    BlockWithParent {
        parent: if number == 0 { B256::ZERO } else { B256::from(U256::from(number - 1)) },
        block: BlockNumHash { number, hash },
    }
}

fn create_fixture() -> (TempDir, Arc<RocksdbProofsStorage>, u64, B256) {
    let dir = TempDir::new().expect("create temp dir");
    let storage = Arc::new(RocksdbProofsStorage::new(dir.path()).expect("create RocksDB storage"));
    let missing = B256::repeat_byte(0x10);
    let dead = B256::from(U256::from_be_bytes(missing.0) + U256::from(1));
    let live = B256::from(U256::from_be_bytes(missing.0) + U256::from(2));

    storage.store_hashed_accounts(vec![(live, Some(account_at(1)))]).expect("store account");
    storage.set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO)).expect("set anchor");
    storage.commit_initial_state().expect("commit initial state");

    for number in 1..=DEAD_VERSIONS {
        let mut post_state = HashedPostState::default();
        post_state.accounts.insert(dead, Some(account_at(number)));
        storage
            .store_trie_updates(
                block_for(number),
                BlockStateDiff {
                    sorted_trie_updates: TrieUpdates::default().into_sorted(),
                    sorted_post_state: post_state.into_sorted(),
                },
            )
            .expect("store version");
    }
    let head = DEAD_VERSIONS + 1;
    let mut post_state = HashedPostState::default();
    post_state.accounts.insert(dead, None);
    storage
        .store_trie_updates(
            block_for(head),
            BlockStateDiff {
                sorted_trie_updates: TrieUpdates::default().into_sorted(),
                sorted_post_state: post_state.into_sorted(),
            },
        )
        .expect("store tombstone");
    storage.flush_and_compact().expect("flush and compact");

    (dir, storage, head, missing)
}

fn exex_point_read_benches(c: &mut Criterion) {
    let (_dir, storage, head, missing) = create_fixture();

    storage
        .with_batch_session(|session| {
            let mut group = c.benchmark_group("exex_point_reads");
            group.sample_size(10);
            group.bench_function("seek_plus_filter", |b| {
                b.iter(|| {
                    black_box(
                        session
                            .account_hashed_cursor(head)
                            .expect("cursor")
                            .seek(missing)
                            .expect("seek")
                            .filter(|(key, _)| *key == missing),
                    )
                });
            });
            group.bench_function("seek_exact", |b| {
                b.iter(|| {
                    black_box(
                        session
                            .account_hashed_cursor(head)
                            .expect("cursor")
                            .seek_exact(missing)
                            .expect("seek_exact"),
                    )
                });
            });
            group.finish();
            Ok(())
        })
        .expect("benchmark batch session");
}

criterion_group!(benches, exex_point_read_benches);
criterion_main!(benches);
