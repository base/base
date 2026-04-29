//! Network upgrade activation countdown and history view.

use std::{
    collections::HashMap,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base_common_chains::ChainConfig;
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use serde_json::json;
use tokio::{sync::mpsc, task::JoinHandle};
use url::Url;

use crate::{
    app::{Action, Resources, View},
    commands::COLOR_BASE_BLUE,
    tui::Keybinding,
};

// ── Segment display (7-char wide × 7-row tall per digit) ─────────────────────

const SEG: [[&str; 7]; 10] = [
    [" ═════ ", "║     ║", "║     ║", "       ", "║     ║", "║     ║", " ═════ "], // 0
    ["       ", "      ║", "      ║", "       ", "      ║", "      ║", "       "], // 1
    [" ═════ ", "      ║", "      ║", " ═════ ", "║      ", "║      ", " ═════ "], // 2
    [" ═════ ", "      ║", "      ║", " ═════ ", "      ║", "      ║", " ═════ "], // 3
    ["       ", "║     ║", "║     ║", " ═════ ", "      ║", "      ║", "       "], // 4
    [" ═════ ", "║      ", "║      ", " ═════ ", "      ║", "      ║", " ═════ "], // 5
    [" ═════ ", "║      ", "║      ", " ═════ ", "║     ║", "║     ║", " ═════ "], // 6
    [" ═════ ", "      ║", "      ║", "       ", "      ║", "      ║", "       "], // 7
    [" ═════ ", "║     ║", "║     ║", " ═════ ", "║     ║", "║     ║", " ═════ "], // 8
    [" ═════ ", "║     ║", "║     ║", " ═════ ", "      ║", "      ║", " ═════ "], // 9
];

const SEG_ROWS: usize = 7;
const SEG_DIGIT_W: usize = 7;
const SEG_GROUP_W: usize = SEG_DIGIT_W + 1 + SEG_DIGIT_W; // digit + gap + digit = 15
const SEP_W: usize = 3;

const fn colon_row(r: usize) -> &'static str {
    if r == 2 || r == 4 { " ▪ " } else { "   " }
}

// ── Upgrade data ──────────────────────────────────────────────────────────────

#[derive(Debug)]
struct UpgradeSpec {
    name: &'static str,
    timestamp: Option<u64>,
}

#[derive(Debug)]
struct ChainUpgrades {
    display_name: &'static str,
    /// RPC URL for this chain, loaded from `~/.config/base/networks/{name}.yaml` at startup.
    /// Falls back to a hardcoded public URL only for mainnet and sepolia.
    /// `None` for internal networks (zeronet, devnet) when no user config is present.
    rpc: Option<String>,
    specs: Vec<UpgradeSpec>,
}

fn specs_from_config(cfg: &ChainConfig) -> Vec<UpgradeSpec> {
    vec![
        UpgradeSpec { name: "Delta", timestamp: Some(cfg.delta_timestamp) },
        UpgradeSpec { name: "Canyon", timestamp: Some(cfg.canyon_timestamp) },
        UpgradeSpec { name: "Ecotone", timestamp: Some(cfg.ecotone_timestamp) },
        UpgradeSpec { name: "Fjord", timestamp: Some(cfg.fjord_timestamp) },
        UpgradeSpec { name: "Granite", timestamp: Some(cfg.granite_timestamp) },
        UpgradeSpec { name: "Holocene", timestamp: Some(cfg.holocene_timestamp) },
        UpgradeSpec { name: "Isthmus", timestamp: Some(cfg.isthmus_timestamp) },
        UpgradeSpec { name: "Jovian", timestamp: Some(cfg.jovian_timestamp) },
        UpgradeSpec { name: "Azul", timestamp: cfg.azul_timestamp },
    ]
}

/// Reads the `rpc` field from `~/.config/base/networks/{name}.yaml` if it exists.
fn user_config_rpc(name: &str) -> Option<String> {
    let dir = dirs::home_dir()?.join(".config").join("base").join("networks");
    let path = [dir.join(format!("{name}.yaml")), dir.join(format!("{name}.yml"))]
        .into_iter()
        .find(|p| p.exists())?;
    let contents = std::fs::read_to_string(path).ok()?;
    #[derive(serde::Deserialize)]
    struct RpcOnly {
        rpc: Url,
    }
    let parsed: RpcOnly = serde_yaml::from_str(&contents).ok()?;
    Some(parsed.rpc.to_string())
}

fn all_chains() -> [ChainUpgrades; 4] {
    [
        ChainUpgrades {
            display_name: "Devnet",
            rpc: user_config_rpc("devnet"),
            specs: specs_from_config(ChainConfig::devnet()),
        },
        ChainUpgrades {
            display_name: "Zeronet",
            rpc: user_config_rpc("zeronet"),
            specs: specs_from_config(ChainConfig::zeronet()),
        },
        ChainUpgrades {
            display_name: "Sepolia",
            rpc: user_config_rpc("sepolia")
                .or_else(|| Some("https://sepolia.base.org".to_string())),
            specs: specs_from_config(ChainConfig::sepolia()),
        },
        ChainUpgrades {
            display_name: "Mainnet",
            rpc: user_config_rpc("mainnet")
                .or_else(|| Some("https://mainnet.base.org".to_string())),
            specs: specs_from_config(ChainConfig::mainnet()),
        },
    ]
}

// ── Check types ───────────────────────────────────────────────────────────────

/// Expected check names for Azul, in execution order.
const AZUL_CHECK_NAMES: &[&str] = &[
    "CLZ zero",
    "CLZ one",
    "CLZ high-bit",
    "CLZ four-bits",
    "MODEXP size limit",
    "MODEXP min gas",
    "P256VERIFY gas",
    "eth_config",
];

/// Expected check names for Jovian, in execution order.
const JOVIAN_CHECK_NAMES: &[&str] = &["bn256Pairing limit", "extra data v1", "GPO implementation"];

