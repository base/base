use std::{collections::VecDeque, time::Duration};

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use super::format::lerp_rgb;
use crate::{
    app::{DaTracker, FlashblockEntry, L1Block, L1BlockFilter, LoadingState},
    output::{
        COLOR_BASE_BLUE, COLOR_GAS_FILL, COLOR_ROW_SELECTED, COLOR_TARGET, backlog_size_color,
        block_color, format_bytes, format_duration, target_usage_color, truncate_block_number,
    },
    rpc::L1ConnectionMode,
};

const EIGHTH_BLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
const GAS_COLOR_WARM: (u8, u8, u8) = (255, 200, 80);
const GAS_COLOR_HOT: (u8, u8, u8) = (255, 60, 60);

/// Builds a styled gas usage bar line with target marker.
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
pub struct L1BlocksTableParams<'a, I: Iterator<Item = &'a L1Block>> {
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
pub fn render_l1_blocks_table<'a>(
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
                Cell::from(truncate_block_number(l1_block.block_number, l1_col_width)),
                Cell::from(l1_block.blobs_display()).style(blobs_style),
                Cell::from(l1_block.l2_blocks_display()),
                Cell::from(l1_block.compression_display()),
                Cell::from(format_duration(Duration::from_secs(l1_block.age_seconds()))),
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
pub fn render_da_backlog_bar(
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
pub fn render_gas_usage_bar(
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

const fn dim_color(color: Color, opacity: f64) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    Color::Rgb((r as f64 * opacity) as u8, (g as f64 * opacity) as u8, (b as f64 * opacity) as u8)
}
