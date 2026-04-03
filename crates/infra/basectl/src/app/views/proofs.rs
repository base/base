use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

use crate::{
    app::{Action, Resources, View},
    commands::common::COLOR_BASE_BLUE,
    rpc::{LatestProposal, ProofsSnapshot},
    tui::Keybinding,
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
];

/// Proof system monitoring view showing dispute game state, anchor state,
/// and sync gap analysis.
#[derive(Debug, Default)]
pub(crate) struct ProofsView;

impl ProofsView {
    /// Creates a new proofs view.
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl View for ProofsView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn handle_key(&mut self, _key: KeyEvent, _resources: &mut Resources) -> Action {
        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        if resources.config.proofs.is_none() {
            render_unconfigured(frame, area);
            return;
        }

        match resources.proofs.snapshot {
            Some(ref snapshot) => render_dashboard(frame, area, snapshot),
            None => render_loading(frame, area),
        }
    }
}

fn render_unconfigured(frame: &mut Frame<'_>, area: Rect) {
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Proofs monitoring is not configured.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Add a [proofs] section to your chain config with:",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled("  proofs:", Style::default().fg(Color::Cyan))),
        Line::from(Span::styled(
            "    dispute_game_factory: \"0x...\"",
            Style::default().fg(Color::Cyan),
        )),
        Line::from(Span::styled(
            "    anchor_state_registry: \"0x...\"",
            Style::default().fg(Color::Cyan),
        )),
    ];

    let block = Block::default()
        .title(" Proofs ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let para = Paragraph::new(text).block(block).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(para, area);
}

fn render_loading(frame: &mut Frame<'_>, area: Rect) {
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Loading proof system state...",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .title(" Proofs ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let para = Paragraph::new(text).block(block).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(para, area);
}

fn render_dashboard(frame: &mut Frame<'_>, area: Rect, snapshot: &ProofsSnapshot) {
    // 2x2 grid layout.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    let bottom_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    render_chain_state(frame, top_cols[0], snapshot);
    render_anchor_state(frame, top_cols[1], snapshot);
    render_latest_proposal(frame, bottom_cols[0], snapshot);
    render_sync_gaps(frame, bottom_cols[1], snapshot);
}

