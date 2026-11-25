//! Log-only stale-storage metering for state-root throttling.
//!
//! The serial post-execution state root is slowest when a block rewrites many
//! storage slots whose trie paths are no longer in memory. A slot that no
//! canonical block wrote in the last day is a cheap proxy for that. This module
//! counts such slots per executed transaction and logs what an age-tiered limit
//! would have flagged. It never excludes a transaction.
//!
//! - [`StaleStorageCache`] is a fixed-size, lock-free, keyed 8-way set-associative
//!   cache of the hour each storage-trie region (a contract's slots under one
//!   depth-[`STALE_STORAGE_TRIE_PREFIX_NIBBLES`] trie node) was last written. It is stamped off
//!   the transaction loop from canonical-state notifications, on every node that
//!   runs the payload builder, so a leader handoff does not empty it. It starts
//!   empty at process start and is never saved or rebuilt. A slot it does not hold
//!   reads as stale, so its errors only over-meter.
//! - [`BlockStaleTracker`] is the per-build state the builder consults after
//!   executing each transaction.

use std::{
    collections::hash_map::RandomState,
    fmt,
    hash::{BuildHasher, Hasher},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{
    Address, B256, TxHash, U256, keccak256,
    map::{AddressMap, HashSet},
};
use futures::StreamExt;
use reth_chain_state::{CanonStateNotification, CanonStateNotifications};
use reth_primitives_traits::{AlloyBlockHeader, NodePrimitives};
use revm::{database::BundleState, state::EvmState};
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{debug, info, warn};

use crate::StaleStorageMetrics;

/// Seconds per cache stamp. Stamps are hourly.
pub const STALE_STORAGE_EPOCH_SECONDS: u64 = 3_600;

/// Default size of the recency cache, in mebibytes: `2^25` four-byte entries in `2^22` sets.
///
/// Base Mainnet writes an estimated 10 million distinct storage slots a day, which map to fewer
/// distinct trie regions. Ten million live keys is under a third of the entries, so no key is in
/// excess of total capacity, but keys hash to sets unevenly: about 0.08% of sets receive more
/// than eight within the window, and the keys beyond eight in those sets, about 0.045% of live
/// keys, are evicted early. At twice the volume about 5.4% of sets overflow and about 2.0% of
/// keys are evicted early. An evicted key reads as stale, so an undersized cache only
/// over-meters. Raise the size with `--payload.stale-storage-cache-mib` when
/// `cache_unexpired_evictions_total` exceeds about 1% of `cache_writes_total`.
pub const STALE_STORAGE_CACHE_MIB: u32 = 128;

/// Smallest accepted cache size, in mebibytes.
pub const STALE_STORAGE_MIN_CACHE_MIB: u32 = 8;

/// Largest accepted cache size, in mebibytes.
pub const STALE_STORAGE_MAX_CACHE_MIB: u32 = 4_096;

/// Hours after its last canonical write that a pre-existing slot counts as stale.
///
/// One day: long enough that recurring batches stay fresh, short enough that a slot is likely
/// to have left memory.
pub const STALE_STORAGE_WINDOW_HOURS: u64 = 24;

/// Stale slots count only in contracts with more than this many distinct changed slots.
///
/// The transaction-scope count gates on the transaction's own changed slots. The block-scope
/// count gates on the distinct slots the block and the transaction change together, so splitting
/// one batch over several transactions hides at most this many slots per contract, while many
/// transactions rewriting the same few slots do not pass it. Both skip slots created earlier in
/// the block. Ordinary token transfers and swaps stay well under 64.
pub const STALE_STORAGE_CONTRACT_SLOT_GATE: u32 = 64;

/// Dry-run stale-slot limit, per transaction and per block, once the cache holds a full window.
///
/// About 40 to 80 ms of extra state-root time at measured rates: a 145-claim XEN batch (725
/// slots) exceeds it and a 40-claim batch does not.
pub const STALE_STORAGE_LIMIT: u64 = 200;

/// Dry-run limit while the cache holds less than a full window of history.
///
/// A young cache reads every slot it has not seen as stale, so it needs a higher limit.
pub const STALE_STORAGE_FIRST_DAY_LIMIT: u64 = 300;

/// Dry-run limit while the cache holds less than one hour of history, including an empty cache.
pub const STALE_STORAGE_FIRST_HOUR_LIMIT: u64 = 450;

/// Storage-trie depth, in nibbles, of the region one cache entry stands for.
///
/// The cache key is the contract's hashed address plus the first five nibbles of the hashed
/// slot, the slot's path in the contract's storage trie. A slot therefore reads recent when it,
/// or any sibling under the same depth-five trie node, was written within the window, which
/// approximates whether the upper part of its trie path is still warm. The meter still counts
/// slots: dedupe, the gate and the limits use exact slot identities.
///
/// A depth-`d` region shares trie nodes only down to depth `d`; slots in it that do not share
/// deeper branch nodes still fault those in. A contract with `n` slots has about `n / 16^d`
/// slots per region, so depth four would put about 15 slots of a million-slot contract in each
/// region, hiding cold faults in the largest contracts, which are the meter's main target.
/// Depth five keeps that near one slot per region up to a million slots. In a five-hour Base
/// Mainnet replay it left contracts with under a thousand written slots unchanged, removed 2.4%
/// of distinct keys, and changed no dry-run verdict.
pub const STALE_STORAGE_TRIE_PREFIX_NIBBLES: u32 = 5;

/// Entries per cache set.
pub const STALE_STORAGE_CACHE_WAYS: usize = 8;

// Set-index bits stay below the fingerprint bits at the largest size, so the two are independent.
const _: () = assert!(
    StaleStorageCache::sets_for_size((STALE_STORAGE_MAX_CACHE_MIB as u64) << 20).trailing_zeros()
        <= StaleStorageCache::FINGERPRINT_SHIFT
        && StaleStorageCache::FINGERPRINT_SHIFT + 16 <= 64
);

/// Dry-run stale-slot limits in effect for one build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleStorageLimits {
    /// Per-transaction limit on the transaction-scope count.
    pub transaction: u64,
    /// Limit on the block's cumulative block-scope count.
    pub block: u64,
}

impl StaleStorageLimits {
    /// Limits for a cache that has recorded `cache_age_secs` of history; `None` is an empty
    /// cache.
    ///
    /// [`STALE_STORAGE_FIRST_HOUR_LIMIT`] under one hour, [`STALE_STORAGE_FIRST_DAY_LIMIT`] under
    /// one window, then [`STALE_STORAGE_LIMIT`].
    pub const fn at_cache_age(cache_age_secs: Option<u64>) -> Self {
        let limit = match cache_age_secs {
            Some(age) if age >= STALE_STORAGE_WINDOW_HOURS * STALE_STORAGE_EPOCH_SECONDS => {
                STALE_STORAGE_LIMIT
            }
            Some(age) if age >= STALE_STORAGE_EPOCH_SECONDS => STALE_STORAGE_FIRST_DAY_LIMIT,
            _ => STALE_STORAGE_FIRST_HOUR_LIMIT,
        };
        Self { transaction: limit, block: limit }
    }
}

/// One cache set: [`STALE_STORAGE_CACHE_WAYS`] four-byte entries, 32-byte aligned so a set
/// never straddles a cache line.
#[derive(Debug, Default)]
#[repr(C, align(32))]
pub struct StaleStorageCacheSet {
    /// Entries: a 16-bit key fingerprint in the high half and a 16-bit hour stamp, counted from
    /// the cache's base hour, in the low half. A zero stamp marks an empty entry.
    pub entries: [AtomicU32; STALE_STORAGE_CACHE_WAYS],
}

/// What recording one slot write found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaleStorageCacheWrite {
    /// Hours since the slot's previous write if that write is within the window; `None` means
    /// the slot read as stale.
    pub previous_age: Option<u64>,
    /// The write evicted another slot's entry that was still within the window. That slot now
    /// reads as stale although it was written recently.
    pub evicted_unexpired: bool,
}

/// What one occupancy sample found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaleStorageOccupancy {
    /// Entries visited.
    pub visited: u64,
    /// Entries within the window.
    pub live: u64,
}

/// Keyed, 8-way set-associative cache of the hour each `(contract, slot)` was last written by a
/// canonical block.
///
/// Entries are keyed by storage-trie region, not by slot: [`Self::region_key`] covers every slot
/// of a contract whose hashed key starts with the same [`STALE_STORAGE_TRIE_PREFIX_NIBBLES`]
/// nibbles. A slot reads as recent only if its set holds an entry with its region's 16-bit
/// fingerprint and a stamp within [`STALE_STORAGE_WINDOW_HOURS`], meaning the slot or a trie
/// sibling was written recently. Every other slot reads as stale, so errors run one way: a region
/// that was evicted, lost a write race or was never seen over-meters. The only false recent
/// reading beyond that intended sharing is a fingerprint collision: at most `8 / 2^16` per probe
/// of a set full of live entries, about one probe in 8,000, and less in a set that is not full.
/// Set index and fingerprint come from disjoint bits of a `SipHash-2-4` keyed with a per-process
/// random secret, so a sender cannot choose slots that crowd one set.
///
/// Stamps are exact: an entry holds the hours from the cache's base hour to the hour it was
/// written. The base hour is fixed when the cache is built, one window before the wall-clock
/// hour, and never changes, so every reader decodes every entry against the same base. Ages
/// never wrap, so an entry left untouched for any length of time reads as stale, without relying
/// on a sweep. The 16-bit stamp covers [`Self::MAX_STAMP_HOURS`] (about seven years) from the
/// base; past that, writes are dropped and every slot reads stale until the process restarts.
/// Writes timestamped before the base hour, such as blocks replayed while the node syncs, are
/// dropped too.
///
/// Entries are single atomic words updated by compare-and-swap. Builder threads read while the
/// canonical-state task writes; a reader sees either the old or the new entry, both of which
/// were real writes.
pub struct StaleStorageCache {
    sets: Box<[StaleStorageCacheSet]>,
    set_mask: u64,
    secret: (u64, u64),
    /// Timestamp of the first recorded block, or `0` while empty.
    first_recorded_timestamp: AtomicU64,
    /// One more than the last recorded hour, or `0` while empty.
    last_recorded_hour: AtomicU64,
    /// The hour stamps count from, fixed at construction.
    base_hour: u64,
    /// Whether a write past the stamp range has already been logged.
    stamp_range_exhausted: AtomicBool,
    recorded_blocks: AtomicU64,
    sample_cursor: AtomicU64,
}

impl fmt::Debug for StaleStorageCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaleStorageCache")
            .field("entries", &self.len())
            .field("first_recorded_timestamp", &self.first_recorded_timestamp)
            .field("recorded_blocks", &self.recorded_blocks)
            .finish_non_exhaustive()
    }
}

impl StaleStorageCache {
    /// Largest hour offset from the base hour a stamp can hold; stamp `0` marks an empty entry.
    pub const MAX_STAMP_HOURS: u64 = u16::MAX as u64 - 1;
    /// Hours the base hour lies before the wall-clock hour at construction, so blocks from the
    /// last window, such as those a restarted node records first, still have a stamp.
    pub const BASE_MARGIN_HOURS: u64 = STALE_STORAGE_WINDOW_HOURS;
    /// Stamps at most this many hours ahead of the current hour, such as one from a canonical
    /// tip timestamped after the block being built, read as written now. Stamps further ahead
    /// read as stale. A canonical segment timestamped more than this many hours before the
    /// last recorded hour is not recorded.
    pub const FUTURE_TOLERANCE_HOURS: u64 = 2;
    /// Sets an occupancy sample visits per recorded notification.
    pub const OCCUPANCY_SAMPLE_SETS: u64 = 64;
    /// A build this far past the last recorded hour treats the cache as empty, and recording
    /// after such a gap restarts the cache age. The cache missed every write in the gap, so after
    /// four days it is no better than an empty one and gets the young-cache limits. No entry
    /// needs clearing: every entry is then older than the window.
    pub const MAX_RECORD_GAP_HOURS: u64 = 96;
    /// Compare-and-swap attempts per write before giving up. A lost write leaves the slot
    /// reading stale.
    pub const WRITE_ATTEMPTS: usize = 4;
    /// Bytes per set.
    pub const SET_BYTES: u64 = 32;
    /// Bit offset of the 16-bit fingerprint in the key, above every set-index bit.
    pub const FINGERPRINT_SHIFT: u32 = 40;

    /// Creates an empty cache of about `size_bytes` keyed with a random secret, whose base hour
    /// is one window before the current wall-clock hour.
    pub fn new(size_bytes: u64) -> Self {
        let now_secs =
            SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
        let now_hour = now_secs / STALE_STORAGE_EPOCH_SECONDS;
        Self::with_base_hour(size_bytes, now_hour.saturating_sub(Self::BASE_MARGIN_HOURS))
    }

