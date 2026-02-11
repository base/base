use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use chrono::{DateTime, Local};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};

use crate::rpc::{L1BlockInfo, L1ConnectionMode};

pub const BLOB_SIZE: u64 = 128 * 1024;
pub const MAX_HISTORY: usize = 1000;

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

// Primary colors
pub const COLOR_BASE_BLUE: Color = Color::Rgb(0, 82, 255);
pub const COLOR_ACTIVE_BORDER: Color = Color::Rgb(100, 180, 255);

// Table background colors
pub const COLOR_ROW_SELECTED: Color = Color::Rgb(60, 60, 80);
pub const COLOR_ROW_HIGHLIGHTED: Color = Color::Rgb(40, 40, 60);

// Rate/status colors
pub const COLOR_GROWTH: Color = Color::Rgb(255, 180, 100);
pub const COLOR_BURN: Color = Color::Rgb(100, 200, 100);
pub const COLOR_TARGET: Color = Color::Rgb(255, 200, 100);
pub const COLOR_GAS_FILL: Color = Color::Rgb(100, 180, 255);

// =============================================================================
// Duration Constants
// =============================================================================

pub const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(100);
pub const RATE_WINDOW_30S: Duration = Duration::from_secs(30);
pub const RATE_WINDOW_2M: Duration = Duration::from_secs(120);
pub const RATE_WINDOW_5M: Duration = Duration::from_secs(300);

// =============================================================================
// Shared Data Types
// =============================================================================

#[derive(Clone, Debug)]
pub struct FlashblockEntry {
    pub block_number: u64,
    pub index: u64,
    pub tx_count: usize,
    pub gas_used: u64,
    pub gas_limit: u64,
    pub base_fee: Option<u128>,
    pub prev_base_fee: Option<u128>,
    pub timestamp: DateTime<Local>,
    pub time_diff_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct BlockContribution {
    pub block_number: u64,
    pub da_bytes: u64,
    pub timestamp: u64,
}

impl BlockContribution {
    pub fn age_seconds(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(self.timestamp)
    }
}

#[derive(Clone, Debug)]
pub struct L1Block {
    pub block_number: u64,
    pub block_hash: B256,
    pub timestamp: u64,
    pub total_blobs: u64,
    pub base_blobs: u64,
    pub l2_blocks_submitted: Option<u64>,
    pub l2_da_bytes: Option<u64>,
    pub l2_block_range: Option<(u64, u64)>,
}

impl L1Block {
    pub const fn from_info(info: L1BlockInfo) -> Self {
        Self {
            block_number: info.block_number,
            block_hash: info.block_hash,
            timestamp: info.timestamp,
            total_blobs: info.total_blobs,
            base_blobs: info.base_blobs,
            l2_blocks_submitted: None,
            l2_da_bytes: None,
            l2_block_range: None,
        }
    }

    pub const fn has_blobs(&self) -> bool {
        self.total_blobs > 0
    }

    pub const fn has_base_blobs(&self) -> bool {
        self.base_blobs > 0
    }

    pub fn blobs_display(&self) -> String {
        format!("{}/{}", self.base_blobs, self.total_blobs)
    }

    pub fn block_display(&self, max_width: usize) -> String {
        truncate_block_number(self.block_number, max_width)
    }

    pub fn l2_blocks_display(&self) -> String {
        self.l2_blocks_submitted.map_or_else(|| "-".to_string(), |n| n.to_string())
    }

    pub fn compression_ratio(&self) -> Option<f64> {
        let da_bytes = self.l2_da_bytes?;
        if self.base_blobs == 0 {
            return None;
        }
        let l1_bytes = self.base_blobs * BLOB_SIZE;
        Some(da_bytes as f64 / l1_bytes as f64)
    }

    pub fn compression_display(&self) -> String {
        self.compression_ratio().map_or_else(|| "-".to_string(), |r| format!("{r:.2}x"))
    }

    pub fn age_seconds(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(self.timestamp)
    }

