use ratatui::{
    layout::Rect,
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

use super::Keybinding;

/// Width of the help sidebar in characters.
const SIDEBAR_WIDTH: u16 = 28;

/// Help sidebar component that displays keybindings on the right side of the screen.
#[derive(Debug)]
pub struct HelpSidebar;

impl HelpSidebar {
    /// Splits the given area into main content and sidebar areas.
    ///
    /// Returns `(main_area, sidebar_area)` where sidebar is on the right.
    pub fn split_layout(area: Rect) -> (Rect, Rect) {
        let sidebar_width = SIDEBAR_WIDTH.min(area.width);
        let main_width = area.width.saturating_sub(sidebar_width);

        let main_area = Rect { x: area.x, y: area.y, width: main_width, height: area.height };

        let sidebar_area =
            Rect { x: area.x + main_width, y: area.y, width: sidebar_width, height: area.height };

        (main_area, sidebar_area)
    }

    /// Renders the help sidebar with the given keybindings.
    pub fn render(f: &mut Frame, area: Rect, keybindings: &[Keybinding]) {
        let mut lines = vec![Line::from("")];

        for kb in keybindings {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<6}", kb.key),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw(kb.description),
            ]));
        }

        lines.push(Line::from(""));

        let help = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" Help "),
            )
            .style(Style::default().fg(Color::White));

        f.render_widget(help, area);
    }
}