fn check_names_for(hardfork: &str) -> &'static [&'static str] {
    match hardfork {
        "Azul" => AZUL_CHECK_NAMES,
        "Jovian" => JOVIAN_CHECK_NAMES,
        _ => &[],
    }
}

/// Returns the last hardfork spec for a chain that has defined checks, or `None`.
fn target_hardfork(chain: &ChainUpgrades) -> Option<&'static str> {
    chain
        .specs
        .iter()
        .rev()
        .find(|s| s.timestamp.is_some() && !check_names_for(s.name).is_empty())
        .map(|s| s.name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckMode {
    Before,
    After,
}

#[derive(Debug, Clone)]
struct CheckResult {
    passed: Option<bool>,
    detail: String,
}

/// Streaming update sent from the background check task to the view.
#[derive(Debug)]
enum CheckUpdate {
    /// A check is about to run.
    Starting(String),
    /// A check completed.
    Completed { name: String, result: CheckResult },
}

/// State for the checks panel. Tracks streaming results per chain.
#[derive(Debug, Default)]
struct ChecksPanel {
    /// Chain index these checks were (or are being) run for.
    chain_idx: Option<usize>,
    /// Which hardfork's checks are running.
    hardfork: Option<&'static str>,
    mode: Option<CheckMode>,
    rpc_url: String,
    /// Name of the check currently executing.
    current: Option<String>,
    /// Completed results keyed by check name.
    results: HashMap<String, CheckResult>,
    running: bool,
    rx: Option<mpsc::Receiver<CheckUpdate>>,
    handle: Option<JoinHandle<()>>,
    /// Wall-clock instant the most recent run was started. Used to throttle
    /// auto-refresh so we don't re-issue checks faster than the configured
    /// cadence even if the previous run finished quickly.
    last_run_at: Option<Instant>,
}

impl ChecksPanel {
    fn start(
        &mut self,
        chain_idx: usize,
        rpc_url: String,
        hardfork: &'static str,
        mode: CheckMode,
    ) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        let (tx, rx) = mpsc::channel(64);
        let chain_changed = self.chain_idx != Some(chain_idx);
        let hardfork_changed = self.hardfork != Some(hardfork);
        self.chain_idx = Some(chain_idx);
        self.hardfork = Some(hardfork);
        self.mode = Some(mode);
        self.rpc_url = rpc_url.clone();
        self.current = None;
        // Preserve previous results across auto-refreshes so the table updates
        // in-place rather than blanking on every tick. Only clear when the
        // target context actually changed.
        if chain_changed || hardfork_changed {
            self.results.clear();
        }
        self.running = true;
        self.rx = Some(rx);
        self.last_run_at = Some(Instant::now());
        self.handle = Some(tokio::spawn(run_checks_streaming(hardfork, rpc_url, mode, tx)));
    }

    fn reset(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        self.chain_idx = None;
        self.hardfork = None;
        self.mode = None;
        self.rpc_url.clear();
        self.current = None;
        self.results.clear();
        self.running = false;
        self.rx = None;
        self.last_run_at = None;
    }

    fn poll(&mut self) {
        let Some(ref mut rx) = self.rx else { return };
        loop {
            match rx.try_recv() {
                Ok(CheckUpdate::Starting(name)) => {
                    // Drop any prior result for this check so the row shows the
                    // spinner instead of a stale PASS/FAIL while the re-run is
                    // in flight. Without this, an auto-refresh would silently
                    // display old verdicts until each check completes again.
                    self.results.remove(&name);
                    self.current = Some(name);
                }
                Ok(CheckUpdate::Completed { name, result }) => {
                    self.results.insert(name, result);
                    self.current = None;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.running = false;
                    self.current = None;
                    self.rx = None;
                    break;
                }
            }
        }
    }
}

// ── Color zones ───────────────────────────────────────────────────────────────

const SECS_PER_MINUTE: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * SECS_PER_MINUTE;
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;

const fn zone_color(remaining_secs: i64) -> Color {
    match remaining_secs {
        s if s > (30 * SECS_PER_DAY) as i64 => Color::DarkGray,
        s if s > (14 * SECS_PER_DAY) as i64 => COLOR_BASE_BLUE,
        s if s > (7 * SECS_PER_DAY) as i64 => Color::Cyan,
        s if s > (3 * SECS_PER_DAY) as i64 => Color::Green,
        s if s > SECS_PER_DAY as i64 => Color::Yellow,
        s if s > SECS_PER_HOUR as i64 => Color::Rgb(255, 140, 0),
        s if s > (10 * SECS_PER_MINUTE) as i64 => Color::Red,
        _ => Color::Magenta,
    }
}

const fn zone_message(remaining_secs: i64) -> &'static str {
    match remaining_secs {
        s if s > (30 * SECS_PER_DAY) as i64 => "standing by...",
        s if s > (14 * SECS_PER_DAY) as i64 => "less than 30 days to go",
        s if s > (7 * SECS_PER_DAY) as i64 => "under two weeks",
        s if s > (3 * SECS_PER_DAY) as i64 => "less than a week",
        s if s > SECS_PER_DAY as i64 => "under 3 days",
        s if s > SECS_PER_HOUR as i64 => "final 24 hours — all hands on deck",
        s if s > (10 * SECS_PER_MINUTE) as i64 => "under an hour — stand by your terminals",
        _ => "under 10 minutes — THIS IS IT",
    }
}

const SECS_FOUR_WEEKS: u64 = 28 * SECS_PER_DAY;

const CYCLE_COLORS: &[Color] =
    &[Color::LightGreen, Color::Green, Color::Cyan, Color::Yellow, Color::LightGreen];

const CONFETTI: &[&str] =
    &["✦", "✧", "✨", "⚡", "★", "☆", "◆", "◇", "▲", "△", "●", "○", "♦", "♢", "❋", "✿", "❊", "✺"];

