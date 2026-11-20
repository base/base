use std::time::Duration;

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use crate::{
    app::{DaTracker, L1Block, L1BlockFilter, LoadingState},
    output::{COLOR_BASE_BLUE, COLOR_ROW_SELECTED, Format},
    rpc::ConnectionMode,
};

/// Gas below target fades from cool at zero to bright at the target, staying
/// readable on dark terminals at low usage.
const GAS_COLOR_COOL: (u8, u8, u8) = (40, 90, 200);
const GAS_COLOR_BRIGHT: (u8, u8, u8) = (120, 200, 255);
/// Fill color when the gas target is unknown.
const COLOR_GAS_FILL: Color = Color::Rgb(100, 180, 255);
/// Gas above target fades from warm at the target to hot at the gas limit.
const GAS_COLOR_WARM: (u8, u8, u8) = (255, 200, 80);
const GAS_COLOR_HOT: (u8, u8, u8) = (255, 60, 60);
const COLOR_GAS_TARGET: Color = Color::Rgb(255, 200, 100);

/// Left-aligned partial blocks filling one to seven eighths of a cell.
const EIGHTH_BLOCKS: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// Builds a `width`-character bar of one block's gas used relative to its gas limit.
///
/// Each cell resolves eighths using partial block glyphs. Gas up to `gas_target`
/// fades from cool to bright blue as it approaches `gas_target`, then from warm
/// yellow at the target to hot red at the limit. While the bar has not reached
/// the target's cell, a `│` marks it.
pub fn build_gas_bar(
    gas_used: u64,
    gas_limit: u64,
    gas_target: Option<u64>,
    width: usize,
) -> Line<'static> {
    if gas_limit == 0 {
        return Line::from("-");
    }
    let units = width as u128 * 8;
    // Round up so any gas usage is visible.
    let filled =
        (u128::from(gas_used.min(gas_limit)) * units).div_ceil(u128::from(gas_limit)) as usize;
    let target =
        gas_target.map(|t| (u128::from(t.min(gas_limit)) * units / u128::from(gas_limit)) as usize);

    let spans: Vec<Span<'static>> = (0..width)
        .map(|i| {
            let cell_units = filled.saturating_sub(i * 8).min(8);
            if cell_units > 0 {
                let cell_end = i * 8 + cell_units;
                let color = match target {
                    Some(t) if cell_end > t => {
                        let excess = (cell_end - t) as f64 / (units as usize - t).max(1) as f64;
                        Format::lerp_rgb(GAS_COLOR_WARM, GAS_COLOR_HOT, excess.clamp(0.0, 1.0))
                    }
                    Some(t) => {
                        let progress = cell_end as f64 / t.max(1) as f64;
                        Format::lerp_rgb(GAS_COLOR_COOL, GAS_COLOR_BRIGHT, progress.clamp(0.0, 1.0))
                    }
                    None => COLOR_GAS_FILL,
                };
                let glyph = if cell_units == 8 { "█" } else { EIGHTH_BLOCKS[cell_units - 1] };
                Span::styled(glyph, Style::default().fg(color))
            } else if target.is_some_and(|t| t / 8 == i) {
                Span::styled("│", Style::default().fg(COLOR_GAS_TARGET))
            } else {
                Span::styled("·", Style::default().fg(Color::DarkGray))
            }
        })
        .collect();
    Line::from(spans)
}

