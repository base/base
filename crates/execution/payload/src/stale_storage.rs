//! Log-only stale-storage metering for state-root throttling.
//!
//! The serial post-execution state root is slowest when a block rewrites many
//! storage slots whose trie paths are no longer in memory. A slot that no
//! canonical block wrote in the last day is a cheap proxy for that. This module
//! counts such slots per executed transaction and logs what an age-tiered limit
//! would have flagged. It never excludes a transaction.
//!
//! - [`StaleStorageTable`] is a fixed-size, lock-free table of 2-byte hourly
//!   epochs indexed by a keyed `SipHash-2-4` of `(contract, slot)`. It is stamped
//!   off the transaction loop from canonical-state notifications, on every node
//!   that runs the payload builder, so a leader handoff does not empty it. It
//!   starts empty at process start and is never saved or rebuilt.
//! - [`BlockStaleTracker`] is the per-build state the builder consults after
//!   executing each transaction.

use std::{
    collections::hash_map::RandomState,
    fmt,
    hash::{BuildHasher, Hasher},
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicU64, Ordering},
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
use thiserror::Error;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{debug, info, warn};

use crate::StaleStorageMetrics;

/// Seconds per table epoch. Stamps are hourly.
pub const STALE_STORAGE_EPOCH_SECONDS: u64 = 3_600;

/// Settings for the log-only stale-storage meter.
///
/// The meter is off unless a [`StaleStorageMeter`] is configured on
/// [`crate::config::BaseBuilderConfig::stale_storage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleStorageConfig {
    /// Base-2 logarithm of the number of 2-byte table entries. `29` is a one-gibibyte table.
    pub table_log2: u32,
    /// A pre-existing slot is stale when no canonical block wrote it within this many hours.
    pub window_hours: u64,
    /// Stale slots count only in contracts with more than this many distinct changed slots.
    ///
    /// The transaction-scope count gates on the transaction's own changed slots. The block-scope
    /// count gates on the distinct slots the block and the transaction change together, so
    /// splitting one batch over several transactions hides at most this many slots per contract,
    /// while many transactions rewriting the same few slots do not pass it. Both skip slots
    /// created earlier in the block.
    pub contract_slot_gate: u32,
    /// Dry-run per-transaction limit once the table holds a full window of history.
    pub transaction_limit: u64,
    /// Dry-run cumulative per-block limit once the table holds a full window of history.
    pub block_limit: u64,
    /// Dry-run limit while the table holds less than one hour of history.
    pub first_hour_limit: u64,
    /// Dry-run limit while the table holds less than one window of history.
    pub first_day_limit: u64,
}

impl Default for StaleStorageConfig {
    fn default() -> Self {
        Self {
            table_log2: 29,
            window_hours: 24,
            contract_slot_gate: 64,
            transaction_limit: 200,
            block_limit: 200,
            first_hour_limit: 450,
            first_day_limit: 300,
        }
    }
}

/// An invalid [`StaleStorageConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StaleStorageConfigError {
    /// The table size is outside the supported range.
    #[error(
        "stale storage table log2 {0} is outside {min}..={max}",
        min = StaleStorageConfig::MIN_TABLE_LOG2,
        max = StaleStorageConfig::MAX_TABLE_LOG2
    )]
    TableLog2(u32),
    /// The window is zero or too long for the sweep to keep epochs unambiguous.
    #[error(
        "stale storage window of {0} hours is outside 1..={max}",
        max = StaleStorageTable::SWEEP_AGE_HOURS
    )]
    Window(u64),
}

impl StaleStorageConfig {
    /// Smallest supported table, 8 kibibytes.
    pub const MIN_TABLE_LOG2: u32 = 12;
    /// Largest supported table, 2 gibibytes.
    pub const MAX_TABLE_LOG2: u32 = 30;

    /// Checks the table size and window.
    pub const fn validate(&self) -> Result<(), StaleStorageConfigError> {
        if self.table_log2 < Self::MIN_TABLE_LOG2 || self.table_log2 > Self::MAX_TABLE_LOG2 {
            return Err(StaleStorageConfigError::TableLog2(self.table_log2));
        }
        if self.window_hours == 0 || self.window_hours > StaleStorageTable::SWEEP_AGE_HOURS {
            return Err(StaleStorageConfigError::Window(self.window_hours));
        }
        Ok(())
    }

