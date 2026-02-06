use std::{
    io::Stdout,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, hex};
use anyhow::Result;
use clap::Subcommand;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Rect},
    prelude::*,
    widgets::{Block, Borders, Cell, Row, Table},
};
use tokio::time::interval;

use crate::{
    config::ChainConfig,
    l1_client::{FullSystemConfig, fetch_full_system_config},
    tui::{AppFrame, Keybinding, NavResult, StatusInfo, restore_terminal, setup_terminal},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(12);

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding::new("r", "Refresh now"),
    Keybinding::new("?", "Toggle help"),
    Keybinding::new("h", "Home"),
    Keybinding::new("q", "Quit"),
];

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// View the current chain configuration and L1 `SystemConfig` values
    #[command(visible_alias = "v")]
    View,
}

pub async fn run_config(command: ConfigCommand, config: &ChainConfig) -> Result<()> {
    match command {
        ConfigCommand::View => run_view(config).await,
    }
}

/// Run the default config view (called from homescreen)
pub async fn default_view(config: &ChainConfig) -> Result<NavResult> {
    let mut terminal = setup_terminal()?;
    let result = run_view_loop(&mut terminal, config).await;
    restore_terminal(&mut terminal)?;
    result
}

struct ConfigViewState {
    chain_config: ChainConfig,
    current: FullSystemConfig,
    previous: Option<FullSystemConfig>,
    last_fetch: Instant,
    fetch_error: Option<String>,
    show_help: bool,
    is_fetching: bool,
}

impl ConfigViewState {
    fn new(chain_config: ChainConfig) -> Self {
        Self {
            chain_config,
            current: FullSystemConfig::default(),
            previous: None,
            last_fetch: Instant::now(),
            fetch_error: None,
            show_help: false,
            is_fetching: true,
        }
    }

    fn seconds_until_refresh(&self) -> u64 {
        REFRESH_INTERVAL.saturating_sub(self.last_fetch.elapsed()).as_secs()
    }

    fn should_refresh(&self) -> bool {
        self.last_fetch.elapsed() >= REFRESH_INTERVAL
    }
}

async fn run_view(config: &ChainConfig) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let _ = run_view_loop(&mut terminal, config).await?;
    restore_terminal(&mut terminal)?;
    Ok(())
}

async fn run_view_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: &ChainConfig,
) -> Result<NavResult> {
    let mut state = ConfigViewState::new(config.clone());
    let mut events = EventStream::new();
    let mut refresh_interval = interval(Duration::from_millis(100));

    // Initial fetch
    do_fetch(&mut state).await;

    loop {
        terminal.draw(|f| draw_config_view(f, &state))?;

        tokio::select! {
            _ = refresh_interval.tick() => {
                if state.should_refresh() && !state.is_fetching {
                    do_fetch(&mut state).await;
                }
            }
            Some(Ok(Event::Key(key))) = events.next() => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(NavResult::Quit);
                }
                match key.code {
                    KeyCode::Char('q') => return Ok(NavResult::Quit),
                    KeyCode::Char('h') => return Ok(NavResult::Home),
                    KeyCode::Char('?') => state.show_help = !state.show_help,
                    KeyCode::Char('r') if !state.is_fetching => do_fetch(&mut state).await,
                    _ => {}
                }
            }
        }
    }
}

async fn do_fetch(state: &mut ConfigViewState) {
    state.is_fetching = true;

    match fetch_full_system_config(
        state.chain_config.l1_rpc.as_str(),
        state.chain_config.system_config,
    )
    .await
    {
        Ok(new_config) => {
            // Only set previous if we had a successful fetch before
            if state.current != FullSystemConfig::default() {
                state.previous = Some(state.current.clone());
            }
            state.current = new_config;
            state.fetch_error = None;
        }
        Err(e) => {
            state.fetch_error = Some(e.to_string());
        }
    }

    state.last_fetch = Instant::now();
    state.is_fetching = false;
}

fn draw_config_view(f: &mut Frame, state: &ConfigViewState) {
    let layout = AppFrame::split_layout(f.area(), state.show_help);

    // Build status info for the status bar
    let status_info = build_status_info(state);

    // Render the app frame (status bar + help sidebar)
    AppFrame::render(f, &layout, &state.chain_config.name, KEYBINDINGS, Some(&status_info));

    // Split content area for chain config and system config tables
    let content_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // Chain config table
            Constraint::Length(1), // Spacing
            Constraint::Min(14),   // System config table
        ])
        .split(layout.content);

    draw_chain_config_table(f, content_chunks[0], &state.chain_config);
    draw_system_config_table(f, content_chunks[2], state);
}

