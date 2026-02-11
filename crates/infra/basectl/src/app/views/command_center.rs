use std::time::Duration;

use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

use crate::{
    app::{Action, Resources, View},
    commands::common::{
        COLOR_BASE_BLUE, COLOR_BURN, COLOR_GROWTH, L1BlockFilter, RATE_WINDOW_2M,
        backlog_size_color, build_gas_bar, format_bytes, format_duration, format_gwei, format_rate,
        render_da_backlog_bar, render_l1_blocks_table, time_diff_color, truncate_block_number,
    },
    tui::Keybinding,
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "←/→/Tab/1-3", description: "Switch panel" },
    Keybinding { key: "↑/↓", description: "Navigate" },
    Keybinding { key: "Space", description: "Pause flashblocks" },
    Keybinding { key: "y", description: "Copy block number" },
    Keybinding { key: "f", description: "Filter L1 blocks" },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    Flashblocks,
    Da,
    L1Blocks,
}

#[derive(Debug)]
pub struct CommandCenterView {
    focused_panel: Panel,
    da_selected_row: usize,
    flash_selected_row: usize,
    l1_selected_row: usize,
    highlighted_block: Option<u64>,
    l1_filter: L1BlockFilter,
}

impl Default for CommandCenterView {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandCenterView {
    pub const fn new() -> Self {
        Self {
            focused_panel: Panel::Flashblocks,
            da_selected_row: 0,
            flash_selected_row: 0,
            l1_selected_row: 0,
            highlighted_block: None,
            l1_filter: L1BlockFilter::All,
        }
    }

    const fn next_panel(&mut self) {
        self.focused_panel = match self.focused_panel {
            Panel::Flashblocks => Panel::Da,
            Panel::Da => Panel::L1Blocks,
            Panel::L1Blocks => Panel::Flashblocks,
        };
    }

    const fn prev_panel(&mut self) {
        self.focused_panel = match self.focused_panel {
            Panel::Flashblocks => Panel::L1Blocks,
            Panel::Da => Panel::Flashblocks,
            Panel::L1Blocks => Panel::Da,
        };
    }

    fn update_highlighted_block(&mut self, resources: &Resources) {
        self.highlighted_block = match self.focused_panel {
            Panel::Flashblocks => {
                resources.flash.entries.get(self.flash_selected_row).map(|e| e.block_number)
            }
            Panel::Da => resources
                .da
                .tracker
                .block_contributions
                .get(self.da_selected_row)
                .map(|c| c.block_number),
            Panel::L1Blocks => None,
        };
    }

    fn get_copyable_block(&self, resources: &Resources) -> Option<String> {
        match self.focused_panel {
            Panel::Flashblocks => resources
                .flash
                .entries
                .get(self.flash_selected_row)
                .map(|e| e.block_number.to_string()),
            Panel::Da => resources
                .da
                .tracker
                .block_contributions
                .get(self.da_selected_row)
                .map(|c| c.block_number.to_string()),
            Panel::L1Blocks => resources
                .da
                .tracker
                .filtered_l1_blocks(self.l1_filter)
                .nth(self.l1_selected_row)
                .map(|b| b.block_number.to_string()),
        }
    }
}

impl View for CommandCenterView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
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
                self.focused_panel = Panel::Flashblocks;
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
            KeyCode::Char('f') => {
                self.l1_filter = self.l1_filter.next();
                self.l1_selected_row = 0;
                Action::None
            }
            KeyCode::Char(' ') => {
                resources.flash.paused = !resources.flash.paused;
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                match self.focused_panel {
                    Panel::Da => {
                        if self.da_selected_row > 0 {
                            self.da_selected_row -= 1;
                        }
                    }
                    Panel::Flashblocks => {
                        if self.flash_selected_row > 0 {
                            self.flash_selected_row -= 1;
                        }
                    }
                    Panel::L1Blocks => {
                        if self.l1_selected_row > 0 {
                            self.l1_selected_row -= 1;
                        }
                    }
                }
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.focused_panel {
                    Panel::Da => {
                        let max = resources.da.tracker.block_contributions.len().saturating_sub(1);
                        if self.da_selected_row < max {
                            self.da_selected_row += 1;
                        }
                    }
                    Panel::Flashblocks => {
                        let max = resources.flash.entries.len().saturating_sub(1);
                        if self.flash_selected_row < max {
                            self.flash_selected_row += 1;
                        }
                    }
                    Panel::L1Blocks => {
                        let max = resources
                            .da
                            .tracker
                            .filtered_l1_blocks(self.l1_filter)
                            .count()
                            .saturating_sub(1);
                        if self.l1_selected_row < max {
                            self.l1_selected_row += 1;
                        }
                    }
                }
                self.update_highlighted_block(resources);
                Action::None
            }
            KeyCode::Char('y') => {
                if let Some(block_num) = self.get_copyable_block(resources)
                    && let Ok(mut clipboard) = Clipboard::new()
                {
                    let _ = clipboard.set_text(block_num);
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        let at_top = match self.focused_panel {
            Panel::Flashblocks => self.flash_selected_row == 0,
            Panel::Da => self.da_selected_row == 0,
            Panel::L1Blocks => self.l1_selected_row == 0,
        };
        if at_top {
            self.update_highlighted_block(resources);
        }
        Action::None
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, resources: &Resources) {
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

        let panel_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
            ])
            .split(main_chunks[2]);