    /// Dry-run limits for a table that has recorded `table_age_secs` of history.
    ///
    /// An empty or young table reads every slot it has not seen as stale, so it behaves like a
    /// shorter window and needs a higher limit: `first_hour_limit` under one hour,
    /// `first_day_limit` under one window, then the plain limits. `None` is an empty table.
    pub fn limits_at_age(&self, table_age_secs: Option<u64>) -> StaleStorageLimits {
        let full_window_secs = self.window_hours.saturating_mul(STALE_STORAGE_EPOCH_SECONDS);
        let young_limit = match table_age_secs {
            Some(age) if age >= full_window_secs => None,
            Some(age) if age >= STALE_STORAGE_EPOCH_SECONDS => Some(self.first_day_limit),
            _ => Some(self.first_hour_limit),
        };
        StaleStorageLimits {
            transaction: young_limit
                .map_or(self.transaction_limit, |limit| limit.max(self.transaction_limit)),
            block: young_limit.map_or(self.block_limit, |limit| limit.max(self.block_limit)),
        }
    }
}

/// Dry-run stale-slot limits in effect for one build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleStorageLimits {
    /// Per-transaction limit on the transaction-scope count.
    pub transaction: u64,
    /// Limit on the block's cumulative block-scope count.
    pub block: u64,
}

/// Direct-mapped table of the hour each `(contract, slot)` was last written by a canonical block.
///
/// Entries are 2-byte epochs: `0` means never recorded, otherwise `hour % 65_535 + 1`. Two
/// slots that share an entry read as the more recent write, so a collision can only hide
/// staleness. The index is a `SipHash-2-4` keyed with a per-process random secret absorbed
/// before the data, so a sender cannot choose slots that share an entry with a hot one.
///
/// Reads and writes are relaxed atomics: builder threads read while the canonical-state task
/// writes, and a torn ordering between entries only shifts which epoch a probe sees.
pub struct StaleStorageTable {
    entries: Box<[AtomicU16]>,
    mask: u64,
    secret: (u64, u64),
    /// Timestamp of the first recorded block, or `0` while empty.
    first_recorded_timestamp: AtomicU64,
    recorded_blocks: AtomicU64,
    sweep_cursor: AtomicU64,
}

impl fmt::Debug for StaleStorageTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaleStorageTable")
            .field("entries", &self.entries.len())
            .field("first_recorded_timestamp", &self.first_recorded_timestamp)
            .field("recorded_blocks", &self.recorded_blocks)
            .finish_non_exhaustive()
    }
}

impl StaleStorageTable {
    /// Number of distinct epochs a 2-byte entry can hold.
    pub const EPOCH_MODULUS: u64 = 65_535;
    /// Entries older than this many hours are cleared by [`Self::sweep`].
    ///
    /// Half the epoch range: as long as the sweep visits every entry within the other half
    /// (about 3.7 years of hours), no entry lives long enough for its epoch to wrap and read as
    /// recent.
    pub const SWEEP_AGE_HOURS: u64 = Self::EPOCH_MODULUS / 2;
    /// Recorded blocks per full sweep of the table.
    pub const SWEEP_BLOCKS: u64 = 1 << 17;
    /// Stamps at most this many hours ahead of the current hour read as written now.
    ///
    /// Far below the gap between [`Self::SWEEP_AGE_HOURS`] plus a full sweep and the epoch
    /// range, so a swept-table age never falls in this band.
    pub const FUTURE_TOLERANCE_HOURS: u64 = 168;

    /// Creates an empty table of `2^log2` entries keyed with a random secret.
    pub fn new(log2: u32) -> Self {
        let random = RandomState::new();
        let mut first = random.build_hasher();
        first.write_u64(0);
        let mut second = random.build_hasher();
        second.write_u64(1);
        Self::with_secret(log2, (first.finish(), second.finish()))
    }

