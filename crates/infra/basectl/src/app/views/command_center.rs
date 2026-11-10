use std::{collections::VecDeque, time::Duration};

use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use crate::{
    app::{
        Action, L1_BLOCK_WINDOW, L1BlockFilter, RATE_WINDOW_2M, Resources, View,
        views::TransactionPane,
    },
    output::{
        COLOR_BASE_BLUE, COLOR_BURN, COLOR_GROWTH, COLOR_ROW_HIGHLIGHTED, COLOR_ROW_SELECTED,
        Format, L1BlocksTableParams, build_gas_bar, render_da_backlog_bar, render_l1_blocks_table,
        table_rows_to_build,
    },
    rpc::{BlockDaInfo, ConnectionMode},
    tui::{Browser, Keybinding, Toast},
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "←/→/Tab/1-3", description: "Switch panel" },
    Keybinding { key: "↑/k", description: "Move up" },
    Keybinding { key: "↓/j", description: "Move down" },
    Keybinding { key: "g/G", description: "Top/Bottom" },
    Keybinding { key: "Space", description: "Pause L2 block lists" },
    Keybinding { key: "y", description: "Copy block/tx hash" },
    Keybinding { key: "o", description: "Open in explorer" },
    Keybinding { key: "f", description: "Filter L1 blocks" },
    Keybinding { key: "Enter", description: "View transactions" },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    Blocks,
    Da,
    L1Blocks,
    Txns,
}

/// Combined monitoring view with L2 gas, DA, and L1 block panels.
#[derive(Debug)]
pub struct CommandCenterView {
    focused_panel: Panel,
    blocks_table_state: TableState,
    da_table_state: TableState,
    l1_table_state: TableState,
    highlighted_block: Option<u64>,
    /// Copy of the L2 block list taken when the lists were paused. Rendering from
    /// a copy keeps the frozen rows even as the tracker evicts old blocks.
    paused_blocks: Option<VecDeque<BlockDaInfo>>,
    l1_filter: L1BlockFilter,
    tx_pane: Option<TransactionPane>,
    tx_origin_panel: Option<Panel>,
}

impl Default for CommandCenterView {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandCenterView {
    /// Creates a new command center view with default panel selection.
    pub fn new() -> Self {
        let mut blocks_table_state = TableState::default();
        blocks_table_state.select(Some(0));
        let mut da_table_state = TableState::default();
        da_table_state.select(Some(0));
        let mut l1_table_state = TableState::default();
        l1_table_state.select(Some(0));
        Self {
            focused_panel: Panel::Blocks,
            blocks_table_state,
            da_table_state,
            l1_table_state,
            highlighted_block: None,
            paused_blocks: None,
            l1_filter: L1BlockFilter::All,
            tx_pane: None,
            tx_origin_panel: None,
        }
    }

    const fn next_panel(&mut self) {
        self.focused_panel = match self.focused_panel {
            Panel::Blocks => Panel::Da,
            Panel::Da => Panel::L1Blocks,
            Panel::L1Blocks => {
                if self.tx_pane.is_some() {
                    Panel::Txns
                } else {
                    Panel::Blocks
                }
            }
            Panel::Txns => Panel::Blocks,
        };
    }

    const fn prev_panel(&mut self) {
        self.focused_panel = match self.focused_panel {
            Panel::Blocks => {
                if self.tx_pane.is_some() {
                    Panel::Txns
                } else {
                    Panel::L1Blocks
                }
            }
            Panel::Da => Panel::Blocks,
            Panel::L1Blocks => Panel::Da,
            Panel::Txns => Panel::L1Blocks,
        };
    }

    const fn active_table_state(&mut self) -> &mut TableState {
        match self.focused_panel {
            Panel::Blocks => &mut self.blocks_table_state,
            Panel::Da | Panel::Txns => &mut self.da_table_state,
            Panel::L1Blocks => &mut self.l1_table_state,
        }
    }

    fn selected_row(&self, panel: Panel) -> usize {
        match panel {
            Panel::Blocks => self.blocks_table_state.selected().unwrap_or(0),
            Panel::Da | Panel::Txns => self.da_table_state.selected().unwrap_or(0),
            Panel::L1Blocks => self.l1_table_state.selected().unwrap_or(0),
        }
    }

    /// Returns the L2 blocks the Blocks and DA panels show: the paused copy, or the live list.
    fn shown_blocks<'a>(&'a self, resources: &'a Resources) -> &'a VecDeque<BlockDaInfo> {
        self.paused_blocks.as_ref().unwrap_or(&resources.da.tracker.block_contributions)
    }

