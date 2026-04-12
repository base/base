use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use tokio::sync::mpsc;

use crate::{
    app::{Action, Resources, View},
    commands::COLOR_BASE_BLUE,
    rpc::{
        ConductorNodeStatus, PausedPeers, ValidatorNodeStatus, pause_sequencer_node,
        restart_conductor_node, transfer_conductor_leader, unpause_sequencer_node,
    },
    tui::{Keybinding, Toast},
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "←/→", description: "Select node" },
    Keybinding { key: "t", description: "Transfer (any peer)" },
    Keybinding { key: "Enter", description: "Transfer to selected" },
    Keybinding { key: "r", description: "Restart selected node" },
    Keybinding { key: "p", description: "Pause/unpause conductor" },
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
];

type PauseRx = Option<(String, mpsc::Receiver<Result<(String, PausedPeers), String>>)>;

/// HA conductor cluster status view.
///
/// Renders a fixed grid with one column per conductor node and rows for
/// role (leader / follower / offline), unsafe/safe/finalized L2 block, and P2P peer count.
/// The user can navigate columns with `←`/`→` and trigger leadership transfers
/// with `t` (any peer) or `Enter` (selected node). A footer bar always shows
/// the available key bindings. When no conductor configuration is present
/// (e.g. mainnet), a placeholder message is shown instead.
#[derive(Debug, Default)]
pub struct ConductorView {
    selected: usize,
    op_pending: bool,
    /// In-flight result channel for transfer / restart operations.
    op_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// In-flight result channel for pause operations.
    /// Carries `(node_name, result)` where `Ok` includes the peers that were saved.
    pause_rx: PauseRx,
    /// In-flight result channel for unpause operations.
    unpause_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// Saved peer lists for each paused node, keyed by node name.
    /// Presence in this map means the node is currently paused.
    paused_node_peers: HashMap<String, PausedPeers>,
}

impl ConductorView {
    /// Creates a new conductor view.
    pub fn new() -> Self {
        Self::default()
    }

    fn start_transfer(&mut self, resources: &Resources, target_name: Option<String>) {
        let Some(ref nodes) = resources.config.conductors else { return };
        let (tx, rx) = mpsc::channel(1);
        self.op_rx = Some(rx);
        self.op_pending = true;
        tokio::spawn(transfer_conductor_leader(nodes.clone(), target_name, tx));
    }

    fn start_restart(&mut self, resources: &Resources) {
        let Some(ref nodes) = resources.config.conductors else { return };
        let idx = self.selected.min(nodes.len().saturating_sub(1));
        let node = nodes[idx].clone();
        let (tx, rx) = mpsc::channel(1);
        self.op_rx = Some(rx);
        self.op_pending = true;
        tokio::spawn(restart_conductor_node(node, tx));
    }

    fn start_pause_toggle(&mut self, resources: &Resources) {
        let Some(ref nodes) = resources.config.conductors else { return };
        let idx = self.selected.min(nodes.len().saturating_sub(1));
        let node = nodes[idx].clone();
        self.op_pending = true;
        if let Some(peers) = self.paused_node_peers.remove(&node.name) {
            // Already paused — unpause by reconnecting saved peers.
            let (tx, rx) = mpsc::channel(1);
            self.unpause_rx = Some(rx);
            tokio::spawn(unpause_sequencer_node(node, peers, tx));
        } else {
            // Not paused — disconnect all peers and save them.
            let (tx, rx) = mpsc::channel(1);
            self.pause_rx = Some((node.name.clone(), rx));
            tokio::spawn(pause_sequencer_node(node, tx));
        }
    }
}

