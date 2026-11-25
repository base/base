//! The cross-block execution cache of a full node, as base-reth's execution cache: the accounts,
//! storage slots and code canonical blocks read or wrote, kept in memory across blocks, which
//! validation reads before the in-memory layer and the datadir.
//!
//! It holds the state after one canonical block, its head ([`crate::Chain`] tracks which): reads
//! of that state fill it, and when a child of the head becomes canonical its writes are applied
//! and the child becomes the head. Any other head change (a reorg, an unwind) clears it. Only a
//! state at the head with no side-branch blocks on top reads it, so a side branch, an older
//! block, or a stateless (partial) datadir never sees it.
//!
//! It is bounded by entry counts (code by bytes), sized from live probes on Base mainnet: at
//! 128k accounts and 1.5M slots, ~14% of the validating thread's slot misses (21% of the prewarm
//! workers') were entries the cache had evicted, half of them read from disk; 2.5M slots keep
//! 99% of those. When the head moves, a shard over its cap drops entries spread evenly over its
//! table. At its caps it holds about 16 MB of accounts, 370 MB of slots and 64 MiB of code: the
//! slot tables' bucket count is a power of two, so any slot cap from 1.64M to 3.26M (with its
//! overshoot) costs the same 2^18 buckets of 89 bytes per shard.

