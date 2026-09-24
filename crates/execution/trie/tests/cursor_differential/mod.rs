//! Differential cursor tests: `RocksDB` cursors must return exactly what the MDBX store (the
//! previous production backend) returns for random multi-block histories and random seek/next
//! sequences. MDBX is the reference because, unlike the in-memory store, it models storage-trie
//! wipes (`StorageTrieUpdates::is_deleted`).

use rand_08::{Rng, SeedableRng, rngs::StdRng};
use reth_trie::{
    hashed_cursor::HashedStorageCursor, trie_cursor::TrieStorageCursor, updates::StorageTrieUpdates,
};

use super::*;

/// Number of independent random histories checked.
const SEEDS: u64 = 24;
/// Blocks written after the initial state in each history.
const BLOCKS: u64 = 24;
/// Cursor operations driven per cursor and `max_block` value.
const OPS_PER_RUN: usize = 80;
/// Distinct `max_block` values checked per history.
const MAX_BLOCK_SAMPLES: usize = 6;

/// Keyspace shared by the reference and `RocksDB` stores for one history.
struct Keyspace {
    accounts: Vec<B256>,
    storage_addresses: Vec<B256>,
    slots: Vec<B256>,
    account_paths: Vec<Nibbles>,
    storage_paths: Vec<Nibbles>,
}

impl Keyspace {
    fn random(rng: &mut StdRng) -> Self {
        // Few distinct high bytes so keys share long prefixes and interleave with seek targets.
        let hash = |rng: &mut StdRng| {
            let mut bytes: [u8; 32] = rng.r#gen();
            bytes[0] = rng.gen_range(0..4);
            B256::from(bytes)
        };
        let accounts = (0..16).map(|_| hash(rng)).collect();
        let storage_addresses = (0..4).map(|_| hash(rng)).collect();
        let slots = (0..12).map(|_| hash(rng)).collect();
        let paths = |rng: &mut StdRng| {
            let mut paths: Vec<Nibbles> = (0..14).map(|_| random_path(rng)).collect();
            paths.sort();
            paths.dedup();
            paths
        };
        let account_paths = paths(rng);
        let storage_paths = paths(rng);
        Self { accounts, storage_addresses, slots, account_paths, storage_paths }
    }

    fn pick<T: Copy>(rng: &mut StdRng, items: &[T]) -> T {
        items[rng.gen_range(0..items.len())]
    }

    /// Seek target: usually a known key, sometimes an arbitrary one.
    fn hashed_target(rng: &mut StdRng, known: &[B256]) -> B256 {
        match rng.gen_range(0..8) {
            0 => B256::ZERO,
            1 => B256::repeat_byte(0xff),
            2 | 3 => {
                let mut bytes: [u8; 32] = rng.r#gen();
                bytes[0] = rng.gen_range(0..5);
                B256::from(bytes)
            }
            _ => Self::pick(rng, known),
        }
    }

    fn path_target(rng: &mut StdRng, known: &[Nibbles]) -> Nibbles {
        if rng.gen_bool(0.3) { random_path(rng) } else { known[rng.gen_range(0..known.len())] }
    }
}

/// Short nibble path over a small alphabet so paths share prefixes.
fn random_path(rng: &mut StdRng) -> Nibbles {
    let len = rng.gen_range(0..=5);
    Nibbles::from_nibbles(
        (0..len).map(|_| [0u8, 1, 0xe, 0xf][rng.gen_range(0..4)]).collect::<Vec<_>>(),
    )
}

