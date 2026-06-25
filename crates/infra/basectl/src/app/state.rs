use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use alloy_primitives::Bytes;
use chrono::{DateTime, Local};

use crate::rpc::{L1BlockInfo, TxSummary};

/// Size of a single blob in bytes (128 `KiB`).
pub const BLOB_SIZE: u64 = 128 * 1024;
/// Maximum number of entries retained in history buffers.
pub const MAX_HISTORY: usize = 1000;

/// Timeout for terminal event polling.
pub const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(100);
/// Rate calculation window of 30 seconds.
pub const RATE_WINDOW_30S: Duration = Duration::from_secs(30);
/// Rate calculation window of 2 minutes.
pub const RATE_WINDOW_2M: Duration = Duration::from_secs(120);
/// Rate calculation window of 5 minutes.
pub const RATE_WINDOW_5M: Duration = Duration::from_secs(300);
/// Number of recent L1 blocks used for blob share and target usage calculations.
pub const L1_BLOCK_WINDOW: usize = 10;

/// A single flashblock entry displayed in the TUI.
#[derive(Clone, Debug)]
pub struct FlashblockEntry {
    /// L2 block number.
    pub block_number: u64,
    /// Flashblock index within the block.
    pub index: u64,
    /// Number of transactions in this flashblock.
    pub tx_count: usize,
    /// Cumulative gas used up to this flashblock.
    pub gas_used: u64,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Base fee per gas in wei, if available.
    pub base_fee: Option<u128>,
    /// Previous block's base fee for delta display.
    pub prev_base_fee: Option<u128>,
    /// Local timestamp when this flashblock was received.
    pub timestamp: DateTime<Local>,
    /// Time difference in milliseconds from the previous flashblock.
    pub time_diff_ms: Option<i64>,
    /// Raw EIP-2718 encoded transaction bytes, decoded lazily on demand.
    pub raw_txs: Vec<Bytes>,
}

impl FlashblockEntry {
    /// Decodes the raw transaction bytes into summaries on demand.
    ///
    /// This avoids the expensive k256 ECDSA signer recovery on the hot path.
    pub fn decode_txs(&self) -> Vec<TxSummary> {
        crate::rpc::decode_flashblock_transactions(
            &self.raw_txs,
            self.base_fee.and_then(|f| u64::try_from(f).ok()),
        )
    }
}

/// An L2 block's data availability contribution.
#[derive(Clone, Debug)]
pub struct BlockContribution {
    /// L2 block number.
    pub block_number: u64,
    /// DA bytes contributed by this block.
    pub da_bytes: u64,
    /// Unix timestamp of the block.
    pub timestamp: u64,
    /// Total transaction count accumulated from flashblocks.
    pub tx_count: usize,
}

impl BlockContribution {
    /// Returns the age of this block in seconds since its timestamp.
    pub fn age_seconds(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(self.timestamp)
    }
}

/// An L1 block with blob and attribution data.
#[derive(Clone, Debug)]
pub struct L1Block {
    /// L1 block number.
    pub block_number: u64,
    /// Unix timestamp of the L1 block.
    pub timestamp: u64,
    /// Total number of blobs in this L1 block.
    pub total_blobs: u64,
    /// Number of blobs submitted by the Base batcher.
    pub base_blobs: u64,
    /// Number of L2 blocks attributed to this L1 block.
    pub l2_blocks_submitted: Option<u64>,
    /// Total DA bytes from L2 blocks attributed to this L1 block.
    pub l2_da_bytes: Option<u64>,
    /// Range of L2 block numbers attributed to this L1 block.
    pub l2_block_range: Option<(u64, u64)>,
}

impl L1Block {
    /// Creates a new `L1Block` from raw L1 block info.
    pub const fn from_info(info: L1BlockInfo) -> Self {
        Self {
            block_number: info.block_number,
            timestamp: info.timestamp,
            total_blobs: info.total_blobs,
            base_blobs: info.base_blobs,
            l2_blocks_submitted: None,
            l2_da_bytes: None,
            l2_block_range: None,
        }
    }

    /// Returns true if this L1 block contains any blobs.
    pub const fn has_blobs(&self) -> bool {
        self.total_blobs > 0
    }

    /// Returns true if this L1 block contains blobs from the Base batcher.
    pub const fn has_base_blobs(&self) -> bool {
        self.base_blobs > 0
    }

    /// Returns a formatted string of base/total blob counts.
    pub fn blobs_display(&self) -> String {
        format!("{}/{}", self.base_blobs, self.total_blobs)
    }

