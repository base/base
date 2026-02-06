use std::io::Stdout;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    prelude::*,
    widgets::Paragraph,
};

use super::terminal::{restore_terminal, setup_terminal};

/// ASCII art for "basectl" logo - 3D block style
const LOGO: &str = "\
██████╗  █████╗ ███████╗███████╗ ██████╗████████╗██╗
██╔══██╗██╔══██╗██╔════╝██╔════╝██╔════╝╚══██╔══╝██║
██████╔╝███████║███████╗█████╗  ██║        ██║   ██║
██╔══██╗██╔══██║╚════██║██╔══╝  ██║        ██║   ██║
██████╔╝██║  ██║███████║███████╗╚██████╗   ██║   ███████╗
╚═════╝ ╚═╝  ╚═╝╚══════╝╚══════╝ ╚═════╝   ╚═╝   ╚══════╝";

/// Menu item definition
#[derive(Debug)]
pub struct MenuItem {
    pub key: char,
    pub label: &'static str,
    pub description: &'static str,
}

/// Available menu items
const MENU_ITEMS: &[MenuItem] = &[
    MenuItem {
        key: 'c',
        label: "Config",
        description: "View chain configuration and L1 SystemConfig",
    },
    MenuItem { key: 'f', label: "Flashblocks", description: "Subscribe to flashblocks stream" },
    MenuItem { key: 'q', label: "Quit", description: "Exit basectl" },
];

/// Selected command from homescreen
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeSelection {
    Config,
    Flashblocks,
    Quit,
}

/// Run the homescreen TUI and return the user's selection
pub fn run_homescreen() -> Result<HomeSelection> {
    let mut terminal = setup_terminal()?;
    let result = run_homescreen_loop(&mut terminal);
    restore_terminal(&mut terminal)?;
    result
}

fn run_homescreen_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<HomeSelection> {
    let mut selected_index = 0;

    loop {
        terminal.draw(|f| draw_homescreen(f, selected_index))?;

        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // Handle Ctrl+C to exit
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(HomeSelection::Quit);
            }
            match key.code {
                KeyCode::Char('q') => return Ok(HomeSelection::Quit),
                KeyCode::Char('c') => return Ok(HomeSelection::Config),
                KeyCode::Char('f') => return Ok(HomeSelection::Flashblocks),
                KeyCode::Up | KeyCode::Char('k') => {
                    selected_index = selected_index.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if selected_index < MENU_ITEMS.len() - 1 {
                        selected_index += 1;
                    }
                }
                KeyCode::Enter => {
                    return Ok(match selected_index {
                        0 => HomeSelection::Config,
                        1 => HomeSelection::Flashblocks,
                        _ => HomeSelection::Quit,
                    });
                }
                _ => {}
            }
        }
    }
}

fn draw_homescreen(f: &mut Frame, selected_index: usize) {
    let area = f.area();

    // Calculate layout
    let logo_height = LOGO.lines().count() as u16;
    let menu_height = (MENU_ITEMS.len() * 2) as u16 + 2;
    let total_content_height = logo_height + menu_height + 3;

    // Center vertically
    let vertical_padding = area.height.saturating_sub(total_content_height) / 2;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(vertical_padding),
            Constraint::Length(logo_height),
            Constraint::Length(3), // spacing
            Constraint::Length(menu_height),
            Constraint::Min(0),
        ])
        .split(area);

    // Draw logo
    draw_logo(f, chunks[1]);

    // Draw menu
    draw_menu(f, chunks[3], selected_index);
}

fn draw_logo(f: &mut Frame, area: Rect) {
    let base_blue = Color::Rgb(0, 82, 255);

    // Pad each line to same length for consistent centering
    let max_len = LOGO.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    let padded_lines: Vec<Line> = LOGO
        .lines()
        .map(|line| {
            let padding = max_len.saturating_sub(line.chars().count());
            let padded = format!("{}{}", line, " ".repeat(padding));
            Line::from(padded)
        })
        .collect();

    let logo = Paragraph::new(padded_lines)
        .style(Style::default().fg(base_blue))
        .alignment(Alignment::Center);

    f.render_widget(logo, area);
}

fn draw_menu(f: &mut Frame, area: Rect, selected_index: usize) {
    let base_blue = Color::Rgb(0, 82, 255);
    let mut lines = Vec::new();

    for (i, item) in MENU_ITEMS.iter().enumerate() {
        let is_selected = i == selected_index;

        // Key indicator
        let key_style = Style::default().fg(base_blue).add_modifier(Modifier::BOLD);

        // Label style
        let label_style = if is_selected {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        // Description style
        let desc_style = Style::default().fg(Color::DarkGray);

        // Selection indicator
        let selector = if is_selected { "▸ " } else { "  " };
        let selector_style = Style::default().fg(base_blue);

        lines.push(Line::from(vec![
            Span::styled(selector, selector_style),
            Span::styled(format!("[{}]", item.key), key_style),
            Span::raw(" "),
            Span::styled(item.label, label_style),
            Span::raw("  "),
            Span::styled(item.description, desc_style),
        ]));

        // Add spacing between items
        if i < MENU_ITEMS.len() - 1 {
            lines.push(Line::from(""));
        }
    }

    // Create a centered container for the menu with left-aligned text inside
    let menu_width = 60u16.min(area.width);
    let horizontal_padding = area.width.saturating_sub(menu_width) / 2;

    let centered_area =
        Rect { x: area.x + horizontal_padding, y: area.y, width: menu_width, height: area.height };

    let menu = Paragraph::new(lines).alignment(Alignment::Left);

    f.render_widget(menu, centered_area);
}
