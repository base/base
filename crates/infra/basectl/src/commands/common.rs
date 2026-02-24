use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use chrono::{DateTime, Local};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use crate::rpc::{L1BlockInfo, L1ConnectionMode};

/// Size of a single blob in bytes (128 `KiB`).
pub(crate) const BLOB_SIZE: u64 = 128 * 1024;
/// Maximum number of entries retained in history buffers.
pub(crate) const MAX_HISTORY: usize = 1000;

const BLOCK_COLORS: [Color; 24] = [
    Color::Rgb(0, 82, 255),
    Color::Rgb(0, 140, 255),
    Color::Rgb(0, 180, 220),
    Color::Rgb(0, 190, 180),
    Color::Rgb(0, 180, 130),
    Color::Rgb(40, 180, 100),
    Color::Rgb(80, 180, 80),
    Color::Rgb(130, 180, 60),
    Color::Rgb(170, 170, 50),
    Color::Rgb(200, 160, 50),
    Color::Rgb(220, 140, 50),
    Color::Rgb(230, 110, 60),
    Color::Rgb(235, 90, 70),
    Color::Rgb(230, 70, 90),
    Color::Rgb(220, 60, 120),
    Color::Rgb(200, 60, 150),
    Color::Rgb(180, 70, 180),
    Color::Rgb(150, 80, 200),
    Color::Rgb(120, 90, 210),
    Color::Rgb(90, 100, 220),
    Color::Rgb(60, 110, 230),
    Color::Rgb(40, 130, 240),
    Color::Rgb(30, 160, 245),
    Color::Rgb(20, 180, 235),
];

const EIGHTH_BLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

// =============================================================================
// Color Constants
// =============================================================================

/// Primary Base blue color.
pub(crate) const COLOR_BASE_BLUE: Color = Color::Rgb(0, 82, 255);
/// Active border highlight color.
pub(crate) const COLOR_ACTIVE_BORDER: Color = Color::Rgb(100, 180, 255);

/// Background color for the currently selected table row.
pub(crate) const COLOR_ROW_SELECTED: Color = Color::Rgb(60, 60, 80);
/// Background color for a highlighted (cross-referenced) table row.
pub(crate) const COLOR_ROW_HIGHLIGHTED: Color = Color::Rgb(40, 40, 60);

/// Color for DA growth rate indicators.
pub(crate) const COLOR_GROWTH: Color = Color::Rgb(255, 180, 100);
/// Color for DA burn rate indicators.
pub(crate) const COLOR_BURN: Color = Color::Rgb(100, 200, 100);
/// Color for gas target markers.
pub(crate) const COLOR_TARGET: Color = Color::Rgb(255, 200, 100);
/// Color for gas bar fill below target.
pub(crate) const COLOR_GAS_FILL: Color = Color::Rgb(100, 180, 255);

// =============================================================================
// Duration Constants
// =============================================================================

/// Timeout for terminal event polling.
pub(crate) const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(100);
/// Rate calculation window of 30 seconds.
pub(crate) const RATE_WINDOW_30S: Duration = Duration::from_secs(30);
/// Rate calculation window of 2 minutes.
pub(crate) const RATE_WINDOW_2M: Duration = Duration::from_secs(120);
/// Rate calculation window of 5 minutes.
pub(crate) const RATE_WINDOW_5M: Duration = Duration::from_secs(300);
/// Number of recent L1 blocks used for blob share and target usage calculations.
pub(crate) const L1_BLOCK_WINDOW: usize = 10;

// =============================================================================
// Shared Data Types
// =============================================================================

/// A single flashblock entry displayed in the TUI.
#[derive(Clone, Debug)]
pub(crate) struct FlashblockEntry {
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
    /// Decoded transaction summaries from the flashblock stream.
    pub decoded_txs: Vec<crate::rpc::TxSummary>,
}

/// An L2 block's data availability contribution.
#[derive(Clone, Debug)]
pub(crate) struct BlockContribution {
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
    pub(crate) fn age_seconds(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(self.timestamp)
    }
}

