use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::Paragraph,
};

use crate::{
    app::{Action, Resources, View, ViewId},
    commands::common::COLOR_BASE_BLUE,
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
    view_id: Option<ViewId>,
}

const MENU_ITEMS: &[MenuItem] = &[
    MenuItem {
        key: 'a',
        label: "Command Center",
        description: "Combined view of all monitors",
        view_id: Some(ViewId::CommandCenter),
    },
    MenuItem {
        key: 'c',
        label: "Config",
        description: "View chain configuration and L1 SystemConfig",
        view_id: Some(ViewId::Config),
    },
    MenuItem {
        key: 'd',
        label: "DA Monitor",
        description: "Data availability backlog monitor",
        view_id: Some(ViewId::DaMonitor),
    },
    MenuItem {
        key: 'f',
        label: "Flashblocks",
        description: "Subscribe to flashblocks stream",
        view_id: Some(ViewId::Flashblocks),
    },
    MenuItem { key: 'q', label: "Quit", description: "Exit basectl", view_id: None },
];

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "a", description: "Command Center" },
    Keybinding { key: "c", description: "Config" },
    Keybinding { key: "d", description: "DA Monitor" },
    Keybinding { key: "f", description: "Flashblocks" },
    Keybinding { key: "j/k", description: "Navigate" },
    Keybinding { key: "Enter", description: "Select" },
    Keybinding { key: "q", description: "Quit" },
];

#[derive(Debug, Default)]
pub struct HomeView {
    selected_index: usize,
}

impl HomeView {
    pub const fn new() -> Self {
        Self { selected_index: 0 }
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
            KeyCode::Enter => MENU_ITEMS
                .get(self.selected_index)
                .map_or(Action::None, |item| item.view_id.map_or(Action::Quit, Action::SwitchView)),
            _ => Action::None,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, _resources: &Resources) {
        let logo_height = LOGO.lines().count() as u16;
        let menu_height = (MENU_ITEMS.len() * 2) as u16 + 2;
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
        render_menu(frame, chunks[3], self.selected_index);
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

fn render_menu(f: &mut Frame<'_>, area: Rect, selected_index: usize) {
    let mut lines = Vec::new();

    for (i, item) in MENU_ITEMS.iter().enumerate() {
        let is_selected = i == selected_index;

        let key_style = Style::default().fg(COLOR_BASE_BLUE).add_modifier(Modifier::BOLD);

        let label_style = if is_selected {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        let desc_style = Style::default().fg(Color::DarkGray);

        let selector = if is_selected { "▸ " } else { "  " };
        let selector_style = Style::default().fg(COLOR_BASE_BLUE);

        lines.push(Line::from(vec![
            Span::styled(selector, selector_style),
            Span::styled(format!("[{}]", item.key), key_style),
            Span::raw(" "),
            Span::styled(item.label, label_style),
            Span::raw("  "),
            Span::styled(item.description, desc_style),
        ]));

        if i < MENU_ITEMS.len() - 1 {
            lines.push(Line::from(""));
        }
    }

    let menu_width = 60u16.min(area.width);
    let horizontal_padding = area.width.saturating_sub(menu_width) / 2;

    let centered_area =
        Rect { x: area.x + horizontal_padding, y: area.y, width: menu_width, height: area.height };

    let menu = Paragraph::new(lines).alignment(Alignment::Left);

    f.render_widget(menu, centered_area);
}