    /// Returns the number of attributed L2 blocks as a display string.
    pub fn l2_blocks_display(&self) -> String {
        self.l2_blocks_submitted.map_or_else(|| "-".to_string(), |n| n.to_string())
    }

    /// Returns the DA-to-L1 compression ratio, if data is available.
    pub fn compression_ratio(&self) -> Option<f64> {
        let da_bytes = self.l2_da_bytes?;
        if self.base_blobs == 0 {
            return None;
        }
        let l1_bytes = self.base_blobs * BLOB_SIZE;
        Some(da_bytes as f64 / l1_bytes as f64)
    }

    /// Returns the compression ratio as a formatted display string.
    pub fn compression_display(&self) -> String {
        self.compression_ratio().map_or_else(|| "-".to_string(), |r| format!("{r:.2}x"))
    }

    /// Returns the age of this L1 block in seconds.
    pub fn age_seconds(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(self.timestamp)
    }
}

/// Filter mode for the L1 blocks table display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum L1BlockFilter {
    /// Show all L1 blocks.
    #[default]
    All,
    /// Show only L1 blocks containing blobs.
    WithBlobs,
    /// Show only L1 blocks containing Base batcher blobs.
    WithBaseBlobs,
}

impl L1BlockFilter {
    /// Returns the next filter in the cycle.
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::WithBlobs,
            Self::WithBlobs => Self::WithBaseBlobs,
            Self::WithBaseBlobs => Self::All,
        }
    }

    /// Returns a short label for this filter mode.
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::WithBlobs => "Blobs",
            Self::WithBaseBlobs => "Base",
        }
    }
}

/// Tracks byte rate samples over a sliding time window.
#[derive(Debug)]
pub struct RateTracker {
    samples: VecDeque<(Instant, u64)>,
}

impl Default for RateTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RateTracker {
    /// Creates a new rate tracker with an empty sample buffer.
    pub fn new() -> Self {
        Self { samples: VecDeque::with_capacity(300) }
    }

    /// Records a byte count sample at the current instant.
    pub fn add_sample(&mut self, bytes: u64) {
        let now = Instant::now();
        self.samples.push_back((now, bytes));
        let cutoff = now - Duration::from_secs(300);
        while self.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            self.samples.pop_front();
        }
    }

    /// Computes the byte rate (bytes/sec) over the given duration window.
    pub fn rate_over(&self, duration: Duration) -> Option<f64> {
        let now = Instant::now();
        let cutoff = now - duration;

        let (count, total, earliest) = self.samples.iter().filter(|(t, _)| *t >= cutoff).fold(
            (0usize, 0u64, None::<Instant>),
            |(count, total, earliest), (t, b)| {
                (count + 1, total + b, Some(earliest.map_or(*t, |e: Instant| e.min(*t))))
            },
        );

        if count < 2 {
            return None;
        }

        let elapsed = now.duration_since(earliest?).as_secs_f64();
        if elapsed <= 0.0 {
            return None;
        }

        Some(total as f64 / elapsed)
    }
}

/// Progress state during initial backlog loading.
#[derive(Debug)]
pub struct LoadingState {
    /// Number of blocks fetched so far.
    pub current_block: u64,
    /// Total number of blocks to fetch.
    pub total_blocks: u64,
}

/// Tracks DA backlog state, L2 block contributions, and L1 blob data.
#[derive(Debug)]
pub struct DaTracker {
    /// Latest safe L2 block number.
    pub safe_l2_block: u64,
    /// Total DA bytes in the backlog (unsafe minus safe).
    pub da_backlog_bytes: u64,
    /// Per-block DA byte contributions, newest first.
    pub block_contributions: VecDeque<BlockContribution>,
    /// Recent L1 blocks with blob information, newest first.
    pub l1_blocks: VecDeque<L1Block>,
    /// Tracks DA growth rate (bytes added from new L2 blocks).
    pub growth_tracker: RateTracker,
    /// Tracks DA burn rate (bytes consumed when blocks become safe).
    pub burn_tracker: RateTracker,
    /// Timestamp of the last L1 block containing Base blobs.
    pub last_base_blob_time: Option<Instant>,
    /// Safe L2 block at the time of last L1->L2 attribution.
    ///
    /// Used to compute the delta of L2 blocks to attribute to the next L1 blob block.
    last_attributed_safe_l2: u64,
}