    /// Number of live blocks newer than the paused copy, or `None` when not paused.
    fn blocks_since_pause(&self, resources: &Resources) -> Option<usize> {
        let paused = self.paused_blocks.as_ref()?;
        let newest_paused = paused.front().map_or(0, |b| b.block_number);
        Some(
            resources
                .da
                .tracker
                .block_contributions
                .iter()
                .take_while(|b| b.block_number > newest_paused)
                .count(),
        )
    }

    /// Returns the L2 block at `row` of the Blocks and DA panels.
    fn visible_block<'a>(
        &'a self,
        resources: &'a Resources,
        row: usize,
    ) -> Option<&'a BlockDaInfo> {
        self.shown_blocks(resources).get(row)
    }

    /// Returns how many rows the Blocks and DA panels show.
    fn visible_block_count(&self, resources: &Resources) -> usize {
        self.shown_blocks(resources).len()
    }

    fn update_highlighted_block(&mut self, resources: &Resources) {
        let row = self.selected_row(self.focused_panel);
        self.highlighted_block = match self.focused_panel {
            Panel::Blocks | Panel::Da => self.visible_block(resources, row).map(|c| c.block_number),
            Panel::Txns => self.tx_pane.as_ref().map(|p| p.block_number),
            Panel::L1Blocks => None,
        };
    }

    fn get_copyable_block(&self, resources: &Resources) -> Option<String> {
        let row = self.selected_row(self.focused_panel);
        match self.focused_panel {
            Panel::Blocks | Panel::Da => {
                self.visible_block(resources, row).map(|c| c.block_number.to_string())
            }
            Panel::Txns => self.tx_pane.as_ref().map(|p| p.block_number.to_string()),
            Panel::L1Blocks => resources
                .da
                .tracker
                .filtered_l1_blocks(self.l1_filter)
                .nth(row)
                .map(|b| b.block_number.to_string()),
        }
    }

    fn open_selected_block_in_explorer(&self, resources: &mut Resources) {
        let row = self.selected_row(self.focused_panel);
        let url = match self.focused_panel {
            Panel::Blocks | Panel::Da => {
                resources.config.explorer_base_url().and_then(|base_url| {
                    self.visible_block(resources, row)
                        .map(|c| format!("{base_url}/block/{}", c.block_number))
                })
            }
            Panel::Txns => resources.config.explorer_base_url().and_then(|base_url| {
                self.tx_pane.as_ref().map(|p| format!("{base_url}/block/{}", p.block_number))
            }),
            Panel::L1Blocks => resources.config.l1_explorer_base_url().and_then(|base_url| {
                resources
                    .da
                    .tracker
                    .filtered_l1_blocks(self.l1_filter)
                    .nth(row)
                    .map(|b| format!("{base_url}/block/{}", b.block_number))
            }),
        };

        if let Some(url) = url {
            resources.toasts.push(Browser::open(&url));
        }
    }
}