fn account(rng: &mut StdRng, block: u64) -> Account {
    Account { nonce: block, balance: U256::from(rng.r#gen::<u64>()), bytecode_hash: None }
}

fn storage_value(rng: &mut StdRng) -> U256 {
    if rng.gen_bool(0.2) { U256::ZERO } else { U256::from(rng.gen_range(1..u64::MAX)) }
}

fn branch(rng: &mut StdRng) -> BranchNodeCompact {
    BranchNodeCompact::new(
        rng.r#gen::<u16>() | 1,
        0,
        0,
        vec![],
        Some(B256::from(rng.r#gen::<[u8; 32]>())),
    )
}

fn block_hash(number: u64) -> B256 {
    B256::left_padding_from(&number.to_be_bytes())
}

/// Random per-block diff with inserts, updates, zero values, tombstones, and wipes. A small set of
/// "hot" keys is updated almost every block so some keys carry long version chains.
fn random_block_diff(rng: &mut StdRng, keys: &Keyspace, block: u64) -> BlockStateDiff {
    let mut post_state = HashedPostState::default();
    for (index, key) in keys.accounts.iter().enumerate() {
        let touch = if index < 3 { 0.9 } else { 0.25 };
        if rng.gen_bool(touch) {
            let value = (!rng.gen_bool(0.25)).then(|| account(rng, block));
            post_state.accounts.insert(*key, value);
        }
    }
    for address in &keys.storage_addresses {
        if !rng.gen_bool(0.7) {
            continue;
        }
        let mut storage = HashedStorage::new(rng.gen_bool(0.08));
        for (index, slot) in keys.slots.iter().enumerate() {
            let touch = if index < 2 { 0.9 } else { 0.25 };
            if rng.gen_bool(touch) {
                storage.storage.insert(*slot, storage_value(rng));
            }
        }
        post_state.storages.insert(*address, storage);
    }

    let mut trie_updates = TrieUpdates::default();
    for (index, path) in keys.account_paths.iter().enumerate() {
        let touch = if index < 2 { 0.9 } else { 0.25 };
        if rng.gen_bool(touch) {
            if rng.gen_bool(0.25) {
                trie_updates.removed_nodes.insert(*path);
            } else {
                trie_updates.account_nodes.insert(*path, branch(rng));
            }
        }
    }
    for address in &keys.storage_addresses {
        if !rng.gen_bool(0.6) {
            continue;
        }
        let mut storage_trie =
            StorageTrieUpdates { is_deleted: rng.gen_bool(0.08), ..Default::default() };
        for (index, path) in keys.storage_paths.iter().enumerate() {
            let touch = if index < 2 { 0.9 } else { 0.25 };
            if rng.gen_bool(touch) {
                if rng.gen_bool(0.25) {
                    storage_trie.removed_nodes.insert(*path);
                } else {
                    storage_trie.storage_nodes.insert(*path, branch(rng));
                }
            }
        }
        trie_updates.storage_tries.insert(*address, storage_trie);
    }

    BlockStateDiff {
        sorted_trie_updates: trie_updates.into_sorted(),
        sorted_post_state: post_state.into_sorted(),
    }
}

/// Writes the same random history into both stores. `RocksDB` is flushed and compacted halfway
/// so reads merge SST files with memtable data.
fn build_history(
    rng: &mut StdRng,
    keys: &Keyspace,
    reference: &MdbxProofsStorage,
    rocksdb: &TestRocksdbProofsStorage,
) -> Result<(), BaseProofsStorageError> {
    let initial_accounts: Vec<_> =
        keys.accounts.iter().map(|key| (*key, Some(account(rng, 0)))).collect();
    let initial_account_nodes: Vec<_> =
        keys.account_paths.iter().map(|path| (*path, Some(branch(rng)))).collect();
    let mut initial_storages = Vec::new();
    let mut initial_storage_nodes = Vec::new();
    for address in &keys.storage_addresses {
        let slots: Vec<_> = keys.slots.iter().map(|slot| (*slot, storage_value(rng))).collect();
        let nodes: Vec<_> =
            keys.storage_paths.iter().map(|path| (*path, Some(branch(rng)))).collect();
        initial_storages.push((*address, slots));
        initial_storage_nodes.push((*address, nodes));
    }

    let mut blocks = Vec::new();
    for number in 1..=BLOCKS {
        blocks.push((
            BlockWithParent::new(block_hash(number - 1), NumHash::new(number, block_hash(number))),
            random_block_diff(rng, keys, number),
        ));
    }

    let initial = InitialState {
        accounts: initial_accounts,
        account_nodes: initial_account_nodes,
        storages: initial_storages,
        storage_nodes: initial_storage_nodes,
    };
    let rocksdb_store = &rocksdb.storage;
    initial.write(reference)?;
    initial.write(rocksdb_store)?;

    for (index, (block_ref, diff)) in blocks.into_iter().enumerate() {
        reference.store_trie_updates(block_ref, diff.clone())?;
        rocksdb_store.store_trie_updates(block_ref, diff)?;
        if index as u64 == BLOCKS / 2 {
            rocksdb_store.flush_and_compact()?;
        }
    }
    Ok(())
}

/// Trie branch nodes keyed by path, as accepted by the initial-state store APIs.
type BranchNodes = Vec<(Nibbles, Option<BranchNodeCompact>)>;

/// Initial (block 0) state written identically into both stores.
struct InitialState {
    accounts: Vec<(B256, Option<Account>)>,
    account_nodes: BranchNodes,
    storages: Vec<(B256, Vec<(B256, U256)>)>,
    storage_nodes: Vec<(B256, BranchNodes)>,
}

impl InitialState {
    fn write<S: BaseProofsInitialStateStore>(
        &self,
        store: &S,
    ) -> Result<(), BaseProofsStorageError> {
        store.set_initial_state_anchor(BlockNumHash::new(0, block_hash(0)))?;
        store.store_hashed_accounts(self.accounts.clone())?;
        store.store_account_branches(self.account_nodes.clone())?;
        for (address, slots) in &self.storages {
            store.store_hashed_storages(*address, slots.clone())?;
        }
        for (address, nodes) in &self.storage_nodes {
            store.store_storage_branches(*address, nodes.clone())?;
        }
        store.commit_initial_state()?;
        Ok(())
    }
}

/// Drives the same random op sequence on two hashed cursors and asserts identical results.
///
/// Per the cursor contracts, a seek is issued first, after `reset`/`set_hashed_address`, and after
/// any exhausted (`None`) result.
fn drive_hashed<V, A, B>(
    rng: &mut StdRng,
    context: &str,
    expected: &mut A,
    actual: &mut B,
    targets: &[B256],
    addresses: Option<&[B256]>,
) where
    V: PartialEq + std::fmt::Debug,
    A: HashedStorageCursor<Value = V>,
    B: HashedStorageCursor<Value = V>,
{
    let mut must_seek = true;
    for op_index in 0..OPS_PER_RUN {
        let roll = rng.gen_range(0..10);
        let (op, want, got) = if must_seek || roll < 3 {
            let target = Keyspace::hashed_target(rng, targets);
            (
                format!("seek({target})"),
                expected.seek(target).unwrap(),
                actual.seek(target).unwrap(),
            )
        } else if roll == 9 {
            if let Some(addresses) = addresses {
                let address = Keyspace::pick(rng, addresses);
                expected.set_hashed_address(address);
                actual.set_hashed_address(address);
                must_seek = true;
                assert_eq!(
                    expected.is_storage_empty().unwrap(),
                    actual.is_storage_empty().unwrap(),
                    "{context} op {op_index}: is_storage_empty after set_hashed_address({address})"
                );
            } else {
                expected.reset();
                actual.reset();
                must_seek = true;
            }
            continue;
        } else {
            ("next".to_string(), expected.next().unwrap(), actual.next().unwrap())
        };
        assert_eq!(want, got, "{context} op {op_index}: {op}");
        must_seek = want.is_none();
    }
}

/// Wrapper giving account cursors a no-op [`HashedStorageCursor`] impl.
struct AccountCursor<C>(C);

impl<C: HashedCursor> HashedCursor for AccountCursor<C> {
    type Value = C::Value;

    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, reth_db::DatabaseError> {
        self.0.seek(key)
    }

    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, reth_db::DatabaseError> {
        self.0.next()
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

impl<C: HashedCursor> HashedStorageCursor for AccountCursor<C> {
    fn is_storage_empty(&mut self) -> Result<bool, reth_db::DatabaseError> {
        Ok(false)
    }

    fn set_hashed_address(&mut self, _hashed_address: B256) {}
}

/// Drives the same random op sequence on two trie cursors and asserts identical results.
fn drive_trie<A, B>(
    rng: &mut StdRng,
    context: &str,
    expected: &mut A,
    actual: &mut B,
    targets: &[Nibbles],
    addresses: Option<&[B256]>,
) where
    A: TrieStorageCursor,
    B: TrieStorageCursor,
{
    let mut must_seek = true;
    for op_index in 0..OPS_PER_RUN {
        let roll = rng.gen_range(0..10);
        let (op, want, got) = if must_seek || roll < 2 {
            let target = Keyspace::path_target(rng, targets);
            (
                format!("seek({target:?})"),
                expected.seek(target).unwrap(),
                actual.seek(target).unwrap(),
            )
        } else if roll == 2 {
            let target = Keyspace::path_target(rng, targets);
            (
                format!("seek_exact({target:?})"),
                expected.seek_exact(target).unwrap(),
                actual.seek_exact(target).unwrap(),
            )
        } else if roll == 9 {
            if let Some(address) = addresses.map(|addresses| Keyspace::pick(rng, addresses)) {
                expected.set_hashed_address(address);
                actual.set_hashed_address(address);
            } else {
                expected.reset();
                actual.reset();
            }
            must_seek = true;
            continue;
        } else {
            ("next".to_string(), expected.next().unwrap(), actual.next().unwrap())
        };
        assert_eq!(want, got, "{context} op {op_index}: {op}");
        must_seek = want.is_none();
        if must_seek {
            // Stores differ in where an unsuccessful lookup leaves the cursor; only the
            // positioned state is part of the shared contract.
            continue;
        }
        assert_eq!(
            expected.current().unwrap(),
            actual.current().unwrap(),
            "{context} op {op_index}: current() after {op}"
        );
    }
}

/// Wrapper giving account trie cursors a no-op [`TrieStorageCursor`] impl.
struct AccountTrie<C>(C);

impl<C: TrieCursor> TrieCursor for AccountTrie<C> {
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, reth_db::DatabaseError> {
        self.0.seek_exact(key)
    }

    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, reth_db::DatabaseError> {
        self.0.seek(key)
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, reth_db::DatabaseError> {
        self.0.next()
    }

    fn current(&mut self) -> Result<Option<Nibbles>, reth_db::DatabaseError> {
        self.0.current()
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

impl<C: TrieCursor> TrieStorageCursor for AccountTrie<C> {
    fn set_hashed_address(&mut self, _hashed_address: B256) {}
}

/// Random histories with inserts, updates, zero slots, tombstones, and wipes; random seek/next
/// sequences on every cursor kind at random `max_block` values must match the reference store.
#[test]
#[serial_test::serial]
fn rocksdb_cursors_match_reference_store() -> Result<(), BaseProofsStorageError> {
    for seed in 0..SEEDS {
        let mut rng = StdRng::seed_from_u64(seed);
        let keys = Keyspace::random(&mut rng);
        let reference = create_mdbx_proofs_storage();
        let rocksdb = create_rocksdb_proofs_storage();
        build_history(&mut rng, &keys, &reference, &rocksdb)?;

        for sample in 0..MAX_BLOCK_SAMPLES {
            let max_block = match sample {
                0 => 0,
                1 => BLOCKS,
                _ => rng.gen_range(0..=BLOCKS + 1),
            };
            let context = format!("seed {seed} max_block {max_block}");

            drive_hashed(
                &mut rng,
                &format!("{context} accounts"),
                &mut AccountCursor(reference.account_hashed_cursor(max_block)?),
                &mut AccountCursor(rocksdb.account_hashed_cursor(max_block)?),
                &keys.accounts,
                None,
            );

            let address = Keyspace::pick(&mut rng, &keys.storage_addresses);
            drive_hashed(
                &mut rng,
                &format!("{context} storage"),
                &mut reference.storage_hashed_cursor(address, max_block)?,
                &mut rocksdb.storage_hashed_cursor(address, max_block)?,
                &keys.slots,
                Some(&keys.storage_addresses),
            );

            drive_trie(
                &mut rng,
                &format!("{context} account trie"),
                &mut AccountTrie(reference.account_trie_cursor(max_block)?),
                &mut AccountTrie(rocksdb.account_trie_cursor(max_block)?),
                &keys.account_paths,
                None,
            );

            let address = Keyspace::pick(&mut rng, &keys.storage_addresses);
            drive_trie(
                &mut rng,
                &format!("{context} storage trie"),
                &mut reference.storage_trie_cursor(address, max_block)?,
                &mut rocksdb.storage_trie_cursor(address, max_block)?,
                &keys.storage_paths,
                Some(&keys.storage_addresses),
            );
        }
    }
    Ok(())
}
