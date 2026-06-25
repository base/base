use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

use crate::{
    app::{Action, Resources, View},
    output::COLOR_BASE_BLUE,
    rpc::{PodGroupStatus, PodStatus, PodsSnapshot},
    tui::Keybinding,
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "↑/k", description: "Scroll up" },
    Keybinding { key: "↓/j", description: "Scroll down" },
    Keybinding { key: "PgUp/PgDn", description: "Page scroll" },
    Keybinding { key: "g/G", description: "Top/Bottom" },
];

const READY_WIDTH: usize = 7;
const STATUS_WIDTH: usize = 18;
const RESTARTS_WIDTH: usize = 8;
const AGE_WIDTH: usize = 8;
const COLUMN_SPACING: usize = 8;
const MIN_NAME_WIDTH: usize = 22;
const MAX_NAME_WIDTH: usize = 48;
const MIN_GROUP_WIDTH: usize = 72;
const MAX_GROUP_WIDTH: usize = 104;

/// Kubernetes pods view.
#[derive(Debug, Default)]
pub struct PodsView {
    scroll: u16,
    last_width: u16,
    last_height: u16,
}

impl PodsView {
    /// Creates a new pods view.
    pub const fn new() -> Self {
        Self { scroll: 0, last_width: 100, last_height: 24 }
    }

    /// Returns a display color for Kubernetes pod status.
    pub fn status_color(status: &str) -> Color {
        match status {
            "Running" => Color::Green,
            "Pending" | "Terminating" => Color::Yellow,
            "Completed" | "Succeeded" => Color::Cyan,
            "Error" | "Failed" | "OOMKilled" | "CrashLoopBackOff" | "ImagePullBackOff" => {
                Color::Red
            }
            "Unknown" => Color::DarkGray,
            _ => Color::White,
        }
    }

    /// Returns a display color for Kubernetes restart count.
    pub fn restart_color(restarts: &str) -> Color {
        let count = restarts.parse::<u64>().unwrap_or(0);
        match count {
            0 => Color::Green,
            1..=5 => Color::Yellow,
            _ => Color::Red,
        }
    }

    /// Truncates a value to fit in a fixed terminal column.
    pub fn truncate(value: &str, width: usize) -> String {
        if value.chars().count() <= width {
            return value.to_string();
        }
        if width <= 3 {
            return ".".repeat(width);
        }
        let keep = width - 3;
        let mut out: String = value.chars().take(keep).collect();
        out.push_str("...");
        out
    }

    /// Computes the pod group container width for the current viewport.
    pub fn group_width(width: u16) -> usize {
        let viewport = usize::from(width).max(1);
        viewport.min(MAX_GROUP_WIDTH).max(MIN_GROUP_WIDTH.min(viewport))
    }

    /// Computes the pod-name column width for the current group container.
    pub fn name_width(group_width: usize) -> usize {
        let fixed = READY_WIDTH + STATUS_WIDTH + RESTARTS_WIDTH + AGE_WIDTH + COLUMN_SPACING;
        let available = group_width.saturating_sub(3).saturating_sub(fixed);
        available.min(MAX_NAME_WIDTH).max(MIN_NAME_WIDTH.min(available))
    }

    /// Builds all scrollable content lines for a snapshot.
    pub fn snapshot_lines(snapshot: &PodsSnapshot, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let refreshed = snapshot.refreshed_at.format("%H:%M:%S").to_string();
        lines.push(Line::from(vec![
            Span::styled("PODS", Style::default().fg(COLOR_BASE_BLUE).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  refreshed {refreshed}"), Style::default().fg(Color::DarkGray)),
        ]));

        if snapshot.groups.is_empty() {
            lines.push(Line::from(Span::styled(
                "No pod groups configured.",
                Style::default().fg(Color::DarkGray),
            )));
            return lines;
        }

        for group in &snapshot.groups {
            lines.push(Line::raw(""));
            lines.extend(Self::group_lines(group, width));
        }

        lines
    }

    /// Builds rendered lines for one pod group.
    pub fn group_lines(group: &PodGroupStatus, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let group_width = Self::group_width(width);
        let name_width = Self::name_width(group_width);
        let meta = format!("{} · {}", group.group.alias, group.group.namespace);

        lines.push(Self::top_border_line(&group.group.label, group_width));
        lines.push(Self::boxed_line(
            vec![(
                Self::truncate(&meta, group_width.saturating_sub(4)),
                Style::default().fg(Color::DarkGray),
            )],
            group_width,
        ));
        lines.push(Self::separator_line(group_width));
        lines.push(Self::boxed_line(Self::header_parts(name_width), group_width));

        if let Some(error) = group.error.as_ref() {
            lines.push(Self::boxed_line(
                vec![(
                    Self::truncate(error, group_width.saturating_sub(4)),
                    Style::default().fg(Color::Red),
                )],
                group_width,
            ));
            lines.push(Self::bottom_border_line(group_width));
            return lines;
        }

        if group.pods.is_empty() {
            lines.push(Self::boxed_line(
                vec![("no pods".to_string(), Style::default().fg(Color::DarkGray))],
                group_width,
            ));
            lines.push(Self::bottom_border_line(group_width));
            return lines;
        }

        for pod in &group.pods {
            lines.push(Self::pod_line(pod, name_width, group_width));
        }
        lines.push(Self::bottom_border_line(group_width));
        lines
    }

    fn top_border_line(label: &str, width: usize) -> Line<'static> {
        let label = Self::truncate(label, width.saturating_sub(8));
        let label_width = label.chars().count();
        let fill_width = width.saturating_sub(label_width + 5);
        Line::from(vec![
            Span::raw("┌─ "),
            Span::styled(label, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::raw("─".repeat(fill_width)),
            Span::raw("┐"),
        ])
    }

    fn separator_line(width: usize) -> Line<'static> {
        Line::from(format!("├{}┤", "─".repeat(width.saturating_sub(2))))
    }