/// An L1 block with blob and attribution data.
#[derive(Clone, Debug)]
pub(crate) struct L1Block {
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
    pub(crate) const fn from_info(info: L1BlockInfo) -> Self {
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
    pub(crate) const fn has_blobs(&self) -> bool {
        self.total_blobs > 0
    }

    /// Returns true if this L1 block contains blobs from the Base batcher.
    pub(crate) const fn has_base_blobs(&self) -> bool {
        self.base_blobs > 0
    }

    /// Returns a formatted string of base/total blob counts.
    pub(crate) fn blobs_display(&self) -> String {
        format!("{}/{}", self.base_blobs, self.total_blobs)
    }

    /// Returns the block number truncated to fit within `max_width` characters.
    pub(crate) fn block_display(&self, max_width: usize) -> String {
        truncate_block_number(self.block_number, max_width)
    }

    /// Returns the number of attributed L2 blocks as a display string.
    pub(crate) fn l2_blocks_display(&self) -> String {
        self.l2_blocks_submitted.map_or_else(|| "-".to_string(), |n| n.to_string())
    }

    /// Returns the DA-to-L1 compression ratio, if data is available.
    pub(crate) fn compression_ratio(&self) -> Option<f64> {
        let da_bytes = self.l2_da_bytes?;
        if self.base_blobs == 0 {
            return None;
        }
        let l1_bytes = self.base_blobs * BLOB_SIZE;
        Some(da_bytes as f64 / l1_bytes as f64)
    }

    /// Returns the compression ratio as a formatted display string.
    pub(crate) fn compression_display(&self) -> String {
        self.compression_ratio().map_or_else(|| "-".to_string(), |r| format!("{r:.2}x"))
    }

    /// Returns the age of this L1 block in seconds.
    pub(crate) fn age_seconds(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(self.timestamp)
    }

    /// Returns the block age as a human-readable duration string.
    pub(crate) fn age_display(&self) -> String {
        format_duration(Duration::from_secs(self.age_seconds()))
    }
}

/// Filter mode for the L1 blocks table display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum L1BlockFilter {
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
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::All => Self::WithBlobs,
            Self::WithBlobs => Self::WithBaseBlobs,
            Self::WithBaseBlobs => Self::All,
        }
    }

    /// Returns a short label for this filter mode.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::WithBlobs => "Blobs",
            Self::WithBaseBlobs => "Base",
        }
    }
}

/// Tracks byte rate samples over a sliding time window.
#[derive(Debug)]
pub(crate) struct RateTracker {
    samples: VecDeque<(Instant, u64)>,
}

impl Default for RateTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RateTracker {
    /// Creates a new rate tracker with an empty sample buffer.
    pub(crate) fn new() -> Self {
        Self { samples: VecDeque::with_capacity(300) }
    }

