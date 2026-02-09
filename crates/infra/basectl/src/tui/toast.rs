use std::time::{Duration, Instant};

use ratatui::{
    layout::{Alignment, Rect},
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::sync::mpsc;

/// Duration to display a toast notification
const TOAST_DURATION: Duration = Duration::from_secs(5);

/// Toast severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Warning,
    Error,
}

impl ToastLevel {
    const fn color(self) -> Color {
        match self {
            Self::Info => Color::Cyan,
            Self::Warning => Color::Yellow,
            Self::Error => Color::Red,
        }
    }

    const fn icon(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Warning => "⚠",
            Self::Error => "✗",
        }
    }
}

/// A toast notification message
#[derive(Debug, Clone)]
pub struct Toast {
    pub level: ToastLevel,
    pub message: String,
    pub created_at: Instant,
}

impl Toast {
    pub fn new(level: ToastLevel, message: impl Into<String>) -> Self {
        Self { level, message: message.into(), created_at: Instant::now() }
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self::new(ToastLevel::Info, message)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(ToastLevel::Warning, message)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(ToastLevel::Error, message)
    }

    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > TOAST_DURATION
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
    pub const fn new() -> Self {
        Self { toasts: Vec::new(), rx: None }
    }

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

    /// Get the current active toast (most recent)
    pub fn current(&self) -> Option<&Toast> {
        self.toasts.last()
    }

    /// Render toasts in the bottom-right corner of the given area
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let Some(toast) = self.current() else {
            return;
        };

        let toast_width = (toast.message.len() as u16 + 6).clamp(20, 50);
        let toast_height = 3;

        // Position in bottom-right corner with padding
        let x = area.x + area.width.saturating_sub(toast_width + 2);
        let y = area.y + area.height.saturating_sub(toast_height + 1);

        let toast_area = Rect::new(x, y, toast_width, toast_height);

        // Clear the area behind the toast
        frame.render_widget(Clear, toast_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(toast.level.color()))
            .title(format!(" {} ", toast.level.icon()))
            .title_style(Style::default().fg(toast.level.color()));

        let inner = block.inner(toast_area);
        frame.render_widget(block, toast_area);

        let content = Paragraph::new(toast.message.as_str())
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Left);

        frame.render_widget(content, inner);
    }
}