    /// Creates an empty cache of about `size_bytes` keyed with a random secret, whose stamps
    /// count from `base_hour`.
    pub fn with_base_hour(size_bytes: u64, base_hour: u64) -> Self {
        let random = RandomState::new();
        let mut first = random.build_hasher();
        first.write_u64(0);
        let mut second = random.build_hasher();
        second.write_u64(1);
        Self::with_secret(size_bytes, (first.finish(), second.finish()), base_hour)
    }

    /// Creates an empty cache of about `size_bytes` keyed with `secret`, whose stamps count from
    /// `base_hour`. Every set is written with zeros here, so the whole cache is resident once
    /// this returns.
    pub fn with_secret(size_bytes: u64, secret: (u64, u64), base_hour: u64) -> Self {
        let sets = Self::sets_for_size(size_bytes);
        let sets: Box<[StaleStorageCacheSet]> =
            std::iter::repeat_with(StaleStorageCacheSet::default).take(sets as usize).collect();
        Self {
            set_mask: sets.len() as u64 - 1,
            sets,
            secret,
            first_recorded_timestamp: AtomicU64::new(0),
            last_recorded_hour: AtomicU64::new(0),
            base_hour,
            stamp_range_exhausted: AtomicBool::new(false),
            recorded_blocks: AtomicU64::new(0),
            sample_cursor: AtomicU64::new(0),
        }
    }

    /// Sets in a cache of `size_bytes`: the largest power of two that fits, and at least one.
    pub const fn sets_for_size(size_bytes: u64) -> u64 {
        let sets = size_bytes / Self::SET_BYTES;
        if sets <= 1 { 1 } else { 1 << (63 - sets.leading_zeros()) }
    }

    /// Number of entries.
    pub const fn len(&self) -> usize {
        self.sets.len() * STALE_STORAGE_CACHE_WAYS
    }

    /// Returns `false`: a cache always has at least one set.
    pub const fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    /// Keyed `SipHash-2-4` identity of a `(contract, slot)` pair. The builder dedupes slots
    /// within a block by this key; the cache itself is keyed by [`Self::region_key`].
    ///
    /// The message is the 20-byte address, four zero bytes, and the slot's 32 bytes in
    /// little-endian limb order.
    pub fn slot_key(&self, address: &Address, slot: &U256) -> u64 {
        let bytes = &address.0.0;
        let address_word = |offset: usize| {
            u64::from_le_bytes(core::array::from_fn(|index| {
                bytes.get(offset + index).copied().unwrap_or_default()
            }))
        };
        let limbs = slot.as_limbs();
        Self::siphash24(
            self.secret,
            [
                address_word(0),
                address_word(8),
                address_word(16),
                limbs[0],
                limbs[1],
                limbs[2],
                limbs[3],
            ],
        )
    }

    /// Recency key of the depth-[`STALE_STORAGE_TRIE_PREFIX_NIBBLES`] storage-trie region of
    /// `slot` in the contract whose hashed address is `hashed_address`.
    pub fn region_key(&self, hashed_address: &B256, slot: &U256) -> u64 {
        self.region_key_at_depth(
            hashed_address,
            &keccak256(slot.to_be_bytes::<32>()),
            STALE_STORAGE_TRIE_PREFIX_NIBBLES,
        )
    }

    /// Keyed `SipHash-2-4` of a hashed address and the first `nibbles` nibbles of a hashed slot,
    /// the slot's storage-trie path. Slots that share that path prefix share the key; 64 nibbles
    /// keys each slot separately.
    ///
    /// The secret keeps set index and fingerprint unpredictable even though trie paths are
    /// public, so a sender cannot grind slots or addresses into one set or onto a fingerprint.
    pub fn region_key_at_depth(
        &self,
        hashed_address: &B256,
        hashed_slot: &B256,
        nibbles: u32,
    ) -> u64 {
        let word = |bytes: &B256, index: usize| {
            u64::from_be_bytes(core::array::from_fn(|byte| bytes.0[index * 8 + byte]))
        };
        let path_bits = u64::from(nibbles.min(64)) * 4;
        let path_word = |index: usize| {
            let kept = path_bits.saturating_sub(index as u64 * 64).min(64);
            let mask = if kept == 0 { 0 } else { u64::MAX << (64 - kept) };
            word(hashed_slot, index) & mask
        };
        Self::siphash24(
            self.secret,
            [
                word(hashed_address, 0),
                word(hashed_address, 1),
                word(hashed_address, 2),
                word(hashed_address, 3),
                path_word(0),
                path_word(1),
                path_word(2),
                path_word(3),
            ],
        )
    }

    /// The 16-bit fingerprint of `key`, from bits disjoint from the set index.
    pub const fn fingerprint(key: u64) -> u32 {
        (key >> Self::FINGERPRINT_SHIFT) as u32 & 0xffff
    }

    /// The stored entry for `fingerprint` written `offset` hours after the base hour, at most
    /// [`Self::MAX_STAMP_HOURS`].
    pub const fn encode(fingerprint: u32, offset: u64) -> u32 {
        (fingerprint << 16) | (offset as u32 + 1)
    }

    /// Hours from an entry's stamp to `now_hour`, given the cache's `base_hour`; `None` if empty.
    ///
    /// A stamp up to [`Self::FUTURE_TOLERANCE_HOURS`] after `now_hour` reads as age `0`; one
    /// further ahead reads as `u64::MAX`, which is stale.
    pub const fn entry_age(entry: u32, now_hour: u64, base_hour: u64) -> Option<u64> {
        let stamp = (entry & 0xffff) as u64;
        if stamp == 0 {
            return None;
        }
        let written = base_hour + stamp - 1;
        if written <= now_hour {
            Some(now_hour - written)
        } else if written - now_hour <= Self::FUTURE_TOLERANCE_HOURS {
            Some(0)
        } else {
            Some(u64::MAX)
        }
    }

    /// The hour stamps count from.
    pub const fn base_hour(&self) -> u64 {
        self.base_hour
    }

    /// Whether a write in `now_hour` fits the stamp range: no earlier than the base hour and at
    /// most [`Self::MAX_STAMP_HOURS`] after it.
    pub const fn can_stamp(&self, now_hour: u64) -> bool {
        now_hour >= self.base_hour && now_hour - self.base_hour <= Self::MAX_STAMP_HOURS
    }

    /// The set `key` maps to.
    pub fn set(&self, key: u64) -> &StaleStorageCacheSet {
        &self.sets[(key & self.set_mask) as usize]
    }

    /// Hours since `key` was last written, if that is within the window; `None` means stale:
    /// never recorded, evicted, or written more than [`STALE_STORAGE_WINDOW_HOURS`] ago.
    pub fn age_hours(&self, key: u64, now_hour: u64) -> Option<u64> {
        let fingerprint = Self::fingerprint(key);
        self.set(key)
            .entries
            .iter()
            .map(|entry| entry.load(Ordering::Relaxed))
            .filter(|&entry| entry >> 16 == fingerprint)
            .filter_map(|entry| Self::entry_age(entry, now_hour, self.base_hour))
            .filter(|&age| age <= STALE_STORAGE_WINDOW_HOURS)
            .min()
    }

    /// Whether `key` reads as stale at `now_hour`.
    pub fn is_stale(&self, key: u64, now_hour: u64) -> bool {
        self.age_hours(key, now_hour).is_none()
    }

    /// Records that `key` was written in `now_hour`, reading its previous age first.
    ///
    /// Updates the entry with `key`'s fingerprint, otherwise takes an empty or expired entry,
    /// otherwise evicts the oldest. If every attempt loses a race with another writer, the write
    /// is dropped and the slot reads stale. A write outside the stamp range (see
    /// [`Self::can_stamp`]) is dropped too.
    pub fn record(&self, key: u64, now_hour: u64) -> StaleStorageCacheWrite {
        if !self.can_stamp(now_hour) {
            return StaleStorageCacheWrite::default();
        }
        let base_hour = self.base_hour;
        let fingerprint = Self::fingerprint(key);
        let written = Self::encode(fingerprint, now_hour - base_hour);
        let set = self.set(key);
        let mut previous_age = None;
        for _ in 0..Self::WRITE_ATTEMPTS {
            previous_age = None;
            let mut matched = None;
            // Victim rank: empty first, then expired, then the oldest live entry.
            let mut victim = (0, 0, 0);
            for (way, entry) in set.entries.iter().enumerate() {
                let current = entry.load(Ordering::Relaxed);
                let age = Self::entry_age(current, now_hour, base_hour);
                if age.is_some() && current >> 16 == fingerprint {
                    if let Some(age) = age.filter(|&age| age <= STALE_STORAGE_WINDOW_HOURS) {
                        previous_age =
                            Some(previous_age.map_or(age, |previous: u64| previous.min(age)));
                    }
                    matched.get_or_insert((way, current));
                    continue;
                }
                let rank = match age {
                    None => u64::MAX,
                    Some(age) if age > STALE_STORAGE_WINDOW_HOURS => u64::MAX - 1,
                    Some(age) => age,
                };
                if way == 0 || rank > victim.2 {
                    victim = (way, current, rank);
                }
            }
            let (way, current, evicted_unexpired) = match matched {
                Some((way, current)) => (way, current, false),
                None => (victim.0, victim.1, victim.2 <= STALE_STORAGE_WINDOW_HOURS),
            };
            if current == written {
                return StaleStorageCacheWrite { previous_age, evicted_unexpired: false };
            }
            if set.entries[way]
                .compare_exchange(current, written, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return StaleStorageCacheWrite { previous_age, evicted_unexpired };
            }
        }
        StaleStorageCacheWrite { previous_age, evicted_unexpired: false }
    }

    /// Counts live entries in up to `sets` sets, continuing from where the previous sample
    /// stopped. Read-only: expired entries need no clearing, since ages never wrap and a write
    /// replaces an expired entry before a live one.
    pub fn sample_occupancy(&self, now_hour: u64, sets: u64) -> StaleStorageOccupancy {
        let len = self.sets.len() as u64;
        let count = sets.min(len);
        let start = self.sample_cursor.fetch_add(count, Ordering::Relaxed) % len;
        let mut sample =
            StaleStorageOccupancy { visited: count * STALE_STORAGE_CACHE_WAYS as u64, live: 0 };
        let base_hour = self.base_hour;
        for offset in 0..count {
            for entry in &self.sets[((start + offset) % len) as usize].entries {
                let age = Self::entry_age(entry.load(Ordering::Relaxed), now_hour, base_hour);
                sample.live += u64::from(age.is_some_and(|age| age <= STALE_STORAGE_WINDOW_HOURS));
            }
        }
        sample
    }

    /// Empties every entry in `key`'s set that holds `key`'s fingerprint, so its region reads
    /// stale until written again. Returns how many entries were emptied. An entry rewritten
    /// concurrently is left as it is.
    pub fn invalidate(&self, key: u64) -> u64 {
        let fingerprint = Self::fingerprint(key);
        let mut emptied = 0;
        for entry in &self.set(key).entries {
            let current = entry.load(Ordering::Relaxed);
            if current & 0xffff != 0 && current >> 16 == fingerprint {
                emptied += u64::from(
                    entry
                        .compare_exchange(current, 0, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok(),
                );
            }
        }
        emptied
    }

    /// Whether a build at `now_hour` is more than [`Self::MAX_RECORD_GAP_HOURS`] past the last
    /// recorded hour. Such a build treats the cache as empty, since it misses the gap's writes.
    pub fn is_expired_at(&self, now_hour: u64) -> bool {
        let last = self.last_recorded_hour.load(Ordering::Relaxed);
        last != 0 && now_hour > last - 1 + Self::MAX_RECORD_GAP_HOURS
    }

    /// `SipHash-2-4` of a `8 * N`-byte message given as `N` little-endian words.
    pub const fn siphash24<const N: usize>(key: (u64, u64), words: [u64; N]) -> u64 {
        let mut v = [
            key.0 ^ 0x736f_6d65_7073_6575,
            key.1 ^ 0x646f_7261_6e64_6f6d,
            key.0 ^ 0x6c79_6765_6e65_7261,
            key.1 ^ 0x7465_6462_7974_6573,
        ];
        let mut index = 0;
        while index < words.len() {
            v[3] ^= words[index];
            Self::sip_round(&mut v);
            Self::sip_round(&mut v);
            v[0] ^= words[index];
            index += 1;
        }
        // Final block: no tail bytes, message length in the top byte.
        let last = ((N as u64 * 8) & 0xff) << 56;
        v[3] ^= last;
        Self::sip_round(&mut v);
        Self::sip_round(&mut v);
        v[0] ^= last;
        v[2] ^= 0xff;
        Self::sip_round(&mut v);
        Self::sip_round(&mut v);
        Self::sip_round(&mut v);
        Self::sip_round(&mut v);
        v[0] ^ v[1] ^ v[2] ^ v[3]
    }

    /// One `SipRound` over the state words.
    pub const fn sip_round(v: &mut [u64; 4]) {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13);
        v[1] ^= v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16);
        v[3] ^= v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21);
        v[3] ^= v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17);
        v[1] ^= v[2];
        v[2] = v[2].rotate_left(32);
    }

    /// Seconds of history the cache holds at `now_timestamp`, or `None` while it is empty.
    pub fn age_secs(&self, now_timestamp: u64) -> Option<u64> {
        let first = self.first_recorded_timestamp.load(Ordering::Relaxed);
        (first != 0).then(|| now_timestamp.saturating_sub(first))
    }

    /// Number of canonical blocks recorded since start.
    pub fn recorded_blocks(&self) -> u64 {
        self.recorded_blocks.load(Ordering::Relaxed)
    }
}

