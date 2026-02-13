use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

use crate::{
    app::{Action, Resources, View},
    commands::common::COLOR_BASE_BLUE,
    tui::Keybinding,
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "r", description: "Refresh config" },
];

/// View displaying chain configuration and L1 system config parameters.
#[derive(Debug)]
pub struct ConfigView {
    needs_refresh: bool,
}

impl Default for ConfigView {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigView {
    /// Creates a new config view.
    pub const fn new() -> Self {
        Self { needs_refresh: true }
    }
}

impl View for ConfigView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn handle_key(&mut self, key: KeyEvent, _resources: &mut Resources) -> Action {
        match key.code {
            KeyCode::Char('r') => {
                self.needs_refresh = true;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        render_chain_config(frame, chunks[0], resources);
        render_system_config(frame, chunks[1], resources);
    }
}

fn render_chain_config(f: &mut Frame<'_>, area: Rect, resources: &Resources) {
    let config = &resources.config;

    let batcher_str = config
        .batcher_address
        .as_ref()
        .map(|a| format!("{a:#x}"))
        .unwrap_or_else(|| "-".to_string());

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Chain: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                &config.name,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];

    let fields: &[(&str, &str)] = &[
        ("RPC", config.rpc.as_str()),
        ("Flashblocks WS", config.flashblocks_ws.as_str()),
        ("L1 RPC", config.l1_rpc.as_str()),
        ("Op-Node RPC", config.op_node_rpc.as_ref().map(|u| u.as_str()).unwrap_or("-")),
        ("Batcher Address", &batcher_str),
    ];

    for (label, value) in fields {
        lines.push(Line::from(vec![
            Span::styled(format!("{label}: "), Style::default().fg(Color::DarkGray)),
            Span::styled(*value, Style::default().fg(Color::Cyan)),
        ]));
    }

    let block = Block::default()
        .title(" Chain Configuration ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

fn render_system_config(f: &mut Frame<'_>, area: Rect, resources: &Resources) {
    let block = Block::default()
        .title(" L1 SystemConfig ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let content = resources.system_config.as_ref().map_or_else(
        || {
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Loading system config...",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled("(Requires L1 RPC)", Style::default().fg(Color::DarkGray))),
            ];
            Paragraph::new(lines).alignment(Alignment::Center)
        },
        |sys| {
            let gas_limit_str =
                sys.gas_limit.map(|g| g.to_string()).unwrap_or_else(|| "-".to_string());
            let elasticity_str =
                sys.eip1559_elasticity.map(|e| e.to_string()).unwrap_or_else(|| "-".to_string());
            let denominator_str =
                sys.eip1559_denominator.map(|d| d.to_string()).unwrap_or_else(|| "-".to_string());

            let lines = vec![
                Line::from(vec![
                    Span::styled("Gas Limit: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(gas_limit_str, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("EIP-1559 Elasticity: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(elasticity_str, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("EIP-1559 Denominator: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(denominator_str, Style::default().fg(Color::White)),
                ]),
            ];
            Paragraph::new(lines)
        },
    );

    f.render_widget(content.block(block), area);
}