impl View for CommandCenterView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn consumes_esc(&self) -> bool {
        self.tx_pane.is_some() && self.focused_panel == Panel::Txns
    }

    fn consumes_quit(&self) -> bool {
        self.tx_pane.is_some() && self.focused_panel == Panel::Txns
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        match key.code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.next_panel();
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.prev_panel();
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::Char('1') => {
                self.focused_panel = Panel::Blocks;
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::Char('2') => {
                self.focused_panel = Panel::Da;
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::Char('3') => {
                self.focused_panel = Panel::L1Blocks;
                self.update_highlighted_block(resources);
                Action::None
            }
            _ if self.focused_panel == Panel::Txns && self.tx_pane.is_some() => {
                let pane = self.tx_pane.as_mut().unwrap();
                let should_close = pane.handle_key(key, &mut |toast| {
                    resources.toasts.push(toast);
                });
                if should_close {
                    self.tx_pane = None;
                    self.focused_panel = self.tx_origin_panel.take().unwrap_or(Panel::Blocks);
                }
                Action::None
            }
            KeyCode::Char(' ') => {
                self.paused_blocks = match self.paused_blocks {
                    Some(_) => None,
                    None => Some(resources.da.tracker.block_contributions.clone()),
                };
                Action::None
            }
            KeyCode::Char('f') => {
                self.l1_filter = self.l1_filter.next();
                self.l1_table_state.select(Some(0));
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let state = self.active_table_state();
                if let Some(selected) = state.selected()
                    && selected > 0
                {
                    state.select(Some(selected - 1));
                }
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let block_rows = self.visible_block_count(resources);
                let (state, max) = match self.focused_panel {
                    Panel::Blocks => (&mut self.blocks_table_state, block_rows.saturating_sub(1)),
                    Panel::Da | Panel::Txns => {
                        (&mut self.da_table_state, block_rows.saturating_sub(1))
                    }
                    Panel::L1Blocks => (
                        &mut self.l1_table_state,
                        resources
                            .da
                            .tracker
                            .filtered_l1_blocks(self.l1_filter)
                            .count()
                            .saturating_sub(1),
                    ),
                };
                if let Some(selected) = state.selected()
                    && selected < max
                {
                    state.select(Some(selected + 1));
                }
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::Char('g') => {
                self.active_table_state().select(Some(0));
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::Char('G') => {
                let max = match self.focused_panel {
                    Panel::Blocks | Panel::Da | Panel::Txns => self.visible_block_count(resources),
                    Panel::L1Blocks => {
                        resources.da.tracker.filtered_l1_blocks(self.l1_filter).count()
                    }
                }
                .saturating_sub(1);
                self.active_table_state().select(Some(max));
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::Char('y') => {
                if let Some(block_num) = self.get_copyable_block(resources)
                    && let Ok(mut clipboard) = Clipboard::new()
                    && clipboard.set_text(&block_num).is_ok()
                {
                    resources.toasts.push(Toast::info(format!("Copied {block_num}")));
                }
                Action::None
            }
            KeyCode::Char('o') => {
                self.open_selected_block_in_explorer(resources);
                Action::None
            }
            KeyCode::Enter => {
                let origin = self.focused_panel;
                if matches!(origin, Panel::Blocks | Panel::Da) {
                    let row = self.selected_row(origin);
                    if let Some(contrib) = self.visible_block(resources, row) {
                        self.tx_pane = Some(TransactionPane::for_block(
                            contrib.block_number,
                            resources.config.rpc.as_str(),
                            resources.config.explorer_base_url(),
                        ));
                        self.tx_origin_panel = Some(origin);
                        self.focused_panel = Panel::Txns;
                    }
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        if let Some(ref mut pane) = self.tx_pane {
            pane.poll();
        }
        let at_top = self.selected_row(self.focused_panel) == 0;
        if at_top {
            self.update_highlighted_block(resources);
        }
        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(5), Constraint::Min(0)])
            .split(area);

        render_da_backlog_bar(
            frame,
            main_chunks[0],
            &resources.da.tracker,
            resources.da.loading.as_ref(),
            resources.da.loaded,
            self.highlighted_block,
        );

        let info_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(main_chunks[1]);

        render_config_panel(frame, info_chunks[0], resources);
        render_stats_panel(frame, info_chunks[1], resources);

        let (panel_chunks, tx_pane_area) = if self.tx_pane.is_some() {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(25),
                    Constraint::Percentage(12),
                    Constraint::Percentage(13),
                    Constraint::Percentage(50),
                ])
                .split(main_chunks[2]);
            let tx_pane_area = chunks[3];
            (chunks, Some(tx_pane_area))
        } else {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(40),
                    Constraint::Percentage(30),
                    Constraint::Percentage(30),
                ])
                .split(main_chunks[2]);
            (chunks, None)
        };

        render_blocks_panel(frame, panel_chunks[0], resources, self);

        render_da_panel(frame, panel_chunks[1], resources, self);

        render_l1_blocks_table(
            frame,
            panel_chunks[2],
            L1BlocksTableParams {
                l1_blocks: resources.da.tracker.filtered_l1_blocks(self.l1_filter),
                is_active: self.focused_panel == Panel::L1Blocks,
                table_state: &mut self.l1_table_state,
                filter: self.l1_filter,
                title: "L1 Blocks",
                connection_mode: resources.da.l1_connection_mode,
            },
        );

        if let (Some(ref mut pane), Some(tx_pane_area)) = (self.tx_pane.as_mut(), tx_pane_area) {
            pane.render(frame, tx_pane_area, self.focused_panel == Panel::Txns);
        }
    }
}

