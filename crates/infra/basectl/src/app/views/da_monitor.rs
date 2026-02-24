use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Paragraph, TableState},
};

use crate::{
    app::{Action, Resources, View, views::TransactionPane},
    commands::common::{
        COLOR_BASE_BLUE, COLOR_BURN, COLOR_GROWTH, L1_BLOCK_WINDOW, L1BlockFilter,
        L1BlocksTableParams, RATE_WINDOW_2M, RATE_WINDOW_5M, RATE_WINDOW_30S, format_duration,
        format_rate, render_da_backlog_bar, render_l1_blocks_table, target_usage_color,
        truncate_block_number,
    },
    tui::Keybinding,
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "↑/k ↓/j", description: "Navigate" },
    Keybinding { key: "g/G", description: "Top/Bottom" },
    Keybinding { key: "←/h →/l", description: "Switch panel" },
    Keybinding { key: "Tab", description: "Next panel" },
    Keybinding { key: "f", description: "Filter L1 blocks" },
    Keybinding { key: "Enter", description: "View transactions" },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    L2Blocks,
    L1Blocks,
    Txns,
}

/// View for monitoring data availability backlog and L1 blob submissions.
#[derive(Debug)]
pub(crate) struct DaMonitorView {
    selected_panel: Panel,
    l2_table_state: TableState,
    l1_table_state: TableState,
    l1_filter: L1BlockFilter,
    tx_pane: Option<TransactionPane>,
}

impl Default for DaMonitorView {
    fn default() -> Self {
        Self::new()
    }
}

impl DaMonitorView {
    /// Creates a new DA monitor view with default panel selection.
    pub(crate) fn new() -> Self {
        let mut l2_table_state = TableState::default();
        l2_table_state.select(Some(0));
        let mut l1_table_state = TableState::default();
        l1_table_state.select(Some(0));
        Self {
            selected_panel: Panel::L2Blocks,
            l2_table_state,
            l1_table_state,
            l1_filter: L1BlockFilter::All,
            tx_pane: None,
        }
    }

    const fn active_table_state(&mut self) -> &mut TableState {
        match self.selected_panel {
            Panel::L2Blocks | Panel::Txns => &mut self.l2_table_state,
            Panel::L1Blocks => &mut self.l1_table_state,
        }
    }

    const fn next_panel(&mut self) {
        self.selected_panel = match self.selected_panel {
            Panel::L2Blocks => {
                if self.tx_pane.is_some() {
                    Panel::Txns
                } else {
                    Panel::L1Blocks
                }
            }
            Panel::Txns => Panel::L1Blocks,
            Panel::L1Blocks => Panel::L2Blocks,
        };
    }

    fn panel_len(&self, panel: Panel, resources: &Resources) -> usize {
        match panel {
            Panel::L2Blocks => resources.da.tracker.block_contributions.len(),
            Panel::L1Blocks => resources.da.tracker.filtered_l1_blocks(self.l1_filter).count(),
            Panel::Txns => 0,
        }
    }
}