/// Stale-slot counts by age, in hours since the slot was last written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaleAgeBuckets {
    /// Written this hour or the previous one.
    pub le_1h: u64,
    /// Written 2 to 6 hours ago.
    pub h1_6: u64,
    /// Written 7 to 24 hours ago.
    pub h6_24: u64,
    /// Stale: not in the cache within the window, whether never written, written earlier or
    /// evicted.
    pub stale: u64,
}

impl StaleAgeBuckets {
    /// Metric label for each bucket, in field order.
    pub const LABELS: [&'static str; 4] = ["le_1h", "1h_6h", "6h_24h", "stale"];

    /// Adds one slot of the given age; `None` is stale.
    pub const fn add(&mut self, age_hours: Option<u64>) {
        match age_hours {
            None => self.stale += 1,
            Some(0..=1) => self.le_1h += 1,
            Some(2..=6) => self.h1_6 += 1,
            Some(_) => self.h6_24 += 1,
        }
    }

    /// Counts in [`Self::LABELS`] order.
    pub const fn counts(&self) -> [u64; 4] {
        [self.le_1h, self.h1_6, self.h6_24, self.stale]
    }
}

/// What recording one canonical chain segment found, measured before stamping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CanonicalStaleRecord {
    /// Changed slots stamped.
    pub changed_slots: u64,
    /// Pre-existing changed slots by age, in every contract.
    pub ungated: StaleAgeBuckets,
    /// Pre-existing changed slots by age, in contracts past the gate.
    pub gated: StaleAgeBuckets,
    /// Created slots in contracts past the gate.
    pub gated_created_slots: u64,
    /// Cleared slots in contracts past the gate.
    pub gated_cleared_slots: u64,
    /// Largest changed-slot count of one contract.
    pub max_contract_changed_slots: u64,
    /// Writes that evicted another slot's entry still within the window.
    pub unexpired_evictions: u64,
    /// What the occupancy sample found.
    pub occupancy: StaleStorageOccupancy,
    /// Changed slots not recorded, and why; see [`StaleStorageDroppedWrites`].
    pub dropped: Option<StaleStorageDroppedWrites>,
    /// Recording resumed after a gap longer than [`StaleStorageCache::MAX_RECORD_GAP_HOURS`],
    /// so the cache age restarted.
    pub restarted_after_gap: bool,
    /// Cache entries emptied for the changed slots of reverted blocks.
    pub reverted_invalidations: u64,
}

/// Why a canonical segment's writes were not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleStorageDropReason {
    /// The segment predates the cache's base hour, as while the node syncs old blocks.
    BeforeBaseHour,
    /// The segment is past the stamp range of the base hour; a process restart resets it.
    PastStampRange,
    /// The segment is timestamped more than [`StaleStorageCache::FUTURE_TOLERANCE_HOURS`]
    /// before the last recorded hour.
    Backward,
}

/// Changed slots of a canonical segment that were not recorded. Their regions keep whatever the
/// cache held, so they can only read stale, never newly recent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleStorageDroppedWrites {
    /// Why the segment was not recorded.
    pub reason: StaleStorageDropReason,
    /// Changed slots not recorded.
    pub slots: u64,
}

/// The cache shared by the canonical-state recorder and the payload builder.
///
/// The window, gate and limits are the `STALE_STORAGE_*` constants; only the cache size is a
/// parameter.
#[derive(Debug, Clone)]
pub struct StaleStorageMeter {
    /// Shared recency cache.
    pub cache: Arc<StaleStorageCache>,
}

impl StaleStorageMeter {
    /// Allocates an empty cache of about `size_bytes`; see [`StaleStorageCache::sets_for_size`].
    pub fn new(size_bytes: u64) -> Self {
        Self { cache: Arc::new(StaleStorageCache::new(size_bytes)) }
    }

    /// Allocates an empty cache of `cache_mib` mebibytes.
    pub fn with_cache_mib(cache_mib: u32) -> Self {
        Self::new(u64::from(cache_mib) << 20)
    }

    /// Starts per-build tracking for a block at `block_timestamp`.
    pub fn block_tracker(&self, block_number: u64, block_timestamp: u64) -> BlockStaleTracker {
        let now_hour = block_timestamp / STALE_STORAGE_EPOCH_SECONDS;
        let cache_expired = self.cache.is_expired_at(now_hour);
        let cache_age_secs =
            if cache_expired { None } else { self.cache.age_secs(block_timestamp) };
        BlockStaleTracker {
            cache: Arc::clone(&self.cache),
            block_number,
            now_hour,
            cache_expired,
            cache_age_secs,
            limits: StaleStorageLimits::at_cache_age(cache_age_secs),
            contract_changed_slots: AddressMap::default(),
            changed_slot_keys: HashSet::default(),
            created_slot_keys: HashSet::default(),
            counted_stale_keys: HashSet::default(),
            pending_contracts: Vec::new(),
            pending_changed_keys: Vec::new(),
            pending_created_keys: Vec::new(),
            contract_slots: Vec::new(),
            pending_stale_keys: Vec::new(),
            pending: None,
            totals: BlockStaleTotals::default(),
        }
    }

    /// Stamps every changed slot of a canonical segment of `blocks` blocks whose tip is at
    /// `timestamp`, and measures the age of each pre-existing one first.
    ///
    /// Every slot is classified against the cache as it was before the segment, and only then
    /// are the segment's regions stamped. Trie siblings share a region, so classifying and
    /// stamping slot by slot would let a slot read its sibling's stamp from the same segment,
    /// and the result would depend on iteration order.
    pub fn record_bundle(
        &self,
        bundle: &BundleState,
        blocks: u64,
        timestamp: u64,
    ) -> CanonicalStaleRecord {
        let now_hour = timestamp / STALE_STORAGE_EPOCH_SECONDS;
        let gate = u64::from(STALE_STORAGE_CONTRACT_SLOT_GATE);
        let mut record = CanonicalStaleRecord::default();
        let last = self.cache.last_recorded_hour.load(Ordering::Relaxed);
        let reason = if now_hour < self.cache.base_hour {
            Some(StaleStorageDropReason::BeforeBaseHour)
        } else if !self.cache.can_stamp(now_hour) {
            Some(StaleStorageDropReason::PastStampRange)
        } else if last != 0 && now_hour + StaleStorageCache::FUTURE_TOLERANCE_HOURS < last - 1 {
            Some(StaleStorageDropReason::Backward)
        } else {
            None
        };
        if let Some(reason) = reason {
            let slots = bundle
                .state
                .values()
                .map(|account| account.storage.values().filter(|slot| slot.is_changed()).count())
                .sum::<usize>() as u64;
            record.dropped = Some(StaleStorageDroppedWrites { reason, slots });
            return record;
        }
        if self.cache.is_expired_at(now_hour) {
            self.cache.first_recorded_timestamp.store(timestamp.max(1), Ordering::Relaxed);
            record.restarted_after_gap = true;
        }
        let mut keys = Vec::new();
        for (address, account) in &bundle.state {
            let changed = account.storage.values().filter(|slot| slot.is_changed()).count() as u64;
            if changed == 0 {
                continue;
            }
            record.changed_slots += changed;
            record.max_contract_changed_slots = record.max_contract_changed_slots.max(changed);
            let gated = changed > gate;
            let hashed_address = keccak256(address);
            for (slot_id, slot) in &account.storage {
                if !slot.is_changed() {
                    continue;
                }
                let key = self.cache.region_key(&hashed_address, slot_id);
                keys.push(key);
                if slot.previous_or_original_value.is_zero() {
                    record.gated_created_slots += u64::from(gated);
                    continue;
                }
                let age = self.cache.age_hours(key, now_hour);
                record.ungated.add(age);
                if gated {
                    record.gated.add(age);
                    record.gated_cleared_slots += u64::from(slot.present_value.is_zero());
                }
            }
        }
        for key in keys {
            record.unexpired_evictions +=
                u64::from(self.cache.record(key, now_hour).evicted_unexpired);
        }
        // Timestamps are never zero on a real chain; `max(1)` keeps `0` meaning empty.
        let _ = self.cache.first_recorded_timestamp.compare_exchange(
            0,
            timestamp.max(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        self.cache.last_recorded_hour.fetch_max(now_hour + 1, Ordering::Relaxed);
        self.cache.recorded_blocks.fetch_add(blocks, Ordering::Relaxed);
        record.occupancy =
            self.cache.sample_occupancy(now_hour, StaleStorageCache::OCCUPANCY_SAMPLE_SETS);
        record
    }

    /// Empties the cache entries for the region of every slot any block in `bundle` wrote, so
    /// writes that left the canonical chain no longer make those regions read recent. Returns
    /// how many entries were emptied. A region the replacement chain also wrote reads stale until
    /// that chain's segment is recorded.
    ///
    /// The net state alone misses a slot a later block in the segment set back to its original
    /// value, which reads unchanged there but was stamped when each block was recorded. Such a
    /// slot stays in the net state, and every block's reverts list it, so both are walked.
    pub fn invalidate_bundle(&self, bundle: &BundleState) -> u64 {
        let mut emptied = 0;
        let mut invalidate = |address: &Address, slots: &mut dyn Iterator<Item = &U256>| {
            let mut hashed_address = None;
            for slot_id in slots {
                let hashed_address = hashed_address.get_or_insert_with(|| keccak256(address));
                emptied += self.cache.invalidate(self.cache.region_key(hashed_address, slot_id));
            }
        };
        for (address, account) in &bundle.state {
            invalidate(address, &mut account.storage.keys());
        }
        for (address, revert) in bundle.reverts.iter().flatten() {
            invalidate(address, &mut revert.storage.keys());
        }
        emptied
    }

    /// Records a canonical notification: invalidates the regions written by any reverted
    /// segment, then records the committed segment. Returns `None` if nothing was committed.
    pub fn record_notification<N: NodePrimitives>(
        &self,
        notification: &CanonStateNotification<N>,
    ) -> Option<CanonicalStaleRecord> {
        let reverted_invalidations = notification
            .reverted()
            .map_or(0, |reverted| self.invalidate_bundle(&reverted.execution_outcome().bundle));
        if reverted_invalidations > 0 {
            StaleStorageMetrics::cache_reorg_invalidations_total()
                .increment(reverted_invalidations);
        }
        let committed = notification.committed();
        if committed.is_empty() {
            return None;
        }
        let tip = committed.tip();
        let mut record = self.record_bundle(
            &committed.execution_outcome().bundle,
            committed.len() as u64,
            tip.timestamp(),
        );
        record.reverted_invalidations = reverted_invalidations;
        if let Some(dropped) = record.dropped {
            StaleStorageMetrics::cache_dropped_writes_total().increment(dropped.slots);
            match dropped.reason {
                StaleStorageDropReason::BeforeBaseHour => debug!(
                    target: "payload_builder::stale_storage",
                    block_number = tip.number(),
                    base_hour = self.cache.base_hour,
                    "stale storage segment predates the cache's base hour; not recorded"
                ),
                StaleStorageDropReason::PastStampRange => {
                    if !self.cache.stamp_range_exhausted.swap(true, Ordering::Relaxed) {
                        warn!(
                            target: "payload_builder::stale_storage",
                            block_number = tip.number(),
                            base_hour = self.cache.base_hour,
                            "stale storage cache is past its stamp range; recording stopped until restart"
                        );
                    }
                }
                StaleStorageDropReason::Backward => warn!(
                    target: "payload_builder::stale_storage",
                    block_number = tip.number(),
                    block_timestamp = tip.timestamp(),
                    "stale storage segment is timestamped before the last recorded hour; not recorded"
                ),
            }
            return Some(record);
        }
        let cache_age_secs = self.cache.age_secs(tip.timestamp()).unwrap_or_default();
        StaleStorageMetrics::recorded_blocks_total().increment(committed.len() as u64);
        StaleStorageMetrics::cache_age_seconds().set(cache_age_secs as f64);
        StaleStorageMetrics::cache_writes_total().increment(record.changed_slots);
        if record.unexpired_evictions > 0 {
            StaleStorageMetrics::cache_unexpired_evictions_total()
                .increment(record.unexpired_evictions);
        }
        if record.occupancy.visited > 0 {
            StaleStorageMetrics::cache_occupancy_ratio()
                .set(record.occupancy.live as f64 / record.occupancy.visited as f64);
        }
        if record.restarted_after_gap {
            warn!(
                target: "payload_builder::stale_storage",
                block_number = tip.number(),
                max_gap_hours = StaleStorageCache::MAX_RECORD_GAP_HOURS,
                "stale storage recording resumed after a long gap; restarted the cache age"
            );
        }
        for (gate, buckets) in [("ungated", &record.ungated), ("gated", &record.gated)] {
            for (label, count) in StaleAgeBuckets::LABELS.into_iter().zip(buckets.counts()) {
                if count > 0 {
                    StaleStorageMetrics::canonical_stale_slots_total(gate, label).increment(count);
                }
            }
        }
        let gated_stale = record.gated.stale;
        if gated_stale > 0 {
            info!(
                target: "payload_builder::stale_storage",
                block_number = tip.number(),
                block_hash = %tip.hash(),
                blocks = committed.len(),
                reorg = matches!(notification, CanonStateNotification::Reorg { .. }),
                gas_used = tip.gas_used(),
                changed_slots = record.changed_slots,
                gated_stale_le_1h = record.gated.le_1h,
                gated_stale_1h_6h = record.gated.h1_6,
                gated_stale_6h_24h = record.gated.h6_24,
                gated_stale = record.gated.stale,
                ungated_stale = record.ungated.stale,
                unexpired_evictions = record.unexpired_evictions,
                gated_created_slots = record.gated_created_slots,
                gated_cleared_slots = record.gated_cleared_slots,
                max_contract_changed_slots = record.max_contract_changed_slots,
                cache_age_secs,
                "recorded canonical block storage staleness"
            );
        } else {
            debug!(
                target: "payload_builder::stale_storage",
                block_number = tip.number(),
                blocks = committed.len(),
                changed_slots = record.changed_slots,
                ungated_stale = record.ungated.stale,
                unexpired_evictions = record.unexpired_evictions,
                max_contract_changed_slots = record.max_contract_changed_slots,
                cache_age_secs,
                "recorded canonical block storage staleness"
            );
        }
        Some(record)
    }

    /// Stamps the cache from canonical-state notifications until the stream ends.
    ///
    /// A lagged stream leaves a hole: only slots written in the missed blocks and nowhere else
    /// read stale. The cache is kept rather than reset.
    pub async fn maintain<N: NodePrimitives>(self, events: CanonStateNotifications<N>) {
        let mut events = BroadcastStream::new(events);
        loop {
            match events.next().await {
                Some(Ok(notification)) => {
                    self.record_notification(&notification);
                }
                Some(Err(BroadcastStreamRecvError::Lagged(missed))) => {
                    StaleStorageMetrics::canonical_stream_lagged_total().increment(missed);
                    warn!(
                        target: "payload_builder::stale_storage",
                        missed = missed,
                        "stale storage canonical stream lagged; keeping existing cache"
                    );
                }
                None => break,
            }
        }
    }
}

/// Stale-slot counts for one executed transaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaleTxSample {
    /// Changed slots in every contract.
    pub changed_slots: u32,
    /// Contracts with at least one changed slot.
    pub storage_tries: u32,
    /// Largest changed-slot count of one contract in this transaction.
    pub max_contract_changed_slots: u32,
    /// Stale slots in contracts where this transaction alone is past the gate.
    pub stale_transaction: u32,
    /// Stale slots in contracts the block is past the gate for, not counted earlier in the block.
    pub stale_block: u32,
    /// Changed slots in contracts the block is past the gate for.
    pub gated_changed_slots: u32,
    /// Created slots in contracts the block is past the gate for.
    pub gated_created_slots: u32,
    /// Cleared slots in contracts the block is past the gate for.
    pub gated_cleared_slots: u32,
    /// The contract with the most block-scope stale slots, if any.
    pub top_contract: Option<Address>,
}

/// Running totals of the transactions committed to one build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockStaleTotals {
    /// Committed transactions evaluated.
    pub transactions: u64,
    /// Sum of block-scope stale counts.
    pub stale_block: u64,
    /// Largest transaction-scope stale count.
    pub max_stale_transaction: u64,
    /// Changed slots in contracts past the block gate.
    pub gated_changed_slots: u64,
    /// Created slots in contracts past the block gate.
    pub gated_created_slots: u64,
    /// Cleared slots in contracts past the block gate.
    pub gated_cleared_slots: u64,
    /// Largest cumulative changed-slot count of one contract.
    pub max_contract_changed_slots: u64,
    /// Largest storage-trie count of one transaction.
    pub max_storage_tries: u64,
    /// Transactions the age-tiered transaction limit would have flagged.
    pub flagged_transaction_scope: u64,
    /// Transactions the age-tiered block limit would have flagged.
    pub flagged_block_scope: u64,
    /// Block-scope stale total of the transactions the age-tiered limits would have admitted.
    pub tiered_admitted_stale_block: u64,
    /// Block-scope stale total of the transactions the plain limit would have admitted.
    pub plain_admitted_stale_block: u64,
    /// Transactions added without a verdict, such as sequencer transactions.
    pub unmetered_transactions: u64,
    /// Block-scope stale slots of the transactions added without a verdict.
    pub unmetered_stale_block: u64,
}

