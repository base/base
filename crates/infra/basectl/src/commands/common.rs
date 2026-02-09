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

use crate::rpc::BlobSubmission;

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
    pub timestamp: Instant,
}

#[derive(Clone, Debug)]
pub struct BatchSubmission {
    pub da_bytes: u64,
    pub estimated_blobs: u64,
    pub actual_blobs: Option<u64>,
    pub l1_blob_bytes: Option<u64>,
    pub blocks_submitted: u64,
    pub timestamp: Instant,
    pub block_range: (u64, u64),
    pub l1_block_number: Option<u64>,
    pub l1_block_hash: Option<B256>,
}

impl BatchSubmission {
    pub fn new(da_bytes: u64, blocks_submitted: u64, block_range: (u64, u64)) -> Self {
        Self {
            da_bytes,
            estimated_blobs: da_bytes.div_ceil(BLOB_SIZE),
            actual_blobs: None,
            l1_blob_bytes: None,
            blocks_submitted,
            timestamp: Instant::now(),
            block_range,
            l1_block_number: None,
            l1_block_hash: None,
        }
    }

    pub const fn record_l1_submission(&mut self, blob_sub: &BlobSubmission) {
        self.actual_blobs = Some(blob_sub.blob_count);
        self.l1_blob_bytes = Some(blob_sub.l1_blob_bytes);
        self.l1_block_number = Some(blob_sub.block_number);
        self.l1_block_hash = Some(blob_sub.block_hash);
    }

    pub const fn has_l1_data(&self) -> bool {
        self.actual_blobs.is_some()
    }

    pub fn compression_ratio(&self) -> Option<f64> {
        self.l1_blob_bytes.filter(|&b| b > 0).map(|l1_bytes| self.da_bytes as f64 / l1_bytes as f64)
    }

    pub fn blob_count_display(&self) -> String {
        self.actual_blobs
            .map_or_else(|| format!("~{}", self.estimated_blobs), |actual| actual.to_string())
    }

