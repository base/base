use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

use crate::{
    app::{Action, Resources, View},
    commands::common::{
        COLOR_BASE_BLUE, COLOR_BURN, COLOR_GROWTH, L1BlockFilter, RATE_WINDOW_2M, RATE_WINDOW_5M,
        RATE_WINDOW_30S, format_duration, format_rate, render_da_backlog_bar,
        render_l1_blocks_table, truncate_block_number,
    },
    tui::Keybinding,
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "↑/k ↓/j", description: "Navigate" },
    Keybinding { key: "←/h →/l", description: "Switch panel" },
    Keybinding { key: "Tab", description: "Next panel" },
    Keybinding { key: "f", description: "Filter L1 blocks" },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    L2Blocks,
    L1Blocks,
}

#[derive(Debug)]
pub struct DaMonitorView {
    selected_panel: Panel,
    selected_row: usize,
    l1_filter: L1BlockFilter,
}

impl Default for DaMonitorView {
    fn default() -> Self {
        Self::new()
    }
}

impl DaMonitorView {
    pub const fn new() -> Self {
        Self { selected_panel: Panel::L2Blocks, selected_row: 0, l1_filter: L1BlockFilter::All }
    }

    #[allow(clippy::missing_const_for_fn)]
    fn next_panel(&mut self) {
        self.selected_panel = match self.selected_panel {
            Panel::L2Blocks => Panel::L1Blocks,
            Panel::L1Blocks => Panel::L2Blocks,
        };
        self.selected_row = 0;
    }

    fn panel_len(&self, panel: Panel, resources: &Resources) -> usize {
        match panel {
            Panel::L2Blocks => resources.da.tracker.block_contributions.len(),
            Panel::L1Blocks => resources.da.tracker.filtered_l1_blocks(self.l1_filter).count(),
        }
    }
}

impl View for DaMonitorView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_row > 0 {
                    self.selected_row -= 1;
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = self.panel_len(self.selected_panel, resources).saturating_sub(1);
                if self.selected_row < max {
                    self.selected_row += 1;
                }
                Action::None
            }
            KeyCode::Left
            | KeyCode::Char('h')
            | KeyCode::Right
            | KeyCode::Char('l')
            | KeyCode::Tab => {
                self.next_panel();
                Action::None
            }
            KeyCode::Char('f') => {
                self.l1_filter = self.l1_filter.next();
                self.selected_row = 0;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, resources: &Resources) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(5), Constraint::Min(0)])
            .split(area);

        let highlighted_block = if self.selected_panel == Panel::L2Blocks {
            resources.da.tracker.block_contributions.get(self.selected_row).map(|c| c.block_number)
        } else {
            None
        };

        render_da_backlog_bar(
            frame,
            chunks[0],
            &resources.da.tracker,
            resources.da.loading.as_ref(),
            resources.da.loaded,
            highlighted_block,
        );

        render_stats_panel(frame, chunks[1], resources, self.l1_filter);

        let panel_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);

        render_blocks_panel(
            frame,
            panel_chunks[0],
            resources,
            self.selected_panel == Panel::L2Blocks,
            self.selected_row,
        );

        render_l1_blocks_table(
            frame,
            panel_chunks[1],
            resources.da.tracker.filtered_l1_blocks(self.l1_filter),
            self.selected_panel == Panel::L1Blocks,
            if self.selected_panel == Panel::L1Blocks { self.selected_row } else { 0 },
            self.l1_filter,
            "L1 Blocks",
            resources.da.l1_connection_mode,
        );
    }
}

fn format_share(share: Option<f64>) -> String {
    share.map_or_else(|| "-".to_string(), |s| format!("{:.0}%", s * 100.0))
}