use std::{
    collections::HashMap,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_primitives::{Address, B256, U256};
use base_tiger_trie::{AccountInfo, BlockChanges};
use evm2::bytecode::Bytecode;

/// Shards, each with its own lock, so validation and the prewarm workers rarely contend.
const SHARDS: usize = 16;
/// Most accounts cached.
pub(crate) const MAX_ACCOUNTS: usize = 1 << 17;
/// Most storage slots cached.
pub(crate) const MAX_SLOTS: usize = 5 << 19;
/// Most code bytes cached.
pub(crate) const MAX_CODE_BYTES: usize = 64 << 20;

#[derive(Debug, Default)]
struct Shard {
    accounts: HashMap<Address, Option<AccountInfo>>,
    storage: HashMap<(Address, U256), U256>,
    codes: HashMap<B256, Bytecode>,
    code_bytes: usize,
}

/// A shard over its cap drops its excess and this fraction of its cap when the head moves.
const EVICT_FRACTION: usize = 64;
/// Between head moves a shard takes entries up to its cap and this fraction of it more, so it
/// stays bounded however much a block reads, with room for a block's reads at any shard count.
const OVERSHOOT_FRACTION: usize = 8;

/// Inserts into `map` unless it is full beyond its overshoot (see [`OVERSHOOT_FRACTION`]) until
/// [`ExecCache::apply`] trims it.
fn bounded_insert<K: std::hash::Hash + Eq, V>(map: &mut HashMap<K, V>, cap: usize, key: K, value: V) {
    if map.len() < cap + cap / OVERSHOOT_FRACTION || map.contains_key(&key) {
        map.insert(key, value);
    }
}

/// Trims `map` to below `cap` by dropping every n-th entry in table order. Table order is hash
/// order, so entries of every age go alike. (Taking the first entries instead takes mostly the
/// newest: the scan starts where earlier evictions emptied the table, where new entries land.)
fn trim<K, V>(map: &mut HashMap<K, V>, cap: usize) {
    if map.len() <= cap {
        return;
    }
    let every = map.len() / (map.len() - cap + cap / EVICT_FRACTION);
    let mut index = 0;
    map.retain(|_, _| {
        index += 1;
        index % every != 0
    });
}

/// The cache (see the module documentation).
#[derive(Debug)]
pub struct ExecCache {
    shards: [Mutex<Shard>; SHARDS],
    /// Bumped by every [`Self::apply`] and [`Self::clear`], before they touch a shard. A state
    /// reads and fills the cache only while the generation it was created at is current, so a
    /// state taken before the head moved (an RPC call, a sibling's validation) neither reads the
    /// child's values nor inserts its own stale ones. Checked under the shard lock.
    generation: AtomicU64,
}

impl Default for ExecCache {
    fn default() -> Self {
        Self { shards: std::array::from_fn(|_| Mutex::default()), generation: AtomicU64::new(0) }
    }
}

impl ExecCache {
    fn shard(&self, byte: u8) -> MutexGuard<'_, Shard> {
        self.shards[usize::from(byte) % SHARDS].lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn slot_shard(&self, address: &Address, slot: &U256) -> MutexGuard<'_, Shard> {
        self.shard(address[19] ^ slot.byte(0))
    }

    /// The cache's current generation, for a state created at its head now.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    /// The cached account at `address` (`Some(None)`: cached as absent), while `generation` is
    /// current.
    pub fn account(&self, generation: u64, address: &Address) -> Option<Option<AccountInfo>> {
        let shard = self.shard(address[19]);
        if !self.current(generation) {
            return None;
        }
        shard.accounts.get(address).copied()
    }

    /// Caches the account at `address` as the head's state reads it.
    pub fn put_account(&self, generation: u64, address: Address, account: Option<AccountInfo>) {
        let mut shard = self.shard(address[19]);
        if self.current(generation) {
            bounded_insert(&mut shard.accounts, MAX_ACCOUNTS / SHARDS, address, account);
        }
    }

    /// The cached value of storage `slot` of `address`.
    pub fn storage(&self, generation: u64, address: &Address, slot: &U256) -> Option<U256> {
        let shard = self.slot_shard(address, slot);
        if !self.current(generation) {
            return None;
        }
        shard.storage.get(&(*address, *slot)).copied()
    }

    /// Caches storage `slot` of `address` as the head's state reads it.
    pub fn put_storage(&self, generation: u64, address: Address, slot: U256, value: U256) {
        let mut shard = self.slot_shard(&address, &slot);
        if self.current(generation) {
            bounded_insert(&mut shard.storage, MAX_SLOTS / SHARDS, (address, slot), value);
        }
    }

    /// The cached code with hash `hash`.
    pub fn code(&self, hash: &B256) -> Option<Bytecode> {
        self.shard(hash[0]).codes.get(hash).cloned()
    }

    /// Caches code `code` with hash `hash`, analyzed, so later blocks skip the analysis.
    pub fn put_code(&self, hash: B256, code: Bytecode) {
        let mut shard = self.shard(hash[0]);
        if shard.codes.contains_key(&hash) {
            return;
        }
        while shard.code_bytes + code.len() > MAX_CODE_BYTES / SHARDS {
            let Some(evicted) = shard.codes.keys().next().copied() else { return };
            let removed = shard.codes.remove(&evicted).map_or(0, |code| code.len());
            shard.code_bytes -= removed;
        }
        shard.code_bytes += code.len();
        shard.codes.insert(hash, code);
    }

    /// Applies the writes of `changes`, a child of the head that became canonical: the cache
    /// then holds the state after it. Only entries already cached change (a write is not a read
    /// of the next block), except code, which is immutable.
    pub fn apply(&self, changes: &BlockChanges) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        // Wiped and deleted storages first: their slots read zero unless the block sets them.
        let cleared: Vec<Address> = changes
            .wiped
            .iter()
            .copied()
            .chain(changes.accounts.iter().filter(|(_, account)| account.is_none()).map(|(address, _)| *address))
            .collect();
        if !cleared.is_empty() {
            for shard in &self.shards {
                let mut shard = shard.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                shard.storage.retain(|(address, _), _| !cleared.contains(address));
            }
        }
        for (address, slot, value) in &changes.storage {
            if let Some(cached) = self.slot_shard(address, slot).storage.get_mut(&(*address, *slot)) {
                *cached = *value;
            }
        }
        for (address, account) in &changes.accounts {
            if let Some(cached) = self.shard(address[19]).accounts.get_mut(address) {
                *cached = *account;
            }
        }
        for (hash, code) in &changes.codes {
            self.put_code(*hash, Bytecode::new_raw(code.clone()));
        }
        // Off the execution path: the block is validated and the next one not started.
        for shard in &self.shards {
            let mut shard = shard.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            trim(&mut shard.accounts, MAX_ACCOUNTS / SHARDS);
            trim(&mut shard.storage, MAX_SLOTS / SHARDS);
        }
    }

    /// Empties the cache.
    pub fn clear(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        for shard in &self.shards {
            *shard.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Shard::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256};
    use base_tiger_trie::{AccountInfo, BlockChanges};

    use super::{EVICT_FRACTION, ExecCache, MAX_ACCOUNTS};

    fn account(nonce: u64) -> Option<AccountInfo> {
        Some(AccountInfo { nonce, ..AccountInfo::default() })
    }

    /// A canonical child's writes update what is cached: a later write of the same key wins, a
    /// wipe or a deletion drops the account's cached slots, and writes of keys not cached are not
    /// cached (they were not read on the old head).
    #[test]
    fn apply_moves_the_cache_to_the_child() {
        let cache = ExecCache::default();
        let (a, b, c) = (Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(3));
        cache.put_account(cache.generation(), a, account(1));
        cache.put_account(cache.generation(), b, account(1));
        cache.put_storage(cache.generation(), a, U256::from(1), U256::from(10));
        cache.put_storage(cache.generation(), b, U256::from(1), U256::from(10));
        cache.put_storage(cache.generation(), b, U256::from(2), U256::from(20));
        cache.apply(&BlockChanges {
            accounts: vec![(a, account(2)), (a, account(3)), (b, None), (c, account(1))],
            storage: vec![(a, U256::from(1), U256::from(11)), (a, U256::from(1), U256::from(12)), (b, U256::from(2), U256::from(21))],
            wiped: Vec::new(),
            codes: vec![(B256::repeat_byte(9), Bytes::from_static(b"code"))],
        });
        assert_eq!(cache.account(cache.generation(), &a), Some(account(3)));
        assert_eq!(cache.account(cache.generation(), &b), Some(None));
        assert_eq!(cache.account(cache.generation(), &c), None, "a write of a key not cached is not cached");
        assert_eq!(cache.storage(cache.generation(), &a, &U256::from(1)), Some(U256::from(12)));
        assert_eq!(cache.storage(cache.generation(), &b, &U256::from(1)), None, "a deleted account's slots go");
        assert_eq!(cache.storage(cache.generation(), &b, &U256::from(2)), None, "a deleted account's slots go, also written ones");
        assert_eq!(cache.code(&B256::repeat_byte(9)).map(|code| code.original_bytes()), Some(Bytes::from_static(b"code")));

        cache.put_storage(cache.generation(), a, U256::from(5), U256::from(50));
        cache.apply(&BlockChanges { wiped: vec![a], ..BlockChanges::default() });
        assert_eq!(cache.storage(cache.generation(), &a, &U256::from(5)), None, "a wipe drops the cached slots");
        assert_eq!(cache.account(cache.generation(), &a), Some(account(3)));
    }

    /// A state taken before the head moved neither reads the child's values nor caches its own:
    /// an RPC call or a sibling's validation that was running across the `apply`.
    #[test]
    fn a_state_from_before_the_head_moved_does_not_use_the_cache() {
        let cache = ExecCache::default();
        let (a, slot) = (Address::repeat_byte(1), U256::from(1));
        let old = cache.generation();
        cache.put_storage(old, a, slot, U256::from(10));
        cache.apply(&BlockChanges { storage: vec![(a, slot, U256::from(11))], ..BlockChanges::default() });
        assert_eq!(cache.storage(old, &a, &slot), None, "the old state reads its own view, not the child's");
        let b = Address::repeat_byte(2);
        cache.put_storage(old, b, slot, U256::from(20));
        cache.put_account(old, b, account(7));
        let now = cache.generation();
        assert_eq!(cache.storage(now, &b, &slot), None, "a stale value read on the old head is not cached");
        assert_eq!(cache.account(now, &b), None);
        assert_eq!(cache.storage(now, &a, &slot), Some(U256::from(11)));
    }

    fn address(index: u64) -> Address {
        let mut address = Address::ZERO;
        address[12..].copy_from_slice(&index.to_be_bytes());
        address
    }

    /// When the head moves, each shard is trimmed to its share of the cap, dropping old and new
    /// entries alike: the entries cached since the last trim survive about as well as older ones.
    #[test]
    fn bounded() {
        let cache = ExecCache::default();
        let filled = MAX_ACCOUNTS as u64 + 1000;
        for index in 0..filled {
            cache.put_account(cache.generation(), address(index), account(index));
        }
        cache.apply(&BlockChanges::default());
        let held: usize = cache.shards.iter().map(|shard| shard.lock().unwrap().accounts.len()).sum();
        assert!(held <= MAX_ACCOUNTS, "{held} accounts cached");
        assert!(held > MAX_ACCOUNTS - 2 * MAX_ACCOUNTS / EVICT_FRACTION, "the cache fills: {held}");
        // Twenty rounds of 1000 new accounts, each followed by a head move.
        for index in filled..filled + 20_000 {
            cache.put_account(cache.generation(), address(index), account(index));
            if index % 1000 == 0 {
                cache.apply(&BlockChanges::default());
            }
        }
        cache.apply(&BlockChanges::default());
        let survived = |range: std::ops::Range<u64>| {
            let len = range.end - range.start;
            let held = range.filter(|index| cache.account(cache.generation(), &address(*index)).is_some()).count();
            held as f64 / len as f64
        };
        let (newest, oldest) = (survived(filled + 19_000..filled + 20_000), survived(0..filled));
        assert!(newest > 0.9 && newest >= oldest, "newest {newest}, oldest {oldest}");
    }
}
