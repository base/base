use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Cell, Row, Table, TableState},
};

use crate::{
    app::{Action, Resources, View, views::TransactionPane},
    commands::common::{
        COLOR_ACTIVE_BORDER, COLOR_ROW_HIGHLIGHTED, COLOR_ROW_SELECTED, block_color,
        block_color_bright, build_gas_bar, format_gas, format_gwei, render_gas_usage_bar,
        time_diff_color, truncate_block_number,
    },
    tui::{Keybinding, Toast},
};

const GAS_BAR_CHARS: usize = 40;
const DEFAULT_ELASTICITY: u64 = 6;

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "Enter", description: "View transactions" },
    Keybinding { key: "\u{2190}/h \u{2192}/l", description: "Switch panel" },
    Keybinding { key: "Space", description: "Pause/Resume" },
    Keybinding { key: "Up/k", description: "Scroll up" },
    Keybinding { key: "Down/j", description: "Scroll down" },
    Keybinding { key: "PgUp", description: "Page up" },
    Keybinding { key: "PgDn", description: "Page down" },
    Keybinding { key: "Home/g", description: "Top (auto-scroll)" },
    Keybinding { key: "y", description: "Copy block number" },
];

/// View for displaying the live flashblocks stream with gas usage.
pub(crate) struct FlashblocksView {
    table_state: TableState,
    auto_scroll: bool,
    tx_pane: Option<TransactionPane>,
    focused_on_txns: bool,
}

impl Default for FlashblocksView {
    fn default() -> Self {
        Self::new()
    }
}

impl FlashblocksView {
    /// Creates a new flashblocks view with auto-scroll enabled.
    pub(crate) fn new() -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self { table_state, auto_scroll: true, tx_pane: None, focused_on_txns: false }
    }
}