impl Default for DaTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DaTracker {
    /// Creates a new empty DA tracker.
    pub fn new() -> Self {
        Self {
            safe_l2_block: 0,
            da_backlog_bytes: 0,
            block_contributions: VecDeque::with_capacity(MAX_HISTORY),
            l1_blocks: VecDeque::with_capacity(MAX_HISTORY),
            growth_tracker: RateTracker::new(),
            burn_tracker: RateTracker::new(),
            last_base_blob_time: None,
            last_attributed_safe_l2: 0,
        }
    }

    /// Sets the initial backlog state from the safe block and total DA bytes.
    pub const fn set_initial_backlog(&mut self, safe_block: u64, da_bytes: u64) {
        self.safe_l2_block = safe_block;
        self.da_backlog_bytes = da_bytes;
        self.last_attributed_safe_l2 = safe_block;
    }

    /// Adds a block from the initial backlog fetch.
    pub fn add_backlog_block(&mut self, block_number: u64, da_bytes: u64, timestamp: u64) {
        let contribution = BlockContribution { block_number, da_bytes, timestamp, tx_count: 0 };
        self.block_contributions.push_front(contribution);
        if self.block_contributions.len() > MAX_HISTORY {
            self.block_contributions.pop_back();
        }
    }

    /// Records a new L2 block and adds its DA bytes to the backlog.
    pub fn add_block(&mut self, block_number: u64, da_bytes: u64, timestamp: u64) {
        if block_number <= self.safe_l2_block {
            return;
        }

        self.da_backlog_bytes = self.da_backlog_bytes.saturating_add(da_bytes);
        self.growth_tracker.add_sample(da_bytes);

        let contribution = BlockContribution { block_number, da_bytes, timestamp, tx_count: 0 };
        self.block_contributions.push_front(contribution);
        if self.block_contributions.len() > MAX_HISTORY {
            self.block_contributions.pop_back();
        }
    }

    /// Updates an existing block's DA bytes with accurate data from a full fetch.
    pub fn update_block_info(&mut self, block_number: u64, accurate_da_bytes: u64, timestamp: u64) {
        for contrib in &mut self.block_contributions {
            if contrib.block_number == block_number {
                let diff = accurate_da_bytes as i64 - contrib.da_bytes as i64;
                contrib.da_bytes = accurate_da_bytes;
                contrib.timestamp = timestamp;

                if block_number > self.safe_l2_block {
                    if diff > 0 {
                        self.da_backlog_bytes = self.da_backlog_bytes.saturating_add(diff as u64);
                    } else {
                        self.da_backlog_bytes =
                            self.da_backlog_bytes.saturating_sub((-diff) as u64);
                    }
                }
                return;
            }
        }

        // Block not found - insert it in sorted position (gap fill)
        let contribution =
            BlockContribution { block_number, da_bytes: accurate_da_bytes, timestamp, tx_count: 0 };

        if block_number > self.safe_l2_block {
            self.da_backlog_bytes = self.da_backlog_bytes.saturating_add(accurate_da_bytes);
        }

        let insert_pos = self
            .block_contributions
            .iter()
            .position(|c| c.block_number < block_number)
            .unwrap_or(self.block_contributions.len());
        self.block_contributions.insert(insert_pos, contribution);

        if self.block_contributions.len() > MAX_HISTORY {
            self.block_contributions.pop_back();
        }
    }

    /// Updates the safe head and subtracts newly safe block bytes from the backlog.
    pub fn update_safe_head(&mut self, safe_block: u64) {
        if safe_block <= self.safe_l2_block {
            return;
        }

        let old_safe = self.safe_l2_block;
        self.safe_l2_block = safe_block;

        let submitted_bytes: u64 = self
            .block_contributions
            .iter()
            .filter(|c| c.block_number > old_safe && c.block_number <= safe_block)
            .map(|c| c.da_bytes)
            .sum();

        self.da_backlog_bytes = self.da_backlog_bytes.saturating_sub(submitted_bytes);
        self.burn_tracker.add_sample(submitted_bytes);

        self.try_attribute_l2_to_l1();
    }

    /// Records a new L1 block and attempts to attribute L2 blocks to it.
    pub fn record_l1_block(&mut self, info: L1BlockInfo) {
        if self.l1_blocks.iter().any(|b| b.block_number == info.block_number) {
            return;
        }

        let l1_block = L1Block::from_info(info);

        if l1_block.base_blobs > 0 {
            self.last_base_blob_time = Some(Instant::now());
        }

        self.l1_blocks.push_front(l1_block);
        if self.l1_blocks.len() > MAX_HISTORY {
            self.l1_blocks.pop_back();
        }

        self.try_attribute_l2_to_l1();
    }