    /// Creates an empty table of `2^log2` entries keyed with `secret`.
    pub fn with_secret(log2: u32, secret: (u64, u64)) -> Self {
        let entries = std::iter::repeat_with(|| AtomicU16::new(0)).take(1usize << log2).collect();
        Self {
            entries,
            mask: (1u64 << log2) - 1,
            secret,
            first_recorded_timestamp: AtomicU64::new(0),
            recorded_blocks: AtomicU64::new(0),
            sweep_cursor: AtomicU64::new(0),
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `false`: a table always has at least `2^MIN_TABLE_LOG2` entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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

    /// Hours since the entry for `key` was stamped, or `None` if it was never stamped.
    pub fn age_hours(&self, key: u64, now_hour: u64) -> Option<u64> {
        Self::epoch_age(self.entry(key).load(Ordering::Relaxed), now_hour)
    }

    /// Whether `key` was not stamped within `window_hours` of `now_hour`.
    pub fn is_stale(&self, key: u64, now_hour: u64, window_hours: u64) -> bool {
        self.age_hours(key, now_hour).is_none_or(|age| age > window_hours)
    }

    /// Records that `key` was written in `now_hour`.
    pub fn stamp(&self, key: u64, now_hour: u64) {
        self.entry(key).store(Self::encode_epoch(now_hour), Ordering::Relaxed);
    }

    /// Clears up to `count` entries older than [`Self::SWEEP_AGE_HOURS`], continuing from where
    /// the previous sweep stopped. Returns the number of entries cleared.
    pub fn sweep(&self, now_hour: u64, count: u64) -> u64 {
        let len = self.entries.len() as u64;
        let count = count.min(len);
        let start = self.sweep_cursor.fetch_add(count, Ordering::Relaxed) % len;
        let mut cleared = 0;
        for offset in 0..count {
            let entry = &self.entries[((start + offset) % len) as usize];
            let raw = entry.load(Ordering::Relaxed);
            if Self::epoch_age(raw, now_hour).is_some_and(|age| age > Self::SWEEP_AGE_HOURS) {
                entry.store(0, Ordering::Relaxed);
                cleared += 1;
            }
        }
        cleared
    }

    /// Entries [`Self::sweep`] visits per recorded canonical block, so a full pass takes
    /// [`Self::SWEEP_BLOCKS`] blocks.
    pub fn sweep_entries_per_block(&self) -> u64 {
        (self.entries.len() as u64).div_ceil(Self::SWEEP_BLOCKS)
    }

    /// The entry `key` maps to.
    pub fn entry(&self, key: u64) -> &AtomicU16 {
        &self.entries[(key & self.mask) as usize]
    }

    /// The stored form of `hour`: `hour % EPOCH_MODULUS + 1`, leaving `0` for never written.
    pub const fn encode_epoch(hour: u64) -> u16 {
        (hour % Self::EPOCH_MODULUS) as u16 + 1
    }

    /// Hours from a stored epoch to `now_hour`, modulo the epoch range; `None` for `0`.
    ///
    /// A stamp up to [`Self::FUTURE_TOLERANCE_HOURS`] after `now_hour`, such as one from a
    /// canonical tip timestamped after the block being built, reads as age `0` rather than
    /// wrapping to nearly the full epoch range.
    pub const fn epoch_age(raw: u16, now_hour: u64) -> Option<u64> {
        if raw == 0 {
            return None;
        }
        let stamped = raw as u64 - 1;
        let age =
            (now_hour % Self::EPOCH_MODULUS + Self::EPOCH_MODULUS - stamped) % Self::EPOCH_MODULUS;
        if age > Self::EPOCH_MODULUS - Self::FUTURE_TOLERANCE_HOURS {
            return Some(0);
        }
        Some(age)
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

    /// Seconds of history the table holds at `now_timestamp`, or `None` while it is empty.
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
    /// Written 25 hours to 7 days ago.
    pub d1_7: u64,
    /// Written more than 7 days ago.
    pub gt_7d: u64,
    /// Never recorded since the table started.
    pub never: u64,
}

impl StaleAgeBuckets {
    /// Metric label for each bucket, in field order.
    pub const LABELS: [&'static str; 6] = ["le_1h", "1h_6h", "6h_24h", "1d_7d", "gt_7d", "never"];

    /// Adds one slot of the given age.
    pub const fn add(&mut self, age_hours: Option<u64>) {
        match age_hours {
            None => self.never += 1,
            Some(0..=1) => self.le_1h += 1,
            Some(2..=6) => self.h1_6 += 1,
            Some(7..=24) => self.h6_24 += 1,
            Some(25..=168) => self.d1_7 += 1,
            Some(_) => self.gt_7d += 1,
        }
    }

    /// Counts in [`Self::LABELS`] order.
    pub const fn counts(&self) -> [u64; 6] {
        [self.le_1h, self.h1_6, self.h6_24, self.d1_7, self.gt_7d, self.never]
    }

    /// Slots last written more than 24 hours ago or never recorded. The buckets are fixed, so
    /// this ignores [`StaleStorageConfig::window_hours`].
    pub const fn older_than_day(&self) -> u64 {
        self.d1_7 + self.gt_7d + self.never
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
    /// Entries the sweep cleared.
    pub swept_entries: u64,
}

/// The table and settings shared by the canonical-state recorder and the payload builder.
#[derive(Debug, Clone)]
pub struct StaleStorageMeter {
    /// Meter settings.
    pub config: StaleStorageConfig,
    /// Shared epoch table.
    pub table: Arc<StaleStorageTable>,
}

impl StaleStorageMeter {
    /// Validates `config` and allocates an empty table.
    pub fn new(config: StaleStorageConfig) -> Result<Self, StaleStorageConfigError> {
        config.validate()?;
        Ok(Self { config, table: Arc::new(StaleStorageTable::new(config.table_log2)) })
    }

    /// Starts per-build tracking for a block at `block_timestamp`.
    pub fn block_tracker(&self, block_number: u64, block_timestamp: u64) -> BlockStaleTracker {
        let table_age_secs = self.table.age_secs(block_timestamp);
        BlockStaleTracker {
            table: Arc::clone(&self.table),
            config: self.config,
            block_number,
            now_hour: block_timestamp / STALE_STORAGE_EPOCH_SECONDS,
            table_age_secs,
            limits: self.config.limits_at_age(table_age_secs),
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
        let gate = u64::from(self.config.contract_slot_gate);
        let mut record = CanonicalStaleRecord::default();
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
                let key = self.table.slot_key(address, slot_id);
                if slot.previous_or_original_value.is_zero() {
                    record.gated_created_slots += u64::from(gated);
                } else {
                    // Read the age before stamping: the same cache line, so the age costs no
                    // extra memory access.
                    let age = self.table.age_hours(key, now_hour);
                    record.ungated.add(age);
                    if gated {
                        record.gated.add(age);
                        record.gated_cleared_slots += u64::from(slot.present_value.is_zero());
                    }
                }
                self.table.stamp(key, now_hour);
            }
        }
        // Timestamps are never zero on a real chain; `max(1)` keeps `0` meaning empty.
        let _ = self.table.first_recorded_timestamp.compare_exchange(
            0,
            timestamp.max(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        self.table.recorded_blocks.fetch_add(blocks, Ordering::Relaxed);
        record.swept_entries = self
            .table
            .sweep(now_hour, self.table.sweep_entries_per_block().saturating_mul(blocks.max(1)));
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
        let table_age_secs = self.table.age_secs(tip.timestamp()).unwrap_or_default();
        StaleStorageMetrics::recorded_blocks_total().increment(committed.len() as u64);
        StaleStorageMetrics::table_age_seconds().set(table_age_secs as f64);
        for (gate, buckets) in [("ungated", &record.ungated), ("gated", &record.gated)] {
            for (label, count) in StaleAgeBuckets::LABELS.into_iter().zip(buckets.counts()) {
                if count > 0 {
                    StaleStorageMetrics::canonical_stale_slots_total(gate, label).increment(count);
                }
            }
        }
        let gated_stale = record.gated.older_than_day();
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
                gated_stale_1d_7d = record.gated.d1_7,
                gated_stale_gt_7d = record.gated.gt_7d,
                gated_stale_never = record.gated.never,
                ungated_stale_gt_1d = record.ungated.older_than_day(),
                gated_created_slots = record.gated_created_slots,
                gated_cleared_slots = record.gated_cleared_slots,
                max_contract_changed_slots = record.max_contract_changed_slots,
                table_age_secs,
                "recorded canonical block storage staleness"
            );
        } else {
            debug!(
                target: "payload_builder::stale_storage",
                block_number = tip.number(),
                blocks = committed.len(),
                changed_slots = record.changed_slots,
                ungated_stale_gt_1d = record.ungated.older_than_day(),
                max_contract_changed_slots = record.max_contract_changed_slots,
                table_age_secs,
                "recorded canonical block storage staleness"
            );
        }
        Some(record)
    }

    /// Stamps the table from canonical-state notifications until the stream ends.
    ///
    /// A lagged stream leaves a hole: only slots written in the missed blocks and nowhere else
    /// read stale. The table is kept rather than reset.
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
                        "stale storage canonical stream lagged; keeping existing table"
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
    table: Arc<StaleStorageTable>,
    config: StaleStorageConfig,
    block_number: u64,
    now_hour: u64,
    table_age_secs: Option<u64>,
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
    /// Dry-run limits for this build, from the table's age when the build started.
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
    /// old, never-recorded slot. Only contracts past a gate probe the table.
    pub fn evaluate(&mut self, state: &EvmState) -> StaleTxSample {
        self.pending_contracts.clear();
        self.pending_changed_keys.clear();
        self.pending_created_keys.clear();
        self.pending_stale_keys.clear();
        let gate = self.config.contract_slot_gate;
        let mut sample = StaleTxSample::default();
        let mut top_contract_stale = 0;
        for (address, account) in state {
            self.contract_slots.clear();
            let mut new_changed = 0u32;
            for (slot_id, slot) in &account.storage {
                if !slot.is_changed() {
                    continue;
                }
                let key = self.table.slot_key(address, slot_id);
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
                if !self.table.is_stale(key, self.now_hour, self.config.window_hours) {
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
            exceeds_plain_transaction_limit: stale_transaction > self.config.transaction_limit,
            exceeds_plain_block_limit: stale_block > 0 && block_total > self.config.block_limit,
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
            table_age_secs = self.table_age_secs,
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
                table_age_secs = self.table_age_secs,
                "stale storage build summary"
            );
        } else {
            debug!(
                target: "payload_builder::stale_storage",
                block_number = self.block_number,
                transactions = totals.transactions,
                max_contract_changed_slots = totals.max_contract_changed_slots,
                table_age_secs = self.table_age_secs,
                "stale storage build summary"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Receipt;
    use base_common_consensus::{BaseBlock, BasePrimitives, BaseReceipt};
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

    fn config() -> StaleStorageConfig {
        StaleStorageConfig { table_log2: 16, ..StaleStorageConfig::default() }
    }

    fn meter() -> StaleStorageMeter {
        StaleStorageMeter {
            config: config(),
            table: Arc::new(StaleStorageTable::with_secret(16, (11, 13))),
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
            meter.table.stamp(meter.table.slot_key(&address, &word(slot)), hour);
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
        assert_eq!(StaleStorageTable::siphash24(key, words), 0xb78d_bfaf_3a8d_83bd);
    }

    #[test]
    fn keyed_slot_key_separates_slots_an_unkeyed_mix_would_collapse() {
        // Slots whose last word equals a multiplicative mixing constant collide under an unkeyed
        // mix for every contract; a key absorbed before the data separates them.
        let suffix = U256::from(0xe703_7ed1_a0b4_28db_u64) << 192;
        let a = suffix | U256::from(1);
        let b = suffix | U256::from(2);
        let table = StaleStorageTable::with_secret(12, (11, 13));
        assert_ne!(table.slot_key(&contract(7), &a), table.slot_key(&contract(7), &b));
        assert_ne!(table.slot_key(&contract(7), &a), table.slot_key(&contract(8), &a));
        let other = StaleStorageTable::with_secret(12, (12, 13));
        assert_ne!(table.slot_key(&contract(7), &a), other.slot_key(&contract(7), &a));
        assert_eq!(table.slot_key(&contract(7), &a), table.slot_key(&contract(7), &a));
    }

    // ---------------------------------------------------------------- epochs

    #[test]
    fn an_empty_table_reads_every_slot_as_never_written() {
        let table = StaleStorageTable::with_secret(12, (1, 2));
        assert_eq!(table.age_hours(7, NOW_HOUR), None);
        assert!(table.is_stale(7, NOW_HOUR, 24));
        assert_eq!(table.age_secs(NOW), None);
        assert_eq!(table.recorded_blocks(), 0);
    }

    #[test]
    fn an_entry_is_stale_only_after_the_window() {
        let table = StaleStorageTable::with_secret(12, (1, 2));
        table.stamp(7, NOW_HOUR);
        assert_eq!(table.age_hours(7, NOW_HOUR), Some(0));
        assert!(!table.is_stale(7, NOW_HOUR + 24, 24));
        assert!(table.is_stale(7, NOW_HOUR + 25, 24));
        // 300 hours old stays stale: a one-byte epoch would alias it to 45 hours.
        assert_eq!(table.age_hours(7, NOW_HOUR + 300), Some(300));
    }

    #[test]
    fn epochs_wrap_cleanly_across_the_modulus() {
        let table = StaleStorageTable::with_secret(12, (1, 2));
        let hour = StaleStorageTable::EPOCH_MODULUS * 7 - 3;
        table.stamp(9, hour);
        assert_eq!(table.age_hours(9, hour + 5), Some(5));
        assert_eq!(table.age_hours(9, hour + 1_000), Some(1_000));
    }

    #[test]
    fn a_stamp_slightly_in_the_future_reads_as_fresh() {
        let table = StaleStorageTable::with_secret(12, (1, 2));
        table.stamp(4, NOW_HOUR + 2);
        assert_eq!(table.age_hours(4, NOW_HOUR), Some(0));
        assert!(!table.is_stale(4, NOW_HOUR, 24));
    }

    #[test]
    fn colliding_slots_read_as_the_more_recent_write() {
        let table = StaleStorageTable::with_secret(12, (1, 2));
        table.stamp(3, NOW_HOUR);
        assert!(!table.is_stale(3 + (1 << 12), NOW_HOUR + 10, 24));
    }

    #[test]
    fn the_sweep_clears_entries_before_their_epoch_can_wrap() {
        let table = StaleStorageTable::with_secret(12, (1, 2));
        table.stamp(1, NOW_HOUR);
        table.stamp(2, NOW_HOUR + 40_000);
        // Without a sweep, an entry a full modulus old reads as written this hour.
        let wrapped = NOW_HOUR + StaleStorageTable::EPOCH_MODULUS;
        assert_eq!(table.age_hours(1, wrapped), Some(0));
        // A sweep past half the modulus clears the old entry and keeps the recent one.
        let cleared = table.sweep(NOW_HOUR + 40_001, table.len() as u64);
        assert_eq!(cleared, 1);
        assert_eq!(table.age_hours(1, wrapped), None);
        assert_eq!(table.age_hours(2, NOW_HOUR + 40_001), Some(1));
    }

    #[test]
    fn the_sweep_resumes_where_it_stopped() {
        let table = StaleStorageTable::with_secret(12, (1, 2));
        for key in 0..table.len() as u64 {
            table.stamp(key, NOW_HOUR);
        }
        let later = NOW_HOUR + StaleStorageTable::SWEEP_AGE_HOURS + 1;
        assert_eq!(table.sweep(later, 1_000), 1_000);
        assert_eq!(table.age_hours(999, later), None);
        assert_eq!(table.age_hours(1_000, later), Some(StaleStorageTable::SWEEP_AGE_HOURS + 1));
        assert_eq!(table.sweep(later, table.len() as u64), table.len() as u64 - 1_000);
    }

    #[test]
    fn config_validation_rejects_bad_sizes_and_windows() {
        assert!(StaleStorageConfig::default().validate().is_ok());
        let small = StaleStorageConfig { table_log2: 11, ..StaleStorageConfig::default() };
        assert_eq!(small.validate(), Err(StaleStorageConfigError::TableLog2(11)));
        let zero = StaleStorageConfig { window_hours: 0, ..StaleStorageConfig::default() };
        assert_eq!(zero.validate(), Err(StaleStorageConfigError::Window(0)));
    }

    #[test]
    fn limits_are_raised_while_the_table_is_young() {
        let config = StaleStorageConfig::default();
        let tiered = |age| config.limits_at_age(age);
        assert_eq!(tiered(None), StaleStorageLimits { transaction: 450, block: 450 });
        assert_eq!(tiered(Some(3_599)), StaleStorageLimits { transaction: 450, block: 450 });
        assert_eq!(tiered(Some(3_600)), StaleStorageLimits { transaction: 300, block: 300 });
        assert_eq!(
            tiered(Some(24 * HOUR - 1)),
            StaleStorageLimits { transaction: 300, block: 300 }
        );
        assert_eq!(tiered(Some(24 * HOUR)), StaleStorageLimits { transaction: 200, block: 200 });
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
        // An empty table: the first-hour limit of 450 applies.
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
        assert_eq!(record.gated.never, 70);
        assert_eq!(record.ungated.never, 71);
        assert_eq!(record.max_contract_changed_slots, 70);
        assert_eq!(meter.table.recorded_blocks(), 1);
        assert_eq!(meter.table.age_secs(NOW + 60), Some(60));
        // Every changed slot is stamped, created ones included.
        let key = meter.table.slot_key(&contract(8), &word(1));
        assert_eq!(meter.table.age_hours(key, NOW_HOUR + 2), Some(2));
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
            |byte| meter.table.age_hours(meter.table.slot_key(&contract(byte), &word(1)), NOW_HOUR);
        assert_eq!(age(1), None, "the lagged notification was dropped");
        assert_eq!(age(2), Some(0));
        assert_eq!(age(3), Some(0));
    }
}