fn render_config_panel(f: &mut Frame<'_>, area: Rect, resources: &Resources) {
    let block = Block::default()
        .title(" Gas Config ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));

    // Gas parameters come from the newest L2 header: the L2 applies its own values,
    // which can differ from the L1 `SystemConfig` (Denim scaled them once at activation).
    let gas_line = resources.da.tracker.block_contributions.front().map_or_else(
        || Line::from(Span::styled("Waiting for blocks...", Style::default().fg(Color::DarkGray))),
        |block| {
            let elasticity = block.eip1559_elasticity.filter(|&e| e > 0);
            let gas_target = elasticity.map(|e| block.gas_limit / u64::from(e));
            let param = |value: Option<String>| value.unwrap_or_else(|| "-".to_string());
            Line::from(vec![
                Span::styled("Target: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    param(gas_target.map(format_gas_value)),
                    Style::default().fg(Color::Green),
                ),
                Span::raw("  "),
                Span::styled("Limit: ", Style::default().fg(Color::DarkGray)),
                Span::styled(format_gas_value(block.gas_limit), Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled("E/D: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "{}/{}",
                        param(block.eip1559_elasticity.map(|e| e.to_string())),
                        param(block.eip1559_denominator.map(|d| d.to_string()))
                    ),
                    Style::default().fg(Color::Cyan),
                ),
            ])
        },
    );

    let scalars_line = resources.system_config.as_ref().map_or_else(
        || Line::from(Span::styled("Loading...", Style::default().fg(Color::DarkGray))),
        |sys| {
            let basefee_scalar =
                sys.base_fee_scalar.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());
            let blobbasefee_scalar =
                sys.blob_base_fee_scalar.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());

            Line::from(vec![
                Span::styled("DA Scalars: ", Style::default().fg(Color::DarkGray)),
                Span::styled("base=", Style::default().fg(Color::DarkGray)),
                Span::styled(basefee_scalar, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled("blob=", Style::default().fg(Color::DarkGray)),
                Span::styled(blobbasefee_scalar, Style::default().fg(Color::Yellow)),
            ])
        },
    );

    let para = Paragraph::new(vec![gas_line, scalars_line]).block(block);
    f.render_widget(para, area);
}

fn format_gas_value(gas: u64) -> String {
    if gas >= 1_000_000 {
        format!("{:.1}M", gas as f64 / 1_000_000.0)
    } else if gas >= 1_000 {
        format!("{:.1}K", gas as f64 / 1_000.0)
    } else {
        gas.to_string()
    }
}