// ── Time helpers ──────────────────────────────────────────────────────────────

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn fmt_timestamp(ts: u64) -> String {
    if ts == 0 {
        return "genesis".to_string();
    }
    DateTime::<Utc>::from_timestamp(ts as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn fmt_elapsed(elapsed_secs: u64) -> String {
    let days = elapsed_secs / SECS_PER_DAY;
    let hours = (elapsed_secs % SECS_PER_DAY) / SECS_PER_HOUR;
    let minutes = (elapsed_secs % SECS_PER_HOUR) / SECS_PER_MINUTE;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m ago")
    } else if hours > 0 {
        format!("{hours}h {minutes}m ago")
    } else {
        format!("{minutes}m ago")
    }
}

// ── View ──────────────────────────────────────────────────────────────────────

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "←/→", description: "Switch chain" },
    Keybinding { key: "1-4", description: "Jump to chain" },
    Keybinding { key: "r", description: "Run checks now" },
    Keybinding { key: "a", description: "Toggle auto-refresh" },
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
];

/// Cadence at which checks are re-run when auto-refresh is enabled.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Network upgrade activation countdown and history view.
#[derive(Debug)]
pub struct UpgradesView {
    chains: [ChainUpgrades; 4],
    selected_chain: usize,
    tick_count: u64,
    checks: ChecksPanel,
    /// When true, re-run the activation checks on the configured cadence
    /// without requiring the user to press [r].
    auto_refresh: bool,
}

impl Default for UpgradesView {
    fn default() -> Self {
        Self::new()
    }
}

impl UpgradesView {
    /// Creates a new upgrades view.
    pub fn new() -> Self {
        Self {
            chains: all_chains(),
            selected_chain: 0,
            tick_count: 0,
            checks: ChecksPanel {
                chain_idx: None,
                hardfork: None,
                mode: None,
                rpc_url: String::new(),
                current: None,
                results: HashMap::new(),
                running: false,
                rx: None,
                handle: None,
                last_run_at: None,
            },
            auto_refresh: true,
        }
    }

    /// Kick off an activation-check run for the currently selected chain, if
    /// the chain has a hardfork with defined checks and a usable RPC URL.
    fn start_checks(&mut self, resources: &Resources) {
        let now = now_unix();
        let chain = &self.chains[self.selected_chain];
        let Some(spec) =
            target_hardfork(chain).and_then(|name| chain.specs.iter().find(|s| s.name == name))
        else {
            return;
        };
        let ts = spec.timestamp.unwrap();
        let Some(rpc) = self.rpc_for_selected(resources) else { return };
        let mode = if ts > now { CheckMode::Before } else { CheckMode::After };
        self.checks.start(self.selected_chain, rpc, spec.name, mode);
    }

    fn rpc_for_selected(&self, resources: &Resources) -> Option<String> {
        let chain = &self.chains[self.selected_chain];
        let loaded = resources.config.name.to_lowercase();
        let selected = chain.display_name.to_lowercase();
        if loaded == selected || (selected == "devnet" && loaded.contains("devnet")) {
            Some(resources.config.rpc.to_string())
        } else {
            chain.rpc.clone()
        }
    }
}

impl View for UpgradesView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                let prev = self.selected_chain.saturating_sub(1);
                if prev != self.selected_chain {
                    self.selected_chain = prev;
                    self.checks.reset();
                }
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                if self.selected_chain < self.chains.len() - 1 {
                    self.selected_chain += 1;
                    self.checks.reset();
                }
            }
            KeyCode::Char(c @ '1'..='4') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < self.chains.len() && idx != self.selected_chain {
                    self.selected_chain = idx;
                    self.checks.reset();
                }
            }
            KeyCode::Char('r') if !self.checks.running => {
                self.start_checks(resources);
            }
            KeyCode::Char('a') => {
                self.auto_refresh = !self.auto_refresh;
            }
            _ => {}
        }
        Action::None
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        self.tick_count = self.tick_count.wrapping_add(1);
        self.checks.poll();

        if self.auto_refresh && !self.checks.running {
            let due = self.checks.last_run_at.is_none_or(|t| t.elapsed() >= AUTO_REFRESH_INTERVAL);
            if due {
                self.start_checks(resources);
            }
        }

        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, _resources: &Resources) {
        let now = now_unix();
        let chain = &self.chains[self.selected_chain];

        let upcoming = chain
            .specs
            .iter()
            .filter_map(|s| s.timestamp.filter(|&ts| ts > 0).map(|ts| (s.name, ts)))
            .filter(|(_, ts)| *ts > now)
            .min_by_key(|(_, ts)| *ts);

        let latest_activated = chain
            .specs
            .iter()
            .filter_map(|s| s.timestamp.filter(|&ts| ts > 0).map(|ts| (s.name, ts)))
            .filter(|(_, ts)| *ts <= now)
            .max_by_key(|(_, ts)| *ts);

        // Layout: chain tabs | main display | bottom (history + checks) | footer
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(16),
                Constraint::Min(6),
                Constraint::Length(1),
            ])
            .split(area);

        // Dull the activated banner if the hardfork is stale (> 4 weeks old) or if a
        // newer hardfork is already active on any other chain, meaning this network is
        // running behind the frontier.
        let dull = if let Some((_, lat_ts)) = latest_activated {
            let stale = now.saturating_sub(lat_ts) > SECS_FOUR_WEEKS;
            let superseded = self.chains.iter().enumerate().any(|(i, c)| {
                i != self.selected_chain
                    && c.specs
                        .iter()
                        .filter_map(|s| s.timestamp.filter(|&ts| ts > 0 && ts <= now))
                        .any(|ts| ts > lat_ts)
            });
            stale || superseded
        } else {
            false
        };

        render_chain_tabs(frame, outer[0], &self.chains, self.selected_chain);

        match upcoming {
            Some((name, ts)) => {
                let remaining = ts as i64 - now as i64;
                render_countdown(frame, outer[1], name, ts, remaining, now, self.tick_count);
            }
            None => match latest_activated {
                Some((name, ts)) => {
                    render_activated(
                        frame,
                        outer[1],
                        name,
                        ts,
                        now.saturating_sub(ts),
                        self.tick_count,
                        dull,
                    );
                }
                None => render_tbd(frame, outer[1]),
            },
        }

        let bottom = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(outer[2]);

        render_history(frame, bottom[0], chain, now);
        let active_hf = target_hardfork(chain);
        render_checks_panel(
            frame,
            bottom[1],
            &self.checks,
            self.tick_count,
            active_hf,
            self.auto_refresh,
        );
        render_footer(frame, outer[3], self.checks.running, self.auto_refresh);
    }
}