    pub fn age_display(&self) -> String {
        format_duration(Duration::from_secs(self.age_seconds()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum L1BlockFilter {
    #[default]
    All,
    WithBlobs,
    WithBaseBlobs,
}

impl L1BlockFilter {
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::WithBlobs,
            Self::WithBlobs => Self::WithBaseBlobs,
            Self::WithBaseBlobs => Self::All,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::WithBlobs => "Blobs",
            Self::WithBaseBlobs => "Base",
        }
    }
}

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
    pub fn new() -> Self {
        Self { samples: VecDeque::with_capacity(300) }
    }

    pub fn add_sample(&mut self, bytes: u64) {
        let now = Instant::now();
        self.samples.push_back((now, bytes));
        let cutoff = now - Duration::from_secs(300);
        while self.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            self.samples.pop_front();
        }
    }

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

#[derive(Debug)]
pub struct LoadingState {
    pub current_block: u64,
    pub total_blocks: u64,
}

// =============================================================================
// DA Tracker - Shared State Management for DA Monitoring
// =============================================================================

#[derive(Debug)]
pub struct DaTracker {
    pub safe_l2_block: u64,
    pub da_backlog_bytes: u64,
    pub block_contributions: VecDeque<BlockContribution>,
    pub l1_blocks: VecDeque<L1Block>,
    pub growth_tracker: RateTracker,
    pub burn_tracker: RateTracker,
    pub total_blob_tracker: RateTracker,
    pub base_blob_tracker: RateTracker,
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
    pub fn new() -> Self {
        Self {
            safe_l2_block: 0,
            da_backlog_bytes: 0,
            block_contributions: VecDeque::with_capacity(MAX_HISTORY),
            l1_blocks: VecDeque::with_capacity(MAX_HISTORY),
            growth_tracker: RateTracker::new(),
            burn_tracker: RateTracker::new(),
            total_blob_tracker: RateTracker::new(),
            base_blob_tracker: RateTracker::new(),
            last_base_blob_time: None,
            last_attributed_safe_l2: 0,
        }
    }

    pub const fn set_initial_backlog(&mut self, safe_block: u64, da_bytes: u64) {
        self.safe_l2_block = safe_block;
        self.da_backlog_bytes = da_bytes;
        self.last_attributed_safe_l2 = safe_block;
    }

    pub fn add_backlog_block(&mut self, block_number: u64, da_bytes: u64, timestamp: u64) {
        let contribution = BlockContribution { block_number, da_bytes, timestamp };
        self.block_contributions.push_front(contribution);
        if self.block_contributions.len() > MAX_HISTORY {
            self.block_contributions.pop_back();
        }
    }

    pub fn add_block(&mut self, block_number: u64, da_bytes: u64, timestamp: u64) {
        if block_number <= self.safe_l2_block {
            return;
        }

        self.da_backlog_bytes = self.da_backlog_bytes.saturating_add(da_bytes);
        self.growth_tracker.add_sample(da_bytes);

        let contribution = BlockContribution { block_number, da_bytes, timestamp };
        self.block_contributions.push_front(contribution);
        if self.block_contributions.len() > MAX_HISTORY {
            self.block_contributions.pop_back();
        }
    }

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
            BlockContribution { block_number, da_bytes: accurate_da_bytes, timestamp };

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