/// Dry-run outcome of one committed transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleTxVerdict {
    /// The transaction's counts.
    pub sample: StaleTxSample,
    /// Block-scope total before this transaction, over every committed transaction.
    pub cumulative_stale_block: u64,
    /// Block-scope total before this transaction, over the transactions the age-tiered limits
    /// would have admitted. The age-tiered block limit applies to this total.
    pub tiered_admitted_stale_block: u64,
    /// Block-scope total before this transaction, over the transactions the plain limit would
    /// have admitted. The plain block limit applies to this total.
    pub plain_admitted_stale_block: u64,
    /// The age-tiered transaction limit would have flagged it.
    pub exceeds_transaction_limit: bool,
    /// The age-tiered block limit would have flagged it.
    pub exceeds_block_limit: bool,
    /// The plain transaction limit would have flagged it.
    pub exceeds_plain_transaction_limit: bool,
    /// The plain block limit would have flagged it.
    pub exceeds_plain_block_limit: bool,
}

impl StaleTxVerdict {
    /// Whether any limit, tiered or plain, would have flagged the transaction.
    pub const fn is_flagged(&self) -> bool {
        self.exceeds_transaction_limit
            || self.exceeds_block_limit
            || self.exceeds_plain_transaction_limit
            || self.exceeds_plain_block_limit
    }
}

/// Per-build stale-slot state for the payload builder.
///
/// Call [`Self::evaluate`] on each executed transaction's state in the commit path and
/// [`Self::commit`] once the block builder commits it, or [`Self::include_unmetered`] for a
/// transaction enforcement could not exclude, such as a sequencer transaction. A transaction that
/// is evaluated but not committed is discarded by the next evaluation.
#[derive(Debug)]
pub struct BlockStaleTracker {
    cache: Arc<StaleStorageCache>,
    block_number: u64,
    now_hour: u64,
    /// The build is too far past the last recorded hour to trust the cache; every slot is stale.
    cache_expired: bool,
    cache_age_secs: Option<u64>,
    limits: StaleStorageLimits,
    /// Distinct changed slots per contract across committed transactions.
    contract_changed_slots: AddressMap<u32>,
    /// Slots changed by committed transactions of this block.
    changed_slot_keys: HashSet<u64>,
    /// Slots created by committed transactions of this block: they did not exist before it.
    created_slot_keys: HashSet<u64>,
    /// Stale slots already counted in this block's block-scope total.
    counted_stale_keys: HashSet<u64>,
    pending_contracts: Vec<(Address, u32)>,
    pending_changed_keys: Vec<u64>,
    pending_created_keys: Vec<u64>,
    /// Scratch `(identity key, slot, created, cleared)` for the changed slots of one contract.
    contract_slots: Vec<(u64, U256, bool, bool)>,
    pending_stale_keys: Vec<u64>,
    pending: Option<StaleTxSample>,
    totals: BlockStaleTotals,
}

impl BlockStaleTracker {
    /// Dry-run limits for this build, from the cache's age when the build started.
    pub const fn limits(&self) -> StaleStorageLimits {
        self.limits
    }

    /// Running totals of committed transactions.
    pub const fn totals(&self) -> BlockStaleTotals {
        self.totals
    }

    /// Counts the stale slots of an executed transaction without changing the block state.
    ///
    /// Only changed slots count toward the gate; slots that were only read are skipped. Each
    /// changed slot is hashed once, so the block-scope gate counts distinct slots and a later
    /// transaction that rewrites a slot created earlier in the block does not read it as an
    /// old, never-recorded slot. Only contracts past a gate probe the cache, by trie region
    /// ([`StaleStorageCache::region_key`]); each slot still counts once.
    pub fn evaluate(&mut self, state: &EvmState) -> StaleTxSample {
        self.pending_contracts.clear();
        self.pending_changed_keys.clear();
        self.pending_created_keys.clear();
        self.pending_stale_keys.clear();
        let gate = STALE_STORAGE_CONTRACT_SLOT_GATE;
        let mut sample = StaleTxSample::default();
        let mut top_contract_stale = 0;
        for (address, account) in state {
            self.contract_slots.clear();
            let mut new_changed = 0u32;
            for (slot_id, slot) in &account.storage {
                if !slot.is_changed() {
                    continue;
                }
                let key = self.cache.slot_key(address, slot_id);
                if !self.changed_slot_keys.contains(&key) {
                    new_changed += 1;
                    self.pending_changed_keys.push(key);
                }
                self.contract_slots.push((
                    key,
                    *slot_id,
                    slot.original_value().is_zero(),
                    slot.present_value().is_zero(),
                ));
            }
            let changed = self.contract_slots.len() as u32;
            if changed == 0 {
                continue;
            }
            sample.changed_slots += changed;
            sample.storage_tries += 1;
            sample.max_contract_changed_slots = sample.max_contract_changed_slots.max(changed);
            self.pending_contracts.push((*address, new_changed));
            let before = self.contract_changed_slots.get(address).copied().unwrap_or_default();
            let gated_block = before.saturating_add(new_changed) > gate;
            // A transaction past the gate is always past the block gate: its rewrites of slots
            // the block already changed are in `before`.
            let gated_transaction = changed > gate;
            if gated_block {
                sample.gated_changed_slots += changed;
            }
            let mut contract_stale = 0;
            // Hashed once per gated contract, only if one of its slots is probed.
            let mut hashed_address = None;
            for &(key, slot_id, created, cleared) in &self.contract_slots {
                if created {
                    self.pending_created_keys.push(key);
                    sample.gated_created_slots += u32::from(gated_block);
                    continue;
                }
                if !gated_block {
                    continue;
                }
                if self.created_slot_keys.contains(&key) {
                    continue;
                }
                sample.gated_cleared_slots += u32::from(cleared);
                if !self.cache_expired {
                    let hashed_address = hashed_address.get_or_insert_with(|| keccak256(address));
                    if !self
                        .cache
                        .is_stale(self.cache.region_key(hashed_address, &slot_id), self.now_hour)
                    {
                        continue;
                    }
                }
                sample.stale_transaction += u32::from(gated_transaction);
                if !self.counted_stale_keys.contains(&key) {
                    contract_stale += 1;
                    self.pending_stale_keys.push(key);
                }
            }
            sample.stale_block += contract_stale;
            if contract_stale > top_contract_stale {
                top_contract_stale = contract_stale;
                sample.top_contract = Some(*address);
            }
        }
        self.pending = Some(sample);
        sample
    }

    /// Adds the last evaluated transaction's slots to the block's gate and dedupe state without
    /// counting it toward any total. Returns its sample, or `None` if nothing was evaluated since
    /// the last commit.
    pub fn merge_pending(&mut self) -> Option<StaleTxSample> {
        let sample = self.pending.take()?;
        for (address, changed) in self.pending_contracts.drain(..) {
            let total = self.contract_changed_slots.entry(address).or_default();
            *total = total.saturating_add(changed);
            self.totals.max_contract_changed_slots =
                self.totals.max_contract_changed_slots.max(u64::from(*total));
        }
        self.changed_slot_keys.extend(self.pending_changed_keys.drain(..));
        self.created_slot_keys.extend(self.pending_created_keys.drain(..));
        self.counted_stale_keys.extend(self.pending_stale_keys.drain(..));
        Some(sample)
    }