    /// Records a byte count sample at the current instant.
    pub(crate) fn add_sample(&mut self, bytes: u64) {
        let now = Instant::now();
        self.samples.push_back((now, bytes));
        let cutoff = now - Duration::from_secs(300);
        while self.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            self.samples.pop_front();
        }
    }

    /// Computes the byte rate (bytes/sec) over the given duration window.
    pub(crate) fn rate_over(&self, duration: Duration) -> Option<f64> {
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
pub(crate) struct LoadingState {
    /// Number of blocks fetched so far.
    pub current_block: u64,
    /// Total number of blocks to fetch.
    pub total_blocks: u64,
}

// =============================================================================
// DA Tracker - Shared State Management for DA Monitoring
// =============================================================================

/// Tracks DA backlog state, L2 block contributions, and L1 blob data.
#[derive(Debug)]
pub(crate) struct DaTracker {
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
    /// Safe L2 block at the time of last L1→L2 attribution.
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
    pub(crate) fn new() -> Self {
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
    pub(crate) const fn set_initial_backlog(&mut self, safe_block: u64, da_bytes: u64) {
        self.safe_l2_block = safe_block;
        self.da_backlog_bytes = da_bytes;
        self.last_attributed_safe_l2 = safe_block;
    }

    /// Adds a block from the initial backlog fetch.
    pub(crate) fn add_backlog_block(&mut self, block_number: u64, da_bytes: u64, timestamp: u64) {
        let contribution = BlockContribution { block_number, da_bytes, timestamp, tx_count: 0 };
        self.block_contributions.push_front(contribution);
        if self.block_contributions.len() > MAX_HISTORY {
            self.block_contributions.pop_back();
        }
    }

    /// Records a new L2 block and adds its DA bytes to the backlog.
    pub(crate) fn add_block(&mut self, block_number: u64, da_bytes: u64, timestamp: u64) {
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
    pub(crate) fn update_block_info(
        &mut self,
        block_number: u64,
        accurate_da_bytes: u64,
        timestamp: u64,
    ) {
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
    pub(crate) fn update_safe_head(&mut self, safe_block: u64) {
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
    pub(crate) fn record_l1_block(&mut self, info: L1BlockInfo) {
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
                // Skip zero-share entries — don't write invalid ranges
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
    pub(crate) fn filtered_l1_blocks(
        &self,
        filter: L1BlockFilter,
    ) -> impl Iterator<Item = &L1Block> {
        self.l1_blocks.iter().filter(move |b| match filter {
            L1BlockFilter::All => true,
            L1BlockFilter::WithBlobs => b.has_blobs(),
            L1BlockFilter::WithBaseBlobs => b.has_base_blobs(),
        })
    }

    /// Returns the Base batcher's share of total blobs over the last `n` L1 blocks.
    pub(crate) fn base_blob_share(&self, n: usize) -> Option<f64> {
        let blocks: Vec<_> = self.l1_blocks.iter().take(n).collect();
        if blocks.is_empty() {
            return None;
        }
        let total: u64 = blocks.iter().map(|b| b.total_blobs).sum();
        let base: u64 = blocks.iter().map(|b| b.base_blobs).sum();
        if total > 0 { Some(base as f64 / total as f64) } else { None }
    }

    /// Returns the blob target usage ratio over the last `n` L1 blocks.
    pub(crate) fn blob_target_usage(&self, n: usize, l1_blob_target: u64) -> Option<f64> {
        let blocks: Vec<_> = self.l1_blocks.iter().take(n).collect();
        if blocks.is_empty() || l1_blob_target == 0 {
            return None;
        }
        let total_blobs: u64 = blocks.iter().map(|b| b.total_blobs).sum();
        let expected = blocks.len() as f64 * l1_blob_target as f64;
        Some(total_blobs as f64 / expected)
    }
}

// =============================================================================
// Formatting Functions
// =============================================================================

/// Formats a byte count into a human-readable string (e.g. "1.5M").
pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1}G", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1}M", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.0}K", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes}B")
    }
}

/// Formats a gas value into a human-readable string (e.g. "30.0M").
pub(crate) fn format_gas(gas: u64) -> String {
    if gas >= 1_000_000 {
        format!("{:.1}M", gas as f64 / 1_000_000.0)
    } else if gas >= 1_000 {
        format!("{:.0}K", gas as f64 / 1_000.0)
    } else {
        gas.to_string()
    }
}

/// Truncates a block number to fit within `max_width` characters.
pub(crate) fn truncate_block_number(block_number: u64, max_width: usize) -> String {
    let s = block_number.to_string();
    if s.len() <= max_width { s } else { format!("…{}", &s[s.len() - (max_width - 1)..]) }
}

/// Formats a duration into a compact human-readable string (e.g. "2m30s").
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Formats a byte rate into a human-readable string (e.g. "1.2K/s").
pub(crate) fn format_rate(rate: Option<f64>) -> String {
    match rate {
        Some(r) if r >= 1_000_000.0 => format!("{:.1}M/s", r / 1_000_000.0),
        Some(r) if r >= 1_000.0 => format!("{:.1}K/s", r / 1_000.0),
        Some(r) => format!("{r:.0}B/s"),
        None => "-".to_string(),
    }
}

/// Formats a wei value as gwei with appropriate precision.
pub(crate) fn format_gwei(wei: u128) -> String {
    let gwei = wei as f64 / 1_000_000_000.0;
    if gwei >= 1.0 { format!("{gwei:.2} gwei") } else { format!("{gwei:.4} gwei") }
}

const BACKLOG_THRESHOLDS: &[(u64, Color)] = &[
    (5_000_000, Color::Rgb(100, 200, 100)),
    (10_000_000, Color::Rgb(150, 220, 100)),
    (20_000_000, Color::Rgb(200, 220, 80)),
    (30_000_000, Color::Rgb(240, 200, 60)),
    (45_000_000, Color::Rgb(255, 160, 60)),
    (60_000_000, Color::Rgb(255, 100, 80)),
];

/// Returns a color indicating backlog severity based on byte count.
pub(crate) fn backlog_size_color(bytes: u64) -> Color {
    BACKLOG_THRESHOLDS
        .iter()
        .find(|(threshold, _)| bytes < *threshold)
        .map_or(Color::Rgb(255, 80, 120), |(_, color)| *color)
}

/// Returns a unique color for the given block number.
pub(crate) const fn block_color(block_number: u64) -> Color {
    BLOCK_COLORS[(block_number as usize) % BLOCK_COLORS.len()]
}

/// Returns a brightened version of the block color for emphasis.
pub(crate) const fn block_color_bright(block_number: u64) -> Color {
    let Color::Rgb(r, g, b) = BLOCK_COLORS[(block_number as usize) % BLOCK_COLORS.len()] else {
        unreachable!()
    };
    Color::Rgb(
        r.saturating_add((255 - r) / 2),
        g.saturating_add((255 - g) / 2),
        b.saturating_add((255 - b) / 2),
    )
}

const fn dim_color(color: Color, opacity: f64) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    Color::Rgb((r as f64 * opacity) as u8, (g as f64 * opacity) as u8, (b as f64 * opacity) as u8)
}

