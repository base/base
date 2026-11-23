//! Log-only stale-storage metering for state-root throttling.
//!
//! The serial post-execution state root is slowest when a block rewrites many
//! storage slots whose trie paths are no longer in memory. A slot that no
//! canonical block wrote in the last day is a cheap proxy for that. This module
//! counts such slots per executed transaction and logs what an age-tiered limit
//! would have flagged. It never excludes a transaction.
//!
//! - [`StaleStorageCache`] is a fixed-size, lock-free, keyed 8-way set-associative
//!   cache of the hour each `(contract, slot)` was last written. It is stamped off
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
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use alloy_primitives::{
    Address, TxHash, U256,
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
/// Base Mainnet writes an estimated 10 million distinct storage slots a day. At that volume
/// about 0.04% of sets overflow within the window, and about 1.9% at twice the volume. An
/// overflow evicts the oldest entry, which then reads as stale, so an undersized cache only
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

/// Entries per cache set.
pub const STALE_STORAGE_CACHE_WAYS: usize = 8;

const _: () = assert!(
    STALE_STORAGE_WINDOW_HOURS
        + StaleStorageCache::MAX_RECORD_GAP_HOURS
        + StaleStorageCache::SWEEP_PASS_HOURS
        + StaleStorageCache::FUTURE_TOLERANCE_HOURS
        < StaleStorageCache::STAMP_PERIOD_HOURS
);

// Set-index bits stay below the fingerprint bits at the largest size, so the two are independent.
const _: () = assert!(
    StaleStorageCache::sets_for_size((STALE_STORAGE_MAX_CACHE_MIB as u64) << 20).trailing_zeros()
        <= StaleStorageCache::FINGERPRINT_SHIFT
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
    /// Entries: a 24-bit key fingerprint in the high bits and an hour stamp in the low byte.
    /// A zero stamp byte marks an empty entry.
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

/// What one incremental sweep found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaleStorageSweep {
    /// Entries visited.
    pub visited: u64,
    /// Expired entries cleared.
    pub cleared: u64,
    /// Entries still within the window.
    pub live: u64,
}

/// Keyed, 8-way set-associative cache of the hour each `(contract, slot)` was last written by a
/// canonical block.
///
/// A slot reads as recent only if its set holds an entry with the slot's 24-bit fingerprint and
/// a stamp within [`STALE_STORAGE_WINDOW_HOURS`]. Every other slot reads as stale, so errors run
/// one way: a slot that was evicted, lost a write race or was never seen over-meters. The only
/// false recent reading is a fingerprint collision, about `8 / 2^24` per probe of a full set.
/// Set index and fingerprint come from disjoint bits of a `SipHash-2-4` keyed with a per-process
/// random secret, so a sender cannot choose slots that crowd one set.
///
/// Stamps are `hour % 255 + 1` in one byte, so a stamp repeats every 255 hours. An incremental
/// sweep clears expired entries well before they could wrap and read as recent, and a
/// recording gap longer than [`Self::MAX_RECORD_GAP_HOURS`] clears the whole cache.
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
    recorded_blocks: AtomicU64,
    sweep_cursor: AtomicU64,
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
    /// Number of distinct hour stamps an entry can hold; stamp `0` marks an empty entry.
    pub const STAMP_PERIOD_HOURS: u64 = 255;
    /// Stamps at most this many hours ahead of the current hour, such as one from a canonical
    /// tip timestamped after the block being built, read as written now.
    pub const FUTURE_TOLERANCE_HOURS: u64 = 2;
    /// Recorded blocks per full sweep of the cache.
    pub const SWEEP_BLOCKS: u64 = 1 << 16;
    /// Hours a full sweep takes at two-second blocks, rounded up.
    pub const SWEEP_PASS_HOURS: u64 =
        (Self::SWEEP_BLOCKS * 2).div_ceil(STALE_STORAGE_EPOCH_SECONDS);
    /// A recording gap longer than this clears the cache, and a build this far past the last
    /// recorded hour treats the cache as empty, so no unswept stamp can wrap.
    pub const MAX_RECORD_GAP_HOURS: u64 = 96;
    /// Compare-and-swap attempts per write before giving up. A lost write leaves the slot
    /// reading stale.
    pub const WRITE_ATTEMPTS: usize = 4;
    /// Bytes per set.
    pub const SET_BYTES: u64 = 32;
    /// Bit offset of the 24-bit fingerprint in the key, above every set-index bit.
    pub const FINGERPRINT_SHIFT: u32 = 40;

    /// Creates an empty cache of about `size_bytes` keyed with a random secret.
    pub fn new(size_bytes: u64) -> Self {
        let random = RandomState::new();
        let mut first = random.build_hasher();
        first.write_u64(0);
        let mut second = random.build_hasher();
        second.write_u64(1);
        Self::with_secret(size_bytes, (first.finish(), second.finish()))
    }

    /// Creates an empty cache of about `size_bytes` keyed with `secret`.
    pub fn with_secret(size_bytes: u64, secret: (u64, u64)) -> Self {
        let sets = Self::sets_for_size(size_bytes);
        let sets: Box<[StaleStorageCacheSet]> =
            std::iter::repeat_with(StaleStorageCacheSet::default).take(sets as usize).collect();
        Self {
            set_mask: sets.len() as u64 - 1,
            sets,
            secret,
            first_recorded_timestamp: AtomicU64::new(0),
            last_recorded_hour: AtomicU64::new(0),
            recorded_blocks: AtomicU64::new(0),
            sweep_cursor: AtomicU64::new(0),
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

    /// Keyed `SipHash-2-4` of a `(contract, slot)` pair.
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

    /// The 24-bit fingerprint of `key`, from bits disjoint from the set index.
    pub const fn fingerprint(key: u64) -> u32 {
        (key >> Self::FINGERPRINT_SHIFT) as u32 & 0x00ff_ffff
    }

    /// The stored entry for `fingerprint` written in `hour`.
    pub const fn encode(fingerprint: u32, hour: u64) -> u32 {
        (fingerprint << 8) | ((hour % Self::STAMP_PERIOD_HOURS) as u32 + 1)
    }

    /// Hours from an entry's stamp to `now_hour`, modulo the stamp period; `None` if empty.
    ///
    /// A stamp up to [`Self::FUTURE_TOLERANCE_HOURS`] after `now_hour` reads as age `0` rather
    /// than wrapping to nearly the full period.
    pub const fn entry_age(entry: u32, now_hour: u64) -> Option<u64> {
        let stamp = (entry & 0xff) as u64;
        if stamp == 0 {
            return None;
        }
        let period = Self::STAMP_PERIOD_HOURS;
        let age = (now_hour % period + period - (stamp - 1)) % period;
        if age > period - 1 - Self::FUTURE_TOLERANCE_HOURS {
            return Some(0);
        }
        Some(age)
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
            .filter(|&entry| entry >> 8 == fingerprint)
            .filter_map(|entry| Self::entry_age(entry, now_hour))
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
    /// is dropped and the slot reads stale.
    pub fn record(&self, key: u64, now_hour: u64) -> StaleStorageCacheWrite {
        let fingerprint = Self::fingerprint(key);
        let written = Self::encode(fingerprint, now_hour);
        let set = self.set(key);
        let mut previous_age = None;
        for _ in 0..Self::WRITE_ATTEMPTS {
            previous_age = None;
            let mut matched = None;
            // Victim rank: empty first, then expired, then the oldest live entry.
            let mut victim = (0, 0, 0);
            for (way, entry) in set.entries.iter().enumerate() {
                let current = entry.load(Ordering::Relaxed);
                let age = Self::entry_age(current, now_hour);
                if age.is_some() && current >> 8 == fingerprint {
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

    /// Clears expired entries in up to `sets` sets, continuing from where the previous sweep
    /// stopped.
    pub fn sweep(&self, now_hour: u64, sets: u64) -> StaleStorageSweep {
        let len = self.sets.len() as u64;
        let count = sets.min(len);
        let start = self.sweep_cursor.fetch_add(count, Ordering::Relaxed) % len;
        let mut sweep = StaleStorageSweep::default();
        for offset in 0..count {
            for entry in &self.sets[((start + offset) % len) as usize].entries {
                sweep.visited += 1;
                let current = entry.load(Ordering::Relaxed);
                match Self::entry_age(current, now_hour) {
                    None => {}
                    Some(age) if age <= STALE_STORAGE_WINDOW_HOURS => sweep.live += 1,
                    // A failed exchange means a writer just refreshed or replaced the entry.
                    Some(_) => {
                        sweep.cleared += u64::from(
                            entry
                                .compare_exchange(current, 0, Ordering::Relaxed, Ordering::Relaxed)
                                .is_ok(),
                        );
                    }
                }
            }
        }
        sweep
    }

    /// Sets [`Self::sweep`] visits per recorded canonical block, so a full pass takes
    /// [`Self::SWEEP_BLOCKS`] blocks.
    pub const fn sweep_sets_per_block(&self) -> u64 {
        (self.sets.len() as u64).div_ceil(Self::SWEEP_BLOCKS)
    }

    /// Empties the cache and its history.
    pub fn clear(&self) {
        for set in &self.sets {
            for entry in &set.entries {
                entry.store(0, Ordering::Relaxed);
            }
        }
        self.first_recorded_timestamp.store(0, Ordering::Relaxed);
        self.last_recorded_hour.store(0, Ordering::Relaxed);
    }

    /// Whether a build at `now_hour` is more than [`Self::MAX_RECORD_GAP_HOURS`] past the last
    /// recorded hour. Such a build treats the cache as empty, since unswept stamps may wrap.
    pub fn is_expired_at(&self, now_hour: u64) -> bool {
        let last = self.last_recorded_hour.load(Ordering::Relaxed);
        last != 0 && now_hour > last - 1 + Self::MAX_RECORD_GAP_HOURS
    }

    /// `SipHash-2-4` of a 56-byte message given as seven little-endian words.
    pub const fn siphash24(key: (u64, u64), words: [u64; 7]) -> u64 {
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
        let last = 56_u64 << 56;
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
    /// What the sweep found.
    pub sweep: StaleStorageSweep,
    /// The cache was cleared first because recording had stopped for longer than
    /// [`StaleStorageCache::MAX_RECORD_GAP_HOURS`].
    pub cleared_after_gap: bool,
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
    pub fn record_bundle(
        &self,
        bundle: &BundleState,
        blocks: u64,
        timestamp: u64,
    ) -> CanonicalStaleRecord {
        let now_hour = timestamp / STALE_STORAGE_EPOCH_SECONDS;
        let gate = u64::from(STALE_STORAGE_CONTRACT_SLOT_GATE);
        let mut record = CanonicalStaleRecord::default();
        if self.cache.is_expired_at(now_hour) {
            self.cache.clear();
            record.cleared_after_gap = true;
        }
        for (address, account) in &bundle.state {
            let changed = account.storage.values().filter(|slot| slot.is_changed()).count() as u64;
            if changed == 0 {
                continue;
            }
            record.changed_slots += changed;
            record.max_contract_changed_slots = record.max_contract_changed_slots.max(changed);
            let gated = changed > gate;
            for (slot_id, slot) in &account.storage {
                if !slot.is_changed() {
                    continue;
                }
                let key = self.cache.slot_key(address, slot_id);
                // One set scan reads the previous age and records the write.
                let write = self.cache.record(key, now_hour);
                record.unexpired_evictions += u64::from(write.evicted_unexpired);
                if slot.previous_or_original_value.is_zero() {
                    record.gated_created_slots += u64::from(gated);
                } else {
                    record.ungated.add(write.previous_age);
                    if gated {
                        record.gated.add(write.previous_age);
                        record.gated_cleared_slots += u64::from(slot.present_value.is_zero());
                    }
                }
            }
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
        record.sweep = self
            .cache
            .sweep(now_hour, self.cache.sweep_sets_per_block().saturating_mul(blocks.max(1)));
        record
    }

    /// Records the committed segment of a canonical notification, for both `Commit` and
    /// `Reorg`. Returns `None` for a revert with no new blocks.
    ///
    /// On a reorg the reverted segment's stamps stay: they read as recent writes, which can only
    /// hide staleness.
    pub fn record_notification<N: NodePrimitives>(
        &self,
        notification: &CanonStateNotification<N>,
    ) -> Option<CanonicalStaleRecord> {
        let committed = notification.committed();
        if committed.is_empty() {
            return None;
        }
        let tip = committed.tip();
        let record = self.record_bundle(
            &committed.execution_outcome().bundle,
            committed.len() as u64,
            tip.timestamp(),
        );
        let cache_age_secs = self.cache.age_secs(tip.timestamp()).unwrap_or_default();
        StaleStorageMetrics::recorded_blocks_total().increment(committed.len() as u64);
        StaleStorageMetrics::cache_age_seconds().set(cache_age_secs as f64);
        StaleStorageMetrics::cache_writes_total().increment(record.changed_slots);
        if record.unexpired_evictions > 0 {
            StaleStorageMetrics::cache_unexpired_evictions_total()
                .increment(record.unexpired_evictions);
        }
        if record.sweep.visited > 0 {
            StaleStorageMetrics::cache_occupancy_ratio()
                .set(record.sweep.live as f64 / record.sweep.visited as f64);
        }
        if record.cleared_after_gap {
            warn!(
                target: "payload_builder::stale_storage",
                block_number = tip.number(),
                max_gap_hours = StaleStorageCache::MAX_RECORD_GAP_HOURS,
                "stale storage recording resumed after a long gap; cleared the cache"
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
}

/// Dry-run outcome of one committed transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleTxVerdict {
    /// The transaction's counts.
    pub sample: StaleTxSample,
    /// Block-scope total before this transaction.
    pub cumulative_stale_block: u64,
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
/// [`Self::commit`] once the block builder commits it. A transaction that is evaluated but not
/// committed is discarded by the next evaluation.
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
    /// Scratch `(key, created, cleared)` for the changed slots of one contract.
    contract_slots: Vec<(u64, bool, bool)>,
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
    /// old, never-recorded slot. Only contracts past a gate probe the cache.
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
            for &(key, created, cleared) in &self.contract_slots {
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
                if !self.cache_expired && !self.cache.is_stale(key, self.now_hour) {
                    continue;
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

    /// Adds the last evaluated transaction to the block and logs what the dry-run limits would
    /// have flagged. Returns `None` if nothing was evaluated since the last commit.
    pub fn commit(&mut self, tx_hash: &TxHash) -> Option<StaleTxVerdict> {
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

        let cumulative_stale_block = self.totals.stale_block;
        let stale_transaction = u64::from(sample.stale_transaction);
        let stale_block = u64::from(sample.stale_block);
        let block_total = cumulative_stale_block + stale_block;
        let verdict = StaleTxVerdict {
            sample,
            cumulative_stale_block,
            exceeds_transaction_limit: stale_transaction > self.limits.transaction,
            exceeds_block_limit: stale_block > 0 && block_total > self.limits.block,
            exceeds_plain_transaction_limit: stale_transaction > STALE_STORAGE_LIMIT,
            exceeds_plain_block_limit: stale_block > 0 && block_total > STALE_STORAGE_LIMIT,
        };

        let totals = &mut self.totals;
        totals.transactions += 1;
        totals.stale_block = block_total;
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
        database::{AccountStatus, BundleAccount, states::StorageSlot},
        state::{Account, EvmStorageSlot, TransactionId},
    };
    use tokio::sync::broadcast;

    use super::*;

    const HOUR: u64 = STALE_STORAGE_EPOCH_SECONDS;
    /// A block timestamp at hour 500,000 (2027), well past the epoch modulus.
    const NOW: u64 = 500_000 * HOUR;
    const NOW_HOUR: u64 = NOW / HOUR;

    fn meter() -> StaleStorageMeter {
        StaleStorageMeter { cache: Arc::new(StaleStorageCache::with_secret(1 << 18, (11, 13))) }
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
            meter.cache.record(meter.cache.slot_key(&address, &word(slot)), hour);
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
        let words = core::array::from_fn(|index| {
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
        let cache = StaleStorageCache::with_secret(1 << 12, (11, 13));
        assert_ne!(cache.slot_key(&contract(7), &a), cache.slot_key(&contract(7), &b));
        assert_ne!(cache.slot_key(&contract(7), &a), cache.slot_key(&contract(8), &a));
        let other = StaleStorageCache::with_secret(1 << 12, (12, 13));
        assert_ne!(cache.slot_key(&contract(7), &a), other.slot_key(&contract(7), &a));
        assert_eq!(cache.slot_key(&contract(7), &a), cache.slot_key(&contract(7), &a));
    }

    // ---------------------------------------------------------------- recency cache

    /// A key in `set` with `fingerprint`.
    fn key(set: u64, fingerprint: u64) -> u64 {
        (fingerprint << StaleStorageCache::FINGERPRINT_SHIFT) | set
    }

    /// A cache of exactly one set, so every key shares it.
    fn one_set() -> StaleStorageCache {
        StaleStorageCache::with_secret(StaleStorageCache::SET_BYTES, (1, 2))
    }

    #[test]
    fn the_cache_rounds_its_size_down_to_a_power_of_two_sets() {
        let default_bytes = u64::from(STALE_STORAGE_CACHE_MIB) << 20;
        assert_eq!(StaleStorageCache::sets_for_size(default_bytes), 1 << 22);
        assert_eq!(StaleStorageCache::sets_for_size(100 * 32), 64);
        assert_eq!(StaleStorageCache::sets_for_size(0), 1);
        assert_eq!(StaleStorageCache::with_secret(100 * 32, (1, 2)).len(), 64 * 8);
        assert_eq!(core::mem::size_of::<StaleStorageCacheSet>(), 32);
        assert_eq!(core::mem::align_of::<StaleStorageCacheSet>(), 32);
    }

    #[test]
    fn an_empty_cache_reads_every_slot_as_stale() {
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2));
        assert_eq!(cache.age_hours(7, NOW_HOUR), None);
        assert!(cache.is_stale(7, NOW_HOUR));
        assert_eq!(cache.age_secs(NOW), None);
        assert_eq!(cache.recorded_blocks(), 0);
    }

    #[test]
    fn a_recent_write_reads_recent_until_the_window_ends() {
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2));
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
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2));
        cache.record(4, NOW_HOUR + StaleStorageCache::FUTURE_TOLERANCE_HOURS);
        assert_eq!(cache.age_hours(4, NOW_HOUR), Some(0));
    }

    #[test]
    fn stamps_wrap_cleanly_and_the_sweep_clears_them_before_they_alias() {
        let cache = StaleStorageCache::with_secret(1 << 12, (1, 2));
        let period = StaleStorageCache::STAMP_PERIOD_HOURS;
        let hour = period * 7 - 3;
        cache.record(9, hour);
        assert_eq!(cache.age_hours(9, hour + 5), Some(5));
        assert!(cache.is_stale(9, hour + 30));
        // A stamp a full period old aliases to "this hour" ...
        let raw = StaleStorageCache::encode(StaleStorageCache::fingerprint(9), hour);
        assert_eq!(StaleStorageCache::entry_age(raw, hour + period), Some(0));
        // ... so the sweep clears it once it expires, and it then reads stale at the alias hour.
        let sweep = cache.sweep(hour + 25, 1 << 20);
        assert_eq!((sweep.cleared, sweep.live), (1, 0));
        assert!(cache.is_stale(9, hour + period));
    }

    #[test]
    fn the_sweep_keeps_live_entries_and_resumes_where_it_stopped() {
        let cache = StaleStorageCache::with_secret(64 * 32, (1, 2));
        for set in 0..64 {
            cache.record(key(set, 1), NOW_HOUR);
            cache.record(key(set, 2), NOW_HOUR + 20);
        }
        let later = NOW_HOUR + 30;
        let first = cache.sweep(later, 16);
        assert_eq!(first, StaleStorageSweep { visited: 128, cleared: 16, live: 16 });
        let rest = cache.sweep(later, 48);
        assert_eq!((rest.cleared, rest.live), (48, 48));
        let again = cache.sweep(later, 64);
        assert_eq!((again.cleared, again.live), (0, 64));
        assert!(cache.is_stale(key(3, 1), later));
        assert_eq!(cache.age_hours(key(3, 2), later), Some(10));
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
        let cache = StaleStorageCache::with_secret(4 * 32, (1, 2));
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
        // A full stamp period later the unswept stamps alias to the current hour.
        let later = NOW + StaleStorageCache::STAMP_PERIOD_HOURS * HOUR;
        let mut tracker = meter.block_tracker(2, later);
        assert_eq!(tracker.limits().transaction, STALE_STORAGE_FIRST_HOUR_LIMIT);
        assert_eq!(tracker.evaluate(&updates(contract(7), 0, 100)).stale_transaction, 100);
        // Recording resumes by clearing the cache, so the old stamps cannot read recent.
        let record = meter.record_bundle(
            &bundle([(contract(7), (0..100).map(|s| (s, 2, 3)).collect())]),
            1,
            later,
        );
        assert!(record.cleared_after_gap);
        assert_eq!(record.gated.stale, 100);
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
        let key = meter.cache.slot_key(&contract(8), &word(1));
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
        let age =
            |byte| meter.cache.age_hours(meter.cache.slot_key(&contract(byte), &word(1)), NOW_HOUR);
        assert_eq!(age(1), None, "the lagged notification was dropped");
        assert_eq!(age(2), Some(0));
        assert_eq!(age(3), Some(0));
    }
}
