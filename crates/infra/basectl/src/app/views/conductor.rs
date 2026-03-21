use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use tokio::sync::mpsc;

use crate::{
    app::{Action, Resources, View},
    commands::common::COLOR_BASE_BLUE,
    rpc::{ConductorNodeStatus, transfer_conductor_leader},
    tui::{Keybinding, Toast},
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "←/→", description: "Select node" },
    Keybinding { key: "t", description: "Transfer (any peer)" },
    Keybinding { key: "Enter", description: "Transfer to selected" },
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
];

/// HA conductor cluster status view.
///
/// Renders a fixed grid with one column per conductor node and rows for
/// role (leader / follower / offline), unsafe L2 block, and P2P peer count.
/// The user can navigate columns with `←`/`→` and trigger leadership transfers
/// with `t` (any peer) or `Enter` (selected node). A footer bar always shows
/// the available key bindings. When no conductor configuration is present
/// (e.g. mainnet), a placeholder message is shown instead.
#[derive(Debug, Default)]
pub(crate) struct ConductorView {
    selected: usize,
    op_pending: bool,
    op_rx: Option<mpsc::Receiver<Result<String, String>>>,
}

impl ConductorView {
    /// Creates a new conductor view.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn start_transfer(&mut self, resources: &Resources, target_name: Option<String>) {
        let Some(ref nodes) = resources.config.conductors else { return };
        let (tx, rx) = mpsc::channel(1);
        self.op_rx = Some(rx);
        self.op_pending = true;
        tokio::spawn(transfer_conductor_leader(nodes.clone(), target_name, tx));
    }
}

impl View for ConductorView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        let Some(ref mut rx) = self.op_rx else { return Action::None };
        if let Ok(result) = rx.try_recv() {
            self.op_pending = false;
            self.op_rx = None;
            match result {
                Ok(msg) => resources.toasts.push(Toast::info(msg)),
                Err(msg) => resources.toasts.push(Toast::warning(msg)),
            }
        }
        Action::None
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        let node_count = resources.conductor.nodes.len();

        match key.code {
            KeyCode::Left | KeyCode::Char('h') if node_count > 0 => {
                self.selected = (self.selected + node_count - 1) % node_count;
            }
            KeyCode::Right | KeyCode::Char('l') if node_count > 0 => {
                self.selected = (self.selected + 1) % node_count;
            }
            KeyCode::Char('t') if !self.op_pending => {
                self.start_transfer(resources, None);
            }
            KeyCode::Enter if !self.op_pending && node_count > 0 => {
                let idx = self.selected.min(node_count - 1);
                let target = resources.conductor.nodes[idx].name.clone();
                self.start_transfer(resources, Some(target));
            }
            _ => {}
        }

        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        let content_area = chunks[0];
        let footer_area = chunks[1];

        let nodes = &resources.conductor.nodes;

        if nodes.is_empty() {
            render_unconfigured(frame, content_area);
        } else {
            let selected = self.selected.min(nodes.len().saturating_sub(1));
            render_cluster_table(frame, content_area, nodes, selected, self.op_pending);
        }

        render_footer(frame, footer_area, self.op_pending);
    }
}

fn render_unconfigured(f: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .title(" HA Conductor ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let msg = Paragraph::new("Conductor monitoring requires a config with conductor endpoints.")
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));

    f.render_widget(msg, chunks[1]);
}

fn render_footer(f: &mut Frame<'_>, area: Rect, op_pending: bool) {
    let key_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::DarkGray);
    let sep_style = Style::default().fg(Color::DarkGray);

    let sep = Span::styled("  │  ", sep_style);

    let mut spans = vec![
        Span::styled("[Esc]", key_style),
        Span::raw(" "),
        Span::styled("back", desc_style),
        sep.clone(),
        Span::styled("[←/→]", key_style),
        Span::raw(" "),
        Span::styled("select node", desc_style),
    ];

    spans.push(sep.clone());
    if op_pending {
        spans.push(Span::styled("transferring…", Style::default().fg(Color::Yellow)));
    } else {
        spans.push(Span::styled("[t]", key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("transfer to any peer", desc_style));
        spans.push(sep.clone());
        spans.push(Span::styled("[Enter]", key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("transfer to selected", desc_style));
    }

    spans.push(sep);
    spans.push(Span::styled("[?]", key_style));
    spans.push(Span::raw(" "));
    spans.push(Span::styled("help", desc_style));

    let footer = Paragraph::new(Line::from(spans));
    f.render_widget(footer, area);
}

fn render_cluster_table(
    f: &mut Frame<'_>,
    area: Rect,
    nodes: &[ConductorNodeStatus],
    selected: usize,
    op_pending: bool,
) {
    let title = if op_pending { " HA Conductor [transferring…] " } else { " HA Conductor " };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Column widths: one fixed label column + one equal-width column per node.
    let node_count = nodes.len();
    let label_pct = 15u16;
    let node_pct = (100u16 - label_pct) / node_count as u16;

    let mut constraints = vec![Constraint::Percentage(label_pct)];
    for _ in 0..node_count {
        constraints.push(Constraint::Percentage(node_pct));
    }

    // ── Header row: node names ─────────────────────────────────────────────
    let mut header_cells = vec![Cell::from("")];
    for (i, node) in nodes.iter().enumerate() {
        let is_selected = i == selected;
        // Role-driven color; selection adds underline independently.
        let role_color = match node.is_leader {
            Some(true) => Color::Yellow,
            Some(false) => Color::DarkGray,
            None => Color::Red,
        };
        let mut mods = Modifier::BOLD;
        if is_selected {
            mods |= Modifier::UNDERLINED;
        }
        let style = Style::default().fg(role_color).add_modifier(mods);
        header_cells.push(Cell::from(node.name.as_str()).style(style));
    }
    let header = Row::new(header_cells)
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .height(1);

    // ── Role row ───────────────────────────────────────────────────────────
    let mut role_cells = vec![
        Cell::from("Role").style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.is_leader {
            Some(true) => {
                ("★  LEADER", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            }
            Some(false) => ("   follower", Style::default().fg(Color::DarkGray)),
            None => ("   offline", Style::default().fg(Color::Red)),
        };
        role_cells.push(Cell::from(label).style(style));
    }
    let role_row = Row::new(role_cells).height(1);

    // ── Unsafe L2 row ──────────────────────────────────────────────────────
    let mut l2_cells = vec![
        Cell::from("Unsafe L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.unsafe_l2_block {
            Some(n) if node.is_leader == Some(true) => {
                (format!("   #{n}"), Style::default().fg(Color::Yellow))
            }
            Some(n) => (format!("   #{n}"), Style::default().fg(Color::White)),
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l2_cells.push(Cell::from(label).style(style));
    }
    let l2_row = Row::new(l2_cells).height(1);

    // ── P2P peers row ──────────────────────────────────────────────────────
    let mut peers_cells = vec![
        Cell::from("P2P Peers")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   ?".to_string(), Style::default().fg(Color::Red)),
        };
        peers_cells.push(Cell::from(label).style(style));
    }
    let peers_row = Row::new(peers_cells).height(1);

    let rows = vec![role_row, l2_row, peers_row];
    let table = Table::new(rows, constraints).header(header).row_highlight_style(Style::default());

    f.render_stateful_widget(table, inner, &mut TableState::default());
}
