use std::time::{Duration, Instant};

use ratatui::{
    layout::{Alignment, Rect},
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use tokio::sync::mpsc;

/// Duration to display a toast notification
const TOAST_DURATION: Duration = Duration::from_secs(5);

/// Duration to display a sticky toast unless a key dismisses it first.
const STICKY_TOAST_DURATION: Duration = Duration::from_secs(60);

/// Rows kept clear below a toast: the outer frame's bottom border and the
/// footer line beneath it.
const TOAST_BOTTOM_RESERVED_ROWS: u16 = 2;

/// Columns kept clear right of a toast: the outer frame's border and one
/// column of padding.
const TOAST_RIGHT_RESERVED_COLS: u16 = 2;

/// Widest a toast grows to fit its words; longer unbreakable words (URLs)
/// widen it further, up to the frame.
const TOAST_MAX_WIDTH: u16 = 50;

/// Narrowest toast text width, unless the terminal is narrower still.
const TOAST_MIN_WIDTH: usize = 18;

/// Toast severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    /// Informational message.
    Info,
    /// Warning message.
    Warning,
}

impl ToastLevel {
    const fn color(self) -> Color {
        match self {
            Self::Info => Color::Cyan,
            Self::Warning => Color::Yellow,
        }
    }

    const fn icon(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Warning => "⚠",
        }
    }
}

/// A toast notification message.
#[derive(Debug, Clone)]
pub struct Toast {
    /// Severity level of this toast.
    pub level: ToastLevel,
    /// Display message text.
    pub message: String,
    /// When this toast was created.
    pub created_at: Instant,
    /// Stays up to [`STICKY_TOAST_DURATION`] and is dismissed by the next key,
    /// so its text (for example a URL) can be copied.
    pub sticky: bool,
}

impl Toast {
    /// Creates a new toast with the given level and message.
    pub fn new(level: ToastLevel, message: impl Into<String>) -> Self {
        Self { level, message: message.into(), created_at: Instant::now(), sticky: false }
    }

    /// Creates an informational toast.
    pub fn info(message: impl Into<String>) -> Self {
        Self::new(ToastLevel::Info, message)
    }

    /// Creates a warning toast.
    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(ToastLevel::Warning, message)
    }

    /// Creates an informational toast that stays until a key is pressed or a
    /// minute passes. Newlines in `message` start new lines.
    pub fn sticky_info(message: impl Into<String>) -> Self {
        Self { sticky: true, ..Self::info(message) }
    }

    fn is_expired(&self) -> bool {
        let duration = if self.sticky { STICKY_TOAST_DURATION } else { TOAST_DURATION };
        self.created_at.elapsed() > duration
    }

    /// Word-wraps the message to `width` columns. Explicit newlines are kept
    /// and words longer than `width` (such as URLs) are never split.
    fn wrapped_lines(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for paragraph in self.message.split('\n') {
            let mut line = String::new();
            for word in paragraph.split_whitespace() {
                let needed = if line.is_empty() {
                    word.chars().count()
                } else {
                    line.chars().count() + 1 + word.chars().count()
                };
                if needed > width && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
            lines.push(line);
        }
        lines
    }
}

/// State for managing toast notifications
#[derive(Debug)]
pub struct ToastState {
    toasts: Vec<Toast>,
    rx: Option<mpsc::Receiver<Toast>>,
}

impl Default for ToastState {
    fn default() -> Self {
        Self::new()
    }
}

impl ToastState {
    /// Creates a new empty toast state.
    pub const fn new() -> Self {
        Self { toasts: Vec::new(), rx: None }
    }

    /// Sets the channel for receiving toast notifications from background tasks.
    pub fn set_channel(&mut self, rx: mpsc::Receiver<Toast>) {
        self.rx = Some(rx);
    }

    /// Poll for new toasts and remove expired ones
    pub fn poll(&mut self) {
        // Receive new toasts
        if let Some(ref mut rx) = self.rx {
            while let Ok(toast) = rx.try_recv() {
                self.toasts.push(toast);
            }
        }

        // Remove expired toasts
        self.toasts.retain(|t| !t.is_expired());
    }

    /// Pushes a toast notification directly.
    pub fn push(&mut self, toast: Toast) {
        self.toasts.push(toast);
    }

    /// Removes sticky toasts; returns true if one was showing, in which case
    /// the caller should swallow the key that dismissed it.
    pub fn dismiss_sticky(&mut self) -> bool {
        let showing = self.current().is_some_and(|toast| toast.sticky);
        self.toasts.retain(|toast| !toast.sticky);
        showing
    }