    /// Adds the last evaluated transaction to the block without a verdict, for transactions
    /// enforcement could not exclude, such as sequencer transactions. Its slots still count
    /// toward the gate and are not counted again by later transactions, but its stale slots stay
    /// out of the dry-run totals. Returns `None` if nothing was evaluated since the last commit.
    pub fn include_unmetered(&mut self) -> Option<StaleTxSample> {
        let sample = self.merge_pending()?;
        self.totals.unmetered_transactions += 1;
        self.totals.unmetered_stale_block += u64::from(sample.stale_block);
        Some(sample)
    }

    /// Adds the last evaluated transaction to the block and logs what the dry-run limits would
    /// have flagged. Returns `None` if nothing was evaluated since the last commit.
    ///
    /// Each set of limits, tiered and plain, checks the block scope against the stale total of
    /// the transactions it would have admitted: a transaction it would have flagged would be
    /// excluded under enforcement, so its slots do not push later transactions over.
    pub fn commit(&mut self, tx_hash: &TxHash) -> Option<StaleTxVerdict> {
        let sample = self.merge_pending()?;
        let cumulative_stale_block = self.totals.stale_block;
        let tiered_admitted_stale_block = self.totals.tiered_admitted_stale_block;
        let plain_admitted_stale_block = self.totals.plain_admitted_stale_block;
        let stale_transaction = u64::from(sample.stale_transaction);
        let stale_block = u64::from(sample.stale_block);
        let verdict = StaleTxVerdict {
            sample,
            cumulative_stale_block,
            tiered_admitted_stale_block,
            plain_admitted_stale_block,
            exceeds_transaction_limit: stale_transaction > self.limits.transaction,
            exceeds_block_limit: stale_block > 0
                && tiered_admitted_stale_block + stale_block > self.limits.block,
            exceeds_plain_transaction_limit: stale_transaction > STALE_STORAGE_LIMIT,
            exceeds_plain_block_limit: stale_block > 0
                && plain_admitted_stale_block + stale_block > STALE_STORAGE_LIMIT,
        };

        let totals = &mut self.totals;
        totals.transactions += 1;
        totals.stale_block = cumulative_stale_block + stale_block;
        if !(verdict.exceeds_transaction_limit || verdict.exceeds_block_limit) {
            totals.tiered_admitted_stale_block += stale_block;
        }
        if !(verdict.exceeds_plain_transaction_limit || verdict.exceeds_plain_block_limit) {
            totals.plain_admitted_stale_block += stale_block;
        }
        totals.max_stale_transaction = totals.max_stale_transaction.max(stale_transaction);
        totals.gated_changed_slots += u64::from(sample.gated_changed_slots);
        totals.gated_created_slots += u64::from(sample.gated_created_slots);
        totals.gated_cleared_slots += u64::from(sample.gated_cleared_slots);
        totals.max_storage_tries = totals.max_storage_tries.max(u64::from(sample.storage_tries));
        totals.flagged_transaction_scope += u64::from(verdict.exceeds_transaction_limit);
        totals.flagged_block_scope += u64::from(verdict.exceeds_block_limit);
        if verdict.is_flagged() {
            self.record_flag(tx_hash, &verdict);
        }
        Some(verdict)
    }

    /// Logs and counts a transaction a dry-run limit would have flagged. Runs only for flagged
    /// transactions, so metric handles are looked up here rather than per transaction.
    pub fn record_flag(&self, tx_hash: &TxHash, verdict: &StaleTxVerdict) {
        for (scope, limits, flagged) in [
            ("transaction", "tiered", verdict.exceeds_transaction_limit),
            ("block", "tiered", verdict.exceeds_block_limit),
            ("transaction", "plain", verdict.exceeds_plain_transaction_limit),
            ("block", "plain", verdict.exceeds_plain_block_limit),
        ] {
            if flagged {
                StaleStorageMetrics::would_flag_total(scope, limits).increment(1);
            }
        }
        let sample = &verdict.sample;
        info!(
            target: "payload_builder::stale_storage",
            block_number = self.block_number,
            tx_hash = %tx_hash,
            top_contract = ?sample.top_contract,
            stale_transaction = sample.stale_transaction,
            stale_block = sample.stale_block,
            cumulative_stale_block = verdict.cumulative_stale_block,
            tiered_admitted_stale_block = verdict.tiered_admitted_stale_block,
            plain_admitted_stale_block = verdict.plain_admitted_stale_block,
            changed_slots = sample.changed_slots,
            gated_changed_slots = sample.gated_changed_slots,
            gated_created_slots = sample.gated_created_slots,
            gated_cleared_slots = sample.gated_cleared_slots,
            storage_tries = sample.storage_tries,
            exceeds_transaction_limit = verdict.exceeds_transaction_limit,
            exceeds_block_limit = verdict.exceeds_block_limit,
            exceeds_plain_transaction_limit = verdict.exceeds_plain_transaction_limit,
            exceeds_plain_block_limit = verdict.exceeds_plain_block_limit,
            transaction_limit = self.limits.transaction,
            block_limit = self.limits.block,
            cache_age_secs = self.cache_age_secs,
            "stale storage dry-run limit exceeded"
        );
    }

    /// Records the build's totals.
    pub fn finish(&self) {
        let totals = self.totals;
        StaleStorageMetrics::build_stale_slots().record(totals.stale_block as f64);
        if totals.stale_block > 0 || totals.max_stale_transaction > 0 {
            info!(
                target: "payload_builder::stale_storage",
                block_number = self.block_number,
                transactions = totals.transactions,
                stale_block = totals.stale_block,
                unmetered_transactions = totals.unmetered_transactions,
                unmetered_stale_block = totals.unmetered_stale_block,
                max_stale_transaction = totals.max_stale_transaction,
                gated_changed_slots = totals.gated_changed_slots,
                gated_created_slots = totals.gated_created_slots,
                gated_cleared_slots = totals.gated_cleared_slots,
                max_contract_changed_slots = totals.max_contract_changed_slots,
                max_storage_tries = totals.max_storage_tries,
                flagged_transaction_scope = totals.flagged_transaction_scope,
                flagged_block_scope = totals.flagged_block_scope,
                transaction_limit = self.limits.transaction,
                block_limit = self.limits.block,
                cache_age_secs = self.cache_age_secs,
                "stale storage build summary"
            );
        } else {
            debug!(
                target: "payload_builder::stale_storage",
                block_number = self.block_number,
                transactions = totals.transactions,
                max_contract_changed_slots = totals.max_contract_changed_slots,
                cache_age_secs = self.cache_age_secs,
                "stale storage build summary"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicBool};

    use alloy_consensus::Receipt;
    use base_common_consensus::{BaseBlock, BasePrimitives, BaseReceipt};
    #[cfg(feature = "metrics")]
    use metrics_exporter_prometheus::PrometheusBuilder;
    use reth_execution_types::{Chain, ExecutionOutcome};
    use reth_primitives_traits::RecoveredBlock;
    use revm::{
        database::{
            AccountStatus, BundleAccount,
            states::{AccountRevert, RevertToSlot, StorageSlot, reverts::Reverts},
        },
        state::{Account, EvmStorageSlot, TransactionId},
    };
    use tokio::sync::broadcast;

    use super::*;

    const HOUR: u64 = STALE_STORAGE_EPOCH_SECONDS;
    /// A block timestamp at hour 500,000 (2027), well past the epoch modulus.
    const NOW: u64 = 500_000 * HOUR;
    const NOW_HOUR: u64 = NOW / HOUR;
    /// The base hour a cache built at `NOW` gets.
    const BASE_HOUR: u64 = NOW_HOUR - StaleStorageCache::BASE_MARGIN_HOURS;

    fn meter() -> StaleStorageMeter {
        StaleStorageMeter {
            cache: Arc::new(StaleStorageCache::with_secret(1 << 18, (11, 13), BASE_HOUR)),
        }
    }

    fn contract(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    fn word(value: u64) -> U256 {
        U256::from(value)
    }

    /// One executed transaction's state: `(contract, slot, original, present)` writes.
    fn state(slots: impl IntoIterator<Item = (Address, u64, u64, u64)>) -> EvmState {
        let mut state = EvmState::default();
        for (address, slot, original, present) in slots {
            let account = state.entry(address).or_insert_with(|| {
                let mut account = Account::default();
                account.mark_touch();
                account
            });
            account.storage.insert(
                word(slot),
                EvmStorageSlot::new_changed(word(original), word(present), TransactionId::ZERO),
            );
        }
        state
    }

    /// `count` updates of pre-existing slots `first..first + count` in `address`.
    fn updates(address: Address, first: u64, count: u64) -> EvmState {
        state((first..first + count).map(|slot| (address, slot, 1, 2)))
    }

    fn stamp_slots(
        meter: &StaleStorageMeter,
        address: Address,
        slots: std::ops::Range<u64>,
        hour: u64,
    ) {
        for slot in slots {
            meter.cache.record(meter.cache.region_key(&keccak256(address), &word(slot)), hour);
        }
    }

    fn bundle(entries: impl IntoIterator<Item = (Address, Vec<(u64, u64, u64)>)>) -> BundleState {
        let mut bundle = BundleState::default();
        for (address, slots) in entries {
            let mut account = BundleAccount {
                info: None,
                original_info: None,
                storage: Default::default(),
                status: AccountStatus::Changed,
            };
            for (slot, original, present) in slots {
                account.storage.insert(
                    word(slot),
                    StorageSlot {
                        previous_or_original_value: word(original),
                        present_value: word(present),
                    },
                );
            }
            bundle.state.insert(address, account);
        }
        bundle
    }

    fn chain(number: u64, timestamp: u64, bundle: BundleState) -> Arc<Chain<BasePrimitives>> {
        let mut block: RecoveredBlock<BaseBlock> = Default::default();
        block.set_block_number(number);
        block.set_timestamp(timestamp);
        let execution_outcome = ExecutionOutcome {
            bundle,
            receipts: vec![vec![BaseReceipt::Eip1559(Receipt::default())]],
            requests: Vec::new(),
            first_block: number,
        };
        Arc::new(Chain::new(vec![block], execution_outcome, Default::default()))
    }

    // ---------------------------------------------------------------- keyed hash

    #[test]
    fn siphash_matches_the_reference_implementation() {
        // Key 00..0f and message 00..37: the reference SipHash-2-4 output for that input.
        let key = (0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        let words: [u64; 7] = core::array::from_fn(|index| {
            let base = (index * 8) as u8;
            u64::from_le_bytes(core::array::from_fn(|byte| base + byte as u8))
        });
        assert_eq!(StaleStorageCache::siphash24(key, words), 0xb78d_bfaf_3a8d_83bd);
    }

    #[test]
    fn keyed_slot_key_separates_slots_an_unkeyed_mix_would_collapse() {
        // Slots whose last word equals a multiplicative mixing constant collide under an unkeyed
        // mix for every contract; a key absorbed before the data separates them.
        let suffix = U256::from(0xe703_7ed1_a0b4_28db_u64) << 192;
        let a = suffix | U256::from(1);
        let b = suffix | U256::from(2);
        let cache = StaleStorageCache::with_secret(1 << 12, (11, 13), BASE_HOUR);
        assert_ne!(cache.slot_key(&contract(7), &a), cache.slot_key(&contract(7), &b));
        assert_ne!(cache.slot_key(&contract(7), &a), cache.slot_key(&contract(8), &a));
        let other = StaleStorageCache::with_secret(1 << 12, (12, 13), BASE_HOUR);
        assert_ne!(cache.slot_key(&contract(7), &a), other.slot_key(&contract(7), &a));
        assert_eq!(cache.slot_key(&contract(7), &a), cache.slot_key(&contract(7), &a));
    }

    #[test]
    fn region_keys_merge_trie_siblings_only_up_to_the_chosen_depth() {
        let cache = StaleStorageCache::with_secret(1 << 12, (11, 13), BASE_HOUR);
        let (a, b) = sibling_pairs(1)[0];
        let hashed = |slot: u64| keccak256(word(slot).to_be_bytes::<32>());
        let address = keccak256(contract(7));
        let at = |slot, nibbles| cache.region_key_at_depth(&address, &hashed(slot), nibbles);
        assert_eq!(
            at(a, STALE_STORAGE_TRIE_PREFIX_NIBBLES),
            at(b, STALE_STORAGE_TRIE_PREFIX_NIBBLES)
        );
        assert_eq!(cache.region_key(&address, &word(a)), at(a, STALE_STORAGE_TRIE_PREFIX_NIBBLES));
        assert_ne!(at(a, 64), at(b, 64));
        assert_ne!(
            cache.region_key(&address, &word(a)),
            cache.region_key(&keccak256(contract(8)), &word(a))
        );
    }

    // ---------------------------------------------------------------- recency cache

    /// A key in `set` with `fingerprint`.
    fn key(set: u64, fingerprint: u64) -> u64 {
        (fingerprint << StaleStorageCache::FINGERPRINT_SHIFT) | set
    }

    /// A cache of exactly one set, so every key shares it.
    fn one_set() -> StaleStorageCache {
        StaleStorageCache::with_secret(StaleStorageCache::SET_BYTES, (1, 2), BASE_HOUR)
    }

    #[test]
    fn the_cache_rounds_its_size_down_to_a_power_of_two_sets() {
        let default_bytes = u64::from(STALE_STORAGE_CACHE_MIB) << 20;
        assert_eq!(StaleStorageCache::sets_for_size(default_bytes), 1 << 22);
        assert_eq!(StaleStorageCache::sets_for_size(100 * 32), 64);
        assert_eq!(StaleStorageCache::sets_for_size(0), 1);
        assert_eq!(StaleStorageCache::with_secret(100 * 32, (1, 2), BASE_HOUR).len(), 64 * 8);
        assert_eq!(core::mem::size_of::<StaleStorageCacheSet>(), 32);
        assert_eq!(core::mem::align_of::<StaleStorageCacheSet>(), 32);
    }

    #[test]
    fn an_empty_cache_reads_every_slot_as_stale() {
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2), BASE_HOUR);
        assert_eq!(cache.age_hours(7, NOW_HOUR), None);
        assert!(cache.is_stale(7, NOW_HOUR));
        assert_eq!(cache.age_secs(NOW), None);
        assert_eq!(cache.recorded_blocks(), 0);
    }

    #[test]
    fn a_recent_write_reads_recent_until_the_window_ends() {
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2), BASE_HOUR);
        assert_eq!(cache.record(7, NOW_HOUR), StaleStorageCacheWrite::default());
        assert_eq!(cache.age_hours(7, NOW_HOUR), Some(0));
        assert_eq!(cache.age_hours(7, NOW_HOUR + STALE_STORAGE_WINDOW_HOURS), Some(24));
        assert!(cache.is_stale(7, NOW_HOUR + STALE_STORAGE_WINDOW_HOURS + 1));
        let rewrite = cache.record(7, NOW_HOUR + 10);
        assert_eq!(
            rewrite,
            StaleStorageCacheWrite { previous_age: Some(10), evicted_unexpired: false }
        );
    }

