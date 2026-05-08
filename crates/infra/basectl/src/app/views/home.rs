use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::Paragraph,
};

use crate::{
    app::{Action, Resources, View, ViewId},
    commands::COLOR_BASE_BLUE,
    tui::Keybinding,
};

const LOGO: &str = "\
██████╗  █████╗ ███████╗███████╗ ██████╗████████╗██╗
██╔══██╗██╔══██╗██╔════╝██╔════╝██╔════╝╚══██╔══╝██║
██████╔╝███████║███████╗█████╗  ██║        ██║   ██║
██╔══██╗██╔══██║╚════██║██╔══╝  ██║        ██║   ██║
██████╔╝██║  ██║███████║███████╗╚██████╗   ██║   ███████╗
╚═════╝ ╚═╝  ╚═╝╚══════╝╚══════╝ ╚═════╝   ╚═╝   ╚══════╝";

struct MenuItem {
    key: char,
    label: &'static str,
    description: &'static str,
    badge: Option<&'static str>,
    view_id: Option<ViewId>,
}

const MENU_COLUMN_GAP: u16 = 4;
const MENU_ITEM_HEIGHT: u16 = 3;
const MIN_TWO_COLUMN_WIDTH: u16 = 88;

const MENU_ITEMS: &[MenuItem] = &[
    MenuItem {
        key: 'a',
        label: "Command Center",
        description: "Combined view of all monitors",
        badge: None,
        view_id: Some(ViewId::CommandCenter),
    },
    MenuItem {
        key: 'c',
        label: "Config",
        description: "View chain configuration and L1 SystemConfig",
        badge: None,
        view_id: Some(ViewId::Config),
    },
    MenuItem {
        key: 'd',
        label: "DA Monitor",
        description: "Data availability backlog monitor",
        badge: None,
        view_id: Some(ViewId::DaMonitor),
    },
    MenuItem {
        key: 'f',
        label: "Flashblocks",
        description: "Subscribe to flashblocks stream",
        badge: None,
        view_id: Some(ViewId::Flashblocks),
    },
    MenuItem {
        key: 'h',
        label: "HA Conductor",
        description: "Monitor HA conductor cluster",
        badge: Some("devnet-only"),
        view_id: Some(ViewId::Conductor),
    },
    MenuItem {
        key: 'p',
        label: "Proofs",
        description: "Monitor dispute games and anchor state",
        badge: Some("config-required"),
        view_id: Some(ViewId::Proofs),
    },
    MenuItem {
        key: 'u',
        label: "Upgrades",
        description: "Network upgrade activation countdown and history",
        badge: None,
        view_id: Some(ViewId::Upgrades),
    },
    MenuItem { key: 'q', label: "Quit", description: "Exit basectl", badge: None, view_id: None },
];

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "a", description: "Command Center" },
    Keybinding { key: "c", description: "Config" },
    Keybinding { key: "d", description: "DA Monitor" },
    Keybinding { key: "f", description: "Flashblocks" },
    Keybinding { key: "h", description: "HA Conductor" },
    Keybinding { key: "p", description: "Proofs" },
    Keybinding { key: "u", description: "Upgrades" },
    Keybinding { key: "j/k", description: "Navigate" },
    Keybinding { key: "←/→", description: "Switch column" },
    Keybinding { key: "Enter", description: "Select" },
    Keybinding { key: "q", description: "Quit" },
];

/// Main menu view with navigation to all other views.
#[derive(Debug)]
pub struct HomeView {
    selected_index: usize,
    column_count: usize,
}

impl HomeView {
    /// Creates a new home view with the first menu item selected.
    pub const fn new() -> Self {
        Self { selected_index: 0, column_count: 1 }
    }
}

impl Default for HomeView {
    fn default() -> Self {
        Self::new()
    }
}

impl View for HomeView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn handle_key(&mut self, key: KeyEvent, _resources: &mut Resources) -> Action {
        match key.code {
            KeyCode::Char('a') => Action::SwitchView(ViewId::CommandCenter),
            KeyCode::Char('c') => Action::SwitchView(ViewId::Config),
            KeyCode::Char('d') => Action::SwitchView(ViewId::DaMonitor),
            KeyCode::Char('f') => Action::SwitchView(ViewId::Flashblocks),
            KeyCode::Char('h') => Action::SwitchView(ViewId::Conductor),
            KeyCode::Char('p') => Action::SwitchView(ViewId::Proofs),
            KeyCode::Char('u') => Action::SwitchView(ViewId::Upgrades),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_index = self.selected_index.saturating_sub(1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_index < MENU_ITEMS.len() - 1 {
                    self.selected_index += 1;
                }
                Action::None
            }
            KeyCode::Left => {
                let rows = menu_row_count(self.column_count);
                if self.column_count > 1 && self.selected_index >= rows {
                    self.selected_index -= rows;
                }
                Action::None
            }
            KeyCode::Right => {
                let rows = menu_row_count(self.column_count);
                let next = self.selected_index + rows;
                if self.column_count > 1 && next < MENU_ITEMS.len() {
                    self.selected_index = next;
                }
                Action::None
            }
            KeyCode::Enter => MENU_ITEMS
                .get(self.selected_index)
                .map_or(Action::None, |item| item.view_id.map_or(Action::Quit, Action::SwitchView)),
            _ => Action::None,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, _resources: &Resources) {
        let columns = menu_column_count(area.width);
        self.column_count = columns;
        let logo_height = LOGO.lines().count() as u16;
        let menu_height = menu_height(columns);
        let total_content_height = logo_height + menu_height + 3;

        let vertical_padding = area.height.saturating_sub(total_content_height) / 2;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(vertical_padding),
                Constraint::Length(logo_height),
                Constraint::Length(3),
                Constraint::Length(menu_height),
                Constraint::Min(0),
            ])
            .split(area);

        render_logo(frame, chunks[1]);
        render_menu(frame, chunks[3], self.selected_index, columns);
    }
}