    /// Removes every toast, sticky or not, including those queued behind the
    /// one showing.
    pub fn clear(&mut self) {
        self.toasts.clear();
    }

    /// Returns the most recent active toast, if any.
    pub fn current(&self) -> Option<&Toast> {
        self.toasts.last()
    }

    /// Render toasts in the bottom-right corner of the given area, inside
    /// the outer frame's border and above its footer line.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(toast) = self.current() else {
            return;
        };

        // Inner width: fit the text up to the max width, but never narrower
        // than the longest word so URLs stay on one line.
        let frame_inner = area.width.saturating_sub(6).max(1) as usize;
        let longest_word =
            toast.message.split_whitespace().map(|word| word.chars().count()).max().unwrap_or(0);
        let longest_line =
            toast.message.split('\n').map(|line| line.chars().count()).max().unwrap_or(0);
        let inner_width = longest_line
            .min(TOAST_MAX_WIDTH as usize - 2)
            .max(longest_word)
            .clamp(TOAST_MIN_WIDTH.min(frame_inner), frame_inner);
        let lines = toast.wrapped_lines(inner_width);
        let toast_width = inner_width as u16 + 2;
        // Leave the outer frame's top border and the reserved bottom rows free.
        let max_height = area.height.saturating_sub(TOAST_BOTTOM_RESERVED_ROWS + 1);
        let toast_height = (lines.len() as u16 + 2).min(max_height).max(3);

        let x = area.x + area.width.saturating_sub(toast_width + TOAST_RIGHT_RESERVED_COLS);
        let y = (area.y + area.height)
            .saturating_sub(toast_height + TOAST_BOTTOM_RESERVED_ROWS)
            .max(area.y + 1);

        // On a very narrow terminal the toast is cut to the frame, not drawn
        // past it.
        let toast_area = Rect::new(x, y, toast_width, toast_height).intersection(area);
        if toast_area.is_empty() {
            return;
        }

        // Clear the area behind the toast
        frame.render_widget(Clear, toast_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(toast.level.color()))
            .title(format!(" {} ", toast.level.icon()))
            .title_style(Style::default().fg(toast.level.color()));

        let inner = block.inner(toast_area);
        frame.render_widget(block, toast_area);

        let content = Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
            .style(Style::default().fg(Color::White))
            .alignment(Alignment::Left);

        frame.render_widget(content, inner);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::{Toast, ToastState};

    #[test]
    fn narrow_terminals_render_a_toast_without_panicking() {
        let mut toasts = ToastState::new();
        toasts.push(Toast::warning("approval gateway returned 503: not the leader"));
        for width in [1, 10, 23, 24] {
            let mut terminal = Terminal::new(TestBackend::new(width, 34)).unwrap();
            terminal.draw(|frame| toasts.render(frame, frame.area())).unwrap();
            let rows: String = (0..34)
                .map(|y| {
                    (0..width)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect();
            if width >= 10 {
                assert!(rows.contains('┐'), "width {width}: no toast border");
            }
        }
    }

    #[test]
    fn long_toasts_wrap_on_words_and_keep_urls_whole() {
        let url = "https://review.example/reviews/rev_42/with/a/rather/long/path/segment";
        let mut toasts = ToastState::new();
        toasts.push(Toast::info(format!("Proposed rev_42 and some more words to wrap:\n{url}")));
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal.draw(|frame| toasts.render(frame, frame.area())).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..34)
            .map(|y| (0..120).map(|x| buffer[(x, y)].symbol().to_string()).collect())
            .collect();
        assert!(rows.iter().any(|row| row.contains(url)));
        assert!(rows.iter().any(|row| row.contains("Proposed rev_42")));
    }

    #[test]
    fn sticky_toasts_survive_polls_and_are_dismissed_by_a_key() {
        let mut toasts = ToastState::new();
        toasts.push(Toast::info("plain"));
        assert!(!toasts.dismiss_sticky());
        toasts.push(Toast::sticky_info("copy me"));
        for _ in 0..5 {
            toasts.poll();
        }
        assert_eq!(toasts.current().map(|toast| toast.message.as_str()), Some("copy me"));
        assert!(toasts.dismiss_sticky());
        assert_eq!(toasts.current().map(|toast| toast.message.as_str()), Some("plain"));
        assert!(!toasts.dismiss_sticky());
    }

    #[test]
    fn clear_removes_sticky_and_queued_toasts() {
        let mut toasts = ToastState::new();
        toasts.push(Toast::warning("first"));
        toasts.push(Toast::sticky_info("copy me"));
        toasts.push(Toast::info("last"));
        toasts.clear();
        assert!(toasts.current().is_none());
    }
}