fn render_chain_state(frame: &mut Frame<'_>, area: Rect, snapshot: &ProofsSnapshot) {
    let block = Block::default()
        .title(" Chain State ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let lines = vec![
        kv_line("L1 block", &fmt_opt_block(snapshot.l1_block)),
        kv_line("L2 latest block", &fmt_opt_block(snapshot.l2_latest_block)),
        kv_line("L2 safe block", &fmt_opt_block(snapshot.l2_safe_block)),
        kv_line("L2 finalized block", &fmt_opt_block(snapshot.l2_finalized_block)),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_anchor_state(frame: &mut Frame<'_>, area: Rect, snapshot: &ProofsSnapshot) {
    let paused_str = snapshot
        .system_paused
        .map_or_else(|| "-".to_string(), |p| if p { "Yes".to_string() } else { "No".to_string() });

    let block = Block::default()
        .title(" Onchain Anchor State ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let lines = vec![
        kv_line(
            "Respected game type",
            &snapshot.respected_game_type.map_or_else(|| "-".to_string(), |g| g.to_string()),
        ),
        kv_line(
            "Total games",
            &snapshot.total_games.map_or_else(|| "-".to_string(), format_number),
        ),
        kv_line("Anchor L2 block", &fmt_opt_block(snapshot.anchor_l2_block)),
        kv_line(
            "Anchor root",
            &snapshot.anchor_root.map_or_else(|| "-".to_string(), |r| format!("{r:#x}")),
        ),
        kv_line("System paused", &paused_str),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_latest_proposal(frame: &mut Frame<'_>, area: Rect, snapshot: &ProofsSnapshot) {
    let title = snapshot.respected_game_type.map_or_else(
        || " Latest Proposal ".to_string(),
        |gt| format!(" Latest Proposal (game type {gt}) "),
    );

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let lines = snapshot.latest_proposal.as_ref().map_or_else(
        || {
            vec![Line::from(Span::styled(
                "  No proposals found",
                Style::default().fg(Color::DarkGray),
            ))]
        },
        render_proposal_lines,
    );

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_proposal_lines(proposal: &LatestProposal) -> Vec<Line<'static>> {
    let status_str = match proposal.status {
        0 => "IN_PROGRESS",
        1 => "CHALLENGER_WINS",
        2 => "DEFENDER_WINS",
        _ => "UNKNOWN",
    };

    let status_color = match proposal.status {
        0 => Color::Yellow,
        1 => Color::Red,
        2 => Color::Green,
        _ => Color::DarkGray,
    };

    let created_str = chrono::DateTime::from_timestamp(proposal.created_at as i64, 0)
        .map_or_else(|| "-".to_string(), |dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string());

    vec![
        kv_line("Game address", &format!("{:#x}", proposal.game_address)),
        kv_line("Proposed L2 block", &format_number(proposal.l2_block)),
        kv_line("Root claim", &format!("{:#x}", proposal.root_claim)),
        kv_line_colored("Status", status_str, status_color),
        kv_line("Created at", &created_str),
    ]
}

fn render_sync_gaps(frame: &mut Frame<'_>, area: Rect, snapshot: &ProofsSnapshot) {
    let block = Block::default()
        .title(" Sync Gaps ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let proposed_l2 = snapshot.latest_proposal.as_ref().map(|p| p.l2_block);
    let safe = snapshot.l2_safe_block;
    let latest = snapshot.l2_latest_block;
    let anchor = snapshot.anchor_l2_block;

    let mut lines = Vec::new();

    // Proposer behind safe head.
    if let (Some(proposed), Some(safe_block)) = (proposed_l2, safe) {
        let gap = safe_block.saturating_sub(proposed);
        lines.push(gap_line("Proposer behind safe head", gap));
    }

    // Proposer behind latest head.
    if let (Some(proposed), Some(latest_block)) = (proposed_l2, latest) {
        let gap = latest_block.saturating_sub(proposed);
        lines.push(gap_line("Proposer behind latest head", gap));
    }

    // Anchor behind latest proposal.
    if let (Some(anchor_block), Some(proposed)) = (anchor, proposed_l2) {
        let gap = proposed.saturating_sub(anchor_block);
        lines.push(gap_line("Anchor behind latest proposal", gap));
    }

    // Anchor behind safe head.
    if let (Some(anchor_block), Some(safe_block)) = (anchor, safe) {
        let gap = safe_block.saturating_sub(anchor_block);
        lines.push(gap_line("Anchor behind safe head", gap));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Insufficient data to compute gaps",
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// =============================================================================
// Helpers
// =============================================================================

fn kv_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label}: "), Style::default().fg(Color::DarkGray)),
        Span::styled(value.to_string(), Style::default().fg(Color::White)),
    ])
}

fn kv_line_colored(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label}: "), Style::default().fg(Color::DarkGray)),
        Span::styled(value.to_string(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ])
}

fn gap_line(label: &str, blocks: u64) -> Line<'static> {
    let time_str = format_duration_from_blocks(blocks);

    let color = if blocks > 50_000 {
        Color::Red
    } else if blocks > 10_000 {
        Color::Yellow
    } else {
        Color::Green
    };

    Line::from(vec![
        Span::styled(format!("  {label}: "), Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{} blocks", format_number(blocks)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  (~{time_str})"), Style::default().fg(Color::DarkGray)),
    ])
}

fn fmt_opt_block(val: Option<u64>) -> String {
    val.map_or_else(|| "-".to_string(), format_number)
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Estimates wall-clock duration from a block count using 2-second L2 block time.
fn format_duration_from_blocks(blocks: u64) -> String {
    let total_seconds = blocks * 2;
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}