    pub fn record_l1_block(&mut self, info: L1BlockInfo) {
        if self.l1_blocks.iter().any(|b| b.block_number == info.block_number) {
            return;
        }

        let l1_block = L1Block::from_info(info);

        if l1_block.total_blobs > 0 {
            self.total_blob_tracker.add_sample(l1_block.total_blobs);
        }
        if l1_block.base_blobs > 0 {
            self.base_blob_tracker.add_sample(l1_block.base_blobs);
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

    pub fn filtered_l1_blocks(&self, filter: L1BlockFilter) -> impl Iterator<Item = &L1Block> {
        self.l1_blocks.iter().filter(move |b| match filter {
            L1BlockFilter::All => true,
            L1BlockFilter::WithBlobs => b.has_blobs(),
            L1BlockFilter::WithBaseBlobs => b.has_base_blobs(),
        })
    }

    pub fn base_blob_share(&self, window: Duration) -> Option<f64> {
        let total = self.total_blob_tracker.rate_over(window)?;
        let base = self.base_blob_tracker.rate_over(window)?;
        if total > 0.0 { Some(base / total) } else { None }
    }

    pub fn blob_target_usage(&self, window: Duration, l1_blob_target: u64) -> Option<f64> {
        let blob_rate = self.total_blob_tracker.rate_over(window)?;
        let blocks_per_sec = 1.0 / 12.0;
        let target_rate = l1_blob_target as f64 * blocks_per_sec;
        Some(blob_rate / target_rate)
    }
}

// =============================================================================
// Formatting Functions
// =============================================================================

pub fn format_bytes(bytes: u64) -> String {
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

pub fn format_gas(gas: u64) -> String {
    if gas >= 1_000_000 {
        format!("{:.1}M", gas as f64 / 1_000_000.0)
    } else if gas >= 1_000 {
        format!("{:.0}K", gas as f64 / 1_000.0)
    } else {
        gas.to_string()
    }
}

pub fn truncate_block_number(block_number: u64, max_width: usize) -> String {
    let s = block_number.to_string();
    if s.len() <= max_width { s } else { format!("…{}", &s[s.len() - (max_width - 1)..]) }
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

pub fn format_rate(rate: Option<f64>) -> String {
    match rate {
        Some(r) if r >= 1_000_000.0 => format!("{:.1}M/s", r / 1_000_000.0),
        Some(r) if r >= 1_000.0 => format!("{:.1}K/s", r / 1_000.0),
        Some(r) => format!("{r:.0}B/s"),
        None => "-".to_string(),
    }
}

pub fn format_gwei(wei: u128) -> String {
    let gwei = wei as f64 / 1_000_000_000.0;
    if gwei >= 1.0 { format!("{gwei:.2} gwei") } else { format!("{gwei:.4} gwei") }
}

pub const fn backlog_size_color(bytes: u64) -> Color {
    if bytes < 5_000_000 {
        Color::Rgb(100, 200, 100)
    } else if bytes < 10_000_000 {
        Color::Rgb(150, 220, 100)
    } else if bytes < 20_000_000 {
        Color::Rgb(200, 220, 80)
    } else if bytes < 30_000_000 {
        Color::Rgb(240, 200, 60)
    } else if bytes < 45_000_000 {
        Color::Rgb(255, 160, 60)
    } else if bytes < 60_000_000 {
        Color::Rgb(255, 100, 80)
    } else {
        Color::Rgb(255, 80, 120)
    }
}

pub const fn block_color(block_number: u64) -> Color {
    BLOCK_COLORS[(block_number as usize) % BLOCK_COLORS.len()]
}

pub fn build_gas_bar(
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

    let filled_units = ((gas_used as f64 / gas_limit as f64) * bar_units as f64).round() as usize;
    let filled_units = filled_units.min(bar_units);

    let fill_color = COLOR_GAS_FILL;
    let target_color = COLOR_TARGET;

    let mut spans = Vec::new();
    let mut current_units = 0;

    for char_idx in 0..bar_chars {
        let char_end_units = (char_idx + 1) * 8;
        let is_target_char = char_idx == target_char;

        if is_target_char {
            if current_units >= filled_units {
                spans.push(Span::styled("│", Style::default().fg(target_color)));
            } else {
                spans.push(Span::styled("│", Style::default().fg(target_color).bg(fill_color)));
            }
        } else if current_units >= filled_units {
            spans.push(Span::styled(" ", Style::default()));
        } else if char_end_units <= filled_units {
            spans.push(Span::styled("█", Style::default().fg(fill_color)));
        } else {
            let units_in_char = filled_units - current_units;
            spans.push(Span::styled(
                EIGHTH_BLOCKS[units_in_char - 1].to_string(),
                Style::default().fg(fill_color),
            ));
        }

        current_units = char_end_units;
    }

    Line::from(spans)
}

#[allow(clippy::too_many_arguments)]
pub fn render_l1_blocks_table<'a>(
    f: &mut Frame,
    area: Rect,
    l1_blocks: impl Iterator<Item = &'a L1Block>,
    is_active: bool,
    selected_row: usize,
    filter: L1BlockFilter,
    title: &str,
    connection_mode: Option<L1ConnectionMode>,
) {
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

    // Calculate available width for L1 block column
    // Other columns need: Blobs(5) + L2(4) + Ratio(6) + Age(5) + spacing(4) = 24
    let fixed_cols_width = 5 + 4 + 6 + 5 + 4;
    let l1_col_width = inner.width.saturating_sub(fixed_cols_width).clamp(4, 9) as usize;

    let rows: Vec<Row> = l1_blocks
        .take(inner.height.saturating_sub(1) as usize)
        .enumerate()
        .map(|(idx, l1_block)| {
            let is_selected = is_active && idx == selected_row;

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
    f.render_widget(table, inner);
}

pub fn render_da_backlog_bar(
    f: &mut Frame,
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
    let mut spans: Vec<Span> = Vec::new();
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

pub fn time_diff_color(ms: i64) -> Color {
    if (150..=250).contains(&ms) {
        Color::Green
    } else if (100..150).contains(&ms) {
        Color::Blue
    } else if ms < 100 {
        Color::Magenta
    } else if (250..300).contains(&ms) {
        Color::Yellow
    } else {
        Color::Red
    }
}