impl View for FlashblocksView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn consumes_esc(&self) -> bool {
        self.tx_pane.is_some() && self.focused_on_txns
    }

    fn consumes_quit(&self) -> bool {
        self.tx_pane.is_some() && self.focused_on_txns
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        match key.code {
            KeyCode::Enter => {
                if let Some(idx) = self.table_state.selected()
                    && let Some(entry) = resources.flash.entries.get(idx)
                {
                    self.tx_pane = Some(TransactionPane::with_data(
                        entry.block_number,
                        format!("Flashblock {}::{}", entry.block_number, entry.index),
                        entry.decoded_txs.clone(),
                        resources.config.rpc.as_str(),
                        resources.config.explorer_base_url(),
                    ));
                    self.focused_on_txns = true;
                }
                Action::None
            }
            KeyCode::Left | KeyCode::Char('h') if self.tx_pane.is_some() => {
                self.focused_on_txns = false;
                Action::None
            }
            KeyCode::Right | KeyCode::Char('l') if self.tx_pane.is_some() => {
                self.focused_on_txns = true;
                Action::None
            }
            _ if self.focused_on_txns && self.tx_pane.is_some() => {
                let pane = self.tx_pane.as_mut().unwrap();
                let should_close = pane.handle_key(key, &mut |toast| {
                    resources.toasts.push(toast);
                });
                if should_close {
                    self.tx_pane = None;
                    self.focused_on_txns = false;
                }
                Action::None
            }
            KeyCode::Char(' ') => {
                resources.flash.paused = !resources.flash.paused;
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(selected) = self.table_state.selected() {
                    if selected > 0 {
                        self.table_state.select(Some(selected - 1));
                        self.auto_scroll = false;
                    } else {
                        self.auto_scroll = true;
                    }
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(selected) = self.table_state.selected() {
                    let max = resources.flash.entries.len().saturating_sub(1);
                    if selected < max {
                        self.table_state.select(Some(selected + 1));
                        self.auto_scroll = false;
                    }
                }
                Action::None
            }
            KeyCode::PageUp => {
                if let Some(selected) = self.table_state.selected() {
                    let new_pos = selected.saturating_sub(10);
                    self.table_state.select(Some(new_pos));
                    self.auto_scroll = new_pos == 0;
                }
                Action::None
            }
            KeyCode::PageDown => {
                if let Some(selected) = self.table_state.selected() {
                    let max = resources.flash.entries.len().saturating_sub(1);
                    let new_pos = (selected + 10).min(max);
                    self.table_state.select(Some(new_pos));
                    self.auto_scroll = false;
                }
                Action::None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.table_state.select(Some(0));
                self.auto_scroll = true;
                Action::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                let max = resources.flash.entries.len().saturating_sub(1);
                self.table_state.select(Some(max));
                self.auto_scroll = false;
                Action::None
            }
            KeyCode::Char('y') => {
                if let Some(idx) = self.table_state.selected()
                    && let Some(entry) = resources.flash.entries.get(idx)
                    && let Ok(mut clipboard) = Clipboard::new()
                {
                    let block_num = entry.block_number.to_string();
                    if clipboard.set_text(&block_num).is_ok() {
                        resources.toasts.push(Toast::info(format!("Copied {block_num}")));
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
        if self.auto_scroll && !resources.flash.entries.is_empty() {
            self.table_state.select(Some(0));
        }
        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        let flash = &resources.flash;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let highlighted_block = self
            .table_state
            .selected()
            .and_then(|idx| flash.entries.get(idx))
            .map(|e| e.block_number);

        render_gas_usage_bar(
            frame,
            chunks[0],
            &flash.entries,
            DEFAULT_ELASTICITY,
            highlighted_block,
        );

        let table_area = chunks[1];

        let (block_area, txn_area) = if self.tx_pane.is_some() {
            let pane_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(table_area);
            (pane_chunks[0], Some(pane_chunks[1]))
        } else {
            (table_area, None)
        };

        let missed = resources.flash.missed_flashblocks;
        let missed_str = if missed > 0 { format!(" | {missed} missed") } else { String::new() };
        let title = if flash.paused {
            format!(" Flashblocks [PAUSED] - {} msgs{missed_str} ", flash.message_count)
        } else {
            format!(" Flashblocks - {} msgs{missed_str} ", flash.message_count)
        };

        let border_color = if flash.paused {
            Color::Yellow
        } else if self.focused_on_txns {
            Color::DarkGray
        } else {
            COLOR_ACTIVE_BORDER
        };

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = block.inner(block_area);
        frame.render_widget(block, block_area);

        // Idx(4) + Txs(4) + Gas(7) + BaseFee(12) + Delta(8) + Dt(8) + Fill(42) + Time(8) + spacing = ~100
        let fixed_cols_width = 4 + 4 + 7 + 12 + 8 + 8 + (GAS_BAR_CHARS as u16 + 2) + 8 + 9;
        let block_col_width = inner.width.saturating_sub(fixed_cols_width).clamp(4, 10) as usize;

        let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let header = Row::new(vec![
            Cell::from("Block").style(header_style),
            Cell::from("Idx").style(header_style),
            Cell::from("Txs").style(header_style),
            Cell::from("Gas").style(header_style),
            Cell::from("Base Fee").style(header_style),
            Cell::from("Δ").style(header_style),
            Cell::from("Δt").style(header_style),
            Cell::from("Fill").style(header_style),
            Cell::from("Time").style(header_style),
        ]);

        let rows: Vec<Row<'_>> = flash
            .entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let is_selected = self.table_state.selected() == Some(idx);
                let is_highlighted = highlighted_block == Some(entry.block_number);

                let row_style = if is_selected {
                    Style::default().bg(COLOR_ROW_SELECTED)
                } else if is_highlighted {
                    Style::default().bg(COLOR_ROW_HIGHLIGHTED)
                } else {
                    Style::default()
                };

                let (base_fee_str, base_fee_style) = if entry.index == 0 {
                    let fee_str =
                        entry.base_fee.map(format_gwei).unwrap_or_else(|| "-".to_string());
                    let style = match (entry.base_fee, entry.prev_base_fee) {
                        (Some(curr), Some(prev)) if curr > prev => Style::default().fg(Color::Red),
                        (Some(curr), Some(prev)) if curr < prev => {
                            Style::default().fg(Color::Green)
                        }
                        _ => Style::default().fg(Color::White),
                    };
                    (fee_str, style)
                } else {
                    (String::new(), Style::default())
                };

                let (delta_str, delta_style) = if entry.index == 0 {
                    match (entry.base_fee, entry.prev_base_fee) {
                        (Some(curr), Some(prev)) if curr > prev => (
                            format!("+{:.2}%", (curr - prev) as f64 / prev as f64 * 100.0),
                            Style::default().fg(Color::Red),
                        ),
                        (Some(curr), Some(prev)) if curr < prev => (
                            format!("-{:.2}%", (prev - curr) as f64 / prev as f64 * 100.0),
                            Style::default().fg(Color::Green),
                        ),
                        _ => ("-".to_string(), Style::default().fg(Color::DarkGray)),
                    }
                } else {
                    (String::new(), Style::default())
                };

                let gas_bar = build_gas_bar(
                    entry.gas_used,
                    entry.gas_limit,
                    DEFAULT_ELASTICITY,
                    GAS_BAR_CHARS,
                );

                let (time_diff_str, time_style) = entry.time_diff_ms.map_or_else(
                    || ("-".to_string(), Style::default().fg(Color::DarkGray)),
                    |ms| (format!("+{ms}ms"), Style::default().fg(time_diff_color(ms))),
                );

                let block_style = if entry.index == 0 {
                    Style::default()
                        .fg(block_color_bright(entry.block_number))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(block_color(entry.block_number))
                };

                let time_str = entry.timestamp.format("%H:%M:%S").to_string();

                Row::new(vec![
                    Cell::from(truncate_block_number(entry.block_number, block_col_width))
                        .style(block_style),
                    Cell::from(entry.index.to_string()).style(block_style),
                    Cell::from(entry.tx_count.to_string()).style(block_style),
                    Cell::from(format_gas(entry.gas_used)),
                    Cell::from(base_fee_str).style(base_fee_style),
                    Cell::from(delta_str).style(delta_style),
                    Cell::from(time_diff_str).style(time_style),
                    Cell::from(gas_bar),
                    Cell::from(time_str).style(Style::default().fg(Color::DarkGray)),
                ])
                .style(row_style)
            })
            .collect();

        let widths = [
            Constraint::Max(10),
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Length(7),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(GAS_BAR_CHARS as u16 + 2),
            Constraint::Min(8),
        ];

        let table = Table::new(rows, widths).header(header);

        frame.render_stateful_widget(table, inner, &mut self.table_state.clone());

        if let (Some(pane), Some(area)) = (&mut self.tx_pane, txn_area) {
            pane.render(frame, area, self.focused_on_txns);
        }
    }
}