    fn try_attribute_l2_to_l1(&mut self) {
        if self.safe_l2_block <= self.last_attributed_safe_l2 {
            return;
        }

        let mut unmatched: Vec<usize> = self
            .l1_blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.base_blobs > 0 && b.l2_blocks_submitted.is_none())
            .map(|(i, _)| i)
            .collect();

        if unmatched.is_empty() {
            return;
        }

        // Process oldest first (l1_blocks is newest-first, so reverse)
        unmatched.reverse();

        let total_blobs: u64 = unmatched.iter().map(|&i| self.l1_blocks[i].base_blobs).sum();
        if total_blobs == 0 {
            return;
        }

        let l2_delta = self.safe_l2_block - self.last_attributed_safe_l2;
        let mut cursor = self.last_attributed_safe_l2;

        // Integer apportionment: each entry gets floor(l2_delta * blobs / total_blobs),
        // then distribute remainders by largest fractional part.
        let mut shares: Vec<u64> = Vec::with_capacity(unmatched.len());
        let mut remainders: Vec<(usize, u64)> = Vec::with_capacity(unmatched.len());
        let mut allocated: u64 = 0;

        for (nth, &idx) in unmatched.iter().enumerate() {
            let blobs = self.l1_blocks[idx].base_blobs;
            let floor = l2_delta * blobs / total_blobs;
            // Fractional remainder scaled by total_blobs to avoid floats:
            // remainder = (l2_delta * blobs) % total_blobs
            let frac = (l2_delta * blobs) % total_blobs;
            shares.push(floor);
            remainders.push((nth, frac));
            allocated += floor;
        }

        // Distribute the leftover (l2_delta - allocated) to entries with largest remainders
        let mut leftover = l2_delta - allocated;
        remainders.sort_by(|a, b| b.1.cmp(&a.1));
        for &(nth, _) in &remainders {
            if leftover == 0 {
                break;
            }
            shares[nth] += 1;
            leftover -= 1;
        }

        for (nth, &idx) in unmatched.iter().enumerate() {
            let share = shares[nth];
            if share == 0 {
                // Skip zero-share entries - don't write invalid ranges
                continue;
            }

            let range_start = cursor + 1;
            let range_end = cursor + share;

            let da_bytes: u64 = self
                .block_contributions
                .iter()
                .filter(|c| c.block_number >= range_start && c.block_number <= range_end)
                .map(|c| c.da_bytes)
                .sum();

            let block = &mut self.l1_blocks[idx];
            block.l2_blocks_submitted = Some(share);
            block.l2_da_bytes = Some(da_bytes);
            block.l2_block_range = Some((range_start, range_end));

            cursor += share;
        }

        self.last_attributed_safe_l2 = self.safe_l2_block;
    }

    /// Returns an iterator over L1 blocks matching the given filter.
    pub fn filtered_l1_blocks(&self, filter: L1BlockFilter) -> impl Iterator<Item = &L1Block> {
        self.l1_blocks.iter().filter(move |b| match filter {
            L1BlockFilter::All => true,
            L1BlockFilter::WithBlobs => b.has_blobs(),
            L1BlockFilter::WithBaseBlobs => b.has_base_blobs(),
        })
    }

    /// Returns the Base batcher's share of total blobs over the last `n` L1 blocks.
    pub fn base_blob_share(&self, n: usize) -> Option<f64> {
        let blocks: Vec<_> = self.l1_blocks.iter().take(n).collect();
        if blocks.is_empty() {
            return None;
        }
        let total: u64 = blocks.iter().map(|b| b.total_blobs).sum();
        let base: u64 = blocks.iter().map(|b| b.base_blobs).sum();
        if total > 0 { Some(base as f64 / total as f64) } else { None }
    }

    /// Returns the blob target usage ratio over the last `n` L1 blocks.
    pub fn blob_target_usage(&self, n: usize, l1_blob_target: u64) -> Option<f64> {
        let blocks: Vec<_> = self.l1_blocks.iter().take(n).collect();
        if blocks.is_empty() || l1_blob_target == 0 {
            return None;
        }
        let total_blobs: u64 = blocks.iter().map(|b| b.total_blobs).sum();
        let expected = blocks.len() as f64 * l1_blob_target as f64;
        Some(total_blobs as f64 / expected)
    }
}