const GAS_COLOR_WARM: (u8, u8, u8) = (255, 200, 80);
const GAS_COLOR_HOT: (u8, u8, u8) = (255, 60, 60);

/// Builds a styled gas usage bar line with target marker.
pub(crate) fn build_gas_bar(
    gas_used: u64,
    gas_limit: u64,
    elasticity: u64,
    bar_chars: usize,
) -> Line<'static> {
    if gas_limit == 0 {
        return Line::from("-".to_string());
    }

    let bar_units = bar_chars * 8;
    let gas_target = gas_limit / elasticity;
    let target_char = ((gas_target as f64 / gas_limit as f64) * bar_chars as f64).round() as usize;

    let filled_units = ((gas_used as f64 / gas_limit as f64) * bar_units as f64).ceil() as usize;
    let filled_units = filled_units.min(bar_units);

    let target_units = target_char * 8;
    let excess_chars = bar_chars.saturating_sub(target_char).max(1);

    let excess_color = |char_idx: usize| -> Color {
        let t = (char_idx - target_char) as f64 / excess_chars as f64;
        lerp_rgb(GAS_COLOR_WARM, GAS_COLOR_HOT, t.clamp(0.0, 1.0))
    };

    let mut spans = Vec::new();
    let mut current_units = 0;

    for char_idx in 0..bar_chars {
        let char_end_units = (char_idx + 1) * 8;

        if char_idx == target_char {
            if filled_units <= target_units {
                spans.push(Span::styled("▏", Style::default().fg(COLOR_TARGET)));
            } else {
                let over_units = filled_units.saturating_sub(target_units).min(8);
                let color = excess_color(char_idx);
                if over_units >= 8 {
                    spans.push(Span::styled("█", Style::default().fg(color)));
                } else {
                    let opacity = over_units as f64 / 8.0;
                    let dimmed = dim_color(color, opacity);
                    spans.push(Span::styled(
                        EIGHTH_BLOCKS[over_units - 1].to_string(),
                        Style::default().fg(dimmed),
                    ));
                }
            }
        } else if current_units >= filled_units {
            spans.push(Span::raw(" "));
        } else if char_end_units <= filled_units {
            let fill_color =
                if char_idx < target_char { COLOR_GAS_FILL } else { excess_color(char_idx) };
            spans.push(Span::styled("█", Style::default().fg(fill_color)));
        } else {
            let units_in_char = filled_units - current_units;
            let opacity = units_in_char as f64 / 8.0;
            let fill_color =
                if char_idx < target_char { COLOR_GAS_FILL } else { excess_color(char_idx) };
            let dimmed = dim_color(fill_color, opacity);
            spans.push(Span::styled(
                EIGHTH_BLOCKS[units_in_char - 1].to_string(),
                Style::default().fg(dimmed),
            ));
        }

        current_units = char_end_units;
    }

    Line::from(spans)
}