impl View for ConductorView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        if let Some(ref mut rx) = self.op_rx
            && let Ok(result) = rx.try_recv()
        {
            self.op_pending = false;
            self.op_rx = None;
            match result {
                Ok(msg) => resources.toasts.push(Toast::info(msg)),
                Err(msg) => resources.toasts.push(Toast::warning(msg)),
            }
        }

        if let Some((ref node_name, ref mut rx)) = self.pause_rx
            && let Ok(result) = rx.try_recv()
        {
            self.op_pending = false;
            match result {
                Ok((msg, peers)) => {
                    self.paused_node_peers.insert(node_name.clone(), peers);
                    resources.toasts.push(Toast::info(msg));
                }
                Err(msg) => resources.toasts.push(Toast::warning(msg)),
            }
            self.pause_rx = None;
        }

        if let Some(ref mut rx) = self.unpause_rx
            && let Ok(result) = rx.try_recv()
        {
            self.op_pending = false;
            self.unpause_rx = None;
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
            KeyCode::Char('r') if !self.op_pending && node_count > 0 => {
                self.start_restart(resources);
            }
            KeyCode::Char('p') if !self.op_pending && node_count > 0 => {
                self.start_pause_toggle(resources);
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
        let validators = &resources.validators.nodes;

        if validators.is_empty() {
            if nodes.is_empty() {
                render_unconfigured(frame, content_area);
            } else {
                let selected = self.selected.min(nodes.len().saturating_sub(1));
                render_cluster_table(
                    frame,
                    content_area,
                    nodes,
                    selected,
                    self.op_pending,
                    &self.paused_node_peers,
                );
            }
        } else {
            // Conductor table: 2 border + 1 header + 16 data rows = 19 lines.
            // Validator table: 2 border + 1 header + 14 data rows = 17 lines.
            let conductor_height = 19u16;
            let validator_height = 17u16;
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(conductor_height),
                    Constraint::Length(validator_height),
                    Constraint::Min(0),
                ])
                .split(content_area);

            if nodes.is_empty() {
                render_unconfigured(frame, sections[0]);
            } else {
                let selected = self.selected.min(nodes.len().saturating_sub(1));
                render_cluster_table(
                    frame,
                    sections[0],
                    nodes,
                    selected,
                    self.op_pending,
                    &self.paused_node_peers,
                );
            }
            render_validator_table(frame, sections[1], validators);
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
        spans.push(Span::styled("working…", Style::default().fg(Color::Yellow)));
    } else {
        spans.push(Span::styled("[t]", key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("transfer to any peer", desc_style));
        spans.push(sep.clone());
        spans.push(Span::styled("[Enter]", key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("transfer to selected", desc_style));
        spans.push(sep.clone());
        spans.push(Span::styled("[r]", key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("restart selected", desc_style));
        spans.push(sep.clone());
        spans.push(Span::styled("[p]", key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("pause/unpause conductor", desc_style));
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
    paused_nodes: &HashMap<String, PausedPeers>,
) {
    let title = if op_pending { " HA Conductor [working…] " } else { " HA Conductor " };

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

    // ── Fork detection: find leader's unsafe and safe hashes ──────────────
    let leader_unsafe: Option<(u64, alloy_primitives::B256)> = nodes.iter().find_map(|n| {
        if n.is_leader == Some(true) { n.unsafe_l2_block.zip(n.unsafe_l2_hash) } else { None }
    });
    let leader_safe: Option<(u64, alloy_primitives::B256)> = nodes.iter().find_map(|n| {
        if n.is_leader == Some(true) { n.safe_l2_block.zip(n.safe_l2_hash) } else { None }
    });

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
        Cell::from("  Role").style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = if paused_nodes.contains_key(&node.name) {
            ("⏸  paused", Style::default().fg(Color::Cyan))
        } else {
            match node.is_leader {
                Some(true) => {
                    ("★  LEADER", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
                }
                Some(false) => ("   follower", Style::default().fg(Color::DarkGray)),
                None => ("   offline", Style::default().fg(Color::Red)),
            }
        };
        role_cells.push(Cell::from(label).style(style));
    }
    let role_row = Row::new(role_cells).height(1);

    // ── Unsafe L2 row ──────────────────────────────────────────────────────
    let mut l2_cells = vec![
        Cell::from("  Unsafe L2")
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

    // ── Unsafe L2 hash row ────────────────────────────────────────────────
    let mut l2_hash_cells = vec![
        Cell::from("  Unsafe Hash")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.unsafe_l2_hash {
            Some(h) if node.is_leader == Some(true) => {
                let hex = format!("{h:x}");
                (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::Yellow))
            }
            Some(h) => {
                let hex = format!("{h:x}");
                // Fork: same block number as leader but different hash.
                let is_fork = leader_unsafe
                    .is_some_and(|(lnum, lhash)| node.unsafe_l2_block == Some(lnum) && h != lhash);
                if is_fork {
                    (format!("   ⚠ 0x{}…", &hex[..8]), Style::default().fg(Color::Red))
                } else {
                    (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::White))
                }
            }
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l2_hash_cells.push(Cell::from(label).style(style));
    }
    let l2_hash_row = Row::new(l2_hash_cells).height(1);

    // ── Safe L2 row ────────────────────────────────────────────────────────
    let mut safe_l2_cells = vec![
        Cell::from("  Safe L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.safe_l2_block {
            Some(n) if node.is_leader == Some(true) => {
                (format!("   #{n}"), Style::default().fg(Color::Yellow))
            }
            Some(n) => (format!("   #{n}"), Style::default().fg(Color::White)),
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        safe_l2_cells.push(Cell::from(label).style(style));
    }
    let safe_l2_row = Row::new(safe_l2_cells).height(1);

    // ── Safe L2 hash row ──────────────────────────────────────────────────
    let mut safe_hash_cells = vec![
        Cell::from("  Safe Hash")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.safe_l2_hash {
            Some(h) if node.is_leader == Some(true) => {
                let hex = format!("{h:x}");
                (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::Yellow))
            }
            Some(h) => {
                let hex = format!("{h:x}");
                let is_fork = leader_safe
                    .is_some_and(|(lnum, lhash)| node.safe_l2_block == Some(lnum) && h != lhash);
                if is_fork {
                    (format!("   ⚠ 0x{}…", &hex[..8]), Style::default().fg(Color::Red))
                } else {
                    (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::White))
                }
            }
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        safe_hash_cells.push(Cell::from(label).style(style));
    }
    let safe_hash_row = Row::new(safe_hash_cells).height(1);

    // ── Finalized L2 row ───────────────────────────────────────────────────
    let mut finalized_l2_cells = vec![
        Cell::from("  Finalized L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.finalized_l2_block {
            Some(n) if node.is_leader == Some(true) => {
                (format!("   #{n}"), Style::default().fg(Color::Yellow))
            }
            Some(n) => (format!("   #{n}"), Style::default().fg(Color::White)),
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        finalized_l2_cells.push(Cell::from(label).style(style));
    }
    let finalized_l2_row = Row::new(finalized_l2_cells).height(1);

    // ── Active row (conductor) ─────────────────────────────────────────────
    // `conductor_active` = "sequencer is currently sequencing".
    // Followers stop their sequencer intentionally — active=false is expected.
    // Only flag red when the *leader* reports active=false.
    let mut active_cells = vec![
        Cell::from("  Active")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = if paused_nodes.contains_key(&node.name) {
            ("   paused", Style::default().fg(Color::Cyan))
        } else {
            match (node.is_leader, node.conductor_active) {
                (Some(true), Some(true)) => ("   yes", Style::default().fg(Color::Green)),
                (Some(true), Some(false)) => ("   no", Style::default().fg(Color::Red)),
                (Some(false), Some(false)) => ("   stopped", Style::default().fg(Color::DarkGray)),
                (Some(false), Some(true)) => ("   active?", Style::default().fg(Color::Yellow)),
                _ => ("   ?", Style::default().fg(Color::DarkGray)),
            }
        };
        active_cells.push(Cell::from(label).style(style));
    }
    let active_row = Row::new(active_cells).height(1);

    // ── CL section header ──────────────────────────────────────────────────
    let cl_section = section_row("CL", node_count);

    // ── L1 derivation row ──────────────────────────────────────────────────
    let mut l1_cells = vec![
        Cell::from("  L1 Derived")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match (node.current_l1_block, node.head_l1_block) {
            (Some(cur), Some(head)) => {
                let lag = head.saturating_sub(cur);
                let color = if lag > 10 { Color::Yellow } else { Color::Green };
                (format!("   #{cur} / #{head}"), Style::default().fg(color))
            }
            _ => ("   ? / ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l1_cells.push(Cell::from(label).style(style));
    }
    let l1_row = Row::new(l1_cells).height(1);

    // ── CL peer count row ──────────────────────────────────────────────────
    let mut cl_peers_cells = vec![
        Cell::from("  CL Peers")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.cl_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   ?".to_string(), Style::default().fg(Color::Red)),
        };
        cl_peers_cells.push(Cell::from(label).style(style));
    }
    let cl_peers_row = Row::new(cl_peers_cells).height(1);

    // ── EL section header ──────────────────────────────────────────────────
    let el_section = section_row("EL", node_count);

    // ── EL block row ───────────────────────────────────────────────────────
    let mut el_block_cells = vec![
        Cell::from("  Block").style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_block {
            Some(n) if node.is_leader == Some(true) => {
                (format!("   #{n}"), Style::default().fg(Color::Yellow))
            }
            Some(n) => (format!("   #{n}"), Style::default().fg(Color::White)),
            None => ("   -".to_string(), Style::default().fg(Color::DarkGray)),
        };
        el_block_cells.push(Cell::from(label).style(style));
    }
    let el_block_row = Row::new(el_block_cells).height(1);

    // ── EL syncing row ─────────────────────────────────────────────────────
    let mut el_syncing_cells = vec![
        Cell::from("  Syncing")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_syncing {
            Some(true) => ("   yes", Style::default().fg(Color::Yellow)),
            Some(false) => ("   no", Style::default().fg(Color::Green)),
            None => ("   -", Style::default().fg(Color::DarkGray)),
        };
        el_syncing_cells.push(Cell::from(label).style(style));
    }
    let el_syncing_row = Row::new(el_syncing_cells).height(1);

    // ── EL peer count row ──────────────────────────────────────────────────
    let mut el_peers_cells = vec![
        Cell::from("  EL Peers")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   -".to_string(), Style::default().fg(Color::DarkGray)),
        };
        el_peers_cells.push(Cell::from(label).style(style));
    }
    let el_peers_row = Row::new(el_peers_cells).height(1);

    let spacer = Row::new(vec![Cell::from("")]).height(1);

    let rows = vec![
        // ── Conductor ────────────────────────────────────────────────────
        role_row,
        active_row,
        // ── CL ───────────────────────────────────────────────────────────
        spacer.clone(),
        cl_section,
        l2_row,
        l2_hash_row,
        safe_l2_row,
        safe_hash_row,
        finalized_l2_row,
        l1_row,
        cl_peers_row,
        // ── EL ───────────────────────────────────────────────────────────
        spacer,
        el_section,
        el_block_row,
        el_syncing_row,
        el_peers_row,
    ];
    let table = Table::new(rows, constraints).header(header).row_highlight_style(Style::default());

    f.render_stateful_widget(table, inner, &mut TableState::default());
}

fn render_validator_table(f: &mut Frame<'_>, area: Rect, nodes: &[ValidatorNodeStatus]) {
    let block = Block::default()
        .title(" Validators ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let node_count = nodes.len();
    let label_pct = 15u16;
    let node_pct = (100u16 - label_pct) / node_count as u16;

    let mut constraints = vec![Constraint::Percentage(label_pct)];
    for _ in 0..node_count {
        constraints.push(Constraint::Percentage(node_pct));
    }

    // ── Header row: node names ─────────────────────────────────────────────
    let mut header_cells = vec![Cell::from("")];
    for node in nodes {
        let style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
        header_cells.push(Cell::from(node.name.as_str()).style(style));
    }
    let header = Row::new(header_cells)
        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
        .height(1);

    // ── CL section header ──────────────────────────────────────────────────
    let cl_section = section_row("CL", node_count);

    // ── Unsafe L2 row ──────────────────────────────────────────────────────
    let mut l2_cells = vec![
        Cell::from("  Unsafe L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.unsafe_l2_block.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |n| (format!("   #{n}"), Style::default().fg(Color::White)),
        );
        l2_cells.push(Cell::from(label).style(style));
    }
    let l2_row = Row::new(l2_cells).height(1);

    // ── Unsafe L2 hash row ────────────────────────────────────────────────
    let mut l2_hash_cells = vec![
        Cell::from("  Unsafe Hash")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.unsafe_l2_hash.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |h| {
                let hex = format!("{h:x}");
                (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::White))
            },
        );
        l2_hash_cells.push(Cell::from(label).style(style));
    }
    let l2_hash_row = Row::new(l2_hash_cells).height(1);

    // ── Safe L2 row ────────────────────────────────────────────────────────
    let mut safe_l2_cells = vec![
        Cell::from("  Safe L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.safe_l2_block.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |n| (format!("   #{n}"), Style::default().fg(Color::White)),
        );
        safe_l2_cells.push(Cell::from(label).style(style));
    }
    let safe_l2_row = Row::new(safe_l2_cells).height(1);

    // ── Safe L2 hash row ──────────────────────────────────────────────────
    let mut safe_hash_cells = vec![
        Cell::from("  Safe Hash")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.safe_l2_hash.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |h| {
                let hex = format!("{h:x}");
                (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::White))
            },
        );
        safe_hash_cells.push(Cell::from(label).style(style));
    }
    let safe_hash_row = Row::new(safe_hash_cells).height(1);

    // ── Finalized L2 row ───────────────────────────────────────────────────
    let mut finalized_l2_cells = vec![
        Cell::from("  Finalized L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.finalized_l2_block.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |n| (format!("   #{n}"), Style::default().fg(Color::White)),
        );
        finalized_l2_cells.push(Cell::from(label).style(style));
    }
    let finalized_l2_row = Row::new(finalized_l2_cells).height(1);

    // ── L1 derivation row ──────────────────────────────────────────────────
    let mut l1_cells = vec![
        Cell::from("  L1 Derived")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match (node.current_l1_block, node.head_l1_block) {
            (Some(cur), Some(head)) => {
                let lag = head.saturating_sub(cur);
                let color = if lag > 10 { Color::Yellow } else { Color::Green };
                (format!("   #{cur} / #{head}"), Style::default().fg(color))
            }
            _ => ("   ? / ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l1_cells.push(Cell::from(label).style(style));
    }
    let l1_row = Row::new(l1_cells).height(1);

    // ── CL peer count row ──────────────────────────────────────────────────
    let mut cl_peers_cells = vec![
        Cell::from("  CL Peers")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.cl_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   ?".to_string(), Style::default().fg(Color::Red)),
        };
        cl_peers_cells.push(Cell::from(label).style(style));
    }
    let cl_peers_row = Row::new(cl_peers_cells).height(1);

    // ── EL section header ──────────────────────────────────────────────────
    let el_section = section_row("EL", node_count);

    // ── EL block row ───────────────────────────────────────────────────────
    let mut el_block_cells = vec![
        Cell::from("  Block").style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.el_block.map_or_else(
            || ("   -".to_string(), Style::default().fg(Color::DarkGray)),
            |n| (format!("   #{n}"), Style::default().fg(Color::White)),
        );
        el_block_cells.push(Cell::from(label).style(style));
    }
    let el_block_row = Row::new(el_block_cells).height(1);

    // ── EL syncing row ─────────────────────────────────────────────────────
    let mut el_syncing_cells = vec![
        Cell::from("  Syncing")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_syncing {
            Some(true) => ("   yes", Style::default().fg(Color::Yellow)),
            Some(false) => ("   no", Style::default().fg(Color::Green)),
            None => ("   -", Style::default().fg(Color::DarkGray)),
        };
        el_syncing_cells.push(Cell::from(label).style(style));
    }
    let el_syncing_row = Row::new(el_syncing_cells).height(1);

    // ── EL peer count row ──────────────────────────────────────────────────
    let mut el_peers_cells = vec![
        Cell::from("  EL Peers")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   -".to_string(), Style::default().fg(Color::DarkGray)),
        };
        el_peers_cells.push(Cell::from(label).style(style));
    }
    let el_peers_row = Row::new(el_peers_cells).height(1);

    let spacer = Row::new(vec![Cell::from("")]).height(1);

    let rows = vec![
        // ── CL ───────────────────────────────────────────────────────────
        spacer.clone(),
        cl_section,
        l2_row,
        l2_hash_row,
        safe_l2_row,
        safe_hash_row,
        finalized_l2_row,
        l1_row,
        cl_peers_row,
        // ── EL ───────────────────────────────────────────────────────────
        spacer,
        el_section,
        el_block_row,
        el_syncing_row,
        el_peers_row,
    ];
    let table = Table::new(rows, constraints).header(header).row_highlight_style(Style::default());

    f.render_stateful_widget(table, inner, &mut TableState::default());
}

/// Creates a styled section-separator row for the cluster table.
///
/// Renders as `── LABEL ──────────────` in the label column and `──────────────`
/// in every data column, extending the visual divider fully across all columns.
fn section_row(label: &str, node_count: usize) -> Row<'static> {
    let sep_style = Style::default().fg(Color::DarkGray);
    let heading = format!("── {label} ──────────────");
    let mut cells = vec![Cell::from(heading).style(sep_style)];
    for _ in 0..node_count {
        cells.push(Cell::from("──────────────").style(sep_style));
    }
    Row::new(cells).height(1)
}
