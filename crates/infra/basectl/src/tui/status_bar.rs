use ratatui::{layout::Rect, prelude::*, widgets::Paragraph};

/// Optional status info to display in the center of the status bar.
#[derive(Debug, Clone, Default)]
pub struct StatusInfo {
    pub message: String,
    pub style: Style,
}

impl StatusInfo {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), style: Style::default().fg(Color::DarkGray) }
    }

    pub const fn with_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// Status bar component that displays app info at the bottom of the screen.
#[derive(Debug)]
pub struct StatusBar;

impl StatusBar {
    /// Height of the status bar in lines.
    pub const fn height() -> u16 {
        1
    }

    /// Renders the status bar.
    ///
    /// Left side: `basectl [config_name]`
    /// Center: optional status info
    /// Right side: `? (help)`
    pub fn render(f: &mut Frame, area: Rect, config_name: &str, status_info: Option<&StatusInfo>) {
        let left = format!("basectl [{config_name}]");
        let center = status_info.map(|s| s.message.as_str()).unwrap_or("");
        let center_style = status_info.map(|s| s.style).unwrap_or_default();
        let right = "? (help)";

        let total_width = area.width as usize;
        let left_len = left.len();
        let center_len = center.len();
        let right_len = right.len();

        // Calculate padding for center and right alignment
        let available = total_width.saturating_sub(left_len + center_len + right_len);
        let left_padding = available / 2;
        let right_padding = available.saturating_sub(left_padding);

        let line = Line::from(vec![
            Span::styled(left, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(left_padding)),
            Span::styled(center, center_style),
            Span::raw(" ".repeat(right_padding)),
            Span::styled(right, Style::default().fg(Color::DarkGray)),
        ]);

        let paragraph = Paragraph::new(line);
        f.render_widget(paragraph, area);
    }
}