    pub fn l1_block_display(&self) -> String {
        self.l1_block_number.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string())
    }

    pub fn compression_display(&self) -> String {
        self.compression_ratio().map(|r| format!("{r:.2}x")).unwrap_or_else(|| "-".to_string())
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
        let samples_in_window: Vec<_> = self.samples.iter().filter(|(t, _)| *t >= cutoff).collect();

        if samples_in_window.len() < 2 {
            return None;
        }

        let total: u64 = samples_in_window.iter().map(|(_, b)| *b).sum();
        let elapsed = duration.as_secs_f64();
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
    pub batch_submissions: VecDeque<BatchSubmission>,
    pub growth_tracker: RateTracker,
    pub burn_tracker: RateTracker,
    pub last_blob_time: Option<Instant>,
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
            batch_submissions: VecDeque::with_capacity(MAX_HISTORY),
            growth_tracker: RateTracker::new(),
            burn_tracker: RateTracker::new(),
            last_blob_time: None,
        }
    }

    pub const fn set_initial_backlog(&mut self, safe_block: u64, da_bytes: u64) {
        self.safe_l2_block = safe_block;
        self.da_backlog_bytes = da_bytes;
    }

    pub fn add_block(&mut self, block_number: u64, da_bytes: u64) {
        if block_number <= self.safe_l2_block {
            return;
        }

        self.da_backlog_bytes = self.da_backlog_bytes.saturating_add(da_bytes);
        self.growth_tracker.add_sample(da_bytes);

        let contribution = BlockContribution { block_number, da_bytes, timestamp: Instant::now() };
        self.block_contributions.push_front(contribution);
        if self.block_contributions.len() > MAX_HISTORY {
            self.block_contributions.pop_back();
        }
    }

    pub fn update_block_da(&mut self, block_number: u64, accurate_da_bytes: u64) {
        for contrib in &mut self.block_contributions {
            if contrib.block_number == block_number {
                let diff = accurate_da_bytes as i64 - contrib.da_bytes as i64;
                contrib.da_bytes = accurate_da_bytes;

                if block_number > self.safe_l2_block {
                    if diff > 0 {
                        self.da_backlog_bytes = self.da_backlog_bytes.saturating_add(diff as u64);
                    } else {
                        self.da_backlog_bytes =
                            self.da_backlog_bytes.saturating_sub((-diff) as u64);
                    }
                }
                break;
            }
        }
    }

    pub fn update_safe_head(&mut self, safe_block: u64) -> Option<BatchSubmission> {
        if safe_block <= self.safe_l2_block {
            return None;
        }

        let old_safe = self.safe_l2_block;
        self.safe_l2_block = safe_block;

        let mut submitted_bytes: u64 = 0;
        let mut blocks_submitted: u64 = 0;
        let mut min_block = u64::MAX;
        let mut max_block = 0u64;

        for contrib in &self.block_contributions {
            if contrib.block_number > old_safe && contrib.block_number <= safe_block {
                submitted_bytes = submitted_bytes.saturating_add(contrib.da_bytes);
                blocks_submitted += 1;
                min_block = min_block.min(contrib.block_number);
                max_block = max_block.max(contrib.block_number);
            }
        }

        if submitted_bytes > 0 {
            self.da_backlog_bytes = self.da_backlog_bytes.saturating_sub(submitted_bytes);
            self.burn_tracker.add_sample(submitted_bytes);
            self.last_blob_time = Some(Instant::now());

            let submission =
                BatchSubmission::new(submitted_bytes, blocks_submitted, (min_block, max_block));
            self.batch_submissions.push_front(submission.clone());
            if self.batch_submissions.len() > MAX_HISTORY {
                self.batch_submissions.pop_back();
            }
            Some(submission)
        } else {
            None
        }
    }

    pub fn record_l1_blob_submission(&mut self, blob_sub: &BlobSubmission) {
        if let Some(submission) = self.batch_submissions.iter_mut().rev().find(|s| !s.has_l1_data())
        {
            submission.record_l1_submission(blob_sub);
        }
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
pub fn render_batches_table(
    f: &mut Frame,
    area: Rect,
    batches: &VecDeque<BatchSubmission>,
    is_active: bool,
    selected_row: usize,
    highlighted_batch_idx: Option<usize>,
    has_op_node: bool,
    title: &str,
) {
    let border_color = if is_active { Color::Rgb(255, 100, 100) } else { Color::Red };

    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if !has_op_node {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled("op_node_rpc required", Style::default().fg(Color::Yellow))),
            Line::from(Span::styled("Set in config file:", Style::default().fg(Color::DarkGray))),
            Line::from(Span::styled(
                "~/.base/config/<name>.yaml",
                Style::default().fg(Color::Yellow),
            )),
        ];
        let para = Paragraph::new(lines).alignment(Alignment::Center);
        f.render_widget(para, inner);
        return;
    }

    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("L1 Blk").style(header_style),
        Cell::from("DA").style(header_style),
        Cell::from("Blobs").style(header_style),
        Cell::from("Ratio").style(header_style),
        Cell::from("Blks").style(header_style),
        Cell::from("Age").style(header_style),
    ]);

    let rows: Vec<Row> = batches
        .iter()
        .take(inner.height.saturating_sub(1) as usize)
        .enumerate()
        .map(|(idx, batch)| {
            let is_selected = is_active && idx == selected_row;
            let is_highlighted = highlighted_batch_idx == Some(idx);

            let style = if is_selected {
                Style::default().fg(Color::White).bg(COLOR_ROW_SELECTED)
            } else if is_highlighted {
                Style::default().fg(Color::White).bg(COLOR_ROW_HIGHLIGHTED)
            } else {
                Style::default().fg(Color::White)
            };

            Row::new(vec![
                Cell::from(batch.l1_block_display()),
                Cell::from(format_bytes(batch.da_bytes)),
                Cell::from(batch.blob_count_display()),
                Cell::from(batch.compression_display()),
                Cell::from(batch.blocks_submitted.to_string()),
                Cell::from(format_duration(batch.timestamp.elapsed())),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(5),
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
