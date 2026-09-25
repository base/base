//! Differential witness test: `TrieWitness::overlay_witness`, which prewarms a shared cursor-result
//! cache in parallel, must return exactly the nodes of reth's serial `TrieWitness` over uncached
//! cursors, for random multi-block histories and random targets with deletions.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, Bytes, U256, map::B256Map};
use base_execution_trie::{
    BaseProofsHashedAccountCursorFactory, BaseProofsInitialStateStore, BaseProofsStorage,
    BaseProofsStore, BaseProofsTrieCursorFactory, BlockStateDiff, MdbxProofsStorage,
    RocksdbProofsStorage, proof::DatabaseTrieWitness, provider::BaseProofsStateProviderRef,
};
use rand_08::{Rng, SeedableRng, rngs::StdRng};
use reth_primitives_traits::Account;
use reth_provider::{StateRootProvider, noop::NoopProvider};
use reth_trie::{
    hashed_cursor::HashedPostStateCursorFactory, trie_cursor::InMemoryTrieCursorFactory,
    witness::TrieWitness,
};
use reth_trie_common::{ExecutionWitnessMode, HashedPostState, HashedStorage, TrieInput};
use tempfile::TempDir;

/// Number of independent random histories checked per backend.
const SEEDS: u64 = 6;
/// Blocks written after the initial state in each history.
const BLOCKS: u64 = 6;
/// Accounts in the initial state.
const ACCOUNTS: usize = 48;
/// Maximum storage slots per initial account.
const MAX_SLOTS: usize = 8;
/// Random witness requests checked per history.
const REQUESTS: usize = 8;

/// Keys of the initial state, reused to build block diffs and witness targets.
struct Keyspace {
    accounts: Vec<(B256, Vec<B256>)>,
}

impl Keyspace {
    fn random(rng: &mut StdRng) -> Self {
        let accounts = (0..ACCOUNTS)
            .map(|_| {
                let slots = (0..rng.gen_range(0..=MAX_SLOTS)).map(|_| random_hash(rng)).collect();
                (random_hash(rng), slots)
            })
            .collect();
        Self { accounts }
    }

    /// Random post state touching a fraction of the known accounts and slots, with account
    /// deletions, zeroed (deleted) slots, new keys, and occasional storage wipes.
    fn random_state(&self, rng: &mut StdRng, touch: f64) -> HashedPostState {
        let mut state = HashedPostState::default();
        for (address, slots) in &self.accounts {
            if !rng.gen_bool(touch) {
                continue;
            }
            let account = (!rng.gen_bool(0.15)).then(|| random_account(rng));
            state.accounts.insert(*address, account);
            let mut storage = HashedStorage::new(rng.gen_bool(0.05));
            for slot in slots.iter().chain([&random_hash(rng)]) {
                if rng.gen_bool(0.6) {
                    let value = if rng.gen_bool(0.3) {
                        U256::ZERO
                    } else {
                        U256::from(rng.gen_range(1..u64::MAX))
                    };
                    storage.storage.insert(*slot, value);
                }
            }
            state.storages.insert(*address, storage);
        }
        for _ in 0..rng.gen_range(0..3) {
            state.accounts.insert(random_hash(rng), Some(random_account(rng)));
        }
        state
    }
}

