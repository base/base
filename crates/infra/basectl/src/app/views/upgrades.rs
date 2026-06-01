//! Network upgrade activation countdown and history view.

use std::{
    collections::HashMap,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, U256, hex};
use alloy_sol_types::{SolCall, sol};
use base_common_chains::ChainConfig;
use base_common_consensus::{BASE_BLOCK_TIME_MILLIS, Predeploys, TIMESTAMP_MILLIS_PER_SECOND};
use base_common_genesis::HardForkConfig;
use base_common_precompiles::{ActivationFeature, ActivationRegistryStorage, IActivationRegistry};
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

impl ChainUpgrades {
    fn set_timestamp(&mut self, name: &'static str, timestamp: Option<u64>) {
        let Some(timestamp) = timestamp else { return };
        if let Some(spec) = self.specs.iter_mut().find(|spec| spec.name == name) {
            spec.timestamp = Some(timestamp);
        }
    }

    fn apply_hardforks(&mut self, hardforks: &HardForkConfig) {
        self.set_timestamp("Delta", hardforks.delta_time);
        self.set_timestamp("Canyon", hardforks.canyon_time);
        self.set_timestamp("Ecotone", hardforks.ecotone_time);
        self.set_timestamp("Fjord", hardforks.fjord_time);
        self.set_timestamp("Granite", hardforks.granite_time);
        self.set_timestamp("Holocene", hardforks.holocene_time);
        self.set_timestamp("Isthmus", hardforks.isthmus_time);
        self.set_timestamp("Jovian", hardforks.jovian_time);
        self.set_timestamp("Azul", hardforks.base.azul);
        self.set_timestamp("Beryl", hardforks.base.beryl);
        self.set_timestamp("Subsecond", hardforks.base.subsecond);
    }
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
        UpgradeSpec { name: "Beryl", timestamp: cfg.beryl_timestamp },
        UpgradeSpec { name: "Subsecond", timestamp: cfg.subsecond_timestamp },
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

fn devnet_rpc() -> String {
    "http://localhost:7545/".to_string()
}

fn all_chains() -> [ChainUpgrades; 4] {
    [
        ChainUpgrades {
            display_name: "Devnet",
            rpc: user_config_rpc("devnet").or_else(|| Some(devnet_rpc())),
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

fn chain_name_matches_loaded(display_name: &str, loaded_name: &str) -> bool {
    loaded_name.eq_ignore_ascii_case(display_name)
        || (display_name.eq_ignore_ascii_case("devnet") && loaded_name_is_devnet_alias(loaded_name))
}

fn loaded_name_is_devnet_alias(loaded_name: &str) -> bool {
    const DEVNET_ALIASES: &[&str] = &["devnet", "vibenet", "local-devnet", "local-vibenet"];
    DEVNET_ALIASES.iter().any(|alias| loaded_name.eq_ignore_ascii_case(alias))
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

/// Expected check names for Beryl, in execution order.
const BERYL_CHECK_NAMES: &[&str] = &[
    "registry precompile",
    "registry admin",
    "B-20 token feature",
    "B-20 factory feature",
    "policy registry feature",
    "B-20 stablecoin feature",
    "B-20 security feature",
];

const BERYL_FEATURE_CHECKS: &[(&str, ActivationFeature)] = &[
    ("B-20 token feature", ActivationFeature::B20Token),
    ("B-20 factory feature", ActivationFeature::B20Factory),
    ("policy registry feature", ActivationFeature::PolicyRegistry),
    ("B-20 stablecoin feature", ActivationFeature::B20Stablecoin),
    ("B-20 security feature", ActivationFeature::B20Security),
];

/// Expected check names for Subsecond, in execution order.
const SUBSECOND_CHECK_NAMES: &[&str] = &[
    "BaseTime predeploy code",
    "BaseTime.timestampMs()",
    "BaseTime matches header",
    "header timestampMs field",
    "200ms per block",
    "wall-clock cadence",
    "historical timestampMsAtBlock",
    "BaseTime storage layout",
];

fn check_names_for(hardfork: &str) -> &'static [&'static str] {
    match hardfork {
        "Beryl" => BERYL_CHECK_NAMES,
        "Azul" => AZUL_CHECK_NAMES,
        "Jovian" => JOVIAN_CHECK_NAMES,
        "Subsecond" => SUBSECOND_CHECK_NAMES,
        _ => &[],
    }
}

fn has_checks(hardfork: &str) -> bool {
    !check_names_for(hardfork).is_empty()
}

fn checkable_specs_display(chain: &ChainUpgrades) -> Vec<&UpgradeSpec> {
    chain.specs.iter().filter(|spec| has_checks(spec.name)).rev().collect()
}

/// Returns the hardfork whose checks should be shown for this chain.
fn target_hardfork(chain: &ChainUpgrades, now: u64) -> Option<&'static str> {
    let check_specs: Vec<_> = chain.specs.iter().filter(|spec| has_checks(spec.name)).collect();

    if let Some(upcoming) =
        check_specs.iter().find(|spec| spec.timestamp.is_some_and(|timestamp| timestamp > now))
    {
        return Some(upcoming.name);
    }

    let latest_active = check_specs
        .iter()
        .rposition(|spec| spec.timestamp.is_some_and(|timestamp| timestamp <= now));

    latest_active.map_or_else(
        || check_specs.last().map(|spec| spec.name),
        |index| {
            // Prefer the next checkable hardfork when it exists, even before it
            // is scheduled. At the frontier, keep showing the active hardfork.
            check_specs.get(index + 1).or_else(|| check_specs.get(index)).map(|spec| spec.name)
        },
    )
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
        let mode_changed = self.mode != Some(mode);
        self.chain_idx = Some(chain_idx);
        self.hardfork = Some(hardfork);
        self.mode = Some(mode);
        self.rpc_url = rpc_url.clone();
        self.current = None;
        // Preserve previous results across auto-refreshes so the table updates
        // in-place rather than blanking on every tick. Only clear when the
        // target context actually changed.
        if chain_changed || hardfork_changed || mode_changed {
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

const fn countdown_progress_tenths(start_ts: u64, target_ts: u64, now: u64) -> u16 {
    let total = target_ts.saturating_sub(start_ts);
    if total == 0 {
        return 1000;
    }

    let elapsed = now.saturating_sub(start_ts);
    let elapsed = if elapsed > total { total } else { elapsed };
    let tenths = ((elapsed as u128) * 1000) / (total as u128);
    let max = if now < target_ts { 999 } else { 1000 };
    if tenths > max { max as u16 } else { tenths as u16 }
}

fn fmt_progress_percent(tenths: u16) -> String {
    format!("{}.{:01}%", tenths / 10, tenths % 10)
}

// ── View ──────────────────────────────────────────────────────────────────────

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "←/→", description: "Switch chain" },
    Keybinding { key: "↑/↓", description: "Select checks upgrade" },
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
    selected_check_hardforks: [Option<&'static str>; 4],
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
            selected_check_hardforks: [None; 4],
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
        let Some((hardfork, timestamp)) =
            self.selected_check_spec(now).map(|spec| (spec.name, spec.timestamp))
        else {
            return;
        };
        let Some(ts) = timestamp else {
            self.checks.reset();
            return;
        };
        let Some(rpc) = self.rpc_for_selected(resources) else { return };
        let mode = if ts > now { CheckMode::Before } else { CheckMode::After };
        self.checks.start(self.selected_chain, rpc, hardfork, mode);
    }

    fn selected_check_spec(&self, now: u64) -> Option<&UpgradeSpec> {
        let chain = &self.chains[self.selected_chain];
        self.selected_check_hardforks[self.selected_chain]
            .filter(|name| chain.specs.iter().any(|spec| spec.name == *name && has_checks(name)))
            .or_else(|| target_hardfork(chain, now))
            .and_then(|name| chain.specs.iter().find(|spec| spec.name == name))
    }

    fn selected_check_hardfork(&self, now: u64) -> Option<&'static str> {
        self.selected_check_spec(now).map(|spec| spec.name)
    }

    fn move_selected_check_hardfork(&mut self, direction: i8) {
        let now = now_unix();
        let current = self.selected_check_hardfork(now);
        let checkable: Vec<_> = checkable_specs_display(&self.chains[self.selected_chain])
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        let Some(last_index) = checkable.len().checked_sub(1) else { return };

        let current_index =
            current.and_then(|name| checkable.iter().position(|candidate| *candidate == name));
        let current_index = current_index.unwrap_or(0);
        let next_index = match direction {
            -1 => current_index.saturating_sub(1),
            1 => (current_index + 1).min(last_index),
            _ => current_index,
        };
        let next = checkable[next_index];
        if self.selected_check_hardforks[self.selected_chain] != Some(next) {
            self.selected_check_hardforks[self.selected_chain] = Some(next);
            self.checks.reset();
        }
    }

    fn apply_live_hardforks(&mut self, resources: &Resources) {
        let Some(hardforks) = resources.config.hardforks.as_ref() else { return };
        let chain = &mut self.chains[self.selected_chain];
        if chain_name_matches_loaded(chain.display_name, &resources.config.name) {
            chain.apply_hardforks(hardforks);
        }
    }

    fn rpc_for_selected(&self, resources: &Resources) -> Option<String> {
        let chain = &self.chains[self.selected_chain];
        if chain_name_matches_loaded(chain.display_name, &resources.config.name) {
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
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selected_check_hardfork(-1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selected_check_hardfork(1);
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
        self.apply_live_hardforks(resources);

        if self.auto_refresh && !self.checks.running {
            let due = self.checks.last_run_at.is_none_or(|t| t.elapsed() >= AUTO_REFRESH_INTERVAL);
            let scheduled =
                self.selected_check_spec(now_unix()).is_some_and(|spec| spec.timestamp.is_some());
            if due && scheduled {
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
        let selected_check_spec = self.selected_check_spec(now);
        let selected_hardfork = selected_check_spec.map(|spec| spec.name);

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

        render_history(frame, bottom[0], chain, now, selected_hardfork);
        render_checks_panel(
            frame,
            bottom[1],
            &self.checks,
            self.tick_count,
            selected_check_spec,
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
    let pct_label = fmt_progress_percent(countdown_progress_tenths(start_ts, ts, now));
    let bar_w = 50usize;
    let filled = (bar_w as f64 * pct) as usize;
    let bar = format!("|{}{}|  {pct_label}", "█".repeat(filled), "░".repeat(bar_w - filled));

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

fn render_history(
    frame: &mut Frame<'_>,
    area: Rect,
    chain: &ChainUpgrades,
    now: u64,
    selected_hardfork: Option<&'static str>,
) {
    let block = Block::default()
        .title(" Upgrade History ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let rows: Vec<Row<'static>> = chain
        .specs
        .iter()
        .rev()
        .map(|spec| {
            let selected = selected_hardfork == Some(spec.name);
            let upgrade_name =
                if selected { format!("▶ {}", spec.name) } else { format!("  {}", spec.name) };
            let (date_str, status_str, status_color) = match spec.timestamp {
                None => ("-".to_string(), "− TBD".to_string(), Color::DarkGray),
                Some(ts) if ts <= now => {
                    (fmt_timestamp(ts), "✓ Active".to_string(), Color::LightGreen)
                }
                Some(ts) => (fmt_timestamp(ts), "⏳ Upcoming".to_string(), Color::Yellow),
            };
            Row::new([
                Cell::from(upgrade_name).style(Style::default().fg(Color::White)),
                Cell::from(date_str).style(Style::default().fg(Color::Gray)),
                Cell::from(status_str)
                    .style(Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
            ])
            .style(if selected {
                Style::default().bg(Color::Rgb(20, 35, 60))
            } else {
                Style::default()
            })
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
    selected_spec: Option<&UpgradeSpec>,
    auto_refresh: bool,
) {
    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    // Panel is idle and has never been run.
    if panel.chain_idx.is_none() {
        let Some(spec) = selected_spec else {
            let block = Block::default()
                .title(" Checks ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));
            frame.render_widget(
                Paragraph::new("No activation checks are defined for this network.")
                    .block(block)
                    .alignment(Alignment::Center),
                area,
            );
            return;
        };
        let hf_name = spec.name;
        if spec.timestamp.is_none() {
            let lines: Vec<Line<'static>> = vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("{hf_name} is not scheduled for this network."),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Checks are skipped until an activation timestamp is configured.",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            let block = Block::default()
                .title(format!(" {hf_name} Checks "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));
            frame.render_widget(
                Paragraph::new(lines).block(block).alignment(Alignment::Center),
                area,
            );
            return;
        }
        let check_list = check_names_for(hf_name).join(" · ");
        let hint = if auto_refresh {
            format!("Auto-refreshing {hf_name} checks every 2s · ↑/↓ to change · [a] to disable")
        } else {
            format!(
                "Press [r] to run {hf_name} checks · ↑/↓ to change · [a] to enable auto-refresh"
            )
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

    let widths = [Constraint::Length(24), Constraint::Length(5), Constraint::Min(8)];

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
        Span::styled("[↑/↓]", key_style),
        Span::raw(" "),
        Span::styled("select checks", desc_style),
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
        "Beryl" => run_beryl_checks_streaming(rpc_url, mode, tx).await,
        "Azul" => run_azul_checks_streaming(rpc_url, tx).await,
        "Jovian" => run_jovian_checks_streaming(rpc_url, mode, tx).await,
        "Subsecond" => run_subsecond_checks_streaming(rpc_url, mode, tx).await,
        _ => {}
    }
}

// ── Beryl activation checks ───────────────────────────────────────────────────

fn activation_registry_address() -> String {
    ActivationRegistryStorage::ADDRESS.to_string()
}

fn calldata_hex(calldata: impl AsRef<[u8]>) -> String {
    format!("0x{}", hex::encode(calldata.as_ref()))
}

fn decode_rpc_bytes(value: &str) -> Result<Vec<u8>, String> {
    hex::decode(value.trim_start_matches("0x")).map_err(|e| format!("invalid hex response: {e}"))
}

fn short_address(address: Address) -> String {
    let value = address.to_string();
    if value.len() <= 14 {
        value
    } else {
        format!("{}..{}", &value[..10], &value[value.len() - 4..])
    }
}

async fn activation_admin(client: &HttpClient) -> Result<Address, String> {
    let data = calldata_hex(IActivationRegistry::adminCall {}.abi_encode());
    let to = activation_registry_address();
    let output = eth_call(client, &to, &data).await?;
    let bytes = decode_rpc_bytes(&output)?;
    IActivationRegistry::adminCall::abi_decode_returns(bytes.as_ref()).map_err(|e| e.to_string())
}

async fn activation_feature_state(client: &HttpClient, feature: B256) -> Result<bool, String> {
    let data = calldata_hex(IActivationRegistry::isActivatedCall { feature }.abi_encode());
    let to = activation_registry_address();
    let output = eth_call(client, &to, &data).await?;
    let bytes = decode_rpc_bytes(&output)?;
    IActivationRegistry::isActivatedCall::abi_decode_returns(bytes.as_ref())
        .map_err(|e| e.to_string())
}

fn evaluate_beryl_precompile(mode: CheckMode, result: &Result<Address, String>) -> CheckResult {
    match (mode, result) {
        (CheckMode::Before, Ok(admin)) => CheckResult {
            passed: Some(false),
            detail: format!("responded before Beryl; admin {}", short_address(*admin)),
        },
        (CheckMode::Before, Err(_)) => {
            CheckResult { passed: Some(true), detail: "unavailable before Beryl".to_string() }
        }
        (CheckMode::After, Ok(_)) => CheckResult {
            passed: Some(true),
            detail: format!("responds at {}", activation_registry_address()),
        },
        (CheckMode::After, Err(e)) => {
            CheckResult { passed: Some(false), detail: format!("unavailable after Beryl: {e}") }
        }
    }
}

fn evaluate_beryl_admin(mode: CheckMode, result: Result<Address, String>) -> CheckResult {
    match (mode, result) {
        (CheckMode::Before, Ok(admin)) => CheckResult {
            passed: Some(false),
            detail: format!("admin {} available before Beryl", short_address(admin)),
        },
        (CheckMode::Before, Err(_)) => {
            CheckResult { passed: None, detail: "skipped before Beryl".to_string() }
        }
        (CheckMode::After, Ok(admin)) => {
            CheckResult { passed: Some(true), detail: format!("admin {}", short_address(admin)) }
        }
        (CheckMode::After, Err(e)) => {
            CheckResult { passed: Some(false), detail: format!("admin query failed: {e}") }
        }
    }
}

fn evaluate_beryl_feature(mode: CheckMode, result: Result<bool, String>) -> CheckResult {
    match (mode, result) {
        (CheckMode::Before, Ok(active)) => CheckResult {
            passed: Some(false),
            detail: format!("responded before Beryl: {}", feature_state(active)),
        },
        (CheckMode::Before, Err(_)) => {
            CheckResult { passed: Some(true), detail: "unavailable before Beryl".to_string() }
        }
        (CheckMode::After, Ok(active)) => {
            CheckResult { passed: Some(true), detail: feature_state(active).to_string() }
        }
        (CheckMode::After, Err(e)) => {
            CheckResult { passed: Some(false), detail: format!("query failed: {e}") }
        }
    }
}

const fn feature_state(active: bool) -> &'static str {
    if active { "active" } else { "inactive" }
}

async fn run_beryl_checks_streaming(
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
            send_result!("registry precompile", conn_result);
            for &name in &BERYL_CHECK_NAMES[1..] {
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
            send_result!("registry precompile", conn_result);
            for &name in &BERYL_CHECK_NAMES[1..] {
                send_result!(
                    name,
                    CheckResult { passed: None, detail: "skipped (no connection)".into() }
                );
            }
            return;
        }
    }

    send_start!("registry precompile");
    let admin = activation_admin(&client).await;
    send_result!("registry precompile", evaluate_beryl_precompile(mode, &admin));

    send_start!("registry admin");
    send_result!("registry admin", evaluate_beryl_admin(mode, admin));

    for &(name, feature) in BERYL_FEATURE_CHECKS {
        send_start!(name);
        let result = activation_feature_state(&client, feature.id()).await;
        send_result!(name, evaluate_beryl_feature(mode, result));
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

// ── Subsecond activation checks ───────────────────────────────────────────────

sol! {
    /// Local copy of the `BaseTime` predeploy ABI (avoids a heavyweight dep on
    /// `base-common-evm` from the basectl CLI).
    interface IBaseTime {
        function timestampMs() external view returns (uint256);
        function timestampMillisPart() external view returns (uint256);
        function timestampMsAtBlock(uint256 blockNumber) external view returns (uint256);
    }
}

/// Number of blocks to sample for the strict 200ms-per-block cadence check.
const SUBSECOND_CADENCE_SAMPLE_BLOCKS: u64 = 16;

/// Number of wall-clock cadence samples to collect.
const SUBSECOND_WALL_SAMPLE_COUNT: usize = 12;

/// Tolerance for wall-clock cadence (±ms) around 200ms.
const SUBSECOND_WALL_TOLERANCE_MS: u128 = 150;

/// Wall-clock poll interval while sampling block cadence (ms).
const SUBSECOND_WALL_POLL_MS: u64 = 25;

/// Max wall-clock sampling window (ms) before bailing out.
const SUBSECOND_WALL_MAX_MS: u128 = 6_000;

/// Storage slot 0 of `BaseTime`: latest Unix millisecond timestamp.
const BASE_TIME_LATEST_TIMESTAMP_MS_SLOT: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000000";

/// Storage slot 1 of `BaseTime`: latest L2 block number with a timestamp commit.
const BASE_TIME_LATEST_BLOCK_NUMBER_SLOT: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000001";

fn base_time_address_string() -> String {
    Predeploys::BASE_TIME.to_string()
}

fn parse_hex_quantity(value: &str) -> Result<u64, String> {
    let trimmed = value.trim().trim_matches('"').trim_start_matches("0x");
    u64::from_str_radix(trimmed, 16).map_err(|e| format!("invalid hex quantity {value}: {e}"))
}

fn parse_hex_u256_low_u64(value: &str) -> Result<u64, String> {
    let trimmed = value.trim().trim_matches('"').trim_start_matches("0x");
    if trimmed.is_empty() {
        return Ok(0);
    }
    // Storage slots are 32 bytes; the low 8 bytes fit a u64.
    let start = trimmed.len().saturating_sub(16);
    u64::from_str_radix(&trimmed[start..], 16).map_err(|e| format!("invalid hex u256 {value}: {e}"))
}

async fn rpc_block_number(client: &HttpClient) -> Result<u64, String> {
    let raw = ClientT::request::<String, _>(client, "eth_blockNumber", rpc_params![])
        .await
        .map_err(|e| e.to_string())?;
    parse_hex_quantity(&raw)
}

async fn rpc_get_block(
    client: &HttpClient,
    block_number: u64,
) -> Result<serde_json::Value, String> {
    let n_hex = format!("0x{block_number:x}");
    ClientT::request::<serde_json::Value, _>(
        client,
        "eth_getBlockByNumber",
        rpc_params![n_hex, false],
    )
    .await
    .map_err(|e| e.to_string())
}

async fn rpc_get_latest_block(client: &HttpClient) -> Result<serde_json::Value, String> {
    eth_get_block_by_number_latest(client).await
}

/// Returns `(timestamp_seconds, Option<timestamp_ms>, Option<millis_part>)` for a block JSON.
fn read_block_timestamp_fields(
    block: &serde_json::Value,
) -> Result<(u64, Option<u64>, Option<u16>), String> {
    let ts_secs = block["timestamp"]
        .as_str()
        .ok_or_else(|| "missing timestamp field".to_string())
        .and_then(parse_hex_quantity)?;
    let timestamp_ms = block.get("timestampMs").and_then(|v| v.as_str()).map(parse_hex_quantity);
    let timestamp_millis_part = block
        .get("timestampMillisPart")
        .and_then(|v| v.as_str())
        .map(parse_hex_quantity);

    let timestamp_ms = match timestamp_ms {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return Err(e),
        None => None,
    };
    let timestamp_millis_part = match timestamp_millis_part {
        Some(Ok(v)) => Some(u16::try_from(v).map_err(|_| "millis part too large".to_string())?),
        Some(Err(e)) => return Err(e),
        None => None,
    };

    Ok((ts_secs, timestamp_ms, timestamp_millis_part))
}

/// Computes the canonical millisecond timestamp from header fields. Returns the
/// explicit `timestampMs` when present, otherwise reconstructs it from
/// `timestamp * 1000 + timestampMillisPart` if the part is set.
fn header_timestamp_ms(
    ts_secs: u64,
    timestamp_ms: Option<u64>,
    timestamp_millis_part: Option<u16>,
) -> Option<u64> {
    if let Some(ms) = timestamp_ms {
        return Some(ms);
    }
    let part = timestamp_millis_part?;
    ts_secs
        .checked_mul(u64::from(TIMESTAMP_MILLIS_PER_SECOND))
        .and_then(|ms| ms.checked_add(u64::from(part)))
}

async fn eth_get_code(client: &HttpClient, addr: &str) -> Result<String, String> {
    ClientT::request::<String, _>(client, "eth_getCode", rpc_params![addr, "latest"])
        .await
        .map_err(|e| e.to_string())
}

fn evaluate_predeploy_code(mode: CheckMode, result: &Result<String, String>) -> CheckResult {
    let code = match result {
        Ok(v) => norm(v),
        Err(e) => return CheckResult { passed: Some(false), detail: format!("RPC error: {e}") },
    };
    let installed = !code.is_empty() && code != "0x" && code != "0x0";
    match (mode, installed) {
        (CheckMode::Before, false) => {
            CheckResult { passed: Some(true), detail: "no code (expected before)".into() }
        }
        (CheckMode::Before | CheckMode::After, true) => CheckResult {
            passed: Some(true),
            detail: format!("code present ({} bytes)", code.len().saturating_sub(2) / 2),
        },
        (CheckMode::After, false) => CheckResult {
            passed: Some(false),
            detail: "no code at BaseTime predeploy after Subsecond".into(),
        },
    }
}

fn evaluate_timestamp_ms_call(mode: CheckMode, result: &Result<u64, String>) -> CheckResult {
    match (mode, result) {
        (CheckMode::Before, Err(_)) => {
            CheckResult { passed: Some(true), detail: "reverts before Subsecond".into() }
        }
        (CheckMode::Before, Ok(v)) if *v == 0 => {
            CheckResult { passed: Some(true), detail: "returns 0 before Subsecond".into() }
        }
        (CheckMode::Before, Ok(v)) => CheckResult {
            passed: Some(false),
            detail: format!("unexpectedly returned {v} before Subsecond"),
        },
        (CheckMode::After, Ok(v)) if *v > 0 => {
            CheckResult { passed: Some(true), detail: format!("{v} ms") }
        }
        (CheckMode::After, Ok(_)) => CheckResult {
            passed: Some(false),
            detail: "returned 0 after Subsecond".into(),
        },
        (CheckMode::After, Err(e)) => CheckResult {
            passed: Some(false),
            detail: format!("call failed after Subsecond: {e}"),
        },
    }
}

fn evaluate_header_timestamp_ms_field(
    mode: CheckMode,
    block: &serde_json::Value,
) -> CheckResult {
    let (ts_secs, timestamp_ms, millis_part) = match read_block_timestamp_fields(block) {
        Ok(v) => v,
        Err(e) => return CheckResult { passed: Some(false), detail: e },
    };

    match mode {
        CheckMode::Before => {
            if timestamp_ms.is_none() && millis_part.is_none() {
                CheckResult { passed: Some(true), detail: "no ms fields (expected)".into() }
            } else {
                CheckResult {
                    passed: Some(false),
                    detail: format!(
                        "unexpected ms fields: timestampMs={timestamp_ms:?} part={millis_part:?}"
                    ),
                }
            }
        }
        CheckMode::After => {
            let Some(ms) = timestamp_ms else {
                return CheckResult {
                    passed: Some(false),
                    detail: "missing timestampMs after Subsecond".into(),
                };
            };
            let Some(part) = millis_part else {
                return CheckResult {
                    passed: Some(false),
                    detail: "missing timestampMillisPart after Subsecond".into(),
                };
            };
            let derived = ts_secs
                .checked_mul(u64::from(TIMESTAMP_MILLIS_PER_SECOND))
                .and_then(|v| v.checked_add(u64::from(part)));
            if derived != Some(ms) {
                return CheckResult {
                    passed: Some(false),
                    detail: format!(
                        "timestampMs={ms} != timestamp*1000+part={derived:?}"
                    ),
                };
            }
            if part >= TIMESTAMP_MILLIS_PER_SECOND
                || !part.is_multiple_of(BASE_BLOCK_TIME_MILLIS)
            {
                return CheckResult {
                    passed: Some(false),
                    detail: format!("part {part} not on 200ms grid"),
                };
            }
            CheckResult {
                passed: Some(true),
                detail: format!("timestampMs={ms} part={part}"),
            }
        }
    }
}

fn evaluate_basetime_matches_header(
    mode: CheckMode,
    rpc_value: &Result<u64, String>,
    block: &serde_json::Value,
) -> CheckResult {
    if mode == CheckMode::Before {
        return CheckResult { passed: None, detail: "skipped before Subsecond".into() };
    }
    let basetime_ms = match rpc_value {
        Ok(v) => *v,
        Err(e) => {
            return CheckResult {
                passed: Some(false),
                detail: format!("BaseTime call failed: {e}"),
            };
        }
    };
    let (ts_secs, timestamp_ms, millis_part) = match read_block_timestamp_fields(block) {
        Ok(v) => v,
        Err(e) => return CheckResult { passed: Some(false), detail: e },
    };
    let Some(header_ms) = header_timestamp_ms(ts_secs, timestamp_ms, millis_part) else {
        return CheckResult {
            passed: Some(false),
            detail: "header has no ms timestamp to compare".into(),
        };
    };
    if header_ms == basetime_ms {
        CheckResult { passed: Some(true), detail: format!("{header_ms} ms") }
    } else {
        CheckResult {
            passed: Some(false),
            detail: format!("BaseTime={basetime_ms} ms, header={header_ms} ms"),
        }
    }
}

/// Strict-equality cadence check: walks the last N blocks and asserts that
/// every consecutive pair's millisecond timestamp delta is exactly 200ms.
async fn evaluate_two_hundred_ms_per_block(
    client: &HttpClient,
    mode: CheckMode,
    head: u64,
) -> CheckResult {
    if mode == CheckMode::Before {
        return CheckResult { passed: None, detail: "skipped before Subsecond".into() };
    }
    if head < SUBSECOND_CADENCE_SAMPLE_BLOCKS {
        return CheckResult {
            passed: None,
            detail: format!("need at least {SUBSECOND_CADENCE_SAMPLE_BLOCKS} blocks, have {head}"),
        };
    }
    let start = head + 1 - SUBSECOND_CADENCE_SAMPLE_BLOCKS;
    let mut previous: Option<(u64, u64)> = None; // (block_number, timestamp_ms)
    for n in start..=head {
        let block = match rpc_get_block(client, n).await {
            Ok(b) => b,
            Err(e) => {
                return CheckResult {
                    passed: Some(false),
                    detail: format!("block {n} fetch failed: {e}"),
                };
            }
        };
        let (ts_secs, ts_ms, part) = match read_block_timestamp_fields(&block) {
            Ok(v) => v,
            Err(e) => {
                return CheckResult {
                    passed: Some(false),
                    detail: format!("block {n}: {e}"),
                };
            }
        };
        let Some(current_ms) = header_timestamp_ms(ts_secs, ts_ms, part) else {
            return CheckResult {
                passed: Some(false),
                detail: format!("block {n} has no ms timestamp"),
            };
        };
        if let Some((prev_n, prev_ms)) = previous {
            let delta = current_ms.wrapping_sub(prev_ms) as i128;
            if delta != i128::from(BASE_BLOCK_TIME_MILLIS) {
                return CheckResult {
                    passed: Some(false),
                    detail: format!(
                        "block {n} - {prev_n}: Δ={delta}ms (expected {BASE_BLOCK_TIME_MILLIS})"
                    ),
                };
            }
        }
        previous = Some((n, current_ms));
    }
    CheckResult {
        passed: Some(true),
        detail: format!("{SUBSECOND_CADENCE_SAMPLE_BLOCKS} blocks all at 200ms"),
    }
}

/// Wall-clock cadence sampler: polls `eth_blockNumber` for up to
/// `SUBSECOND_WALL_MAX_MS` ms, collecting per-block wall-clock deltas, and
/// returns the median.
async fn evaluate_wall_clock_cadence(client: &HttpClient, mode: CheckMode) -> CheckResult {
    let poll = Duration::from_millis(SUBSECOND_WALL_POLL_MS);
    let mut last_block = match rpc_block_number(client).await {
        Ok(n) => n,
        Err(e) => {
            return CheckResult { passed: Some(false), detail: format!("RPC error: {e}") };
        }
    };
    let mut last_wall = Instant::now();
    let start = last_wall;
    let mut deltas: Vec<u128> = Vec::new();

    while deltas.len() < SUBSECOND_WALL_SAMPLE_COUNT {
        if start.elapsed().as_millis() > SUBSECOND_WALL_MAX_MS {
            break;
        }
        tokio::time::sleep(poll).await;
        let now_block = match rpc_block_number(client).await {
            Ok(n) => n,
            Err(_) => continue,
        };
        if now_block > last_block {
            let now_wall = Instant::now();
            let total_delta = now_wall.duration_since(last_wall).as_millis();
            let advanced = u128::from(now_block - last_block);
            // Apportion delta evenly across blocks if more than one landed in
            // the same poll. Only the most recent block delta is truly
            // observed at this poll, but the average is still informative.
            let per_block = total_delta / advanced;
            for _ in 0..advanced {
                deltas.push(per_block);
                if deltas.len() >= SUBSECOND_WALL_SAMPLE_COUNT {
                    break;
                }
            }
            last_block = now_block;
            last_wall = now_wall;
        }
    }

    if deltas.is_empty() {
        return CheckResult {
            passed: Some(false),
            detail: format!("no new blocks in {}ms", start.elapsed().as_millis()),
        };
    }
    deltas.sort_unstable();
    let median = deltas[deltas.len() / 2];

    let target = u128::from(BASE_BLOCK_TIME_MILLIS);
    let (lo, hi) = match mode {
        CheckMode::After => {
            (target.saturating_sub(SUBSECOND_WALL_TOLERANCE_MS), target + SUBSECOND_WALL_TOLERANCE_MS)
        }
        CheckMode::Before => {
            // Pre-Subsecond cadence is the legacy seconds-denominated block time.
            // Accept anything ≥ 1s as "not subsecond".
            (1_000, u128::MAX)
        }
    };

    if median >= lo && median <= hi {
        CheckResult {
            passed: Some(true),
            detail: format!(
                "median {median}ms over {} samples (target {target}ms)",
                deltas.len()
            ),
        }
    } else {
        CheckResult {
            passed: Some(false),
            detail: format!(
                "median {median}ms over {} samples (expected {lo}-{hi}ms)",
                deltas.len()
            ),
        }
    }
}

async fn evaluate_historical_timestamp_ms_at_block(
    client: &HttpClient,
    mode: CheckMode,
    head: u64,
) -> CheckResult {
    if mode == CheckMode::Before {
        return CheckResult { passed: None, detail: "skipped before Subsecond".into() };
    }
    if head == 0 {
        return CheckResult { passed: None, detail: "no parent block to query".into() };
    }
    let target = head.saturating_sub(1);

    let to = base_time_address_string();
    let call = IBaseTime::timestampMsAtBlockCall { blockNumber: U256::from(target) };
    let data = calldata_hex(call.abi_encode());
    let basetime_ms = match eth_call(client, &to, &data).await {
        Ok(v) => {
            let bytes = match decode_rpc_bytes(&v) {
                Ok(b) => b,
                Err(e) => {
                    return CheckResult {
                        passed: Some(false),
                        detail: format!("decode failed: {e}"),
                    };
                }
            };
            match IBaseTime::timestampMsAtBlockCall::abi_decode_returns(bytes.as_ref()) {
                Ok(v) => u64::try_from(v).unwrap_or(0),
                Err(e) => {
                    return CheckResult {
                        passed: Some(false),
                        detail: format!("abi decode failed: {e}"),
                    };
                }
            }
        }
        Err(e) => {
            return CheckResult {
                passed: Some(false),
                detail: format!("call failed: {e}"),
            };
        }
    };

    let block = match rpc_get_block(client, target).await {
        Ok(b) => b,
        Err(e) => {
            return CheckResult {
                passed: Some(false),
                detail: format!("block {target} fetch failed: {e}"),
            };
        }
    };
    let (ts_secs, ts_ms, part) = match read_block_timestamp_fields(&block) {
        Ok(v) => v,
        Err(e) => {
            return CheckResult {
                passed: Some(false),
                detail: format!("block {target}: {e}"),
            };
        }
    };
    let Some(header_ms) = header_timestamp_ms(ts_secs, ts_ms, part) else {
        return CheckResult {
            passed: Some(false),
            detail: format!("block {target} has no ms timestamp"),
        };
    };

    if header_ms == basetime_ms {
        CheckResult {
            passed: Some(true),
            detail: format!("block {target} = {header_ms} ms"),
        }
    } else {
        CheckResult {
            passed: Some(false),
            detail: format!(
                "block {target}: BaseTime={basetime_ms}, header={header_ms}"
            ),
        }
    }
}

async fn evaluate_basetime_storage(
    client: &HttpClient,
    mode: CheckMode,
    head: u64,
    head_block: &serde_json::Value,
) -> CheckResult {
    if mode == CheckMode::Before {
        return CheckResult { passed: None, detail: "skipped before Subsecond".into() };
    }
    let to = base_time_address_string();

    let slot0 = match eth_get_storage_at(client, &to, BASE_TIME_LATEST_TIMESTAMP_MS_SLOT).await {
        Ok(v) => parse_hex_u256_low_u64(&v),
        Err(e) => return CheckResult { passed: Some(false), detail: format!("slot0 RPC: {e}") },
    };
    let slot1 = match eth_get_storage_at(client, &to, BASE_TIME_LATEST_BLOCK_NUMBER_SLOT).await {
        Ok(v) => parse_hex_u256_low_u64(&v),
        Err(e) => return CheckResult { passed: Some(false), detail: format!("slot1 RPC: {e}") },
    };

    let slot0 = match slot0 {
        Ok(v) => v,
        Err(e) => return CheckResult { passed: Some(false), detail: e },
    };
    let slot1 = match slot1 {
        Ok(v) => v,
        Err(e) => return CheckResult { passed: Some(false), detail: e },
    };

    let (ts_secs, ts_ms, part) = match read_block_timestamp_fields(head_block) {
        Ok(v) => v,
        Err(e) => return CheckResult { passed: Some(false), detail: e },
    };
    let Some(header_ms) = header_timestamp_ms(ts_secs, ts_ms, part) else {
        return CheckResult {
            passed: Some(false),
            detail: "head has no ms timestamp".into(),
        };
    };

    if slot0 != header_ms {
        return CheckResult {
            passed: Some(false),
            detail: format!("slot0={slot0} ms, head={header_ms} ms"),
        };
    }
    // Slot 1 may lag by 1 block when the head was just produced but BaseTime
    // is updated by the sequencer at block start; accept head or head-1.
    if slot1 != head && slot1 + 1 != head {
        return CheckResult {
            passed: Some(false),
            detail: format!("slot1={slot1}, head={head}"),
        };
    }
    CheckResult {
        passed: Some(true),
        detail: format!("slot0={header_ms}ms, slot1={slot1}"),
    }
}

async fn run_subsecond_checks_streaming(
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
            let conn = CheckResult {
                passed: Some(false),
                detail: format!("cannot build client for {rpc_url}: {e}"),
            };
            send_result!(SUBSECOND_CHECK_NAMES[0], conn);
            for &name in &SUBSECOND_CHECK_NAMES[1..] {
                send_result!(
                    name,
                    CheckResult { passed: None, detail: "skipped (no connection)".into() }
                );
            }
            return;
        }
    };

    let head = match rpc_block_number(&client).await {
        Ok(n) => n,
        Err(e) => {
            let conn = CheckResult {
                passed: Some(false),
                detail: format!("cannot reach {rpc_url}: {e}"),
            };
            send_result!(SUBSECOND_CHECK_NAMES[0], conn);
            for &name in &SUBSECOND_CHECK_NAMES[1..] {
                send_result!(
                    name,
                    CheckResult { passed: None, detail: "skipped (no connection)".into() }
                );
            }
            return;
        }
    };

    let head_block = rpc_get_latest_block(&client).await;

    // ── BaseTime predeploy code ──────────────────────────────────────────────
    send_start!("BaseTime predeploy code");
    let code = eth_get_code(&client, &base_time_address_string()).await;
    send_result!("BaseTime predeploy code", evaluate_predeploy_code(mode, &code));

    // ── BaseTime.timestampMs() ───────────────────────────────────────────────
    send_start!("BaseTime.timestampMs()");
    let to = base_time_address_string();
    let data = calldata_hex(IBaseTime::timestampMsCall {}.abi_encode());
    let raw = eth_call(&client, &to, &data).await;
    let basetime_call = raw.and_then(|v| {
        let bytes = decode_rpc_bytes(&v)?;
        IBaseTime::timestampMsCall::abi_decode_returns(bytes.as_ref())
            .map_err(|e| e.to_string())
            .and_then(|u| u64::try_from(u).map_err(|e| e.to_string()))
    });
    send_result!("BaseTime.timestampMs()", evaluate_timestamp_ms_call(mode, &basetime_call));

    // ── BaseTime matches header ──────────────────────────────────────────────
    send_start!("BaseTime matches header");
    let basetime_vs_header = match &head_block {
        Ok(b) => evaluate_basetime_matches_header(mode, &basetime_call, b),
        Err(e) => CheckResult { passed: Some(false), detail: format!("head fetch failed: {e}") },
    };
    send_result!("BaseTime matches header", basetime_vs_header);

    // ── header timestampMs field ─────────────────────────────────────────────
    send_start!("header timestampMs field");
    let header_field = match &head_block {
        Ok(b) => evaluate_header_timestamp_ms_field(mode, b),
        Err(e) => CheckResult { passed: Some(false), detail: format!("head fetch failed: {e}") },
    };
    send_result!("header timestampMs field", header_field);

    // ── 200ms per block (strict equality across recent blocks) ───────────────
    send_start!("200ms per block");
    let cadence = evaluate_two_hundred_ms_per_block(&client, mode, head).await;
    send_result!("200ms per block", cadence);

    // ── wall-clock cadence ───────────────────────────────────────────────────
    send_start!("wall-clock cadence");
    let wall = evaluate_wall_clock_cadence(&client, mode).await;
    send_result!("wall-clock cadence", wall);

    // ── historical timestampMsAtBlock ───────────────────────────────────────
    send_start!("historical timestampMsAtBlock");
    let history = evaluate_historical_timestamp_ms_at_block(&client, mode, head).await;
    send_result!("historical timestampMsAtBlock", history);

    // ── BaseTime storage layout ─────────────────────────────────────────────
    send_start!("BaseTime storage layout");
    let storage = match &head_block {
        Ok(b) => evaluate_basetime_storage(&client, mode, head, b).await,
        Err(e) => CheckResult { passed: Some(false), detail: format!("head fetch failed: {e}") },
    };
    send_result!("BaseTime storage layout", storage);
}

#[cfg(test)]
mod tests {
    use base_common_genesis::HardforkConfig;
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::config::MonitoringConfig;

    #[test]
    fn unscheduled_beryl_follows_active_azul_checks() {
        let mut chain = ChainUpgrades {
            display_name: "Devnet",
            rpc: None,
            specs: specs_from_config(ChainConfig::devnet()),
        };
        assert_eq!(target_hardfork(&chain, 100), Some("Beryl"));

        chain.apply_hardforks(&HardForkConfig {
            base: HardforkConfig { azul: Some(10), beryl: Some(12), subsecond: None },
            ..HardForkConfig::default()
        });

        assert_eq!(target_hardfork(&chain, 11), Some("Beryl"));
        let beryl = chain.specs.iter().find(|spec| spec.name == "Beryl").unwrap();
        assert_eq!(beryl.timestamp, Some(12));
    }

    #[test]
    fn upcoming_azul_remains_target_before_beryl() {
        let mut chain = ChainUpgrades {
            display_name: "Mainnet",
            rpc: None,
            specs: specs_from_config(ChainConfig::mainnet()),
        };
        chain.apply_hardforks(&HardForkConfig {
            jovian_time: Some(10),
            base: HardforkConfig { azul: Some(20), beryl: None, subsecond: None },
            ..HardForkConfig::default()
        });

        assert_eq!(target_hardfork(&chain, 15), Some("Azul"));
        assert_eq!(target_hardfork(&chain, 21), Some("Beryl"));
    }

    #[test]
    fn live_hardforks_do_not_clear_known_static_timestamps() {
        let mut chain = ChainUpgrades {
            display_name: "Mainnet",
            rpc: None,
            specs: specs_from_config(ChainConfig::mainnet()),
        };
        let delta = chain.specs.iter().find(|spec| spec.name == "Delta").unwrap().timestamp;

        chain.apply_hardforks(&HardForkConfig {
            base: HardforkConfig { azul: Some(20), beryl: None, subsecond: None },
            ..HardForkConfig::default()
        });

        assert_eq!(chain.specs.iter().find(|spec| spec.name == "Delta").unwrap().timestamp, delta);
        assert_eq!(
            chain.specs.iter().find(|spec| spec.name == "Azul").unwrap().timestamp,
            Some(20)
        );
    }

    #[test]
    fn devnet_selection_accepts_vibenet_configs() {
        assert!(chain_name_matches_loaded("Devnet", "devnet"));
        assert!(chain_name_matches_loaded("Devnet", "LOCAL-VIBENET"));
        assert!(chain_name_matches_loaded("Devnet", "local-vibenet"));
        assert!(!chain_name_matches_loaded("Devnet", "not-vibenet-really"));
        assert!(!chain_name_matches_loaded("Mainnet", "devnet"));
    }

    #[test]
    fn checkable_specs_are_display_ordered() {
        let chain = ChainUpgrades {
            display_name: "Devnet",
            rpc: None,
            specs: specs_from_config(ChainConfig::devnet()),
        };
        let names: Vec<_> =
            checkable_specs_display(&chain).into_iter().map(|spec| spec.name).collect();

        assert_eq!(names, vec!["Subsecond", "Beryl", "Azul", "Jovian"]);
    }

    #[test]
    fn arrow_keys_select_check_hardfork() {
        let mut view = UpgradesView::new();
        let mut resources = Resources::new(MonitoringConfig::mainnet());

        assert_eq!(view.selected_check_hardfork(100), Some("Beryl"));

        view.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut resources);
        assert_eq!(view.selected_check_hardfork(100), Some("Azul"));

        view.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &mut resources);
        assert_eq!(view.selected_check_hardfork(100), Some("Beryl"));
    }

    #[test]
    fn unscheduled_selected_checks_do_not_start() {
        let mut view = UpgradesView::new();
        view.selected_chain = 3;
        let resources = Resources::new(MonitoringConfig::mainnet());

        assert_eq!(view.selected_check_hardfork(now_unix()), Some("Beryl"));
        view.start_checks(&resources);

        assert!(!view.checks.running);
        assert!(view.checks.chain_idx.is_none());
    }

    #[test]
    fn countdown_progress_does_not_show_complete_before_activation() {
        let target_ts = 90 * SECS_PER_DAY;
        let now = target_ts - 1;

        let tenths = countdown_progress_tenths(0, target_ts, now);

        assert_eq!(tenths, 999);
        assert_eq!(fmt_progress_percent(tenths), "99.9%");
    }

    #[test]
    fn countdown_progress_shows_complete_at_activation() {
        let target_ts = 90 * SECS_PER_DAY;

        let tenths = countdown_progress_tenths(0, target_ts, target_ts);

        assert_eq!(tenths, 1000);
        assert_eq!(fmt_progress_percent(tenths), "100.0%");
    }

    // ── Subsecond check helpers ───────────────────────────────────────────────

    #[test]
    fn subsecond_appears_in_check_names() {
        assert_eq!(check_names_for("Subsecond"), SUBSECOND_CHECK_NAMES);
        assert!(check_names_for("Subsecond").contains(&"200ms per block"));
    }

    #[test]
    fn parse_hex_quantity_handles_prefixed_values() {
        assert_eq!(parse_hex_quantity("0x10").unwrap(), 16);
        assert_eq!(parse_hex_quantity("\"0x10\"").unwrap(), 16);
        assert!(parse_hex_quantity("0xZZ").is_err());
    }

    #[test]
    fn parse_hex_u256_low_u64_truncates_to_low_8_bytes() {
        let full = format!("0x{:0>64}", "1234567890abcdef");
        assert_eq!(parse_hex_u256_low_u64(&full).unwrap(), 0x1234_5678_90ab_cdef);
        assert_eq!(parse_hex_u256_low_u64("0x").unwrap(), 0);
    }

    #[test]
    fn header_timestamp_ms_prefers_explicit_field() {
        assert_eq!(header_timestamp_ms(100, Some(100_400), Some(400)), Some(100_400));
        assert_eq!(header_timestamp_ms(100, None, Some(400)), Some(100_400));
        assert_eq!(header_timestamp_ms(100, None, None), None);
    }

    #[test]
    fn evaluate_predeploy_code_passes_when_code_present_after() {
        let r = evaluate_predeploy_code(CheckMode::After, &Ok("0xdeadbeef".to_string()));
        assert_eq!(r.passed, Some(true));

        let r = evaluate_predeploy_code(CheckMode::After, &Ok("0x".to_string()));
        assert_eq!(r.passed, Some(false));
    }

    #[test]
    fn evaluate_predeploy_code_accepts_missing_before() {
        let r = evaluate_predeploy_code(CheckMode::Before, &Ok("0x".to_string()));
        assert_eq!(r.passed, Some(true));
    }

    #[test]
    fn evaluate_timestamp_ms_call_passes_after_with_value() {
        let r = evaluate_timestamp_ms_call(CheckMode::After, &Ok(1_762_425_600_200));
        assert_eq!(r.passed, Some(true));

        let r = evaluate_timestamp_ms_call(CheckMode::After, &Ok(0));
        assert_eq!(r.passed, Some(false));

        let r = evaluate_timestamp_ms_call(CheckMode::After, &Err("revert".into()));
        assert_eq!(r.passed, Some(false));
    }

    #[test]
    fn evaluate_timestamp_ms_call_passes_before_with_revert_or_zero() {
        let r = evaluate_timestamp_ms_call(CheckMode::Before, &Err("revert".into()));
        assert_eq!(r.passed, Some(true));

        let r = evaluate_timestamp_ms_call(CheckMode::Before, &Ok(0));
        assert_eq!(r.passed, Some(true));

        let r = evaluate_timestamp_ms_call(CheckMode::Before, &Ok(123));
        assert_eq!(r.passed, Some(false));
    }

    #[test]
    fn evaluate_header_timestamp_ms_field_after_validates_grid() {
        // 0x64 = 100s, 0xc8 = 200ms part, 100*1000+200 = 100_200 = 0x18768.
        let block = serde_json::json!({
            "timestamp": "0x64",
            "timestampMs": "0x18768",
            "timestampMillisPart": "0xc8",
        });
        let r = evaluate_header_timestamp_ms_field(CheckMode::After, &block);
        assert_eq!(r.passed, Some(true), "{}", r.detail);

        // Inconsistent: timestampMs doesn't match seconds*1000 + part (100_201 = 0x18769).
        let block = serde_json::json!({
            "timestamp": "0x64",
            "timestampMs": "0x18769",
            "timestampMillisPart": "0xc8",
        });
        let r = evaluate_header_timestamp_ms_field(CheckMode::After, &block);
        assert_eq!(r.passed, Some(false), "{}", r.detail);

        // Bad grid alignment (part=100 isn't a multiple of 200), 100_100 = 0x18704.
        let block = serde_json::json!({
            "timestamp": "0x64",
            "timestampMs": "0x18704",
            "timestampMillisPart": "0x64",
        });
        let r = evaluate_header_timestamp_ms_field(CheckMode::After, &block);
        assert_eq!(r.passed, Some(false), "{}", r.detail);
    }

    #[test]
    fn evaluate_header_timestamp_ms_field_before_rejects_present_fields() {
        let block = serde_json::json!({ "timestamp": "0x64" });
        let r = evaluate_header_timestamp_ms_field(CheckMode::Before, &block);
        assert_eq!(r.passed, Some(true), "{}", r.detail);

        let block = serde_json::json!({
            "timestamp": "0x64",
            "timestampMs": "0x18768",
            "timestampMillisPart": "0xc8",
        });
        let r = evaluate_header_timestamp_ms_field(CheckMode::Before, &block);
        assert_eq!(r.passed, Some(false), "{}", r.detail);
    }

    #[test]
    fn evaluate_basetime_matches_header_skips_before() {
        let block = serde_json::json!({ "timestamp": "0x64" });
        let r = evaluate_basetime_matches_header(CheckMode::Before, &Err("n/a".into()), &block);
        assert_eq!(r.passed, None);
    }

    #[test]
    fn evaluate_basetime_matches_header_compares_after() {
        // 100*1000+200 = 100_200 = 0x18768.
        let block = serde_json::json!({
            "timestamp": "0x64",
            "timestampMs": "0x18768",
            "timestampMillisPart": "0xc8",
        });
        let r = evaluate_basetime_matches_header(CheckMode::After, &Ok(100_200), &block);
        assert_eq!(r.passed, Some(true), "{}", r.detail);

        let r = evaluate_basetime_matches_header(CheckMode::After, &Ok(100_400), &block);
        assert_eq!(r.passed, Some(false), "{}", r.detail);
    }
}
