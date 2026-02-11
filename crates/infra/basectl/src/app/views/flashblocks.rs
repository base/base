use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    prelude::*,
    widgets::{Block, Borders, Cell, Row, Table, TableState},
};

use crate::{
    app::{Action, Resources, View},
    commands::common::{
        COLOR_ACTIVE_BORDER, COLOR_ROW_HIGHLIGHTED, COLOR_ROW_SELECTED, build_gas_bar, format_gas,
        format_gwei, time_diff_color, truncate_block_number,
    },
    tui::Keybinding,
};

const GAS_BAR_CHARS: usize = 40;
const DEFAULT_ELASTICITY: u64 = 6;

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "Space", description: "Pause/Resume" },
    Keybinding { key: "Up/k", description: "Scroll up" },
    Keybinding { key: "Down/j", description: "Scroll down" },
    Keybinding { key: "PgUp", description: "Page up" },
    Keybinding { key: "PgDn", description: "Page down" },
    Keybinding { key: "Home/g", description: "Top (auto-scroll)" },
    Keybinding { key: "y", description: "Copy block number" },
];

#[derive(Debug)]
pub struct FlashblocksView {
    table_state: TableState,
    auto_scroll: bool,
}

impl Default for FlashblocksView {
    fn default() -> Self {
        Self::new()
    }
}

impl FlashblocksView {
    pub fn new() -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self { table_state, auto_scroll: true }
    }
}

impl View for FlashblocksView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        match key.code {
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
                    let _ = clipboard.set_text(entry.block_number.to_string());
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        if self.auto_scroll && !resources.flash.entries.is_empty() {
            self.table_state.select(Some(0));
        }
        Action::None
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, resources: &Resources) {
        let flash = &resources.flash;

        let missed = resources.flash.missed_flashblocks;
        let missed_str = if missed > 0 { format!(" | {missed} missed") } else { String::new() };
        let title = if flash.paused {
            format!(" Flashblocks [PAUSED] - {} msgs{missed_str} ", flash.message_count)
        } else {
            format!(" Flashblocks - {} msgs{missed_str} ", flash.message_count)
        };

        let border_color = if flash.paused { Color::Yellow } else { COLOR_ACTIVE_BORDER };

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let highlighted_block = self
            .table_state
            .selected()
            .and_then(|idx| flash.entries.get(idx))
            .map(|e| e.block_number);

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

        let rows: Vec<Row> = flash
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

                let first_fb_style = if entry.index == 0 {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::White)
                };

                let time_str = entry.timestamp.format("%H:%M:%S").to_string();

                Row::new(vec![
                    Cell::from(truncate_block_number(entry.block_number, block_col_width))
                        .style(first_fb_style),
                    Cell::from(entry.index.to_string()).style(first_fb_style),
                    Cell::from(entry.tx_count.to_string()).style(first_fb_style),
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
    }
}
