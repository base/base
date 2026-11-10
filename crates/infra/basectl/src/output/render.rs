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

const COLOR_GAS_FILL: Color = Color::Rgb(100, 180, 255);
const COLOR_GAS_OVER_TARGET: Color = Color::Rgb(255, 120, 60);
const COLOR_GAS_TARGET: Color = Color::Rgb(255, 200, 100);

/// Builds a `width`-character bar of one block's gas used relative to its gas limit.
///
/// Gas up to `gas_target` is drawn blue and gas above it orange; when the block
/// stays under target, a `│` marks where the target is.
pub fn build_gas_bar(
    gas_used: u64,
    gas_limit: u64,
    gas_target: Option<u64>,
    width: usize,
) -> Line<'static> {
    if gas_limit == 0 {
        return Line::from("-");
    }
    let to_chars = |gas: u64| {
        (u128::from(gas.min(gas_limit)) * width as u128 / u128::from(gas_limit)) as usize
    };
    // Round partially filled cells up so any gas usage is visible.
    let filled = (u128::from(gas_used.min(gas_limit)) * width as u128)
        .div_ceil(u128::from(gas_limit)) as usize;
    let target = gas_target.map(to_chars);

    let spans: Vec<Span<'static>> = (0..width)
        .map(|i| {
            if i < filled {
                let color = if target.is_some_and(|t| i >= t) {
                    COLOR_GAS_OVER_TARGET
                } else {
                    COLOR_GAS_FILL
                };
                Span::styled("█", Style::default().fg(color))
            } else if target == Some(i) {
                Span::styled("│", Style::default().fg(COLOR_GAS_TARGET))
            } else {
                Span::styled("·", Style::default().fg(Color::DarkGray))
            }
        })
        .collect();
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
    let mut spans: Vec<Span<'_>> = Vec::new();
    let mut chars_used = 0usize;

    for contrib in backlog_blocks.iter().rev() {
        let color = Format::block_color(contrib.block_number);
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
    use super::build_gas_bar;

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
        assert_eq!(bar_text(1, None), "█·········");
    }

    #[test]
    fn gas_bar_marks_target_until_filled_past_it() {
        assert_eq!(bar_text(100, Some(500)), "█····│····");
        assert_eq!(bar_text(800, Some(500)), "████████··");
    }

    #[test]
    fn gas_bar_without_gas_limit_is_a_placeholder() {
        assert_eq!(build_gas_bar(10, 0, Some(5), 10).to_string(), "-");
    }
}
