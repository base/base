use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

use super::Keybinding;

const HELP_SIDEBAR_WIDTH: u16 = 30;

/// Layout regions produced by splitting the terminal area.
#[derive(Debug)]
pub(crate) struct AppLayout {
    /// Main content area for the active view.
    pub content: Rect,
    /// Optional help sidebar area.
    pub sidebar: Option<Rect>,
}

/// Handles the top-level application frame layout and help sidebar rendering.
#[derive(Debug)]
pub(crate) struct AppFrame;

impl AppFrame {
    /// Splits the terminal area into content and optional help sidebar.
    pub(crate) fn split_layout(area: Rect, show_help: bool) -> AppLayout {
        if show_help && area.width > HELP_SIDEBAR_WIDTH + 20 {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(20), Constraint::Length(HELP_SIDEBAR_WIDTH)])
                .split(area);

            AppLayout { content: chunks[0], sidebar: Some(chunks[1]) }
        } else {
            AppLayout { content: area, sidebar: None }
        }
    }

    /// Renders the help sidebar if it is visible in the layout.
    pub(crate) fn render(
        f: &mut Frame<'_>,
        layout: &AppLayout,
        config_name: &str,
        keybindings: &[Keybinding],
    ) {
        if let Some(sidebar) = layout.sidebar {
            render_help_sidebar(f, sidebar, config_name, keybindings);
        }
    }
}

fn render_help_sidebar(
    f: &mut Frame<'_>,
    area: Rect,
    config_name: &str,
    keybindings: &[Keybinding],
) {
    let block = Block::default()
        .title(format!(" Help [{config_name}] "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'_>> = keybindings
        .iter()
        .map(|kb| {
            Line::from(vec![
                Span::styled(format!("{:>12}", kb.key), Style::default().fg(Color::Yellow)),
                Span::raw("  "),
                Span::styled(kb.description, Style::default().fg(Color::White)),
            ])
        })
        .collect();

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("           ?", Style::default().fg(Color::Yellow)),
        Span::raw("  "),
        Span::styled("Close help", Style::default().fg(Color::White)),
    ]));

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}