fn draw_chain_config_table(f: &mut Frame, area: Rect, config: &ChainConfig) {
    let label_style = Style::default().fg(Color::Cyan);
    let value_style = Style::default().fg(Color::White);

    let rows = vec![
        Row::new(vec![
            Cell::from("Name").style(label_style),
            Cell::from(config.name.clone()).style(value_style),
        ]),
        Row::new(vec![
            Cell::from("L2 RPC").style(label_style),
            Cell::from(config.rpc.to_string()).style(value_style),
        ]),
        Row::new(vec![
            Cell::from("Flashblocks WS").style(label_style),
            Cell::from(config.flashblocks_ws.to_string()).style(value_style),
        ]),
        Row::new(vec![
            Cell::from("L1 RPC").style(label_style),
            Cell::from(config.l1_rpc.to_string()).style(value_style),
        ]),
        Row::new(vec![
            Cell::from("SystemConfig").style(label_style),
            Cell::from(format!("{:#x}", config.system_config)).style(value_style),
        ]),
    ];

    let table = Table::new(rows, [Constraint::Length(16), Constraint::Min(40)]).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" Chain Configuration ")
            .title_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
    );

    f.render_widget(table, area);
}

fn draw_system_config_table(f: &mut Frame, area: Rect, state: &ConfigViewState) {
    let cur = &state.current;
    let prev = state.previous.as_ref();

    // Macro to check if a field changed between refreshes
    macro_rules! changed {
        ($field:ident) => {
            prev.map_or(false, |p| cur.$field != p.$field)
        };
    }

    let rows = vec![
        make_row("Gas Limit", format_gas_limit(cur.gas_limit), changed!(gas_limit)),
        make_row(
            "EIP-1559 Elasticity",
            format_option(cur.eip1559_elasticity),
            changed!(eip1559_elasticity),
        ),
        make_row(
            "EIP-1559 Denominator",
            format_option(cur.eip1559_denominator),
            changed!(eip1559_denominator),
        ),
        make_row("Batcher Hash", format_batcher_hash(cur.batcher_hash), changed!(batcher_hash)),
        make_row("Overhead", format_option(cur.overhead), changed!(overhead)),
        make_row("Scalar", format_option(cur.scalar), changed!(scalar)),
        make_row(
            "Unsafe Block Signer",
            format_address(cur.unsafe_block_signer),
            changed!(unsafe_block_signer),
        ),
        make_row("Start Block", format_option(cur.start_block), changed!(start_block)),
        make_row("Basefee Scalar", format_option(cur.basefee_scalar), changed!(basefee_scalar)),
        make_row(
            "Blobbasefee Scalar",
            format_option(cur.blobbasefee_scalar),
            changed!(blobbasefee_scalar),
        ),
    ];

    let table =
        Table::new(rows, [Constraint::Length(20), Constraint::Min(40), Constraint::Length(10)])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(" L1 SystemConfig (auto-refresh) ")
                    .title_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            );

    f.render_widget(table, area);
}

fn make_row(field: &str, value: String, changed: bool) -> Row<'static> {
    let label_style = Style::default().fg(Color::Cyan);
    let value_style = if changed {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let status = if changed { "CHANGED" } else { "" };
    let status_style = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);

    Row::new(vec![
        Cell::from(field.to_string()).style(label_style),
        Cell::from(value).style(value_style),
        Cell::from(status).style(status_style),
    ])
}

fn build_status_info(state: &ConfigViewState) -> StatusInfo {
    if state.is_fetching {
        StatusInfo::new("Refreshing...").with_style(Style::default().fg(Color::Yellow))
    } else if let Some(ref err) = state.fetch_error {
        StatusInfo::new(format!("Error: {err}")).with_style(Style::default().fg(Color::Red))
    } else {
        StatusInfo::new(format!("Next refresh in {}s", state.seconds_until_refresh()))
    }
}

// Formatting helpers

const UNAVAILABLE: &str = "(unavailable)";

fn format_gas_limit(gas: Option<u64>) -> String {
    match gas {
        Some(g) if g >= 1_000_000 => format!("{g} ({:.0}M)", g as f64 / 1_000_000.0),
        Some(g) if g >= 1_000 => format!("{g} ({:.1}K)", g as f64 / 1_000.0),
        Some(g) => g.to_string(),
        None => UNAVAILABLE.to_string(),
    }
}

fn format_option<T: std::fmt::Display>(val: Option<T>) -> String {
    val.map_or_else(|| UNAVAILABLE.to_string(), |v| v.to_string())
}

fn format_batcher_hash(hash: Option<[u8; 32]>) -> String {
    hash.map_or_else(|| UNAVAILABLE.to_string(), |h| format!("0x{}", hex::encode(h)))
}

fn format_address(addr: Option<Address>) -> String {
    addr.map_or_else(|| UNAVAILABLE.to_string(), |a| format!("{a:#x}"))
}