impl View for DaMonitorView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn consumes_esc(&self) -> bool {
        self.tx_pane.is_some() && self.selected_panel == Panel::Txns
    }

    fn consumes_quit(&self) -> bool {
        self.tx_pane.is_some() && self.selected_panel == Panel::Txns
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        match key.code {
            KeyCode::Enter if self.selected_panel == Panel::L2Blocks => {
                let row = self.l2_table_state.selected().unwrap_or(0);
                if let Some(contrib) = resources.da.tracker.block_contributions.get(row) {
                    let block_number = contrib.block_number;
                    let title = format!("Block {block_number}");

                    let block_entries: Vec<_> = resources
                        .flash
                        .entries
                        .iter()
                        .filter(|e| e.block_number == block_number)
                        .collect();

                    self.tx_pane = Some(TransactionPane::for_block(
                        block_number,
                        title,
                        &block_entries,
                        resources.config.rpc.as_str(),
                        resources.config.explorer_base_url(),
                    ));
                    self.selected_panel = Panel::Txns;
                }
                Action::None
            }
            _ if self.selected_panel == Panel::Txns && self.tx_pane.is_some() => {
                let pane = self.tx_pane.as_mut().unwrap();
                let should_close = pane.handle_key(key, &mut |toast| {
                    resources.toasts.push(toast);
                });
                if should_close {
                    self.tx_pane = None;
                    self.selected_panel = Panel::L2Blocks;
                }
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let state = self.active_table_state();
                if let Some(selected) = state.selected()
                    && selected > 0
                {
                    state.select(Some(selected - 1));
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = self.panel_len(self.selected_panel, resources).saturating_sub(1);
                let state = self.active_table_state();
                if let Some(selected) = state.selected()
                    && selected < max
                {
                    state.select(Some(selected + 1));
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
                self.l1_table_state.select(Some(0));
                Action::None
            }
            KeyCode::Char('g') => {
                self.active_table_state().select(Some(0));
                Action::None
            }
            KeyCode::Char('G') => {
                let max = self.panel_len(self.selected_panel, resources).saturating_sub(1);
                self.active_table_state().select(Some(max));
                Action::None
            }
            _ => Action::None,
        }
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        if let Some(ref mut pane) = self.tx_pane {
            pane.poll();
        }
        let _ = resources;
        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(5), Constraint::Min(0)])
            .split(area);

        let l2_selected = self.l2_table_state.selected().unwrap_or(0);
        let highlighted_block = if self.selected_panel == Panel::L2Blocks {
            resources.da.tracker.block_contributions.get(l2_selected).map(|c| c.block_number)
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

        let panel_chunks = if self.tx_pane.is_some() {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(25),
                    Constraint::Percentage(25),
                    Constraint::Percentage(50),
                ])
                .split(chunks[2])
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(chunks[2])
        };

        render_blocks_panel(
            frame,
            panel_chunks[0],
            resources,
            self.selected_panel == Panel::L2Blocks,
            &self.l2_table_state,
        );

        render_l1_blocks_table(
            frame,
            panel_chunks[1],
            L1BlocksTableParams {
                l1_blocks: resources.da.tracker.filtered_l1_blocks(self.l1_filter),
                is_active: self.selected_panel == Panel::L1Blocks,
                table_state: &mut self.l1_table_state,
                filter: self.l1_filter,
                title: "L1 Blocks",
                connection_mode: resources.da.l1_connection_mode,
            },
        );

        if let Some(ref mut pane) = self.tx_pane {
            pane.render(frame, panel_chunks[2], self.selected_panel == Panel::Txns);
        }
    }
}

fn format_share(share: Option<f64>) -> String {
    share.map_or_else(|| "-".to_string(), |s| format!("{:.0}%", s * 100.0))
}

fn render_stats_panel(f: &mut Frame<'_>, area: Rect, resources: &Resources, filter: L1BlockFilter) {
    let tracker = &resources.da.tracker;

    let growth_30s = tracker.growth_tracker.rate_over(RATE_WINDOW_30S);
    let growth_2m = tracker.growth_tracker.rate_over(RATE_WINDOW_2M);
    let growth_5m = tracker.growth_tracker.rate_over(RATE_WINDOW_5M);

    let burn_30s = tracker.burn_tracker.rate_over(RATE_WINDOW_30S);
    let burn_2m = tracker.burn_tracker.rate_over(RATE_WINDOW_2M);
    let burn_5m = tracker.burn_tracker.rate_over(RATE_WINDOW_5M);

    let base_share = tracker.base_blob_share(L1_BLOCK_WINDOW);
    let target_usage = tracker.blob_target_usage(L1_BLOCK_WINDOW, resources.config.l1_blob_target);

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
            Span::styled(
                format_share(target_usage),
                Style::default().fg(target_usage.map_or(Color::DarkGray, target_usage_color)),
            ),
            Span::raw("  "),
            Span::styled("Base: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_share(base_share), Style::default().fg(COLOR_BASE_BLUE)),
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
    f: &mut Frame<'_>,
    area: Rect,
    resources: &Resources,
    is_active: bool,
    table_state: &TableState,
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

    let fixed_cols_width = 8 + 5 + 6 + 4;
    let block_col_width = inner.width.saturating_sub(fixed_cols_width).clamp(4, 10) as usize;

    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("Block").style(header_style),
        Cell::from("Txs").style(header_style),
        Cell::from("DA").style(header_style),
        Cell::from("Age").style(header_style),
    ]);

    let selected_row = table_state.selected();

    let rows: Vec<Row<'_>> = tracker
        .block_contributions
        .iter()
        .enumerate()
        .map(|(idx, contrib)| {
            let is_selected = is_active && selected_row == Some(idx);
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

            let tx_str =
                if contrib.tx_count > 0 { contrib.tx_count.to_string() } else { "-".to_string() };

            Row::new(vec![
                Cell::from(truncate_block_number(contrib.block_number, block_col_width))
                    .style(block_style),
                Cell::from(tx_str),
                Cell::from(fmt_bytes(contrib.da_bytes)),
                Cell::from(fmt_dur(Duration::from_secs(contrib.age_seconds()))),
            ])
            .style(style)
        })
        .collect();

    let widths =
        [Constraint::Max(10), Constraint::Length(5), Constraint::Length(8), Constraint::Min(6)];
    let table = Table::new(rows, widths).header(header);
    f.render_stateful_widget(table, inner, &mut table_state.clone());
}