fn render_logo(f: &mut Frame<'_>, area: Rect) {
    let max_len = LOGO.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    let padded_lines: Vec<Line<'_>> = LOGO
        .lines()
        .map(|line| {
            let padding = max_len.saturating_sub(line.chars().count());
            let padded = format!("{}{}", line, " ".repeat(padding));
            Line::from(padded)
        })
        .collect();

    let logo = Paragraph::new(padded_lines)
        .style(Style::default().fg(COLOR_BASE_BLUE))
        .alignment(Alignment::Center);

    f.render_widget(logo, area);
}

fn render_menu(f: &mut Frame<'_>, area: Rect, selected_index: usize, columns: usize) {
    let menu_width = if columns == 2 { 104u16 } else { 64u16 }.min(area.width);
    let horizontal_padding = area.width.saturating_sub(menu_width) / 2;
    let centered_area =
        Rect { x: area.x + horizontal_padding, y: area.y, width: menu_width, height: area.height };

    if columns == 1 {
        render_menu_column(f, centered_area, selected_index, 0, 1);
        return;
    }

    let column_areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Length(MENU_COLUMN_GAP),
            Constraint::Percentage(50),
        ])
        .split(centered_area);

    render_menu_column(f, column_areas[0], selected_index, 0, columns);
    render_menu_column(f, column_areas[2], selected_index, 1, columns);
}

fn render_menu_column(
    f: &mut Frame<'_>,
    area: Rect,
    selected_index: usize,
    column_index: usize,
    columns: usize,
) {
    let mut lines = Vec::new();
    let rows = menu_row_count(columns);

    for row in 0..rows {
        let index = column_index * rows + row;
        let Some(item) = MENU_ITEMS.get(index) else {
            lines.push(Line::from(""));
            lines.push(Line::from(""));
            if row < rows - 1 {
                lines.push(Line::from(""));
            }
            continue;
        };

        let is_selected = index == selected_index;
        let key_style = Style::default().fg(COLOR_BASE_BLUE).add_modifier(Modifier::BOLD);

        let label_style = if is_selected {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        let desc_style = Style::default().fg(Color::DarkGray);

        let selector = if is_selected { "▸ " } else { "  " };
        let selector_style = Style::default().fg(COLOR_BASE_BLUE);

        let mut title_spans = vec![
            Span::styled(selector, selector_style),
            Span::styled(format!("[{}]", item.key), key_style),
            Span::raw(" "),
            Span::styled(item.label, label_style),
        ];

        if let Some(badge) = item.badge {
            title_spans.push(Span::raw("  "));
            title_spans.push(Span::styled(format!(" {badge} "), badge_style(badge)));
        }

        lines.push(Line::from(title_spans));
        lines.push(Line::from(vec![
            Span::raw("      "),
            Span::styled(
                truncate_description(item.description, area.width.saturating_sub(6)),
                desc_style,
            ),
        ]));

        if row < rows - 1 {
            lines.push(Line::from(""));
        }
    }

    let menu = Paragraph::new(lines).alignment(Alignment::Left);
    f.render_widget(menu, area);
}

const fn menu_column_count(width: u16) -> usize {
    if width >= MIN_TWO_COLUMN_WIDTH { 2 } else { 1 }
}

const fn menu_row_count(columns: usize) -> usize {
    MENU_ITEMS.len().div_ceil(columns)
}

const fn menu_height(columns: usize) -> u16 {
    (menu_row_count(columns) as u16).saturating_mul(MENU_ITEM_HEIGHT)
}

fn badge_style(badge: &str) -> Style {
    match badge {
        "devnet-only" => {
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
        }
        _ => Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
    }
}

fn truncate_description(description: &str, width: u16) -> String {
    let width = usize::from(width);
    let len = description.chars().count();
    if len <= width {
        return description.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }

    let take = width - 3;
    let mut truncated = description.chars().take(take).collect::<String>();
    truncated.push_str("...");
    truncated
}