fn random_hash(rng: &mut StdRng) -> B256 {
    B256::from(rng.r#gen::<[u8; 32]>())
}

fn random_account(rng: &mut StdRng) -> Account {
    Account {
        nonce: rng.gen_range(0..16),
        balance: U256::from(rng.r#gen::<u64>()),
        bytecode_hash: None,
    }
}

fn block_hash(number: u64) -> B256 {
    B256::left_padding_from(&number.to_be_bytes())
}

/// Writes the initial leaves, then `BLOCKS` random diffs whose trie updates are computed from
/// the history itself, so stored branch nodes are consistent with the leaves.
fn build_history<S>(
    rng: &mut StdRng,
    keys: &Keyspace,
    store: &S,
    storage: &BaseProofsStorage<Arc<S>>,
) where
    S: BaseProofsStore + BaseProofsInitialStateStore,
{
    store.set_initial_state_anchor(BlockNumHash::new(0, block_hash(0))).expect("anchor");
    store
        .store_hashed_accounts(
            keys.accounts
                .iter()
                .map(|(address, _)| (*address, Some(random_account(rng))))
                .collect(),
        )
        .expect("store accounts");
    for (address, slots) in &keys.accounts {
        let values = slots.iter().map(|slot| (*slot, U256::from(rng.gen_range(1..u64::MAX))));
        store.store_hashed_storages(*address, values.collect()).expect("store storages");
    }
    store.commit_initial_state().expect("commit initial state");

    for number in 1..=BLOCKS {
        let post_state = keys.random_state(rng, 0.3);
        let (_, trie_updates) =
            BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), storage, number - 1)
                .state_root_with_updates(post_state.clone())
                .expect("state root");
        let block_ref =
            BlockWithParent::new(block_hash(number - 1), NumHash::new(number, block_hash(number)));
        let diff = BlockStateDiff {
            sorted_trie_updates: trie_updates.into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        };
        storage.store_trie_updates(block_ref, diff).expect("store block");
    }
}

/// Reth's serial witness over uncached cursors sharing one read transaction.
fn reference_witness<S>(
    storage: &BaseProofsStorage<Arc<S>>,
    block_number: u64,
    input: TrieInput,
    target: HashedPostState,
    mode: ExecutionWitnessMode,
) -> Result<B256Map<Bytes>, String>
where
    S: BaseProofsStore,
{
    let tx = storage.ro_tx().expect("read tx");
    let trie_factory = BaseProofsTrieCursorFactory::new(storage, &tx, block_number);
    let hashed_factory = BaseProofsHashedAccountCursorFactory::new(storage, &tx, block_number);
    let nodes_sorted = input.nodes.into_sorted();
    let state_sorted = input.state.into_sorted();
    TrieWitness::new(trie_factory.clone(), hashed_factory.clone())
        .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(trie_factory, &nodes_sorted))
        .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
            hashed_factory,
            &state_sorted,
        ))
        .with_prefix_sets_mut(input.prefix_sets)
        .always_include_root_node()
        .with_execution_witness_mode(mode)
        .compute(target)
        .map_err(|error| error.to_string())
}

fn check_backend<S>(open: impl Fn(&Path) -> S)
where
    S: BaseProofsStore + BaseProofsInitialStateStore + 'static,
{
    for seed in 0..SEEDS {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(open(dir.path()));
        let storage = BaseProofsStorage::from(Arc::clone(&store));
        let mut rng = StdRng::seed_from_u64(seed);
        let keys = Keyspace::random(&mut rng);
        build_history(&mut rng, &keys, store.as_ref(), &storage);
        for request in 0..REQUESTS {
            let block_number = rng.gen_range(0..=BLOCKS);
            let target = keys.random_state(&mut rng, 0.25);
            let input = if rng.gen_bool(0.5) {
                TrieInput::default()
            } else {
                TrieInput::from_state(keys.random_state(&mut rng, 0.1))
            };
            let mode = if rng.gen_bool(0.5) {
                ExecutionWitnessMode::Legacy
            } else {
                ExecutionWitnessMode::Canonical
            };
            let expected =
                reference_witness(&storage, block_number, input.clone(), target.clone(), mode)
                    .map(BTreeMap::from_iter);
            let actual = TrieWitness::overlay_witness(&storage, block_number, input, target, mode)
                .map(BTreeMap::from_iter)
                .map_err(|error| error.to_string());
            assert!(expected.as_ref().is_ok_and(|nodes| !nodes.is_empty()), "seed {seed}");
            assert_eq!(actual, expected, "seed {seed} request {request} block {block_number}");
        }
    }
}

#[test]
fn cached_witness_matches_uncached_rocksdb() {
    check_backend(|path| RocksdbProofsStorage::new(path).expect("rocksdb"));
}

#[test]
fn cached_witness_matches_uncached_mdbx() {
    check_backend(|path| MdbxProofsStorage::new(path).expect("mdbx"));
}