fn render_stats_panel(f: &mut Frame<'_>, area: Rect, resources: &Resources) {
    let tracker = &resources.da.tracker;

    let backlog_color = Format::backlog_size_color(tracker.da_backlog_bytes);
    let growth_rate = tracker.growth_tracker.rate_over(RATE_WINDOW_2M);
    let burn_rate = tracker.burn_tracker.rate_over(RATE_WINDOW_2M);
    let time_since = tracker.last_base_blob_time.map(|t| t.elapsed());
    let base_share = tracker.base_blob_share(L1_BLOCK_WINDOW);
    let target_usage = tracker.blob_target_usage(L1_BLOCK_WINDOW, resources.config.l1_blob_target);

    let lines = vec![
        Line::from(vec![
            Span::styled("Rate: ", Style::default().fg(Color::DarkGray)),
            Span::styled("↑", Style::default().fg(COLOR_GROWTH)),
            Span::styled(Format::rate(growth_rate), Style::default().fg(COLOR_GROWTH)),
            Span::raw(" "),
            Span::styled("↓", Style::default().fg(COLOR_BURN)),
            Span::styled(Format::rate(burn_rate), Style::default().fg(COLOR_BURN)),
        ]),
        Line::from(vec![
            Span::styled("DA: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                Format::bytes(tracker.da_backlog_bytes),
                Style::default().fg(backlog_color),
            ),
            Span::raw("  "),
            Span::styled("L1: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                target_usage.map_or_else(|| "-".to_string(), |u| format!("{:.0}%", u * 100.0)),
                Style::default()
                    .fg(target_usage.map_or(Color::DarkGray, Format::target_usage_color)),
            ),
            Span::raw("  "),
            Span::styled("Base: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                base_share.map_or_else(|| "-".to_string(), |s| format!("{:.0}%", s * 100.0)),
                Style::default().fg(COLOR_BASE_BLUE),
            ),
            Span::raw("  "),
            Span::styled("Last: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                time_since.map(Format::duration).unwrap_or_else(|| "-".to_string()),
                Style::default().fg(Color::White),
            ),
        ]),
    ];

    let block = Block::default()
        .title(" Stats ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

fn render_da_panel(f: &mut Frame<'_>, area: Rect, resources: &Resources, view: &CommandCenterView) {
    let tracker = &resources.da.tracker;
    let blocks = view.shown_blocks(resources);
    let is_active = view.focused_panel == Panel::Da;
    let table_state = &view.da_table_state;
    let highlighted_block = view.highlighted_block;
    let blocks_since_pause = view.blocks_since_pause(resources);
    let border_color = if is_active { Color::Rgb(100, 255, 100) } else { Color::Green };

    let mode_label = resources.da.l2_connection_mode.map_or("", ConnectionMode::title_suffix);
    let block = Block::default()
        .title(format!(" L2 Blocks (DA↑){mode_label}{} ", paused_label(blocks_since_pause)))
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

    let rows: Vec<Row<'_>> = blocks
        .iter()
        .take(table_rows_to_build(table_state, inner.height))
        .enumerate()
        .map(|(idx, contrib)| {
            let is_selected = is_active && selected_row == Some(idx);
            let is_highlighted = highlighted_block == Some(contrib.block_number);
            let is_safe = contrib.block_number <= tracker.safe_l2_block;

            let style = if is_selected {
                Style::default().fg(Color::White).bg(COLOR_ROW_SELECTED)
            } else if is_highlighted {
                Style::default().fg(Color::White).bg(COLOR_ROW_HIGHLIGHTED)
            } else {
                Style::default().fg(Color::White)
            };

            let block_style = if is_safe {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Format::block_color(contrib.block_number))
            };

            let tx_str =
                if contrib.tx_count > 0 { contrib.tx_count.to_string() } else { "-".to_string() };

            Row::new(vec![
                Cell::from(Format::truncate_block_number(contrib.block_number, block_col_width))
                    .style(block_style),
                Cell::from(tx_str),
                Cell::from(Format::bytes(contrib.da_bytes)),
                Cell::from(Format::duration(Duration::from_secs(contrib.age_seconds()))),
            ])
            .style(style)
        })
        .collect();

    let widths =
        [Constraint::Max(10), Constraint::Length(5), Constraint::Length(8), Constraint::Min(6)];
    let table = Table::new(rows, widths).header(header);
    f.render_stateful_widget(table, inner, &mut table_state.clone());
}

/// Narrowest and widest the gas bar gets as the panel resizes.
const MIN_GAS_BAR_CHARS: u16 = 10;
const MAX_GAS_BAR_CHARS: u16 = 60;
/// Width of every blocks-panel column except the gas bar, plus column spacing.
const BLOCKS_PANEL_OTHER_COLS_WIDTH: u16 = 10 + 4 + 12 + 3;

fn render_blocks_panel(
    f: &mut Frame<'_>,
    area: Rect,
    resources: &Resources,
    view: &CommandCenterView,
) {
    let blocks = view.shown_blocks(resources);
    let is_active = view.focused_panel == Panel::Blocks;
    let table_state = &view.blocks_table_state;
    let highlighted_block = view.highlighted_block;
    let blocks_since_pause = view.blocks_since_pause(resources);
    let border_color = if is_active { Color::Rgb(100, 180, 255) } else { COLOR_BASE_BLUE };
    let mode_label = resources.da.l2_connection_mode.map_or("", ConnectionMode::title_suffix);

    let block = Block::default()
        .title(format!(" L2 Blocks (Gas){mode_label}{} ", paused_label(blocks_since_pause)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let gas_bar_chars = inner
        .width
        .saturating_sub(BLOCKS_PANEL_OTHER_COLS_WIDTH)
        .clamp(MIN_GAS_BAR_CHARS, MAX_GAS_BAR_CHARS);
    let fixed_cols_width = 4 + gas_bar_chars + 12 + 3;
    let block_col_width = inner.width.saturating_sub(fixed_cols_width).clamp(4, 10) as usize;
    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("Block").style(header_style),
        Cell::from("Txs").style(header_style),
        Cell::from("Gas").style(header_style),
        Cell::from("Base Fee").style(header_style),
    ]);

    let selected_row = table_state.selected();

    let rows: Vec<Row<'_>> = blocks
        .iter()
        .take(table_rows_to_build(table_state, inner.height))
        .enumerate()
        .map(|(idx, info)| {
            let style = if is_active && selected_row == Some(idx) {
                Style::default().fg(Color::White).bg(COLOR_ROW_SELECTED)
            } else if highlighted_block == Some(info.block_number) {
                Style::default().fg(Color::White).bg(COLOR_ROW_HIGHLIGHTED)
            } else {
                Style::default().fg(Color::White)
            };

            // Rows are newest first, so the parent block is the next row when present.
            let parent_base_fee = blocks
                .get(idx + 1)
                .filter(|parent| parent.block_number + 1 == info.block_number)
                .and_then(|parent| parent.base_fee_per_gas);
            let base_fee_style = match (info.base_fee_per_gas, parent_base_fee) {
                (Some(fee), Some(parent)) if fee > parent => Style::default().fg(Color::Red),
                (Some(fee), Some(parent)) if fee < parent => Style::default().fg(Color::Green),
                _ => Style::default().fg(Color::White),
            };
            let base_fee = info
                .base_fee_per_gas
                .map_or_else(|| "-".to_string(), |fee| Format::fee_per_gas(u128::from(fee)));

            let gas_target =
                info.eip1559_elasticity.filter(|&e| e > 0).map(|e| info.gas_limit / u64::from(e));

            Row::new(vec![
                Cell::from(Format::truncate_block_number(info.block_number, block_col_width))
                    .style(Style::default().fg(Format::block_color(info.block_number))),
                Cell::from(info.tx_count.to_string()),
                Cell::from(build_gas_bar(
                    info.gas_used,
                    info.gas_limit,
                    gas_target,
                    gas_bar_chars as usize,
                )),
                Cell::from(base_fee).style(base_fee_style),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Max(10),
        Constraint::Length(4),
        Constraint::Length(gas_bar_chars),
        Constraint::Min(12),
    ];
    let table = Table::new(rows, widths).header(header);
    f.render_stateful_widget(table, inner, &mut table_state.clone());
}

/// Title suffix for a paused L2 block list, counting the blocks that arrived since.
fn paused_label(blocks_since_pause: Option<usize>) -> String {
    blocks_since_pause.map_or_else(String::new, |count| format!(" [PAUSED +{count}]"))
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::{Terminal, backend::TestBackend};

    use super::CommandCenterView;
    use crate::{
        MonitoringConfig,
        app::{MAX_HISTORY, Resources, View},
        rpc::BlockDaInfo,
    };

    fn add_block(resources: &mut Resources, block_number: u64) {
        resources.da.tracker.update_block_info(BlockDaInfo { block_number, ..Default::default() });
    }

    fn render(view: &mut CommandCenterView, resources: &Resources) -> String {
        let mut terminal = Terminal::new(TestBackend::new(200, 30)).unwrap();
        terminal.draw(|frame| view.render(frame, frame.area(), resources)).unwrap();
        terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect()
    }

    #[tokio::test]
    async fn space_freezes_block_lists_until_pressed_again() {
        let mut resources = Resources::new(MonitoringConfig::mainnet());
        let mut view = CommandCenterView::new();
        add_block(&mut resources, 55_000_100);

        view.handle_key(KeyEvent::from(KeyCode::Char(' ')), &mut resources);
        add_block(&mut resources, 55_000_101);
        let paused = render(&mut view, &resources);
        assert!(paused.contains("55000100"));
        assert!(!paused.contains("55000101"));
        assert!(paused.contains("[PAUSED +1]"));

        view.handle_key(KeyEvent::from(KeyCode::Char(' ')), &mut resources);
        let resumed = render(&mut view, &resources);
        assert!(resumed.contains("55000101"));
        assert!(!resumed.contains("PAUSED"));
    }

    #[tokio::test]
    async fn paused_rows_survive_history_eviction() {
        let mut resources = Resources::new(MonitoringConfig::mainnet());
        let mut view = CommandCenterView::new();
        add_block(&mut resources, 55_000_100);

        view.handle_key(KeyEvent::from(KeyCode::Char(' ')), &mut resources);
        let newest = 55_000_100 + MAX_HISTORY as u64 + 5;
        for block_number in 55_000_101..=newest {
            add_block(&mut resources, block_number);
        }
        resources.da.tracker.update_safe_head(newest);
        assert!(
            resources.da.tracker.block_contributions.iter().all(|b| b.block_number != 55_000_100)
        );

        let paused = render(&mut view, &resources);
        assert!(paused.contains("55000100"));
        assert!(paused.contains(&format!("[PAUSED +{MAX_HISTORY}]")));
    }
}