// ── Rendering helpers ─────────────────────────────────────────────────────────

fn render_chain_tabs(frame: &mut Frame<'_>, area: Rect, chains: &[ChainUpgrades], selected: usize) {
    let block =
        Block::default().borders(Borders::ALL).border_style(Style::default().fg(COLOR_BASE_BLUE));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut spans = vec![Span::raw("  ")];
    for (i, chain) in chains.iter().enumerate() {
        let label = format!(" {} ", chain.display_name);
        if i == selected {
            spans.push(Span::styled(
                label,
                Style::default().fg(Color::Black).bg(COLOR_BASE_BLUE).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
        }
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled("  ←/→  1·2·3·4", Style::default().fg(Color::DarkGray)));
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn render_countdown(
    frame: &mut Frame<'_>,
    area: Rect,
    name: &'static str,
    ts: u64,
    remaining: i64,
    now: u64,
    _tick: u64,
) {
    let color = zone_color(remaining);
    let msg = zone_message(remaining);

    let secs = remaining.max(0) as u64;
    let (days, hours, minutes, seconds) = (
        secs / SECS_PER_DAY,
        (secs % SECS_PER_DAY) / SECS_PER_HOUR,
        (secs % SECS_PER_HOUR) / SECS_PER_MINUTE,
        secs % SECS_PER_MINUTE,
    );

    let start_ts = ts.saturating_sub(90 * SECS_PER_DAY);
    let total = ts.saturating_sub(start_ts) as f64;
    let elapsed = now.saturating_sub(start_ts) as f64;
    let pct = if total > 0.0 { (elapsed / total).clamp(0.0, 1.0) } else { 1.0 };
    let bar_w = 50usize;
    let filled = (bar_w as f64 * pct) as usize;
    let bar =
        format!("|{}{}|  {:.1}%", "█".repeat(filled), "░".repeat(bar_w - filled), pct * 100.0);

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    lines.extend(clock_lines(days, hours, minutes, seconds, color));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(bar, Style::default().fg(color))));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        msg,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!("target {}  ·  ts {ts}", fmt_timestamp(ts)),
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .title(format!(" ⚡  BASE {name} UPGRADE  ⚡ "))
        .title_style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color));

    frame.render_widget(Paragraph::new(lines).block(block).alignment(Alignment::Center), area);
}