    fn bottom_border_line(width: usize) -> Line<'static> {
        Line::from(format!("└{}┘", "─".repeat(width.saturating_sub(2))))
    }

    fn boxed_line(parts: Vec<(String, Style)>, width: usize) -> Line<'static> {
        let content_width: usize = parts.iter().map(|(value, _)| value.chars().count()).sum();
        let padding = width.saturating_sub(content_width + 3);
        let mut spans = Vec::with_capacity(parts.len() + 3);
        spans.push(Span::raw("│ "));
        spans.extend(parts.into_iter().map(|(value, style)| Span::styled(value, style)));
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::raw("│"));
        Line::from(spans)
    }

    fn header_parts(name_width: usize) -> Vec<(String, Style)> {
        let style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD);
        vec![
            (format!("{:<width$}", "NAME", width = name_width), style),
            ("  ".to_string(), Style::default()),
            (format!("{:^READY_WIDTH$}", "READY"), style),
            ("  ".to_string(), Style::default()),
            (format!("{:^STATUS_WIDTH$}", "STATUS"), style),
            ("  ".to_string(), Style::default()),
            (format!("{:>RESTARTS_WIDTH$}", "RESTARTS"), style),
            ("  ".to_string(), Style::default()),
            (format!("{:>AGE_WIDTH$}", "AGE"), style),
        ]
    }

    /// Builds a single pod row line.
    pub fn pod_line(pod: &PodStatus, name_width: usize, group_width: usize) -> Line<'static> {
        let name = Self::truncate(&pod.name, name_width);
        let status = Self::truncate(&pod.status, STATUS_WIDTH);
        Self::boxed_line(
            vec![
                (format!("{name:<name_width$}"), Style::default().fg(Color::Cyan)),
                ("  ".to_string(), Style::default()),
                (format!("{:^READY_WIDTH$}", pod.ready), Style::default().fg(Color::White)),
                ("  ".to_string(), Style::default()),
                (
                    format!("{status:^STATUS_WIDTH$}"),
                    Style::default().fg(Self::status_color(&pod.status)),
                ),
                ("  ".to_string(), Style::default()),
                (
                    format!("{:>RESTARTS_WIDTH$}", pod.restarts),
                    Style::default().fg(Self::restart_color(&pod.restarts)),
                ),
                ("  ".to_string(), Style::default()),
                (format!("{:>AGE_WIDTH$}", pod.age), Style::default().fg(Color::White)),
            ],
            group_width,
        )
    }

    /// Returns the maximum scroll offset for the current resources and height.
    pub fn max_scroll(resources: &Resources, width: u16, height: u16) -> u16 {
        let line_count = resources
            .pods
            .snapshot
            .as_ref()
            .map_or(1usize, |snapshot| Self::snapshot_lines(snapshot, width).len());
        line_count.saturating_sub(usize::from(height)) as u16
    }

    /// Keeps the scroll offset inside the current content bounds.
    pub fn clamp_scroll(&mut self, resources: &Resources, width: u16, height: u16) {
        self.scroll = self.scroll.min(Self::max_scroll(resources, width, height));
    }

    /// Renders the not-yet-loaded or unconfigured state.
    pub fn render_empty(frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        let message = if resources.config.pods.is_some() {
            "Loading pods..."
        } else {
            "No pod groups configured for this network."
        };
        let paragraph = Paragraph::new(message)
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, area);
    }

    /// Renders the footer.
    pub fn render_footer(frame: &mut Frame<'_>, area: Rect) {
        let key_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let desc_style = Style::default().fg(Color::DarkGray);
        let line = Line::from(vec![
            Span::styled("[Esc]", key_style),
            Span::raw(" "),
            Span::styled("back", desc_style),
            Span::styled("  |  ", desc_style),
            Span::styled("[j/k]", key_style),
            Span::raw(" "),
            Span::styled("scroll", desc_style),
            Span::styled("  |  ", desc_style),
            Span::styled("[n]", key_style),
            Span::raw(" "),
            Span::styled("network", desc_style),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

impl View for PodsView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        let max = Self::max_scroll(resources, self.last_width, self.last_height);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1).min(max)
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10).min(max),
            KeyCode::Char('g') => self.scroll = 0,
            KeyCode::Char('G') => self.scroll = max,
            _ => {}
        }
        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        let block = Block::default()
            .title(" Pods ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(COLOR_BASE_BLUE));
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);

        self.last_width = inner.width;
        self.last_height = inner.height;
        self.clamp_scroll(resources, inner.width, inner.height);

        if let Some(snapshot) = resources.pods.snapshot.as_ref() {
            let lines = Self::snapshot_lines(snapshot, inner.width);
            let paragraph = Paragraph::new(lines).scroll((self.scroll, 0));
            frame.render_widget(paragraph, inner);
        } else {
            Self::render_empty(frame, inner, resources);
        }

        Self::render_footer(frame, chunks[1]);
    }
}