fn render_stats_panel(f: &mut Frame, area: Rect, resources: &Resources, filter: L1BlockFilter) {
    let tracker = &resources.da.tracker;

    let growth_30s = tracker.growth_tracker.rate_over(RATE_WINDOW_30S);
    let growth_2m = tracker.growth_tracker.rate_over(RATE_WINDOW_2M);
    let growth_5m = tracker.growth_tracker.rate_over(RATE_WINDOW_5M);

    let burn_30s = tracker.burn_tracker.rate_over(RATE_WINDOW_30S);
    let burn_2m = tracker.burn_tracker.rate_over(RATE_WINDOW_2M);
    let burn_5m = tracker.burn_tracker.rate_over(RATE_WINDOW_5M);

    let base_share_2m = tracker.base_blob_share(RATE_WINDOW_2M);
    let target_usage_2m =
        tracker.blob_target_usage(RATE_WINDOW_2M, resources.config.l1_blob_target);

    let time_since = tracker.last_base_blob_time.map(|t| t.elapsed());

    let lines = vec![
        Line::from(vec![
            Span::styled("Growth:  ", Style::default().fg(Color::DarkGray)),
            Span::styled("30s ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_rate(growth_30s), Style::default().fg(COLOR_GROWTH)),
            Span::raw("  "),
            Span::styled("2m ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_rate(growth_2m), Style::default().fg(COLOR_GROWTH)),
            Span::raw("  "),
            Span::styled("5m ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_rate(growth_5m), Style::default().fg(COLOR_GROWTH)),
        ]),
        Line::from(vec![
            Span::styled("Burn:    ", Style::default().fg(Color::DarkGray)),
            Span::styled("30s ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_rate(burn_30s), Style::default().fg(COLOR_BURN)),
            Span::raw("  "),
            Span::styled("2m ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_rate(burn_2m), Style::default().fg(COLOR_BURN)),
            Span::raw("  "),
            Span::styled("5m ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_rate(burn_5m), Style::default().fg(COLOR_BURN)),
        ]),
        Line::from(vec![
            Span::styled("L1 Target: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_share(target_usage_2m), Style::default().fg(Color::Yellow)),
            Span::raw("  "),
            Span::styled("Base: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_share(base_share_2m), Style::default().fg(COLOR_BASE_BLUE)),
            Span::raw("  "),
            Span::styled("Last: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                time_since.map(format_duration).unwrap_or_else(|| "-".to_string()),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("Filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(filter.label(), Style::default().fg(Color::Cyan)),
        ]),
    ];

    let block = Block::default()
        .title(" DA Stats ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

fn render_blocks_panel(
    f: &mut Frame,
    area: Rect,
    resources: &Resources,
    is_active: bool,
    selected_row: usize,
) {
    use ratatui::widgets::{Cell, Row, Table};

    use crate::commands::common::{
        COLOR_ROW_SELECTED, block_color, format_bytes as fmt_bytes, format_duration as fmt_dur,
    };

    let tracker = &resources.da.tracker;
    let border_color = if is_active { Color::Rgb(100, 255, 100) } else { Color::Green };

    let block = Block::default()
        .title(format!(" L2 Blocks (DA↑) [{}] ", tracker.block_contributions.len()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // DA(8) + Age(6) + spacing(3) = 17
    let fixed_cols_width = 8 + 6 + 3;
    let block_col_width = inner.width.saturating_sub(fixed_cols_width).clamp(4, 10) as usize;

    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("Block").style(header_style),
        Cell::from("DA").style(header_style),
        Cell::from("Age").style(header_style),
    ]);

    let rows: Vec<Row> = tracker
        .block_contributions
        .iter()
        .take(inner.height.saturating_sub(1) as usize)
        .enumerate()
        .map(|(idx, contrib)| {
            let is_selected = is_active && idx == selected_row;
            let is_safe = contrib.block_number <= tracker.safe_l2_block;

            let style = if is_selected {
                Style::default().fg(Color::White).bg(COLOR_ROW_SELECTED)
            } else {
                Style::default().fg(Color::White)
            };

            let block_style = if is_safe {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(block_color(contrib.block_number))
            };

            Row::new(vec![
                Cell::from(truncate_block_number(contrib.block_number, block_col_width))
                    .style(block_style),
                Cell::from(fmt_bytes(contrib.da_bytes)),
                Cell::from(fmt_dur(Duration::from_secs(contrib.age_seconds()))),
            ])
            .style(style)
        })
        .collect();

    let widths = [Constraint::Max(10), Constraint::Length(8), Constraint::Min(6)];
    let table = Table::new(rows, widths).header(header);
    f.render_widget(table, inner);
}