        render_flash_panel(
            frame,
            panel_chunks[0],
            resources,
            self.focused_panel == Panel::Flashblocks,
            self.flash_selected_row,
            self.highlighted_block,
        );

        render_da_panel(
            frame,
            panel_chunks[1],
            resources,
            self.focused_panel == Panel::Da,
            self.da_selected_row,
            self.highlighted_block,
        );

        render_l1_blocks_table(
            frame,
            panel_chunks[2],
            resources.da.tracker.filtered_l1_blocks(self.l1_filter),
            self.focused_panel == Panel::L1Blocks,
            self.l1_selected_row,
            self.l1_filter,
            "L1 Blocks",
            resources.da.l1_connection_mode,
        );
    }
}

#[allow(clippy::option_if_let_else)]
fn render_config_panel(f: &mut Frame, area: Rect, resources: &Resources) {
    let block = Block::default()
        .title(" L1 Config ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));

    let content = match &resources.system_config {
        Some(sys) => {
            let gas_limit = sys.gas_limit.unwrap_or(0);
            let elasticity = sys.eip1559_elasticity.unwrap_or(0) as u64;
            let gas_target = if elasticity > 0 { gas_limit / elasticity } else { 0 };
            let denominator = sys.eip1559_denominator.unwrap_or(0);

            let basefee_scalar =
                sys.basefee_scalar.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());
            let blobbasefee_scalar =
                sys.blobbasefee_scalar.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());

            vec![
                Line::from(vec![
                    Span::styled("Target: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format_gas_value(gas_target), Style::default().fg(Color::Green)),
                    Span::raw("  "),
                    Span::styled("Limit: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format_gas_value(gas_limit), Style::default().fg(Color::Cyan)),
                    Span::raw("  "),
                    Span::styled("E/D: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{elasticity}/{denominator}"),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("DA Scalars: ", Style::default().fg(Color::DarkGray)),
                    Span::styled("base=", Style::default().fg(Color::DarkGray)),
                    Span::styled(basefee_scalar, Style::default().fg(Color::Yellow)),
                    Span::raw(" "),
                    Span::styled("blob=", Style::default().fg(Color::DarkGray)),
                    Span::styled(blobbasefee_scalar, Style::default().fg(Color::Yellow)),
                ]),
            ]
        }
        None => {
            vec![Line::from(Span::styled("Loading...", Style::default().fg(Color::DarkGray)))]
        }
    };

    let para = Paragraph::new(content).block(block);
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

fn render_stats_panel(f: &mut Frame, area: Rect, resources: &Resources) {
    let tracker = &resources.da.tracker;

    let backlog_color = backlog_size_color(tracker.da_backlog_bytes);
    let growth_rate = tracker.growth_tracker.rate_over(RATE_WINDOW_2M);
    let burn_rate = tracker.burn_tracker.rate_over(RATE_WINDOW_2M);
    let time_since = tracker.last_base_blob_time.map(|t| t.elapsed());
    let base_share = tracker.base_blob_share(RATE_WINDOW_2M);
    let target_usage = tracker.blob_target_usage(RATE_WINDOW_2M, resources.config.l1_blob_target);

    let flash_status = if resources.flash.paused { " [PAUSED]" } else { "" };

    let lines = vec![
        Line::from(vec![
            Span::styled("DA: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_bytes(tracker.da_backlog_bytes),
                Style::default().fg(backlog_color),
            ),
            Span::raw("  "),
            Span::styled("↑", Style::default().fg(COLOR_GROWTH)),
            Span::styled(format_rate(growth_rate), Style::default().fg(COLOR_GROWTH)),
            Span::raw(" "),
            Span::styled("↓", Style::default().fg(COLOR_BURN)),
            Span::styled(format_rate(burn_rate), Style::default().fg(COLOR_BURN)),
            Span::raw("  "),
            Span::styled("L1: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                target_usage.map_or_else(|| "-".to_string(), |u| format!("{:.0}%", u * 100.0)),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(" "),
            Span::styled("Base: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                base_share.map_or_else(|| "-".to_string(), |s| format!("{:.0}%", s * 100.0)),
                Style::default().fg(COLOR_BASE_BLUE),
            ),
        ]),
        Line::from(vec![
            Span::styled("Last: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                time_since.map(format_duration).unwrap_or_else(|| "-".to_string()),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("Flash: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                resources.flash.message_count.to_string(),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("Missed: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                resources.flash.missed_flashblocks.to_string(),
                Style::default().fg(if resources.flash.missed_flashblocks > 0 {
                    Color::Red
                } else {
                    Color::Green
                }),
            ),
            Span::styled(flash_status, Style::default().fg(Color::Yellow)),
        ]),
    ];

    let block = Block::default()
        .title(" Stats ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

fn render_da_panel(
    f: &mut Frame,
    area: Rect,
    resources: &Resources,
    is_active: bool,
    selected_row: usize,
    highlighted_block: Option<u64>,
) {
    use ratatui::widgets::{Cell, Row, Table};

    use crate::commands::common::{
        COLOR_ROW_HIGHLIGHTED, COLOR_ROW_SELECTED, block_color, format_bytes as fmt_bytes,
        format_duration as fmt_dur,
    };

    let tracker = &resources.da.tracker;
    let border_color = if is_active { Color::Rgb(100, 255, 100) } else { Color::Green };

    let block = Block::default()
        .title(" L2 Blocks (DA↑) ")
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

const GAS_BAR_CHARS: usize = 20;
const DEFAULT_ELASTICITY: u64 = 6;

fn render_flash_panel(
    f: &mut Frame,
    area: Rect,
    resources: &Resources,
    is_active: bool,
    selected_row: usize,
    highlighted_block: Option<u64>,
) {
    use ratatui::widgets::{Cell, Row, Table};

    use crate::commands::common::{COLOR_ROW_HIGHLIGHTED, COLOR_ROW_SELECTED};

    let flash = &resources.flash;
    let border_color = if is_active { Color::Rgb(100, 180, 255) } else { Color::Rgb(0, 82, 255) };

    let title = if flash.paused { " Flashblocks [PAUSED] " } else { " Flashblocks " };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Idx(4) + Txs(4) + Gas(22) + BaseFee(14) + Dt(8) + spacing(6) = 58
    let fixed_cols_width = 4 + 4 + (GAS_BAR_CHARS as u16 + 2) + 14 + 8 + 6;
    let block_col_width = inner.width.saturating_sub(fixed_cols_width).clamp(4, 10) as usize;

    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("Block").style(header_style),
        Cell::from("Idx").style(header_style),
        Cell::from("Txs").style(header_style),
        Cell::from("Gas").style(header_style),
        Cell::from("Base Fee").style(header_style),
        Cell::from("Δt").style(header_style),
    ]);

    let rows: Vec<Row> = flash
        .entries
        .iter()
        .take(inner.height.saturating_sub(1) as usize)
        .enumerate()
        .map(|(idx, entry)| {
            let is_selected = is_active && idx == selected_row;
            let is_highlighted = highlighted_block == Some(entry.block_number);

            let style = if is_selected {
                Style::default().fg(Color::White).bg(COLOR_ROW_SELECTED)
            } else if is_highlighted {
                Style::default().fg(Color::White).bg(COLOR_ROW_HIGHLIGHTED)
            } else {
                Style::default().fg(Color::White)
            };

            let (base_fee_str, base_fee_style) = if entry.index == 0 {
                let fee_str = entry.base_fee.map(format_gwei).unwrap_or_else(|| "-".to_string());
                let style = match (entry.base_fee, entry.prev_base_fee) {
                    (Some(curr), Some(prev)) if curr > prev => Style::default().fg(Color::Red),
                    (Some(curr), Some(prev)) if curr < prev => Style::default().fg(Color::Green),
                    _ => Style::default().fg(Color::White),
                };
                (fee_str, style)
            } else {
                (String::new(), Style::default())
            };

            let gas_bar =
                build_gas_bar(entry.gas_used, entry.gas_limit, DEFAULT_ELASTICITY, GAS_BAR_CHARS);

            let (time_diff_str, time_style) = entry.time_diff_ms.map_or_else(
                || ("-".to_string(), Style::default().fg(Color::DarkGray)),
                |ms| (format!("+{ms}ms"), Style::default().fg(time_diff_color(ms))),
            );

            let first_fb_style = if entry.index == 0 {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::White)
            };

            Row::new(vec![
                Cell::from(truncate_block_number(entry.block_number, block_col_width))
                    .style(first_fb_style),
                Cell::from(entry.index.to_string()).style(first_fb_style),
                Cell::from(entry.tx_count.to_string()).style(first_fb_style),
                Cell::from(gas_bar),
                Cell::from(base_fee_str).style(base_fee_style),
                Cell::from(time_diff_str).style(time_style),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Max(10),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(GAS_BAR_CHARS as u16 + 2),
        Constraint::Length(14),
        Constraint::Min(8),
    ];

    let table = Table::new(rows, widths).header(header);
    f.render_widget(table, inner);
}