    #[test]
    fn a_stamp_slightly_in_the_future_reads_as_recent() {
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2), BASE_HOUR);
        cache.record(4, NOW_HOUR + StaleStorageCache::FUTURE_TOLERANCE_HOURS);
        assert_eq!(cache.age_hours(4, NOW_HOUR), Some(0));
    }

    #[test]
    fn a_stamp_far_in_the_future_reads_stale() {
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2), BASE_HOUR);
        cache.record(4, NOW_HOUR + StaleStorageCache::FUTURE_TOLERANCE_HOURS + 1);
        assert!(cache.is_stale(4, NOW_HOUR));
    }

    #[test]
    fn writes_outside_the_stamp_range_are_dropped() {
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2), BASE_HOUR);
        let base = cache.base_hour();
        let last = base + StaleStorageCache::MAX_STAMP_HOURS;
        assert!(cache.can_stamp(last));
        cache.record(2, last);
        assert_eq!(cache.age_hours(2, last), Some(0));
        assert!(!cache.can_stamp(last + 1));
        cache.record(3, last + 1);
        assert!(cache.is_stale(3, last + 1));
        assert!(!cache.can_stamp(base - 1));
        cache.record(4, base - 1);
        assert!(cache.is_stale(4, base - 1));
    }

    #[test]
    fn the_occupancy_sample_counts_live_entries_and_resumes_where_it_stopped() {
        let cache = StaleStorageCache::with_secret(64 * 32, (1, 2), BASE_HOUR);
        for set in 0..64 {
            cache.record(key(set, 1), NOW_HOUR);
            cache.record(key(set, 2), NOW_HOUR + 20);
        }
        let later = NOW_HOUR + 30;
        let first = cache.sample_occupancy(later, 16);
        assert_eq!(first, StaleStorageOccupancy { visited: 128, live: 16 });
        let rest = cache.sample_occupancy(later, 48);
        assert_eq!(rest, StaleStorageOccupancy { visited: 384, live: 48 });
        // Sampling clears nothing.
        assert_eq!(cache.sample_occupancy(NOW_HOUR + 20, 64).live, 128);
    }

    #[test]
    fn fingerprint_collisions_against_full_live_sets_stay_rare() {
        const SPREAD: u64 = 0x9e37_79b9_7f4a_7c15;
        let cache = StaleStorageCache::with_secret(256 * 32, (1, 2), BASE_HOUR);
        for count in 0..256 * 8 {
            let set = count / 8;
            cache.record(key(set, count % 8 * 997 + 1), NOW_HOUR);
        }
        let probes = 200_000u64;
        let recent = (1..=probes)
            .filter(|count| !cache.is_stale(count.wrapping_mul(SPREAD), NOW_HOUR))
            .count();
        // Expected `probes * 8 / 2^16`, about 24.
        assert!(recent < 60, "{recent} false recent readings");
    }

    #[test]
    fn set_overflow_evicts_the_oldest_entry_which_then_reads_stale() {
        let cache = one_set();
        for way in 0..8 {
            let write = cache.record(key(0, way + 1), NOW_HOUR + way);
            assert!(!write.evicted_unexpired);
        }
        let write = cache.record(key(0, 9), NOW_HOUR + 8);
        assert!(write.evicted_unexpired);
        let now = NOW_HOUR + 8;
        assert!(cache.is_stale(key(0, 1), now), "the evicted oldest slot must read stale");
        for fingerprint in 2..=9 {
            assert!(!cache.is_stale(key(0, fingerprint), now));
        }
    }

    #[test]
    fn an_expired_entry_is_replaced_before_a_live_one() {
        let cache = one_set();
        cache.record(key(0, 1), NOW_HOUR);
        for fingerprint in 2..=8 {
            cache.record(key(0, fingerprint), NOW_HOUR + 30);
        }
        let write = cache.record(key(0, 9), NOW_HOUR + 30);
        assert!(!write.evicted_unexpired);
        for fingerprint in 2..=9 {
            assert!(!cache.is_stale(key(0, fingerprint), NOW_HOUR + 30));
        }
    }

    #[test]
    fn a_region_left_unwritten_for_whole_stamp_periods_reads_stale() {
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2), BASE_HOUR);
        cache.record(key(3, 7), NOW_HOUR);
        for gap in [25, 255, 256, 510, 512, 765, 768, 1_000, 65_535, 65_536] {
            assert_eq!(cache.age_hours(key(3, 7), NOW_HOUR + gap), None, "gap of {gap} hours");
        }
    }

    #[test]
    fn evicting_an_entry_from_whole_stamp_periods_ago_is_not_an_unexpired_eviction() {
        for gap in [255, 256, 510, 512] {
            let cache = one_set();
            for fingerprint in 1..=8 {
                cache.record(key(0, fingerprint), NOW_HOUR);
            }
            let write = cache.record(key(0, 9), NOW_HOUR + gap);
            assert!(!write.evicted_unexpired, "gap of {gap} hours");
            assert_eq!(write.previous_age, None);
        }
    }

    #[test]
    fn a_slot_missing_from_a_full_set_reads_stale() {
        let cache = one_set();
        for fingerprint in 1..=8 {
            cache.record(key(0, fingerprint), NOW_HOUR);
        }
        assert!(cache.is_stale(key(0, 9), NOW_HOUR));
        assert!(cache.is_stale(key(0, 0x00ff_ffff), NOW_HOUR));
        for fingerprint in 1..=8 {
            assert!(!cache.is_stale(key(0, fingerprint), NOW_HOUR));
        }
    }

    #[test]
    fn concurrent_writers_never_make_a_missing_slot_read_recent() {
        let cache = StaleStorageCache::with_secret(4 * 32, (1, 2), BASE_HOUR);
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            for writer in 0..2u64 {
                let cache = &cache;
                let stop = &stop;
                scope.spawn(move || {
                    let mut round = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        for fingerprint in 1..=64 {
                            let hour = NOW_HOUR + (round + writer) % 3;
                            cache.record(key(fingerprint % 4, fingerprint), hour);
                        }
                        round += 1;
                    }
                });
            }
            for _ in 0..2 {
                let cache = &cache;
                scope.spawn(move || {
                    for _ in 0..20_000 {
                        for fingerprint in 1_000..1_064 {
                            assert!(cache.is_stale(key(fingerprint % 4, fingerprint), NOW_HOUR));
                        }
                    }
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            stop.store(true, Ordering::Relaxed);
        });
    }

    #[test]
    fn limits_are_raised_while_the_cache_is_young() {
        let tiered = StaleStorageLimits::at_cache_age;
        assert_eq!(tiered(None), StaleStorageLimits { transaction: 450, block: 450 });
        assert_eq!(tiered(Some(3_599)), StaleStorageLimits { transaction: 450, block: 450 });
        assert_eq!(tiered(Some(3_600)), StaleStorageLimits { transaction: 300, block: 300 });
        assert_eq!(
            tiered(Some(24 * HOUR - 1)),
            StaleStorageLimits { transaction: 300, block: 300 }
        );
        assert_eq!(tiered(Some(24 * HOUR)), StaleStorageLimits { transaction: 200, block: 200 });
    }

    #[test]
    fn a_build_long_after_the_last_recording_reads_every_slot_as_stale() {
        let meter = meter();
        meter.record_bundle(
            &bundle([(contract(7), (0..100).map(|s| (s, 1, 2)).collect())]),
            1,
            NOW,
        );
        // Past the gap limit the cache has missed too many writes to be trusted.
        let later = NOW + (StaleStorageCache::MAX_RECORD_GAP_HOURS + 1) * HOUR;
        let mut tracker = meter.block_tracker(2, later);
        assert_eq!(tracker.limits().transaction, STALE_STORAGE_FIRST_HOUR_LIMIT);
        assert_eq!(tracker.evaluate(&updates(contract(7), 0, 100)).stale_transaction, 100);
        // Recording resumes by restarting the cache age; the old entries are past the window.
        let record = meter.record_bundle(
            &bundle([(contract(7), (0..100).map(|s| (s, 2, 3)).collect())]),
            1,
            later,
        );
        assert!(record.restarted_after_gap);
        assert_eq!(record.gated.stale, 100);
        assert_eq!(meter.cache.age_secs(later), Some(0));
    }

    #[test]
    fn recording_past_the_stamp_range_drops_the_writes_and_keeps_the_base() {
        let meter = meter();
        meter.record_bundle(&bundle([(contract(7), vec![(1, 1, 2)])]), 1, NOW);
        let past = (BASE_HOUR + StaleStorageCache::MAX_STAMP_HOURS + 1) * HOUR;
        let record =
            meter.record_bundle(&bundle([(contract(8), vec![(1, 1, 2), (2, 1, 2)])]), 1, past);
        assert_eq!(
            record.dropped,
            Some(StaleStorageDroppedWrites {
                reason: StaleStorageDropReason::PastStampRange,
                slots: 2
            })
        );
        assert_eq!(meter.cache.base_hour(), BASE_HOUR);
        let age = |byte, hour| {
            let key = meter.cache.region_key(&keccak256(contract(byte)), &word(1));
            meter.cache.age_hours(key, hour)
        };
        assert_eq!(age(8, past / HOUR), None);
        assert_eq!(age(7, NOW_HOUR), Some(0));
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn the_eviction_metric_counts_only_unexpired_evictions() {
        let one_set_meter = || StaleStorageMeter { cache: Arc::new(one_set()) };
        let nine_slots = |first: u64| (first..first + 9).map(|s| (s, 1, 2)).collect::<Vec<_>>();

        // Nine recent writes into one eight-way set evict one live entry.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let meter = one_set_meter();
        let record = metrics::with_local_recorder(&recorder, || {
            meter.record_notification(&CanonStateNotification::Commit {
                new: chain(1, NOW, bundle([(contract(7), nine_slots(0))])),
            })
        });
        assert_eq!(record.map(|record| record.unexpired_evictions), Some(1));
        assert!(handle.render().contains("cache_unexpired_evictions_total 1"));

        // Writes that replace expired entries do not count.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let meter = one_set_meter();
        meter.record_bundle(&bundle([(contract(7), nine_slots(0)[..8].to_vec())]), 1, NOW);
        let record = metrics::with_local_recorder(&recorder, || {
            meter.record_notification(&CanonStateNotification::Commit {
                new: chain(2, NOW + 30 * HOUR, bundle([(contract(8), vec![(1, 1, 2)])])),
            })
        });
        assert_eq!(record.map(|record| record.unexpired_evictions), Some(0));
        assert!(!handle.render().contains("cache_unexpired_evictions_total"));
        assert!(handle.render().contains("cache_writes_total 1"));
    }

    // ---------------------------------------------------------------- per-transaction meter

    #[test]
    fn nothing_counts_in_a_contract_at_or_under_the_gate() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        assert_eq!(tracker.evaluate(&updates(contract(7), 0, 64)).stale_transaction, 0);
        let sample = tracker.evaluate(&updates(contract(7), 0, 65));
        assert_eq!(sample.stale_transaction, 65);
        assert_eq!(sample.stale_block, 65);
    }

    #[test]
    fn slots_written_within_the_window_are_fresh() {
        let meter = meter();
        stamp_slots(&meter, contract(7), 0..40, NOW_HOUR - 10);
        let mut tracker = meter.block_tracker(1, NOW);
        assert_eq!(tracker.evaluate(&updates(contract(7), 0, 100)).stale_transaction, 60);
        let mut later = meter.block_tracker(1, NOW + 15 * HOUR);
        assert_eq!(later.evaluate(&updates(contract(7), 0, 100)).stale_transaction, 100);
    }

    #[test]
    fn read_only_slots_do_not_count_toward_the_gate() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        let mut state = updates(contract(7), 0, 10);
        let account = state.get_mut(&contract(7)).unwrap();
        for slot in 100..200 {
            account.storage.insert(word(slot), EvmStorageSlot::new(word(5), TransactionId::ZERO));
        }
        let sample = tracker.evaluate(&state);
        assert_eq!(sample.changed_slots, 10);
        assert_eq!(sample.stale_transaction, 0);
        assert_eq!(sample.gated_changed_slots, 0);
    }

    #[test]
    fn creates_count_toward_the_gate_but_only_pre_existing_slots_are_probed() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        let creates = (0..50).map(|slot| (contract(7), slot, 0, 1));
        let updates = (100..120).map(|slot| (contract(7), slot, 1, 2));
        let sample = tracker.evaluate(&state(creates.chain(updates)));
        assert_eq!(sample.changed_slots, 70);
        assert_eq!(sample.stale_transaction, 20);
        assert_eq!(sample.gated_created_slots, 50);
    }

    #[test]
    fn each_contract_is_gated_separately() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        let mut writes: Vec<_> = (0..70).map(|slot| (contract(7), slot, 1, 2)).collect();
        writes.extend((0..30).map(|slot| (contract(8), slot, 1, 2)));
        let sample = tracker.evaluate(&state(writes));
        assert_eq!(sample.stale_transaction, 70);
        assert_eq!(sample.top_contract, Some(contract(7)));
        assert_eq!(sample.storage_tries, 2);
        assert_eq!(sample.max_contract_changed_slots, 70);
    }

    #[test]
    fn a_batch_split_under_the_gate_counts_once_the_block_passes_it() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        let first = tracker.evaluate(&updates(contract(7), 0, 40));
        assert_eq!((first.stale_transaction, first.stale_block), (0, 0));
        tracker.commit(&TxHash::ZERO);
        let second = tracker.evaluate(&updates(contract(7), 40, 40));
        assert_eq!((second.stale_transaction, second.stale_block), (0, 40));
        tracker.commit(&TxHash::ZERO);
        assert_eq!(tracker.totals().stale_block, 40);
        // A new block starts from zero.
        let mut next = meter.block_tracker(2, NOW);
        assert_eq!(next.evaluate(&updates(contract(7), 80, 40)).stale_block, 0);
    }

    #[test]
    fn rewrites_of_the_same_slots_do_not_pass_the_block_gate() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        for _ in 0..30 {
            let sample = tracker.evaluate(&updates(contract(7), 0, 5));
            assert_eq!((sample.stale_transaction, sample.stale_block), (0, 0));
            tracker.commit(&TxHash::ZERO);
        }
        assert_eq!(tracker.totals().max_contract_changed_slots, 5);
        // Distinct slots still accumulate toward the gate.
        let sample = tracker.evaluate(&updates(contract(7), 5, 60));
        assert_eq!((sample.stale_transaction, sample.stale_block), (0, 60));
    }

    #[test]
    fn an_uncommitted_transaction_does_not_advance_the_block() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        tracker.evaluate(&updates(contract(7), 0, 40));
        tracker.evaluate(&updates(contract(7), 0, 40));
        assert_eq!(tracker.evaluate(&updates(contract(7), 100, 40)).stale_block, 0);
        tracker.commit(&TxHash::ZERO);
        assert_eq!(tracker.totals().transactions, 1);
        assert_eq!(tracker.evaluate(&updates(contract(7), 200, 30)).stale_block, 30);
    }

    #[test]
    fn transaction_scope_count_does_not_depend_on_the_rest_of_the_block() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        let wide = |first| {
            state(
                (1..=4u8)
                    .flat_map(move |c| (first..first + 60).map(move |s| (contract(c), s, 1, 2))),
            )
        };
        let sample = tracker.evaluate(&wide(0));
        assert_eq!((sample.stale_transaction, sample.stale_block), (0, 0));
        tracker.evaluate(&state(
            (1..=4u8).flat_map(|c| (1_000..1_005).map(move |s| (contract(c), s, 1, 2))),
        ));
        tracker.commit(&TxHash::ZERO);
        // The block holds 5 slots of each contract; with this transaction it is past the gate.
        let sample = tracker.evaluate(&wide(0));
        assert_eq!((sample.stale_transaction, sample.stale_block), (0, 240));
    }

    #[test]
    fn slots_created_earlier_in_the_block_are_not_stale() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        tracker.evaluate(&state((0..250).map(|slot| (contract(7), slot, 0, 1))));
        tracker.commit(&TxHash::ZERO);
        let rewrites = (0..125).map(|slot| (contract(7), slot, 1, 2));
        let clears = (125..250).map(|slot| (contract(7), slot, 1, 0));
        let sample = tracker.evaluate(&state(rewrites.chain(clears)));
        assert_eq!((sample.stale_transaction, sample.stale_block), (0, 0));
        // Clearing a slot the block created is not a clear of a pre-existing slot.
        assert_eq!(sample.gated_cleared_slots, 0);
    }

    #[test]
    fn clears_of_pre_existing_slots_in_a_gated_contract_are_counted() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        let clears = (0..40).map(|slot| (contract(7), slot, 1, 0));
        let updates = (40..80).map(|slot| (contract(7), slot, 1, 2));
        let sample = tracker.evaluate(&state(clears.chain(updates)));
        assert_eq!(sample.gated_cleared_slots, 40);
        assert_eq!(sample.stale_transaction, 80);
    }

    #[test]
    fn a_stale_slot_rewritten_in_the_block_counts_once_toward_the_block() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        let first = tracker.evaluate(&updates(contract(7), 0, 150));
        assert_eq!((first.stale_transaction, first.stale_block), (150, 150));
        tracker.commit(&TxHash::ZERO);
        let second = tracker.evaluate(&state((0..150).map(|slot| (contract(7), slot, 2, 3))));
        assert_eq!((second.stale_transaction, second.stale_block), (150, 0));
        tracker.commit(&TxHash::ZERO);
        assert_eq!(tracker.totals().stale_block, 150);
    }

    #[test]
    fn commit_reports_the_dry_run_limits_without_excluding() {
        let meter = meter();
        // An empty cache: the first-hour limit of 450 applies.
        let mut tracker = meter.block_tracker(1, NOW);
        assert_eq!(tracker.limits(), StaleStorageLimits { transaction: 450, block: 450 });
        tracker.evaluate(&updates(contract(7), 0, 300));
        let verdict = tracker.commit(&TxHash::ZERO).unwrap();
        assert!(!verdict.exceeds_transaction_limit);
        assert!(verdict.exceeds_plain_transaction_limit);
        assert!(verdict.is_flagged());
        tracker.evaluate(&updates(contract(8), 0, 200));
        let verdict = tracker.commit(&TxHash::ZERO).unwrap();
        assert_eq!(verdict.cumulative_stale_block, 300);
        assert!(verdict.exceeds_block_limit);
        assert!(!verdict.exceeds_transaction_limit);
        let totals = tracker.totals();
        assert_eq!((totals.flagged_transaction_scope, totals.flagged_block_scope), (0, 1));
        assert_eq!(totals.stale_block, 500);
        assert_eq!(tracker.commit(&TxHash::ZERO), None);
    }

    #[test]
    fn a_transaction_a_limit_would_exclude_does_not_count_toward_that_limits_block_total() {
        let meter = meter();
        // An empty cache: the tiered limit is 450 and the plain limit 200.
        let mut tracker = meter.block_tracker(1, NOW);
        tracker.evaluate(&updates(contract(7), 0, 460));
        let first = tracker.commit(&TxHash::ZERO).unwrap();
        assert!(first.exceeds_transaction_limit && first.exceeds_plain_transaction_limit);
        tracker.evaluate(&updates(contract(8), 0, 70));
        let second = tracker.commit(&TxHash::ZERO).unwrap();
        assert!(!second.is_flagged(), "the excluded first transaction must not push it over");
        tracker.evaluate(&updates(contract(9), 0, 150));
        let third = tracker.commit(&TxHash::ZERO).unwrap();
        assert!(third.exceeds_plain_block_limit, "70 + 150 admitted under the plain limit");
        assert!(!third.exceeds_block_limit, "70 + 150 admitted under the tiered limit");
        let totals = tracker.totals();
        assert_eq!(totals.stale_block, 680, "the build total counts every committed transaction");
        assert_eq!((totals.flagged_transaction_scope, totals.flagged_block_scope), (1, 1));
    }

    #[test]
    fn an_unmetered_transaction_counts_toward_the_gate_and_dedupe_but_not_the_verdicts() {
        let meter = meter();
        let mut tracker = meter.block_tracker(1, NOW);
        tracker.evaluate(&updates(contract(7), 0, 300));
        let sample = tracker.include_unmetered().unwrap();
        assert_eq!(sample.stale_block, 300);
        // A later transaction rewriting the same slots finds them already loaded.
        assert_eq!(tracker.evaluate(&updates(contract(7), 0, 300)).stale_block, 0);
        tracker.commit(&TxHash::ZERO);
        // A gated contract's stale slots are judged without the unmetered 300.
        tracker.evaluate(&updates(contract(8), 0, 150));
        let verdict = tracker.commit(&TxHash::ZERO).unwrap();
        assert!(!verdict.is_flagged());
        let totals = tracker.totals();
        assert_eq!((totals.unmetered_transactions, totals.unmetered_stale_block), (1, 300));
        assert_eq!((totals.transactions, totals.stale_block), (2, 150));
        assert_eq!(tracker.include_unmetered(), None);
    }

    // ---------------------------------------------------------------- canonical recorder

    #[test]
    fn recording_measures_age_before_stamping() {
        let meter = meter();
        let gated: Vec<_> = (0..70).map(|slot| (slot, 1, 2)).collect();
        let small = vec![(0, 1, 2), (1, 0, 5)];
        let record =
            meter.record_bundle(&bundle([(contract(7), gated), (contract(8), small)]), 1, NOW);
        assert_eq!(record.changed_slots, 72);
        assert_eq!(record.gated.stale, 70);
        assert_eq!(record.ungated.stale, 71);
        assert_eq!(record.max_contract_changed_slots, 70);
        assert_eq!(meter.cache.recorded_blocks(), 1);
        assert_eq!(meter.cache.age_secs(NOW + 60), Some(60));
        // Every changed slot is stamped, created ones included.
        let key = meter.cache.region_key(&keccak256(contract(8)), &word(1));
        assert_eq!(meter.cache.age_hours(key, NOW_HOUR + 2), Some(2));
        let again = meter.record_bundle(
            &bundle([(contract(7), (0..70).map(|s| (s, 2, 3)).collect())]),
            1,
            NOW + 3 * HOUR,
        );
        assert_eq!(again.gated.h1_6, 70);
    }

    #[test]
    fn recorded_slots_are_fresh_for_the_next_build() {
        let meter = meter();
        meter.record_bundle(
            &bundle([(contract(7), (0..100).map(|s| (s, 1, 2)).collect())]),
            1,
            NOW,
        );
        let mut tracker = meter.block_tracker(2, NOW + 2);
        assert_eq!(tracker.evaluate(&updates(contract(7), 0, 100)).stale_transaction, 0);
    }

    /// `count` disjoint pairs of distinct slots whose hashed keys share their first
    /// [`STALE_STORAGE_TRIE_PREFIX_NIBBLES`] nibbles: siblings under one storage-trie node.
    fn sibling_pairs(count: usize) -> Vec<(u64, u64)> {
        let shift = 64 - 4 * STALE_STORAGE_TRIE_PREFIX_NIBBLES;
        let mut first_by_prefix = std::collections::HashMap::new();
        let mut pairs = Vec::new();
        for slot in 0u64.. {
            let hashed = keccak256(word(slot).to_be_bytes::<32>());
            let prefix = u64::from_be_bytes(core::array::from_fn(|byte| hashed.0[byte])) >> shift;
            match first_by_prefix.remove(&prefix) {
                Some(sibling) => pairs.push((sibling, slot)),
                None => {
                    first_by_prefix.insert(prefix, slot);
                }
            }
            if pairs.len() == count {
                break;
            }
        }
        pairs
    }

    #[test]
    fn a_slot_whose_trie_sibling_was_written_recently_reads_recent() {
        let meter = meter();
        let pairs = sibling_pairs(70);
        meter.record_bundle(
            &bundle([(contract(7), pairs.iter().map(|&(written, _)| (written, 1, 2)).collect())]),
            1,
            NOW,
        );
        let mut tracker = meter.block_tracker(2, NOW + 2);
        let siblings = state(pairs.iter().map(|&(_, read)| (contract(7), read, 1, 2)));
        assert_eq!(tracker.evaluate(&siblings).stale_transaction, 0);
    }

    #[test]
    fn a_trie_sibling_in_another_contract_does_not_make_a_slot_recent() {
        let meter = meter();
        let pairs = sibling_pairs(70);
        meter.record_bundle(
            &bundle([(contract(8), pairs.iter().map(|&(written, _)| (written, 1, 2)).collect())]),
            1,
            NOW,
        );
        let mut tracker = meter.block_tracker(2, NOW + 2);
        let siblings = state(pairs.iter().map(|&(_, read)| (contract(7), read, 1, 2)));
        assert_eq!(tracker.evaluate(&siblings).stale_transaction, 70);
    }

    #[test]
    fn stale_siblings_in_one_trie_region_each_count_as_a_slot() {
        let meter = meter();
        let pairs = sibling_pairs(40);
        let both = state(
            pairs.iter().flat_map(|&(a, b)| [(contract(7), a, 1, 2), (contract(7), b, 1, 2)]),
        );
        let mut tracker = meter.block_tracker(2, NOW);
        let sample = tracker.evaluate(&both);
        assert_eq!(
            (sample.changed_slots, sample.stale_transaction, sample.stale_block),
            (80, 80, 80)
        );
        tracker.commit(&TxHash::ZERO);
        // A later transaction rewriting the same slots adds nothing to the block count.
        assert_eq!(tracker.evaluate(&both).stale_block, 0);
    }

    #[test]
    fn a_departed_region_stays_stale_while_other_slots_keep_recording() {
        let meter = meter();
        meter.record_bundle(&bundle([(contract(7), vec![(1, 1, 2)])]), 1, NOW);
        let departed = meter.cache.region_key(&keccak256(contract(7)), &word(1));
        // Other contracts keep writing every 12 hours; the departed region is never refreshed.
        let mut hour = 0;
        for gap in [255, 256, 510, 765] {
            while hour + 12 <= gap {
                hour += 12;
                meter.record_bundle(
                    &bundle([(contract(8), vec![(hour, 1, 2)])]),
                    1,
                    NOW + hour * HOUR,
                );
            }
            assert_eq!(meter.cache.age_hours(departed, NOW_HOUR + gap), None, "gap of {gap} hours");
            let mut tracker = meter.block_tracker(2, NOW + gap * HOUR);
            let rewrite = state((0..70).map(|slot| (contract(7), slot + 1, 1, 2)));
            assert_eq!(tracker.evaluate(&rewrite).stale_transaction, 70, "gap of {gap} hours");
        }
    }

    #[test]
    fn recording_classifies_trie_siblings_against_the_cache_before_the_bundle() {
        let meter = meter();
        let pairs = sibling_pairs(40);
        let forward: Vec<_> = pairs.iter().flat_map(|&(a, b)| [(a, 1, 2), (b, 1, 2)]).collect();
        let record = meter.record_bundle(&bundle([(contract(7), forward)]), 1, NOW);
        assert_eq!((record.gated.stale, record.ungated.stale), (80, 80));
        assert_eq!(record.gated.le_1h, 0);

        // A sibling written in an earlier bundle makes the slot recent, whatever the order.
        let again = meter.record_bundle(
            &bundle([(contract(7), pairs.iter().map(|&(_, b)| (b, 2, 3)).collect())]),
            1,
            NOW + HOUR,
        );
        assert_eq!((again.ungated.le_1h, again.ungated.stale), (40, 0));
    }

    #[test]
    fn a_write_on_a_replaced_branch_does_not_keep_its_region_recent() {
        let meter = meter();
        let (written, sibling) = sibling_pairs(1)[0];
        let branch = bundle([(contract(5), vec![(written, 1, 2)])]);
        meter.record_notification(&CanonStateNotification::Commit {
            new: chain(1, NOW, branch.clone()),
        });
        meter.record_notification(&CanonStateNotification::Reorg {
            old: chain(1, NOW, branch),
            new: chain(1, NOW, bundle([(contract(6), vec![(1, 1, 2)])])),
        });
        let age = |byte, slot| {
            let key = meter.cache.region_key(&keccak256(contract(byte)), &word(slot));
            meter.cache.age_hours(key, NOW_HOUR)
        };
        assert_eq!(age(5, written), None, "the replaced branch's write");
        assert_eq!(age(5, sibling), None, "a trie sibling of the replaced write");
        assert_eq!(age(6, 1), Some(0), "the replacement branch's write");
    }

    #[test]
    fn a_write_undone_later_on_a_replaced_branch_does_not_keep_its_region_recent() {
        let meter = meter();
        // The old branch writes A -> B in one block and B -> A in the next.
        let there = bundle([(contract(5), vec![(1, 1, 2)])]);
        let back = bundle([(contract(5), vec![(1, 2, 1)])]);
        meter.record_notification(&CanonStateNotification::Commit {
            new: chain(1, NOW, there.clone()),
        });
        meter.record_notification(&CanonStateNotification::Commit {
            new: chain(2, NOW, back.clone()),
        });
        // The reverted chain's net bundle shows the slot as unchanged.
        let mut reverted = there;
        reverted.state_size = reverted.state.values().map(BundleAccount::size_hint).sum();
        reverted.extend(back);
        assert!(!reverted.state[&contract(5)].storage[&word(1)].is_changed());
        meter.record_notification(&CanonStateNotification::Reorg {
            old: chain(1, NOW, reverted),
            new: chain(1, NOW, bundle([(contract(6), vec![(1, 1, 2)])])),
        });
        let key = meter.cache.region_key(&keccak256(contract(5)), &word(1));
        assert_eq!(meter.cache.age_hours(key, NOW_HOUR), None);
    }

    #[test]
    fn a_slot_listed_only_in_a_reverted_blocks_reverts_is_invalidated() {
        let meter = meter();
        meter.record_bundle(&bundle([(contract(5), vec![(1, 1, 2)])]), 1, NOW);
        let mut revert = AccountRevert::default();
        revert.storage.insert(word(1), RevertToSlot::Some(word(1)));
        let reverted = BundleState {
            reverts: Reverts::new(vec![vec![(contract(5), revert)]]),
            ..Default::default()
        };
        assert_eq!(meter.invalidate_bundle(&reverted), 1);
        let key = meter.cache.region_key(&keccak256(contract(5)), &word(1));
        assert_eq!(meter.cache.age_hours(key, NOW_HOUR), None);
    }

    #[test]
    fn an_entry_read_across_a_recording_gap_keeps_its_true_age() {
        let meter = meter();
        let key = meter.cache.region_key(&keccak256(contract(4)), &word(1));
        meter.record_bundle(&bundle([(contract(4), vec![(1, 1, 2)])]), 1, NOW);
        let fingerprint = StaleStorageCache::fingerprint(key);
        let entries = &meter.cache.set(key).entries;
        let index = entries
            .iter()
            .position(|entry| entry.load(Ordering::Relaxed) >> 16 == fingerprint)
            .unwrap();
        // A reader loads this entry, then the recorder resumes after a long gap ...
        let written = entries[index].load(Ordering::Relaxed);
        let later = NOW + (StaleStorageCache::MAX_RECORD_GAP_HOURS + 1) * HOUR;
        meter.record_bundle(&bundle([(contract(9), vec![(1, 1, 2)])]), 1, later);
        // ... and the entry, still in hand or still in the set, must decode as written at `NOW`.
        entries[index].store(written, Ordering::Relaxed);
        assert_eq!(meter.cache.age_hours(key, later / HOUR), None);
        assert_eq!(meter.cache.age_hours(key, NOW_HOUR + 1), Some(1));
    }

    #[test]
    fn recording_an_earlier_timestamp_drops_its_writes_and_keeps_the_cache() {
        let meter = meter();
        meter.record_bundle(&bundle([(contract(1), vec![(1, 1, 2)])]), 1, NOW);
        let back = NOW - (StaleStorageCache::FUTURE_TOLERANCE_HOURS + 1) * HOUR;
        meter.record_bundle(&bundle([(contract(2), vec![(1, 1, 2)])]), 1, back);
        let far_back = NOW - 1_000 * HOUR;
        meter.record_bundle(&bundle([(contract(3), vec![(1, 1, 2)])]), 1, far_back);
        let age = |byte, hour| {
            let key = meter.cache.region_key(&keccak256(contract(byte)), &word(1));
            meter.cache.age_hours(key, hour)
        };
        assert_eq!(age(1, NOW_HOUR), Some(0), "earlier writes are kept");
        assert_eq!(age(2, NOW_HOUR), None, "writes timestamped too far back are dropped");
        assert_eq!(age(2, back / HOUR), None);
        assert_eq!(age(3, far_back / HOUR), None);
        assert_eq!(age(3, NOW_HOUR), None);
        let within = NOW - StaleStorageCache::FUTURE_TOLERANCE_HOURS * HOUR;
        let record = meter.record_bundle(&bundle([(contract(4), vec![(1, 1, 2)])]), 1, within);
        assert_eq!(record.dropped, None, "a segment within the tolerance is recorded");
        assert_eq!(age(4, NOW_HOUR), Some(StaleStorageCache::FUTURE_TOLERANCE_HOURS));
    }

    #[test]
    fn dropped_segments_report_why() {
        let meter = meter();
        let reason = |timestamp| {
            meter
                .record_bundle(&bundle([(contract(1), vec![(1, 1, 2)])]), 1, timestamp)
                .dropped
                .map(|dropped| dropped.reason)
        };
        assert_eq!(reason((BASE_HOUR - 1) * HOUR), Some(StaleStorageDropReason::BeforeBaseHour));
        assert_eq!(reason(NOW), None);
        assert_eq!(reason(NOW - 3 * HOUR), Some(StaleStorageDropReason::Backward));
    }

    #[tokio::test]
    async fn the_recorder_stamps_commits_and_reorgs_and_survives_a_lag() {
        let meter = meter();
        let (sender, receiver) = broadcast::channel(1);
        let lagged = chain(1, NOW, bundle([(contract(1), vec![(1, 1, 2)])]));
        let committed = chain(2, NOW, bundle([(contract(2), vec![(1, 1, 2)])]));
        let reorged = chain(3, NOW, bundle([(contract(3), vec![(1, 1, 2)])]));
        sender.send(CanonStateNotification::Commit { new: lagged }).unwrap();
        sender.send(CanonStateNotification::Commit { new: committed }).unwrap();
        let task = tokio::spawn(meter.clone().maintain(receiver));
        tokio::task::yield_now().await;
        sender
            .send(CanonStateNotification::Reorg {
                old: chain(3, NOW, BundleState::default()),
                new: reorged,
            })
            .unwrap();
        drop(sender);
        task.await.unwrap();
        let age = |byte| {
            let key = meter.cache.region_key(&keccak256(contract(byte)), &word(1));
            meter.cache.age_hours(key, NOW_HOUR)
        };
        assert_eq!(age(1), None, "the lagged notification was dropped");
        assert_eq!(age(2), Some(0));
        assert_eq!(age(3), Some(0));
    }
}