/// Parameters for rendering the L1 blocks table.
#[derive(Debug)]
pub(crate) struct L1BlocksTableParams<'a, I: Iterator<Item = &'a L1Block>> {
    /// Iterator over L1 blocks to display.
    pub l1_blocks: I,
    /// Whether this panel is the active (focused) panel.
    pub is_active: bool,
    /// Table selection state.
    pub table_state: &'a mut TableState,
    /// Active L1 block filter.
    pub filter: L1BlockFilter,
    /// Title displayed in the panel border.
    pub title: &'a str,
    /// Current L1 connection mode indicator.
    pub connection_mode: Option<L1ConnectionMode>,
}

/// Renders the L1 blocks table panel.
pub(crate) fn render_l1_blocks_table<'a>(
    f: &mut Frame<'_>,
    area: Rect,
    params: L1BlocksTableParams<'a, impl Iterator<Item = &'a L1Block>>,
) {
    let L1BlocksTableParams { l1_blocks, is_active, table_state, filter, title, connection_mode } =
        params;
    let border_color = if is_active { Color::Rgb(255, 100, 100) } else { Color::Red };

    let filter_label = filter.label();
    let mode_label = match connection_mode {
        Some(L1ConnectionMode::WebSocket) => " WS",
        Some(L1ConnectionMode::Polling) => " Poll",
        None => "",
    };
    let block = Block::default()
        .title(format!(" {title} [{filter_label}]{mode_label} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("L1 Blk").style(header_style),
        Cell::from("Blobs").style(header_style),
        Cell::from("L2").style(header_style),
        Cell::from("Ratio").style(header_style),
        Cell::from("Age").style(header_style),
    ]);

    let fixed_cols_width = 5 + 4 + 6 + 5 + 4;
    let l1_col_width = inner.width.saturating_sub(fixed_cols_width).clamp(4, 9) as usize;

    let selected_row = table_state.selected();

    let rows: Vec<Row<'_>> = l1_blocks
        .enumerate()
        .map(|(idx, l1_block)| {
            let is_selected = is_active && selected_row == Some(idx);

            let style = if is_selected {
                Style::default().fg(Color::White).bg(COLOR_ROW_SELECTED)
            } else {
                Style::default().fg(Color::White)
            };

            let blobs_style = if l1_block.base_blobs > 0 {
                Style::default().fg(COLOR_BASE_BLUE)
            } else if l1_block.total_blobs > 0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            Row::new(vec![
                Cell::from(l1_block.block_display(l1_col_width)),
                Cell::from(l1_block.blobs_display()).style(blobs_style),
                Cell::from(l1_block.l2_blocks_display()),
                Cell::from(l1_block.compression_display()),
                Cell::from(l1_block.age_display()),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Max(9),
        Constraint::Length(5),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Min(5),
    ];

    let table = Table::new(rows, widths).header(header);
    f.render_stateful_widget(table, inner, table_state);
}

/// Renders a horizontal bar showing the DA backlog with per-block coloring.
pub(crate) fn render_da_backlog_bar(
    f: &mut Frame<'_>,
    area: Rect,
    tracker: &DaTracker,
    loading: Option<&LoadingState>,
    loaded: bool,
    highlighted_block: Option<u64>,
) {
    let block = Block::default()
        .title(" DA Backlog ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < 10 || inner.height < 1 {
        return;
    }

    let bar_width = inner.width.saturating_sub(12) as usize;

    if !loaded {
        let (line1, line2) = match loading {
            Some(ls) if ls.total_blocks > 0 => {
                let pct = (ls.current_block as f64 / ls.total_blocks as f64 * 100.0) as u64;
                let filled = (pct as usize * bar_width / 100).min(bar_width);
                let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled));
                (
                    Line::from(Span::styled(bar, Style::default().fg(Color::Cyan))),
                    Line::from(Span::styled(
                        format!(" Loading {}/{}", ls.current_block, ls.total_blocks),
                        Style::default().fg(Color::Cyan),
                    )),
                )
            }
            _ => (
                Line::from(Span::styled(
                    "░".repeat(bar_width),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(" Loading...", Style::default().fg(Color::Yellow))),
            ),
        };
        let para = Paragraph::new(vec![line1, line2]);
        f.render_widget(para, inner);
        return;
    }

    let backlog_blocks: Vec<_> = tracker
        .block_contributions
        .iter()
        .filter(|c| c.block_number > tracker.safe_l2_block)
        .collect();

    if backlog_blocks.is_empty() || tracker.da_backlog_bytes == 0 {
        let empty_bar = "░".repeat(bar_width);
        let text = format!("{empty_bar} {:>8}", format_bytes(0));
        let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
        f.render_widget(para, inner);
        return;
    }

    let total_backlog = tracker.da_backlog_bytes;
    let mut spans: Vec<Span<'_>> = Vec::new();
    let mut chars_used = 0usize;

    for contrib in backlog_blocks.iter().rev() {
        let color = block_color(contrib.block_number);
        let is_highlighted = highlighted_block == Some(contrib.block_number);

        let proportion = contrib.da_bytes as f64 / total_backlog as f64;
        let char_count = ((proportion * bar_width as f64).round() as usize).max(1);
        let char_count = char_count.min(bar_width - chars_used);

        if char_count > 0 {
            let style = if is_highlighted {
                Style::default().fg(Color::White).bg(color)
            } else {
                Style::default().fg(color)
            };
            let glyph = if is_highlighted { "⣿" } else { "█" };
            spans.push(Span::styled(glyph.repeat(char_count), style));
            chars_used += char_count;
        }

        if chars_used >= bar_width {
            break;
        }
    }

    if chars_used < bar_width {
        spans.push(Span::styled(
            "░".repeat(bar_width - chars_used),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let backlog_color = backlog_size_color(total_backlog);
    spans.push(Span::styled(
        format!(" {:>8}", format_bytes(total_backlog)),
        Style::default().fg(backlog_color).add_modifier(Modifier::BOLD),
    ));

    let line = Line::from(spans);
    let para = Paragraph::new(line);
    f.render_widget(para, inner);
}

/// Renders a horizontal bar showing aggregate gas usage across recent blocks.
pub(crate) fn render_gas_usage_bar(
    f: &mut Frame<'_>,
    area: Rect,
    entries: &VecDeque<FlashblockEntry>,
    elasticity: u64,
    highlighted_block: Option<u64>,
) {
    let mut block_gas: Vec<(u64, u64)> = Vec::new();
    for entry in entries {
        if let Some(last) = block_gas.last_mut()
            && last.0 == entry.block_number
        {
            last.1 = last.1.max(entry.gas_used);
            continue;
        }
        block_gas.push((entry.block_number, entry.gas_used));
    }

    let n_label = block_gas.len();
    let title_widget = Block::default()
        .title(format!(" Gas Usage ({n_label} blocks) "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = title_widget.inner(area);
    f.render_widget(title_widget, area);

    if inner.width < 10 || inner.height < 1 {
        return;
    }

    let bar_width = inner.width.saturating_sub(12) as usize;

    if block_gas.is_empty() {
        let empty_bar = "░".repeat(bar_width);
        let text = format!("{empty_bar} {:>5}", "0%");
        let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
        f.render_widget(para, inner);
        return;
    }

    let n_blocks = block_gas.len() as u64;
    let gas_limit = entries.front().map(|e| e.gas_limit).unwrap_or(0);
    let per_block_target = if elasticity > 0 && gas_limit > 0 { gas_limit / elasticity } else { 0 };
    let total_target = per_block_target * n_blocks;
    let total_limit = gas_limit * n_blocks;
    let total_gas: u64 = block_gas.iter().map(|(_, g)| *g).sum();

    let half = bar_width / 2;
    let target_char = half;

    let gas_to_chars = |gas: u64| -> f64 {
        if total_target == 0 {
            return 0.0;
        }
        let g = gas as f64;
        let t = total_target as f64;
        let l = total_limit as f64;
        if g <= t {
            (g / t) * half as f64
        } else {
            half as f64 + ((g - t) / (l - t)) * (bar_width - half) as f64
        }
    };

    let mut spans: Vec<Span<'_>> = Vec::new();
    let mut chars_used = 0usize;
    let mut cumulative_gas = 0u64;

    for &(block_number, gas_used) in block_gas.iter().rev() {
        if chars_used >= bar_width {
            break;
        }

        let color = block_color(block_number);
        let is_highlighted = highlighted_block == Some(block_number);

        let pos_before = gas_to_chars(cumulative_gas).round() as usize;
        cumulative_gas += gas_used;
        let pos_after = gas_to_chars(cumulative_gas).round() as usize;
        let char_count = pos_after.saturating_sub(pos_before).max(1).min(bar_width - chars_used);

        if char_count > 0 {
            let style = if is_highlighted {
                Style::default().fg(Color::White).bg(color)
            } else {
                Style::default().fg(color)
            };
            let glyph = if is_highlighted { "⣿" } else { "█" };

            if target_char > chars_used && target_char < chars_used + char_count {
                let before = target_char - chars_used;
                let after = char_count - before - 1;
                if before > 0 {
                    spans.push(Span::styled(glyph.repeat(before), style));
                }
                spans.push(Span::styled("│", Style::default().fg(COLOR_TARGET).bg(color)));
                if after > 0 {
                    spans.push(Span::styled(glyph.repeat(after), style));
                }
            } else {
                spans.push(Span::styled(glyph.repeat(char_count), style));
            }
            chars_used += char_count;
        }
    }

    while chars_used < bar_width {
        if chars_used == target_char {
            spans.push(Span::styled("│", Style::default().fg(COLOR_TARGET)));
        } else {
            spans.push(Span::styled("░", Style::default().fg(Color::DarkGray)));
        }
        chars_used += 1;
    }

    let usage_ratio = if total_target > 0 { total_gas as f64 / total_target as f64 } else { 0.0 };
    spans.push(Span::styled(
        format!(" {:>5.0}%", usage_ratio * 100.0),
        Style::default().fg(target_usage_color(usage_ratio)).add_modifier(Modifier::BOLD),
    ));

    let line = Line::from(spans);
    let para = Paragraph::new(line);
    f.render_widget(para, inner);
}

const TARGET_USAGE_MAX: f64 = 1.5;

/// Returns a color representing how close usage is to the target (blue to red).
pub(crate) fn target_usage_color(usage: f64) -> Color {
    let t = usage.clamp(0.0, TARGET_USAGE_MAX);
    if t <= 1.0 {
        lerp_rgb((0, 100, 255), (255, 255, 0), t)
    } else {
        lerp_rgb((255, 255, 0), (255, 0, 0), (t - 1.0) / (TARGET_USAGE_MAX - 1.0))
    }
}

const fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f64) -> Color {
    Color::Rgb(
        (a.0 as f64 + (b.0 as f64 - a.0 as f64) * t) as u8,
        (a.1 as f64 + (b.1 as f64 - a.1 as f64) * t) as u8,
        (a.2 as f64 + (b.2 as f64 - a.2 as f64) * t) as u8,
    )
}

const FLASHBLOCK_TARGET_MS: i64 = 200;
const FLASHBLOCK_TOLERANCE_MS: i64 = 50;

/// Returns a color indicating how close a time delta is to the 200ms target.
pub(crate) fn time_diff_color(ms: i64) -> Color {
    let target = FLASHBLOCK_TARGET_MS;
    let tol = FLASHBLOCK_TOLERANCE_MS;
    if (target - tol..=target + tol).contains(&ms) {
        Color::Green
    } else if (target - 2 * tol..target - tol).contains(&ms) {
        Color::Blue
    } else if ms < target - 2 * tol {
        Color::Magenta
    } else if (target + tol..target + 2 * tol).contains(&ms) {
        Color::Yellow
    } else {
        Color::Red
    }
}