/// Returns how many rows a table must build to show its selection: the selected
/// row plus one screen. Building only these keeps redraws cheap when the
/// backing list holds thousands of blocks.
pub fn table_rows_to_build(table_state: &TableState, visible_rows: u16) -> usize {
    table_state.selected().unwrap_or(0).saturating_add(usize::from(visible_rows))
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
    pub connection_mode: Option<ConnectionMode>,
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
    let mode_label = connection_mode.map_or("", ConnectionMode::title_suffix);
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
                Cell::from(Format::truncate_block_number(l1_block.block_number, l1_col_width)),
                Cell::from(l1_block.blobs_display()).style(blobs_style),
                Cell::from(l1_block.l2_blocks_display()),
                Cell::from(l1_block.compression_display()),
                Cell::from(Format::duration(Duration::from_secs(l1_block.age_seconds()))),
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
        let text = format!("{empty_bar} {:>8}", Format::bytes(0));
        let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
        f.render_widget(para, inner);
        return;
    }

    let total_backlog = tracker.da_backlog_bytes;
    let drawn_bytes: u64 = backlog_blocks.iter().map(|c| c.da_bytes).sum();
    // Place each block by cumulative bytes so hundreds of sub-second blocks keep
    // their true proportions instead of each claiming at least one cell.
    let cells_for = |bytes: u64| {
        (u128::from(bytes) * bar_width as u128).checked_div(u128::from(drawn_bytes)).unwrap_or(0)
            as usize
    };
    let mut spans: Vec<Span<'_>> = Vec::new();
    let mut chars_used = 0usize;
    let mut cumulative_bytes = 0u64;

    for contrib in backlog_blocks.iter().rev() {
        let color = Format::block_color(contrib.block_number);
        let is_highlighted = highlighted_block == Some(contrib.block_number);

        cumulative_bytes += contrib.da_bytes;
        let char_count = cells_for(cumulative_bytes).min(bar_width).saturating_sub(chars_used);

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

    let backlog_color = Format::backlog_size_color(total_backlog);
    spans.push(Span::styled(
        format!(" {:>8}", Format::bytes(total_backlog)),
        Style::default().fg(backlog_color).add_modifier(Modifier::BOLD),
    ));

    let line = Line::from(spans);
    let para = Paragraph::new(line);
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use ratatui::{style::Color, widgets::TableState};

    use super::{COLOR_GAS_FILL, build_gas_bar, table_rows_to_build};

    #[test]
    fn table_builds_rows_through_one_screen_past_the_selection() {
        assert_eq!(table_rows_to_build(&TableState::default(), 20), 20);
        assert_eq!(table_rows_to_build(&TableState::default().with_selected(Some(500)), 20), 520);
    }

    fn bar_text(gas_used: u64, gas_target: Option<u64>) -> String {
        build_gas_bar(gas_used, 1_000, gas_target, 10).to_string()
    }

    #[test]
    fn gas_bar_fills_proportionally_to_gas_limit() {
        assert_eq!(bar_text(0, None), "··········");
        assert_eq!(bar_text(500, None), "█████·····");
        assert_eq!(bar_text(1_000, None), "██████████");
        // Gas beyond the limit is clamped and tiny usage still shows one cell.
        assert_eq!(bar_text(5_000, None), "██████████");
        assert_eq!(bar_text(1, None), "▏·········");
        // Each cell resolves eighths: 530/1000 of 80 units is 43 units.
        assert_eq!(bar_text(530, None), "█████▍····");
    }

    #[test]
    fn gas_bar_marks_target_until_filled_past_it() {
        assert_eq!(bar_text(100, Some(500)), "█····│····");
        assert_eq!(bar_text(800, Some(500)), "████████··");
    }

    #[test]
    fn gas_bar_grades_cool_to_bright_below_target_and_warm_to_hot_above() {
        let bar = build_gas_bar(1_000, 1_000, Some(500), 10);
        let color = |cell: usize| bar.spans[cell].style.fg;

        // Below target: the first cell is 20% of the way to bright; the target cell is bright.
        assert_eq!(color(0), Some(Color::Rgb(56, 112, 211)));
        assert_eq!(color(4), Some(Color::Rgb(120, 200, 255)));
        // One cell past target is 20% of the way from warm to hot; the last cell is hot.
        assert_eq!(color(5), Some(Color::Rgb(255, 172, 76)));
        assert_eq!(color(9), Some(Color::Rgb(255, 60, 60)));
    }

    #[test]
    fn gas_bar_without_target_uses_a_single_fill() {
        let bar = build_gas_bar(1_000, 1_000, None, 10);
        assert!(bar.spans.iter().all(|span| span.style.fg == Some(COLOR_GAS_FILL)));
    }

    #[test]
    fn gas_bar_without_gas_limit_is_a_placeholder() {
        assert_eq!(build_gas_bar(10, 0, Some(5), 10).to_string(), "-");
    }
}