fn render_activated(
    frame: &mut Frame<'_>,
    area: Rect,
    name: &'static str,
    ts: u64,
    elapsed_secs: u64,
    tick: u64,
    dull: bool,
) {
    if dull {
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(""),
            Line::from(Span::styled(
                "✓  activated",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("{name} is live"),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                format!("activated {}  ·  {}", fmt_timestamp(ts), fmt_elapsed(elapsed_secs)),
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let block = Block::default()
            .title(format!(" BASE {name} UPGRADE "))
            .title_style(Style::default().fg(Color::DarkGray))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(Paragraph::new(lines).block(block).alignment(Alignment::Center), area);
        return;
    }

    let cycle_color = CYCLE_COLORS[(tick / 4) as usize % CYCLE_COLORS.len()];
    let n = CONFETTI.len();
    let phase = (tick / 4) as usize;
    let conf_fwd: String = (0..n).map(|i| format!("{}  ", CONFETTI[(phase + i) % n])).collect();
    let conf_bwd: String =
        (0..n).map(|i| format!("{}  ", CONFETTI[(phase + n - 1 - i) % n])).collect();

    let lines: Vec<Line<'static>> = vec![
        Line::from(""),
        Line::from(Span::styled(conf_fwd, Style::default().fg(Color::Yellow))),
        Line::from(""),
        Line::from(Span::styled(
            "  A C T I V A T E D  ",
            Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("{name} is LIVE"),
            Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("activated {}  ·  {}", fmt_timestamp(ts), fmt_elapsed(elapsed_secs)),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(conf_bwd, Style::default().fg(Color::Cyan))),
        Line::from(""),
    ];

    let block = Block::default()
        .title(format!(" ⚡  BASE {name} UPGRADE  ⚡ "))
        .title_style(Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(cycle_color));

    frame.render_widget(Paragraph::new(lines).block(block).alignment(Alignment::Center), area);
}

fn render_tbd(frame: &mut Frame<'_>, area: Rect) {
    let lines: Vec<Line<'static>> = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "T B D",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("upgrade not yet scheduled", Style::default().fg(Color::DarkGray))),
    ];
    let block = Block::default()
        .title(" Upcoming Upgrade ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    frame.render_widget(Paragraph::new(lines).block(block).alignment(Alignment::Center), area);
}

fn render_history(frame: &mut Frame<'_>, area: Rect, chain: &ChainUpgrades, now: u64) {
    let block = Block::default()
        .title(" Upgrade History ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let rows: Vec<Row<'static>> = chain
        .specs
        .iter()
        .rev()
        .map(|spec| {
            let (date_str, status_str, status_color) = match spec.timestamp {
                None => ("-".to_string(), "− TBD".to_string(), Color::DarkGray),
                Some(ts) if ts <= now => {
                    (fmt_timestamp(ts), "✓ Active".to_string(), Color::LightGreen)
                }
                Some(ts) => (fmt_timestamp(ts), "⏳ Upcoming".to_string(), Color::Yellow),
            };
            Row::new([
                Cell::from(spec.name).style(Style::default().fg(Color::White)),
                Cell::from(date_str).style(Style::default().fg(Color::Gray)),
                Cell::from(status_str)
                    .style(Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
            ])
        })
        .collect();

    let header = Row::new(["UPGRADE", "DATE", "STATUS"])
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let widths = [Constraint::Length(10), Constraint::Min(20), Constraint::Length(11)];
    frame.render_widget(Table::new(rows, widths).block(block).header(header), area);
}

fn render_checks_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    panel: &ChecksPanel,
    tick: u64,
    active_hardfork: Option<&'static str>,
    auto_refresh: bool,
) {
    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    // Panel is idle and has never been run.
    if panel.chain_idx.is_none() {
        let hf_name = active_hardfork.unwrap_or("?");
        let check_list = check_names_for(hf_name).join(" · ");
        let hint = if auto_refresh {
            format!("Auto-refreshing {hf_name} checks every 2s — [a] to disable")
        } else {
            format!("Press [r] to run {hf_name} post-upgrade checks · [a] to enable auto-refresh")
        };
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            Line::from(""),
            Line::from(Span::styled(
                format!("Checks: {check_list}"),
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let block = Block::default()
            .title(format!(" {hf_name} Checks "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(Paragraph::new(lines).block(block).alignment(Alignment::Center), area);
        return;
    }

    let hf = panel.hardfork.unwrap_or("?");
    let check_names = check_names_for(hf);

    let mode_str = match panel.mode {
        Some(CheckMode::Before) => "before",
        Some(CheckMode::After) => "after",
        None => "?",
    };

    let passed = panel.results.values().filter(|r| r.passed == Some(true)).count();
    let failed = panel.results.values().filter(|r| r.passed == Some(false)).count();

    let auto_tag = if auto_refresh { "  · auto" } else { "" };
    let (title, border_color) = if panel.running {
        let spin = spinner[(tick / 2) as usize % spinner.len()];
        (format!(" {hf} Checks ({mode_str})  {spin} running…{auto_tag} "), Color::Yellow)
    } else if failed > 0 {
        (format!(" {hf} Checks ({mode_str})  ✓ {passed}  ✗ {failed}{auto_tag} "), Color::Red)
    } else {
        (format!(" {hf} Checks ({mode_str})  ✓ {passed} passed{auto_tag} "), Color::LightGreen)
    };

    let rows: Vec<Row<'static>> = check_names
        .iter()
        .map(|&name| {
            panel.results.get(name).map_or_else(
                || {
                    if panel.current.as_deref() == Some(name) {
                        let spin = spinner[(tick / 2) as usize % spinner.len()];
                        Row::new([
                            Cell::from(name).style(Style::default().fg(Color::White)),
                            Cell::from(spin.to_string()).style(Style::default().fg(Color::Yellow)),
                            Cell::from("").style(Style::default()),
                        ])
                    } else {
                        // Not yet started.
                        Row::new([
                            Cell::from(name).style(Style::default().fg(Color::DarkGray)),
                            Cell::from(""),
                            Cell::from(""),
                        ])
                    }
                },
                |result| {
                    let (status_str, status_color) = match result.passed {
                        None => ("SKIP".to_string(), Color::DarkGray),
                        Some(true) => ("PASS".to_string(), Color::LightGreen),
                        Some(false) => ("FAIL".to_string(), Color::Red),
                    };
                    Row::new([
                        Cell::from(name).style(Style::default().fg(Color::White)),
                        Cell::from(status_str)
                            .style(Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
                        Cell::from(result.detail.clone())
                            .style(Style::default().fg(Color::DarkGray)),
                    ])
                },
            )
        })
        .collect();

    let header = Row::new(["CHECK", "", "DETAIL"])
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let widths = [Constraint::Length(20), Constraint::Length(5), Constraint::Min(8)];

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(border_color))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    // Show the RPC URL below the table as a subtitle via a footer line.
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (table_area, rpc_area) = {
        let s = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(inner);
        (s[0], s[1])
    };

    frame.render_widget(Table::new(rows, widths).header(header), table_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            panel.rpc_url.clone(),
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Right),
        rpc_area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, checks_running: bool, auto_refresh: bool) {
    let key_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::DarkGray);
    let sep = Span::styled("  │  ", Style::default().fg(Color::DarkGray));

    let mut spans = vec![
        Span::styled("[Esc]", key_style),
        Span::raw(" "),
        Span::styled("back", desc_style),
        sep.clone(),
        Span::styled("[←/→]", key_style),
        Span::raw(" "),
        Span::styled("switch chain", desc_style),
        sep.clone(),
        Span::styled("[1-4]", key_style),
        Span::raw(" "),
        Span::styled("jump to chain", desc_style),
        sep.clone(),
    ];

    if checks_running {
        spans.push(Span::styled("checks running…", Style::default().fg(Color::Yellow)));
    } else {
        spans.push(Span::styled("[r]", key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("run checks", desc_style));
    }

    spans.push(sep.clone());
    spans.push(Span::styled("[a]", key_style));
    spans.push(Span::raw(" "));
    let auto_label = if auto_refresh { "auto-refresh: on" } else { "auto-refresh: off" };
    spans.push(Span::styled(auto_label, desc_style));

    spans.push(sep);
    spans.push(Span::styled("[?]", key_style));
    spans.push(Span::raw(" "));
    spans.push(Span::styled("help", desc_style));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── 7-segment clock ───────────────────────────────────────────────────────────

fn clock_lines(
    days: u64,
    hours: u64,
    minutes: u64,
    seconds: u64,
    color: Color,
) -> Vec<Line<'static>> {
    let pairs: Vec<String> = if days > 0 {
        vec![
            format!("{:02}", days.min(99)),
            format!("{hours:02}"),
            format!("{minutes:02}"),
            format!("{seconds:02}"),
        ]
    } else {
        vec![format!("{hours:02}"), format!("{minutes:02}"), format!("{seconds:02}")]
    };
    let labels: &[&str] =
        if days > 0 { &["DAYS", "HRS", "MIN", "SEC"] } else { &["HRS", "MIN", "SEC"] };
    let n = pairs.len();

    let mut lines = Vec::with_capacity(SEG_ROWS + 1);
    lines.extend((0..SEG_ROWS).map(|r| {
        let mut row = String::new();
        for (i, pair) in pairs.iter().enumerate() {
            let d0 = usize::from(pair.as_bytes()[0].wrapping_sub(b'0').min(9));
            let d1 = usize::from(pair.as_bytes()[1].wrapping_sub(b'0').min(9));
            row.push_str(SEG[d0][r]);
            row.push(' ');
            row.push_str(SEG[d1][r]);
            if i < n - 1 {
                row.push_str(colon_row(r));
            }
        }
        Line::from(Span::styled(row, Style::default().fg(color)))
    }));

    let mut label_row = String::new();
    for (i, label) in labels.iter().enumerate() {
        let pad_total = SEG_GROUP_W.saturating_sub(label.len());
        let pad_l = pad_total / 2;
        let pad_r = pad_total - pad_l;
        label_row.push_str(&" ".repeat(pad_l));
        label_row.push_str(label);
        label_row.push_str(&" ".repeat(pad_r));
        if i < n - 1 {
            label_row.push_str(&" ".repeat(SEP_W));
        }
    }
    lines.push(Line::from(Span::styled(label_row, Style::default().fg(Color::DarkGray))));
    lines
}

// ── Activation checks ─────────────────────────────────────────────────────────

/// Route to the correct hardfork's streaming check function.
async fn run_checks_streaming(
    hardfork: &'static str,
    rpc_url: String,
    mode: CheckMode,
    tx: mpsc::Sender<CheckUpdate>,
) {
    match hardfork {
        "Azul" => run_azul_checks_streaming(rpc_url, tx).await,
        "Jovian" => run_jovian_checks_streaming(rpc_url, mode, tx).await,
        _ => {}
    }
}

// ── Jovian activation checks ──────────────────────────────────────────────────

/// bn256Pairing precompile address (EIP-197).
const BN256PAIRING_ADDR: &str = "0x0000000000000000000000000000000000000008";
/// `GasPriceOracle` predeploy proxy address.
const GAS_PRICE_ORACLE_ADDR: &str = "0x420000000000000000000000000000000000000F";
/// EIP-1967 logic/implementation storage slot.
const EIP1967_IMPL_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
/// Expected GPO implementation address after Jovian activation.
const JOVIAN_GPO_IMPL: &str = "4f1db3c6abd250ba86e0928471a8f7db3afd88f1";
/// Expected GPO implementation address under Isthmus (before Jovian).
const ISTHMUS_GPO_IMPL: &str = "93e57a196454cb919193fa9946f14943cf733845";

async fn eth_get_storage_at(client: &HttpClient, addr: &str, slot: &str) -> Result<String, String> {
    ClientT::request::<String, _>(client, "eth_getStorageAt", rpc_params![addr, slot, "latest"])
        .await
        .map_err(|e| e.to_string())
}

async fn eth_get_block_by_number_latest(client: &HttpClient) -> Result<serde_json::Value, String> {
    ClientT::request::<serde_json::Value, _>(
        client,
        "eth_getBlockByNumber",
        rpc_params!["latest", false],
    )
    .await
    .map_err(|e| e.to_string())
}

async fn run_jovian_checks_streaming(
    rpc_url: String,
    mode: CheckMode,
    tx: mpsc::Sender<CheckUpdate>,
) {
    macro_rules! send_start {
        ($name:expr) => {
            if tx.send(CheckUpdate::Starting($name.to_string())).await.is_err() {
                return;
            }
        };
    }
    macro_rules! send_result {
        ($name:expr, $result:expr) => {
            if tx
                .send(CheckUpdate::Completed { name: $name.to_string(), result: $result })
                .await
                .is_err()
            {
                return;
            }
        };
    }

    let client = match make_rpc_client(&rpc_url) {
        Ok(c) => c,
        Err(e) => {
            let conn_result = CheckResult {
                passed: Some(false),
                detail: format!("cannot build client for {rpc_url}: {e}"),
            };
            send_result!("bn256Pairing limit", conn_result);
            for &name in &JOVIAN_CHECK_NAMES[1..] {
                send_result!(
                    name,
                    CheckResult { passed: None, detail: "skipped (no connection)".into() }
                );
            }
            return;
        }
    };

    match ClientT::request::<String, _>(&client, "eth_blockNumber", rpc_params![]).await {
        Ok(_) => {}
        Err(e) => {
            let conn_result =
                CheckResult { passed: Some(false), detail: format!("cannot reach {rpc_url}: {e}") };
            send_result!("bn256Pairing limit", conn_result);
            for &name in &JOVIAN_CHECK_NAMES[1..] {
                send_result!(
                    name,
                    CheckResult { passed: None, detail: "skipped (no connection)".into() }
                );
            }
            return;
        }
    }

    // ── bn256Pairing size limit (427 pairs = 81984 bytes is the Jovian cap) ────
    // 428 pairs × 192 bytes = 82176 bytes, which exceeds the Jovian limit.
    // Identity points (all-zero) are valid in bn256Pairing per EIP-197.
    send_start!("bn256Pairing limit");
    let oversized = format!("0x{}", "00".repeat(428 * 192));
    let r = eth_call(&client, BN256PAIRING_ADDR, &oversized).await;
    let bn256_check = match (mode, r) {
        (CheckMode::Before, Ok(_)) => CheckResult {
            passed: Some(true),
            detail: "oversized input accepted (expected before Jovian)".to_string(),
        },
        (CheckMode::Before, Err(e)) => {
            CheckResult { passed: Some(false), detail: format!("unexpectedly rejected: {e}") }
        }
        (CheckMode::After, Err(_)) => CheckResult {
            passed: Some(true),
            detail: "oversized input rejected (correct)".to_string(),
        },
        (CheckMode::After, Ok(v)) => CheckResult {
            passed: Some(false),
            detail: format!("unexpectedly accepted: {}", v.get(..20).unwrap_or(&v)),
        },
    };
    send_result!("bn256Pairing limit", bn256_check);

    // ── Extra data version byte (0 → 1 at Jovian) ─────────────────────────────
    send_start!("extra data v1");
    let extra_check = match eth_get_block_by_number_latest(&client).await {
        Err(e) => CheckResult { passed: Some(false), detail: format!("RPC error: {e}") },
        Ok(block) => {
            let extra_data = block["extraData"].as_str().unwrap_or("0x");
            let hex_bytes = extra_data.trim_start_matches("0x");
            let first_byte =
                hex_bytes.get(..2).and_then(|s| u8::from_str_radix(s, 16).ok()).unwrap_or(0xFF);
            match (mode, first_byte) {
                (CheckMode::Before, 0) => {
                    CheckResult { passed: Some(true), detail: "version=0 (expected)".to_string() }
                }
                (CheckMode::Before, v) => CheckResult {
                    passed: Some(false),
                    detail: format!("version={v} (expected 0 before Jovian)"),
                },
                (CheckMode::After, 1) => {
                    CheckResult { passed: Some(true), detail: "version=1 (expected)".to_string() }
                }
                (CheckMode::After, v) => CheckResult {
                    passed: Some(false),
                    detail: format!("version={v} (expected 1 after Jovian)"),
                },
            }
        }
    };
    send_result!("extra data v1", extra_check);

    // ── GasPriceOracle EIP-1967 implementation slot ───────────────────────────
    send_start!("GPO implementation");
    let gpo_check =
        match eth_get_storage_at(&client, GAS_PRICE_ORACLE_ADDR, EIP1967_IMPL_SLOT).await {
            Err(e) => CheckResult { passed: Some(false), detail: format!("RPC error: {e}") },
            Ok(slot_val) => {
                let val = norm(&slot_val);
                // Slot value is a zero-padded 32-byte address; last 40 hex chars = address.
                let impl_addr = if val.len() >= 40 { &val[val.len() - 40..] } else { val.as_str() };
                let expected = match mode {
                    CheckMode::After => JOVIAN_GPO_IMPL,
                    CheckMode::Before => ISTHMUS_GPO_IMPL,
                };
                let label = match mode {
                    CheckMode::After => "Jovian",
                    CheckMode::Before => "Isthmus",
                };
                if impl_addr == expected {
                    CheckResult {
                        passed: Some(true),
                        detail: format!("→ 0x{}", impl_addr.get(..8).unwrap_or(impl_addr)),
                    }
                } else {
                    CheckResult {
                        passed: Some(false),
                        detail: format!(
                            "impl=0x{} (expected {label})",
                            impl_addr.get(..8).unwrap_or(impl_addr)
                        ),
                    }
                }
            }
        };
    send_result!("GPO implementation", gpo_check);
}

// ── Azul activation checks ────────────────────────────────────────────────────

const CLZ_PROBE_ADDR: &str = "0x000000000000000000000000000000000000001e";
const CLZ_RUNTIME: &str = "0x6000351e60005260206000f3";
const MODEXP_ADDR: &str = "0x0000000000000000000000000000000000000005";
const MODEXP_GAS_PROBE_ADDR: &str = "0x000000000000000000000000000000000000001d";
const MODEXP_GAS_PROBE_RUNTIME: &str = "0x600060006060600060006005610190f160005260206000f3";
const P256_GAS_PROBE_ADDR: &str = "0x000000000000000000000000000000000000001f";
const P256_GAS_PROBE_RUNTIME: &str = "0x60006000600060006000610100611388f160005260206000f3";

const CLZ_ZERO_INPUT: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
const CLZ_ONE_INPUT: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
const CLZ_HIBIT_INPUT: &str = "0x8000000000000000000000000000000000000000000000000000000000000000";
const CLZ_4BITS_INPUT: &str = "0x0f00000000000000000000000000000000000000000000000000000000000000";

const CLZ_ZERO_RES: &str = "0x0000000000000000000000000000000000000000000000000000000000000100";
const CLZ_ONE_RES: &str = "0x00000000000000000000000000000000000000000000000000000000000000ff";
const CLZ_HIBIT_RES: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
const CLZ_4BITS_RES: &str = "0x0000000000000000000000000000000000000000000000000000000000000004";
const PROBE_SUCCESS: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";

const MODEXP_OVERSIZED: &str = concat!(
    "0x",
    "0000000000000000000000000000000000000000000000000000000000000401",
    "0000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000001",
);

fn norm(h: &str) -> String {
    h.trim().trim_matches('"').to_lowercase()
}

fn make_rpc_client(rpc_url: &str) -> Result<HttpClient, String> {
    HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(12))
        .build(rpc_url)
        .map_err(|e| e.to_string())
}

async fn eth_call(client: &HttpClient, to: &str, data: &str) -> Result<String, String> {
    ClientT::request::<String, _>(
        client,
        "eth_call",
        rpc_params![json!({"to": to, "data": data}), "latest"],
    )
    .await
    .map_err(|e| e.to_string())
}

async fn eth_call_override(
    client: &HttpClient,
    to: &str,
    data: &str,
    override_addr: &str,
    override_code: &str,
) -> Result<String, String> {
    ClientT::request::<String, _>(
        client,
        "eth_call",
        rpc_params![
            json!({"to": to, "data": data}),
            "latest",
            json!({override_addr: {"code": override_code}})
        ],
    )
    .await
    .map_err(|e| e.to_string())
}

fn evaluate_opcode_check(
    _name: &str,
    result: &Result<String, String>,
    expected: &str,
) -> CheckResult {
    let (passed, detail) = match result {
        Err(e) => (false, format!("call failed: {e}")),
        Ok(actual) => {
            if norm(actual) == norm(expected) {
                (true, format!("→ {}", actual.get(..20).unwrap_or(actual)))
            } else {
                (false, format!("got {}", actual.get(..20).unwrap_or(actual)))
            }
        }
    };
    CheckResult { passed: Some(passed), detail }
}

fn evaluate_gas_probe(
    result: &Result<String, String>,
    gas_label: &str,
    after_desc: &str,
) -> CheckResult {
    let actual = match result {
        Err(e) => return CheckResult { passed: Some(false), detail: format!("RPC error: {e}") },
        Ok(v) => norm(v),
    };
    let success_val = norm(PROBE_SUCCESS);

    let (passed, detail) = if actual == success_val {
        (false, format!("{gas_label} CALL succeeded — expected OOG ({after_desc} after Azul)"))
    } else {
        (true, format!("{gas_label} CALL OOG ({after_desc} after Azul)"))
    };
    CheckResult { passed: Some(passed), detail }
}

async fn run_azul_checks_streaming(rpc_url: String, tx: mpsc::Sender<CheckUpdate>) {
    macro_rules! send_start {
        ($name:expr) => {
            if tx.send(CheckUpdate::Starting($name.to_string())).await.is_err() {
                return;
            }
        };
    }
    macro_rules! send_result {
        ($name:expr, $result:expr) => {
            if tx
                .send(CheckUpdate::Completed { name: $name.to_string(), result: $result })
                .await
                .is_err()
            {
                return;
            }
        };
    }

    let client = match make_rpc_client(&rpc_url) {
        Ok(c) => c,
        Err(e) => {
            let conn_result = CheckResult {
                passed: Some(false),
                detail: format!("cannot build client for {rpc_url}: {e}"),
            };
            send_result!("CLZ zero", conn_result);
            for &name in &AZUL_CHECK_NAMES[1..] {
                send_result!(
                    name,
                    CheckResult { passed: None, detail: "skipped (no connection)".into() }
                );
            }
            return;
        }
    };

    // Verify the RPC is reachable with a quick eth_blockNumber call.
    match ClientT::request::<String, _>(&client, "eth_blockNumber", rpc_params![]).await {
        Ok(_) => {}
        Err(e) => {
            let conn_result =
                CheckResult { passed: Some(false), detail: format!("cannot reach {rpc_url}: {e}") };
            send_result!("CLZ zero", conn_result);
            for &name in &AZUL_CHECK_NAMES[1..] {
                send_result!(
                    name,
                    CheckResult { passed: None, detail: "skipped (no connection)".into() }
                );
            }
            return;
        }
    }

    // ── CLZ opcode (0x1e) ─────────────────────────────────────────────────────
    let clz_cases: &[(&str, &str, &str)] = &[
        ("CLZ zero", CLZ_ZERO_INPUT, CLZ_ZERO_RES),
        ("CLZ one", CLZ_ONE_INPUT, CLZ_ONE_RES),
        ("CLZ high-bit", CLZ_HIBIT_INPUT, CLZ_HIBIT_RES),
        ("CLZ four-bits", CLZ_4BITS_INPUT, CLZ_4BITS_RES),
    ];
    for (name, calldata, expected) in clz_cases {
        send_start!(name);
        let r =
            eth_call_override(&client, CLZ_PROBE_ADDR, calldata, CLZ_PROBE_ADDR, CLZ_RUNTIME).await;
        send_result!(name, evaluate_opcode_check(name, &r, expected));
    }

    // ── MODEXP size limit ──────────────────────────────────────────────────────
    send_start!("MODEXP size limit");
    let r = eth_call(&client, MODEXP_ADDR, MODEXP_OVERSIZED).await;
    let modexp_size = r.map_or_else(
        |_| CheckResult {
            passed: Some(true),
            detail: "oversized input rejected (correct)".to_string(),
        },
        |v| CheckResult { passed: Some(false), detail: format!("unexpectedly accepted: {v}") },
    );
    send_result!("MODEXP size limit", modexp_size);

    // ── MODEXP min gas (200 → 500) ─────────────────────────────────────────────
    send_start!("MODEXP min gas");
    let r = eth_call_override(
        &client,
        MODEXP_GAS_PROBE_ADDR,
        "0x",
        MODEXP_GAS_PROBE_ADDR,
        MODEXP_GAS_PROBE_RUNTIME,
    )
    .await;
    send_result!("MODEXP min gas", evaluate_gas_probe(&r, "400-gas", "min=500"));

    // ── P256VERIFY gas (3450 → 6900) ───────────────────────────────────────────
    send_start!("P256VERIFY gas");
    let r = eth_call_override(
        &client,
        P256_GAS_PROBE_ADDR,
        "0x",
        P256_GAS_PROBE_ADDR,
        P256_GAS_PROBE_RUNTIME,
    )
    .await;
    send_result!("P256VERIFY gas", evaluate_gas_probe(&r, "5000-gas", "cost=6900"));

    // ── eth_config RPC method ──────────────────────────────────────────────────
    send_start!("eth_config");
    let cfg_result: Result<serde_json::Value, String> =
        ClientT::request::<serde_json::Value, _>(&client, "eth_config", rpc_params![])
            .await
            .map_err(|e| e.to_string());
    let eth_config_check = match cfg_result {
        Ok(_) => CheckResult { passed: Some(true), detail: "available after Azul".to_string() },
        Err(e) => {
            CheckResult { passed: Some(false), detail: format!("unavailable after Azul: {e}") }
        }
    };
    send_result!("eth_config", eth_config_check);
}
