//! Network upgrade activation countdown and history view.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, Bytes, hex};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::{
    BlockId, BlockNumberOrTag, Filter, TransactionInput, TransactionRequest,
};
use alloy_sol_types::SolCall;
use base_common_chains::{BaseUpgrade, ChainConfig};
use base_common_genesis::UpgradeConfig;
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
    output::COLOR_BASE_BLUE,
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
    upgrade: BaseUpgrade,
    name: &'static str,
    timestamp: Option<u64>,
}

#[derive(Debug)]
struct ChainUpgrades {
    display_name: &'static str,
    chain_id: u64,
    /// RPC URL for this chain, loaded from `~/.config/base/networks/{name}.yaml` at startup.
    /// Falls back to a hardcoded public URL only for mainnet and sepolia.
    /// `None` for internal networks (zeronet, devnet) when no user config is present.
    rpc: Option<String>,
    specs: Vec<UpgradeSpec>,
}

impl ChainUpgrades {
    fn set_timestamp(&mut self, upgrade: BaseUpgrade, timestamp: Option<u64>) {
        let Some(timestamp) = timestamp else { return };
        let Some(spec) = self.specs.iter_mut().find(|spec| spec.upgrade == upgrade) else {
            tracing::warn!(
                chain = %self.display_name,
                upgrade = ?upgrade,
                "missing upgrade spec while applying upgrade timestamp"
            );
            return;
        };
        spec.timestamp = Some(timestamp);
    }

    fn apply_upgrades(&mut self, upgrades: &UpgradeConfig) {
        self.set_timestamp(BaseUpgrade::Delta, upgrades.delta_time);
        self.set_timestamp(BaseUpgrade::Canyon, upgrades.canyon_time);
        self.set_timestamp(BaseUpgrade::Ecotone, upgrades.ecotone_time);
        self.set_timestamp(BaseUpgrade::Fjord, upgrades.fjord_time);
        self.set_timestamp(BaseUpgrade::Granite, upgrades.granite_time);
        self.set_timestamp(BaseUpgrade::Holocene, upgrades.holocene_time);
        self.set_timestamp(BaseUpgrade::Isthmus, upgrades.isthmus_time);
        self.set_timestamp(BaseUpgrade::Jovian, upgrades.jovian_time);
        self.set_timestamp(BaseUpgrade::Azul, upgrades.base.azul);
        self.set_timestamp(BaseUpgrade::Beryl, upgrades.base.beryl);
        self.set_timestamp(BaseUpgrade::Cobalt, upgrades.base.cobalt);
    }

    fn next_scheduled_spec(&self, now: u64) -> Option<&UpgradeSpec> {
        self.specs
            .iter()
            .filter_map(|spec| {
                spec.timestamp
                    .filter(|&timestamp| timestamp > now)
                    .map(|timestamp| (spec, timestamp))
            })
            .min_by_key(|(_, timestamp)| *timestamp)
            .map(|(spec, _)| spec)
    }

    fn seeded_expected_admin(&self, now: u64) -> Option<(&'static str, Address)> {
        // Scan future specs in ascending timestamp order and return the first one whose upgrade
        // has a known seeded admin. This way the panel stays informative even when the
        // immediately-next upgrade (e.g. Azul) predates the activation-registry pattern.
        self.specs
            .iter()
            .filter_map(|spec| {
                spec.timestamp.filter(|&ts| ts > now).and_then(|ts| {
                    ChainConfig::activation_admin_address_for_upgrade_by_chain_id(
                        self.chain_id,
                        spec.upgrade,
                    )
                    .map(|admin| (ts, spec.name, admin))
                })
            })
            .min_by_key(|(ts, _, _)| *ts)
            .map(|(_, name, admin)| (name, admin))
    }
}

fn specs_from_config(cfg: &ChainConfig) -> Vec<UpgradeSpec> {
    vec![
        UpgradeSpec {
            upgrade: BaseUpgrade::Delta,
            name: "Delta",
            timestamp: Some(cfg.delta_timestamp),
        },
        UpgradeSpec {
            upgrade: BaseUpgrade::Canyon,
            name: "Canyon",
            timestamp: Some(cfg.canyon_timestamp),
        },
        UpgradeSpec {
            upgrade: BaseUpgrade::Ecotone,
            name: "Ecotone",
            timestamp: Some(cfg.ecotone_timestamp),
        },
        UpgradeSpec {
            upgrade: BaseUpgrade::Fjord,
            name: "Fjord",
            timestamp: Some(cfg.fjord_timestamp),
        },
        UpgradeSpec {
            upgrade: BaseUpgrade::Granite,
            name: "Granite",
            timestamp: Some(cfg.granite_timestamp),
        },
        UpgradeSpec {
            upgrade: BaseUpgrade::Holocene,
            name: "Holocene",
            timestamp: Some(cfg.holocene_timestamp),
        },
        UpgradeSpec {
            upgrade: BaseUpgrade::Isthmus,
            name: "Isthmus",
            timestamp: Some(cfg.isthmus_timestamp),
        },
        UpgradeSpec {
            upgrade: BaseUpgrade::Jovian,
            name: "Jovian",
            timestamp: Some(cfg.jovian_timestamp),
        },
        UpgradeSpec { upgrade: BaseUpgrade::Azul, name: "Azul", timestamp: cfg.azul_timestamp },
        UpgradeSpec { upgrade: BaseUpgrade::Beryl, name: "Beryl", timestamp: cfg.beryl_timestamp },
        UpgradeSpec {
            upgrade: BaseUpgrade::Cobalt,
            name: "Cobalt",
            timestamp: cfg.cobalt_timestamp,
        },
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
            chain_id: ChainConfig::devnet().chain_id,
            rpc: user_config_rpc("devnet").or_else(|| Some(devnet_rpc())),
            specs: specs_from_config(ChainConfig::devnet()),
        },
        ChainUpgrades {
            display_name: "Zeronet",
            chain_id: ChainConfig::zeronet().chain_id,
            rpc: user_config_rpc("zeronet"),
            specs: specs_from_config(ChainConfig::zeronet()),
        },
        ChainUpgrades {
            display_name: "Sepolia",
            chain_id: ChainConfig::sepolia().chain_id,
            rpc: user_config_rpc("sepolia")
                .or_else(|| Some("https://sepolia.base.org".to_string())),
            specs: specs_from_config(ChainConfig::sepolia()),
        },
        ChainUpgrades {
            display_name: "Mainnet",
            chain_id: ChainConfig::mainnet().chain_id,
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
    "policy registry feature",
    "B-20 stablecoin feature",
    "B-20 asset feature",
];

const BERYL_FEATURE_CHECKS: &[(&str, ActivationFeature)] = &[
    ("policy registry feature", ActivationFeature::PolicyRegistry),
    ("B-20 stablecoin feature", ActivationFeature::B20Stablecoin),
    ("B-20 asset feature", ActivationFeature::B20Asset),
];

fn check_names_for(upgrade: &str) -> &'static [&'static str] {
    match upgrade {
        "Beryl" => BERYL_CHECK_NAMES,
        "Azul" => AZUL_CHECK_NAMES,
        "Jovian" => JOVIAN_CHECK_NAMES,
        _ => &[],
    }
}

fn has_checks(upgrade: &str) -> bool {
    !check_names_for(upgrade).is_empty()
}

fn checkable_specs_display(chain: &ChainUpgrades) -> Vec<&UpgradeSpec> {
    chain.specs.iter().filter(|spec| has_checks(spec.name)).rev().collect()
}

/// Returns the upgrade whose checks should be shown for this chain.
fn target_upgrade(chain: &ChainUpgrades, now: u64) -> Option<&'static str> {
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
            // Prefer the next checkable upgrade when it exists, even before it
            // is scheduled. At the frontier, keep showing the active upgrade.
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
    /// Which upgrade's checks are running.
    upgrade: Option<&'static str>,
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
    fn start(&mut self, chain_idx: usize, rpc_url: String, upgrade: &'static str, mode: CheckMode) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        let (tx, rx) = mpsc::channel(64);
        let chain_changed = self.chain_idx != Some(chain_idx);
        let upgrade_changed = self.upgrade != Some(upgrade);
        let mode_changed = self.mode != Some(mode);
        self.chain_idx = Some(chain_idx);
        self.upgrade = Some(upgrade);
        self.mode = Some(mode);
        self.rpc_url = rpc_url.clone();
        self.current = None;
        // Preserve previous results across auto-refreshes so the table updates
        // in-place rather than blanking on every tick. Only clear when the
        // target context actually changed.
        if chain_changed || upgrade_changed || mode_changed {
            self.results.clear();
        }
        self.running = true;
        self.rx = Some(rx);
        self.last_run_at = Some(Instant::now());
        self.handle = Some(tokio::spawn(run_checks_streaming(upgrade, rpc_url, mode, tx)));
    }

    fn reset(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        self.chain_idx = None;
        self.upgrade = None;
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

const MAX_ADMIN_ACTIVITY_ENTRIES: usize = 32;
const MAX_ADMIN_ACTIVITY_BLOCKS_PER_POLL: u64 = 64;
const ADMIN_ACTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(2);
const ADMIN_ACTIVITY_RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
struct AdminActivityEntry {
    block_number: u64,
    timestamp: u64,
    tx_hash: B256,
    caller: Address,
    action: &'static str,
    detail: String,
}

#[derive(Debug, Default)]
struct AdminActivityChainState {
    watched_rpc_url: String,
    entries: VecDeque<AdminActivityEntry>,
    live_admin: Option<Address>,
    last_scanned_safe_block: Option<u64>,
    status: String,
}

impl AdminActivityChainState {
    fn reset_for_rpc(&mut self, rpc_url: &str) {
        if self.watched_rpc_url == rpc_url {
            return;
        }
        self.clear();
        self.watched_rpc_url = rpc_url.to_string();
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.live_admin = None;
        self.last_scanned_safe_block = None;
        self.status.clear();
    }
}

#[derive(Debug)]
enum AdminActivityUpdate {
    Reset(String),
    ProcessedBlock { entries: Vec<AdminActivityEntry>, live_admin: Option<Address> },
    LiveAdmin(Option<Address>),
    LastScannedSafeBlock(u64),
    Status(String),
}

#[derive(Debug, Default)]
struct AdminActivityWatcher {
    chain_idx: Option<usize>,
    rpc_url: String,
    rx: Option<mpsc::Receiver<AdminActivityUpdate>>,
    handle: Option<JoinHandle<()>>,
}

impl AdminActivityWatcher {
    fn start(&mut self, chain_idx: usize, rpc_url: String, last_scanned_safe_block: Option<u64>) {
        let (tx, rx) = mpsc::channel(64);
        assert!(self.handle.is_none(), "AdminActivityWatcher::start called while already running");
        assert!(self.rx.is_none(), "AdminActivityWatcher::start called with rx already set");
        self.chain_idx = Some(chain_idx);
        self.rpc_url = rpc_url.clone();
        self.rx = Some(rx);
        self.handle =
            Some(tokio::spawn(run_admin_activity_streaming(rpc_url, last_scanned_safe_block, tx)));
    }

    fn stop(&mut self, states: &mut [AdminActivityChainState; 4]) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        self.poll(states);
        self.chain_idx = None;
        self.rpc_url.clear();
        self.rx = None;
    }

    fn needs_restart(&self, chain_idx: usize, rpc_url: &str) -> bool {
        self.chain_idx != Some(chain_idx)
            || self.rpc_url != rpc_url
            || self.handle.as_ref().is_some_and(tokio::task::JoinHandle::is_finished)
    }

    fn poll(&mut self, states: &mut [AdminActivityChainState; 4]) {
        let Some(chain_idx) = self.chain_idx else { return };
        let Some(ref mut rx) = self.rx else { return };

        loop {
            match rx.try_recv() {
                Ok(AdminActivityUpdate::Reset(status)) => {
                    let state = &mut states[chain_idx];
                    state.clear();
                    state.status = status;
                }
                Ok(AdminActivityUpdate::ProcessedBlock { entries, live_admin }) => {
                    let state = &mut states[chain_idx];
                    let last_processed_block = entries.iter().map(|entry| entry.block_number).max();
                    for entry in entries.into_iter().rev() {
                        state.entries.push_front(entry);
                    }
                    while state.entries.len() > MAX_ADMIN_ACTIVITY_ENTRIES {
                        state.entries.pop_back();
                    }
                    if let Some(admin) = live_admin {
                        state.live_admin = Some(admin);
                    }
                    if let Some(block_number) = last_processed_block {
                        state.last_scanned_safe_block = Some(
                            state
                                .last_scanned_safe_block
                                .map_or(block_number, |current| current.max(block_number)),
                        );
                    }
                }
                Ok(AdminActivityUpdate::LiveAdmin(admin)) => {
                    states[chain_idx].live_admin = admin;
                }
                Ok(AdminActivityUpdate::LastScannedSafeBlock(block_number)) => {
                    states[chain_idx].last_scanned_safe_block = Some(block_number);
                }
                Ok(AdminActivityUpdate::Status(status)) => {
                    states[chain_idx].status = status;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    states[chain_idx].status = "Confirmed activity watcher stopped.".to_string();
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
    fmt_timestamp_with(ts, "%Y-%m-%d %H:%M UTC", "genesis")
}

fn fmt_timestamp_with(ts: u64, format: &str, zero_label: &str) -> String {
    if ts == 0 {
        return zero_label.to_string();
    }
    i64::try_from(ts)
        .ok()
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|dt| dt.format(format).to_string())
        .unwrap_or_else(|| ts.to_string())
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
    selected_check_upgrades: [Option<&'static str>; 4],
    tick_count: u64,
    checks: ChecksPanel,
    admin_activity: [AdminActivityChainState; 4],
    admin_activity_watcher: AdminActivityWatcher,
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
            selected_check_upgrades: [None; 4],
            tick_count: 0,
            checks: ChecksPanel {
                chain_idx: None,
                upgrade: None,
                mode: None,
                rpc_url: String::new(),
                current: None,
                results: HashMap::new(),
                running: false,
                rx: None,
                handle: None,
                last_run_at: None,
            },
            admin_activity: std::array::from_fn(|_| AdminActivityChainState::default()),
            admin_activity_watcher: AdminActivityWatcher::default(),
            auto_refresh: true,
        }
    }

    /// Kick off an activation-check run for the currently selected chain, if
    /// the chain has an upgrade with defined checks and a usable RPC URL.
    fn start_checks(&mut self, resources: &Resources) {
        let now = now_unix();
        let Some((upgrade, timestamp)) =
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
        self.checks.start(self.selected_chain, rpc, upgrade, mode);
    }

    fn selected_check_spec(&self, now: u64) -> Option<&UpgradeSpec> {
        let chain = &self.chains[self.selected_chain];
        self.selected_check_upgrades[self.selected_chain]
            .filter(|name| chain.specs.iter().any(|spec| spec.name == *name && has_checks(name)))
            .or_else(|| target_upgrade(chain, now))
            .and_then(|name| chain.specs.iter().find(|spec| spec.name == name))
    }

    fn selected_check_upgrade(&self, now: u64) -> Option<&'static str> {
        self.selected_check_spec(now).map(|spec| spec.name)
    }

    fn move_selected_check_upgrade(&mut self, direction: i8) {
        let now = now_unix();
        let current = self.selected_check_upgrade(now);
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
        if self.selected_check_upgrades[self.selected_chain] != Some(next) {
            self.selected_check_upgrades[self.selected_chain] = Some(next);
            self.checks.reset();
        }
    }

    fn apply_live_upgrades(&mut self, resources: &Resources) {
        let Some(upgrades) = resources.config.upgrades.as_ref() else { return };
        let chain = &mut self.chains[self.selected_chain];
        if chain_name_matches_loaded(chain.display_name, &resources.config.name) {
            chain.apply_upgrades(upgrades);
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

    fn ensure_admin_activity_watcher(&mut self, resources: &Resources) {
        let chain_idx = self.selected_chain;
        let Some(rpc_url) = self.rpc_for_selected(resources) else {
            self.admin_activity_watcher.stop(&mut self.admin_activity);
            self.admin_activity[chain_idx].status =
                "No RPC configured for confirmed activity.".to_string();
            return;
        };

        self.admin_activity[chain_idx].reset_for_rpc(&rpc_url);

        if self.admin_activity_watcher.needs_restart(chain_idx, &rpc_url) {
            self.admin_activity_watcher.stop(&mut self.admin_activity);
            // Capture scan position *after* stop() has drained remaining channel
            // messages, so we don't regress to a stale value.
            let last_scanned_safe_block = self.admin_activity[chain_idx].last_scanned_safe_block;
            self.admin_activity[chain_idx].status =
                "Connecting to confirmed activity watcher…".to_string();
            self.admin_activity_watcher.start(chain_idx, rpc_url, last_scanned_safe_block);
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
                self.move_selected_check_upgrade(-1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selected_check_upgrade(1);
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
        self.admin_activity_watcher.poll(&mut self.admin_activity);
        self.apply_live_upgrades(resources);
        self.ensure_admin_activity_watcher(resources);

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

        let next_scheduled = chain.next_scheduled_spec(now);
        let upcoming =
            next_scheduled.and_then(|spec| spec.timestamp.map(|timestamp| (spec.name, timestamp)));

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

        // Dull the activated banner if the upgrade is stale (> 4 weeks old) or if a
        // newer upgrade is already active on any other chain, meaning this network is
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
        let selected_upgrade = selected_check_spec.map(|spec| spec.name);

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
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(56), Constraint::Percentage(44)])
            .split(bottom[1]);

        render_history(frame, bottom[0], chain, now, selected_upgrade);
        render_checks_panel(
            frame,
            right[0],
            &self.checks,
            self.tick_count,
            selected_check_spec,
            self.auto_refresh,
        );
        let seeded_expected = chain.seeded_expected_admin(now);
        render_admin_activity_panel(
            frame,
            right[1],
            &self.admin_activity[self.selected_chain],
            seeded_expected.map(|(name, _)| name),
            seeded_expected.map(|(_, admin)| admin),
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
    selected_upgrade: Option<&'static str>,
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
            let selected = selected_upgrade == Some(spec.name);
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

    let hf = panel.upgrade.unwrap_or("?");
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

fn render_admin_activity_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AdminActivityChainState,
    next_scheduled_upgrade: Option<&'static str>,
    expected_admin: Option<Address>,
) {
    let block = Block::default()
        .title(" Admin Activity ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    let expected_line = match (next_scheduled_upgrade, expected_admin) {
        (Some(upgrade), Some(admin)) => Line::from(vec![
            Span::styled("Expected: ", Style::default().fg(Color::DarkGray)),
            Span::styled(short_address(admin), Style::default().fg(Color::White)),
            Span::styled(format!("  ({upgrade})"), Style::default().fg(Color::DarkGray)),
        ]),
        (Some(upgrade), None) => Line::from(vec![
            Span::styled("Expected: ", Style::default().fg(Color::DarkGray)),
            Span::styled("unknown", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("  ({upgrade})"), Style::default().fg(Color::DarkGray)),
        ]),
        (None, _) => Line::from(vec![
            Span::styled("Expected: ", Style::default().fg(Color::DarkGray)),
            Span::styled("none scheduled", Style::default().fg(Color::DarkGray)),
        ]),
    };
    let live_line = Line::from(vec![
        Span::styled("Live: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            state.live_admin.map(short_address).unwrap_or_else(|| "unknown".to_string()),
            Style::default().fg(Color::White),
        ),
        Span::styled("  ·  Safe: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            state
                .last_scanned_safe_block
                .map(|block_number| block_number.to_string())
                .unwrap_or_else(|| "-".to_string()),
            Style::default().fg(Color::White),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(vec![expected_line, live_line]).alignment(Alignment::Left),
        sections[0],
    );

    if state.entries.is_empty() {
        let message = if state.status.is_empty() {
            "No confirmed activation-admin activity yet.".to_string()
        } else {
            format!("No confirmed activation-admin activity yet. {}", state.status)
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            sections[1],
        );
        return;
    }

    let rows: Vec<Row<'static>> = state
        .entries
        .iter()
        .map(|entry| {
            let caller_style = if expected_admin == Some(entry.caller) {
                Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            Row::new([
                Cell::from(entry.block_number.to_string()).style(Style::default().fg(Color::White)),
                Cell::from(fmt_activity_timestamp(entry.timestamp))
                    .style(Style::default().fg(Color::Gray)),
                Cell::from(short_hash(entry.tx_hash)).style(Style::default().fg(Color::Cyan)),
                Cell::from(short_address(entry.caller)).style(caller_style),
                Cell::from(format!("{} {}", entry.action, entry.detail))
                    .style(Style::default().fg(Color::Gray)),
            ])
        })
        .collect();

    let header = Row::new(["BLOCK", "TIME", "TX", "CALLER", "ACTION"])
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(8),
        Constraint::Length(14),
        Constraint::Length(16),
        Constraint::Length(14),
        Constraint::Min(12),
    ];

    let table_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(sections[1]);

    frame.render_widget(Table::new(rows, widths).header(header), table_sections[0]);
    frame.render_widget(
        Paragraph::new(state.status.clone())
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Right),
        table_sections[1],
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

fn fmt_activity_timestamp(ts: u64) -> String {
    fmt_timestamp_with(ts, "%m-%d %H:%M:%S", "-")
}

fn short_hash(hash: B256) -> String {
    truncate_hex(format!("{hash:#x}"), 12)
}

fn activation_feature_detail(feature: B256) -> String {
    if feature == ActivationFeature::PolicyRegistry.id() {
        return "policy registry".to_string();
    }
    if feature == ActivationFeature::B20Stablecoin.id() {
        return "B-20 stablecoin".to_string();
    }
    if feature == ActivationFeature::B20Asset.id() {
        return "B-20 asset".to_string();
    }
    format!("feature {}", short_hash(feature))
}

fn decode_admin_activity_log(
    log: &alloy_rpc_types_eth::Log,
    block_number: u64,
    fallback_timestamp: u64,
) -> Option<(AdminActivityEntry, Option<Address>)> {
    let tx_hash = log.transaction_hash?;
    let timestamp = log.block_timestamp.unwrap_or(fallback_timestamp);
    let block_number = log.block_number.unwrap_or(block_number);

    if let Ok(decoded) = log.log_decode::<IActivationRegistry::FeatureActivated>() {
        let event = decoded.data();
        return Some((
            AdminActivityEntry {
                block_number,
                timestamp,
                tx_hash,
                caller: event.caller,
                action: "activate",
                detail: activation_feature_detail(event.feature),
            },
            None,
        ));
    }

    if let Ok(decoded) = log.log_decode::<IActivationRegistry::FeatureDeactivated>() {
        let event = decoded.data();
        return Some((
            AdminActivityEntry {
                block_number,
                timestamp,
                tx_hash,
                caller: event.caller,
                action: "deactivate",
                detail: activation_feature_detail(event.feature),
            },
            None,
        ));
    }

    if let Ok(decoded) = log.log_decode::<IActivationRegistry::AdminChanged>() {
        let event = decoded.data();
        return Some((
            AdminActivityEntry {
                block_number,
                timestamp,
                tx_hash,
                caller: event.caller,
                action: "setAdmin",
                detail: format!(
                    "{} → {}",
                    short_address(event.previousAdmin),
                    short_address(event.newAdmin)
                ),
            },
            Some(event.newAdmin),
        ));
    }

    None
}

const fn admin_activity_scan_end(next_block: u64, safe_number: u64) -> u64 {
    let max_end = next_block.saturating_add(MAX_ADMIN_ACTIVITY_BLOCKS_PER_POLL.saturating_sub(1));
    if max_end < safe_number { max_end } else { safe_number }
}

async fn fetch_block_timestamp<P>(provider: &P, block_number: u64) -> Result<u64, String>
where
    P: Provider + ?Sized,
{
    provider
        .get_block_by_number(BlockNumberOrTag::Number(block_number))
        .await
        .map_err(|error| error.to_string())?
        .map(|block| block.header.timestamp)
        .ok_or_else(|| format!("block {block_number} not found"))
}

async fn fetch_admin_activity_for_range<P>(
    provider: &P,
    start_block: u64,
    end_block: u64,
) -> Result<BTreeMap<u64, Vec<(AdminActivityEntry, Option<Address>)>>, String>
where
    P: Provider + ?Sized,
{
    let filter = Filter::new()
        .address(ActivationRegistryStorage::ADDRESS)
        .from_block(start_block)
        .to_block(end_block);
    let logs = provider.get_logs(&filter).await.map_err(|error| error.to_string())?;
    if logs.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut fallback_timestamps = HashMap::new();
    let mut entries_by_block = BTreeMap::new();
    for log in logs.iter().filter(|log| !log.removed) {
        let block_number = log.block_number.unwrap_or(start_block);
        let timestamp = if let Some(timestamp) = log.block_timestamp {
            timestamp
        } else {
            match fallback_timestamps.get(&block_number).copied() {
                Some(timestamp) => timestamp,
                None => {
                    let timestamp = fetch_block_timestamp(provider, block_number).await?;
                    fallback_timestamps.insert(block_number, timestamp);
                    timestamp
                }
            }
        };
        if let Some(entry) = decode_admin_activity_log(log, block_number, timestamp) {
            entries_by_block.entry(block_number).or_insert_with(Vec::new).push(entry);
        }
    }

    Ok(entries_by_block)
}

async fn run_admin_activity_streaming(
    rpc_url: String,
    last_scanned_safe_block: Option<u64>,
    tx: mpsc::Sender<AdminActivityUpdate>,
) {
    let mut last_safe_block = last_scanned_safe_block;

    loop {
        if tx
            .send(AdminActivityUpdate::Status(
                "Connecting to confirmed activity watcher…".to_string(),
            ))
            .await
            .is_err()
        {
            return;
        }

        let provider = {
            let url = match rpc_url.parse::<alloy_transport_http::reqwest::Url>() {
                Ok(u) => u,
                Err(error) => {
                    if tx
                        .send(AdminActivityUpdate::Status(format!("Invalid RPC URL: {error}")))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    // An invalid URL won't be fixed by retrying, so stop the loop.
                    return;
                }
            };
            let http_client = match alloy_transport_http::reqwest::Client::builder()
                .timeout(Duration::from_secs(12))
                .build()
            {
                Ok(c) => c,
                Err(error) => {
                    if tx
                        .send(AdminActivityUpdate::Status(format!(
                            "RPC connection failed: {error}"
                        )))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(ADMIN_ACTIVITY_RETRY_DELAY).await;
                    continue;
                }
            };
            let transport = alloy_transport_http::Http::with_client(http_client, url);
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .connect_client(alloy_rpc_client::RpcClient::new(transport, false))
        };

        let live_admin_status = match send_safe_live_admin_update(&tx, &provider).await {
            Ok(status) => status,
            Err(()) => return,
        };
        if tx
            .send(AdminActivityUpdate::Status(live_admin_status.map_or_else(
                || "Watching confirmed activation activity".to_string(),
                |detail| format!("Watching confirmed activation activity. {detail}"),
            )))
            .await
            .is_err()
        {
            return;
        }

        let mut interval = tokio::time::interval(ADMIN_ACTIVITY_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        'watch: loop {
            interval.tick().await;

            let safe_block = match provider.get_block_by_number(BlockNumberOrTag::Safe).await {
                Ok(Some(block)) => block,
                Ok(None) => {
                    if tx
                        .send(AdminActivityUpdate::Status(
                            "Safe head is unavailable on this RPC.".to_string(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    break 'watch;
                }
                Err(error) => {
                    if tx
                        .send(AdminActivityUpdate::Status(format!(
                            "Safe head query failed: {error}"
                        )))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    break 'watch;
                }
            };

            let safe_number = safe_block.header.number;
            if last_safe_block.is_some_and(|last| safe_number < last) {
                last_safe_block = None;
                if tx
                    .send(AdminActivityUpdate::Reset(
                        "Safe head moved backwards; resetting confirmed activity cache."
                            .to_string(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
                let reset_status = match send_safe_live_admin_update(&tx, &provider).await {
                    Ok(Some(detail)) => {
                        format!(
                            "Safe head moved backwards; resetting confirmed activity cache. {detail}"
                        )
                    }
                    Ok(None) => {
                        "Safe head moved backwards; resetting confirmed activity cache.".to_string()
                    }
                    Err(()) => return,
                };
                if tx.send(AdminActivityUpdate::Status(reset_status)).await.is_err() {
                    return;
                }
            }
            let mut next_block = match last_safe_block {
                Some(last) if last < safe_number => last + 1,
                Some(_) => {
                    if tx
                        .send(AdminActivityUpdate::LastScannedSafeBlock(safe_number))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                None => safe_number,
            };
            let scan_end = admin_activity_scan_end(next_block, safe_number);
            let mut entries_by_block = match fetch_admin_activity_for_range(
                &provider, next_block, scan_end,
            )
            .await
            {
                Ok(entries_by_block) => entries_by_block,
                Err(error) => {
                    if tx
                            .send(AdminActivityUpdate::Status(format!(
                                "Confirmed activity query failed for blocks {next_block}-{scan_end}: {error}"
                            )))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    break 'watch;
                }
            };

            while next_block <= scan_end {
                let entries = entries_by_block.remove(&next_block).unwrap_or_default();

                let mut block_entries = Vec::with_capacity(entries.len());
                let mut live_admin_update = None;
                for (entry, maybe_live_admin) in entries {
                    block_entries.push(entry);
                    if let Some(admin) = maybe_live_admin {
                        live_admin_update = Some(admin);
                    }
                }
                if !block_entries.is_empty()
                    && tx
                        .send(AdminActivityUpdate::ProcessedBlock {
                            entries: block_entries,
                            live_admin: live_admin_update,
                        })
                        .await
                        .is_err()
                {
                    return;
                }

                last_safe_block = Some(next_block);
                next_block += 1;
            }

            if tx.send(AdminActivityUpdate::LastScannedSafeBlock(scan_end)).await.is_err() {
                return;
            }
        }

        tokio::time::sleep(ADMIN_ACTIVITY_RETRY_DELAY).await;
    }
}

// ── Activation checks ─────────────────────────────────────────────────────────

/// Route to the correct upgrade's streaming check function.
async fn run_checks_streaming(
    upgrade: &'static str,
    rpc_url: String,
    mode: CheckMode,
    tx: mpsc::Sender<CheckUpdate>,
) {
    match upgrade {
        "Beryl" => run_beryl_checks_streaming(rpc_url, mode, tx).await,
        "Azul" => run_azul_checks_streaming(rpc_url, tx).await,
        "Jovian" => run_jovian_checks_streaming(rpc_url, mode, tx).await,
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

fn truncate_hex(value: String, prefix_len: usize) -> String {
    const SUFFIX_LEN: usize = 4;
    if value.len() <= prefix_len + 2 + SUFFIX_LEN {
        value
    } else {
        format!("{}..{}", &value[..prefix_len], &value[value.len() - SUFFIX_LEN..])
    }
}

fn short_address(address: Address) -> String {
    truncate_hex(address.to_string(), 10)
}

async fn activation_admin(client: &HttpClient) -> Result<Address, String> {
    activation_admin_at_tag(client, "latest").await
}

async fn fetch_safe_live_admin<P: Provider + ?Sized>(
    provider: &P,
) -> Result<Option<Address>, String> {
    let request = TransactionRequest::default()
        .to(ActivationRegistryStorage::ADDRESS)
        .input(TransactionInput::new(Bytes::from(IActivationRegistry::adminCall {}.abi_encode())));
    provider
        .call(request)
        .block(BlockId::Number(BlockNumberOrTag::Safe))
        .await
        .map_err(|e| format!("safe live-admin query failed: {e}"))
        .and_then(|bytes| {
            IActivationRegistry::adminCall::abi_decode_returns(&bytes)
                .map(|admin| (!admin.is_zero()).then_some(admin))
                .map_err(|e| format!("safe live-admin decode failed: {e}"))
        })
}

async fn send_safe_live_admin_update<P: Provider + ?Sized>(
    tx: &mpsc::Sender<AdminActivityUpdate>,
    provider: &P,
) -> Result<Option<String>, ()> {
    match fetch_safe_live_admin(provider).await {
        Ok(admin) => {
            if tx.send(AdminActivityUpdate::LiveAdmin(admin)).await.is_err() {
                return Err(());
            }
            Ok(None)
        }
        Err(error) => Ok(Some(error)),
    }
}

async fn activation_admin_at_tag(client: &HttpClient, block_tag: &str) -> Result<Address, String> {
    let data = calldata_hex(IActivationRegistry::adminCall {}.abi_encode());
    let to = activation_registry_address();
    let output = eth_call_at_tag(client, &to, &data, block_tag).await?;
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
    eth_call_at_tag(client, to, data, "latest").await
}

async fn eth_call_at_tag(
    client: &HttpClient,
    to: &str,
    data: &str,
    block_tag: &str,
) -> Result<String, String> {
    ClientT::request::<String, _>(
        client,
        "eth_call",
        rpc_params![json!({"to": to, "data": data}), block_tag],
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

#[cfg(test)]
mod tests {
    use alloy_primitives::{Log as PrimitiveLog, address};
    use alloy_sol_types::SolEvent;
    use base_common_genesis::BaseUpgradeConfig;
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::config::MonitoringConfig;

    #[test]
    fn unscheduled_beryl_follows_active_azul_checks() {
        let mut chain = ChainUpgrades {
            display_name: "Devnet",
            chain_id: ChainConfig::devnet().chain_id,
            rpc: None,
            specs: specs_from_config(ChainConfig::devnet()),
        };
        assert_eq!(target_upgrade(&chain, 100), Some("Beryl"));

        chain.apply_upgrades(&UpgradeConfig {
            base: BaseUpgradeConfig { azul: Some(10), beryl: Some(12), cobalt: None },
            ..UpgradeConfig::default()
        });

        assert_eq!(target_upgrade(&chain, 11), Some("Beryl"));
        let beryl = chain.specs.iter().find(|spec| spec.name == "Beryl").unwrap();
        assert_eq!(beryl.timestamp, Some(12));
    }

    #[test]
    fn upcoming_azul_remains_target_before_beryl() {
        let mut chain = ChainUpgrades {
            display_name: "Mainnet",
            chain_id: ChainConfig::mainnet().chain_id,
            rpc: None,
            specs: specs_from_config(ChainConfig::mainnet()),
        };
        chain.apply_upgrades(&UpgradeConfig {
            jovian_time: Some(10),
            base: BaseUpgradeConfig { azul: Some(20), beryl: None, cobalt: None },
            ..UpgradeConfig::default()
        });

        assert_eq!(target_upgrade(&chain, 15), Some("Azul"));
        assert_eq!(target_upgrade(&chain, 21), Some("Beryl"));
    }

    #[test]
    fn live_upgrades_do_not_clear_known_static_timestamps() {
        let mut chain = ChainUpgrades {
            display_name: "Mainnet",
            chain_id: ChainConfig::mainnet().chain_id,
            rpc: None,
            specs: specs_from_config(ChainConfig::mainnet()),
        };
        let delta = chain.specs.iter().find(|spec| spec.name == "Delta").unwrap().timestamp;

        chain.apply_upgrades(&UpgradeConfig {
            base: BaseUpgradeConfig { azul: Some(20), beryl: None, cobalt: None },
            ..UpgradeConfig::default()
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
            chain_id: ChainConfig::devnet().chain_id,
            rpc: None,
            specs: specs_from_config(ChainConfig::devnet()),
        };
        let names: Vec<_> =
            checkable_specs_display(&chain).into_iter().map(|spec| spec.name).collect();

        assert_eq!(names, vec!["Beryl", "Azul", "Jovian"]);
    }

    #[test]
    fn seeded_expected_admin_skips_upgrades_without_known_admin() {
        // Azul has no seeded admin; seeded_expected_admin should look past it to Cobalt.
        let chain = ChainUpgrades {
            display_name: "Mainnet",
            chain_id: ChainConfig::mainnet().chain_id,
            rpc: None,
            specs: vec![
                UpgradeSpec { upgrade: BaseUpgrade::Azul, name: "Azul", timestamp: Some(20) },
                UpgradeSpec { upgrade: BaseUpgrade::Cobalt, name: "Cobalt", timestamp: Some(30) },
            ],
        };
        let cobalt_admin = ChainConfig::mainnet()
            .activation_admin_address_for_upgrade(BaseUpgrade::Cobalt)
            .map(|admin| ("Cobalt", admin));

        // When Azul is next but has no seeded admin, Cobalt's admin is surfaced instead.
        assert_eq!(chain.seeded_expected_admin(10), cobalt_admin);
        // Once Azul has activated, Cobalt is the next with a known admin.
        assert_eq!(chain.seeded_expected_admin(25), cobalt_admin);
        // Once all future upgrades have passed, nothing is returned.
        assert_eq!(chain.seeded_expected_admin(35), None);
    }

    #[test]
    fn missing_upgrade_spec_does_not_panic_when_applying_timestamp() {
        let mut chain = ChainUpgrades {
            display_name: "Mainnet",
            chain_id: ChainConfig::mainnet().chain_id,
            rpc: None,
            specs: vec![UpgradeSpec {
                upgrade: BaseUpgrade::Azul,
                name: "Azul",
                timestamp: Some(10),
            }],
        };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            chain.set_timestamp(BaseUpgrade::Beryl, Some(20));
        }));

        assert!(result.is_ok());
        assert_eq!(chain.specs[0].timestamp, Some(10));
    }

    #[test]
    fn admin_activity_state_resets_when_rpc_changes() {
        let mut state = AdminActivityChainState {
            watched_rpc_url: "https://rpc-one.example".to_string(),
            entries: VecDeque::from([AdminActivityEntry {
                block_number: 42,
                timestamp: 1_725_000_000,
                tx_hash: B256::repeat_byte(0x44),
                caller: address!("3333333333333333333333333333333333333333"),
                action: "setAdmin",
                detail: "detail".to_string(),
            }]),
            live_admin: Some(address!("2222222222222222222222222222222222222222")),
            last_scanned_safe_block: Some(42),
            status: "Watching confirmed activation activity".to_string(),
        };

        state.reset_for_rpc("https://rpc-two.example");

        assert_eq!(state.watched_rpc_url, "https://rpc-two.example");
        assert!(state.entries.is_empty());
        assert!(state.live_admin.is_none());
        assert!(state.last_scanned_safe_block.is_none());
        assert!(state.status.is_empty());
    }

    #[test]
    fn processed_block_update_advances_progress_before_batch_end() {
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(AdminActivityUpdate::ProcessedBlock {
            entries: vec![AdminActivityEntry {
                block_number: 42,
                timestamp: 1_725_000_000,
                tx_hash: B256::repeat_byte(0x44),
                caller: address!("3333333333333333333333333333333333333333"),
                action: "setAdmin",
                detail: "detail".to_string(),
            }],
            live_admin: Some(address!("2222222222222222222222222222222222222222")),
        })
        .expect("update fits in channel");

        let mut watcher = AdminActivityWatcher {
            chain_idx: Some(0),
            rpc_url: String::new(),
            rx: Some(rx),
            handle: None,
        };
        let mut states = std::array::from_fn(|_| AdminActivityChainState::default());

        watcher.poll(&mut states);

        assert_eq!(states[0].last_scanned_safe_block, Some(42));
        assert_eq!(
            states[0].live_admin,
            Some(address!("2222222222222222222222222222222222222222"))
        );
        assert_eq!(states[0].entries.len(), 1);
        assert_eq!(states[0].entries[0].block_number, 42);

        tx.try_send(AdminActivityUpdate::LastScannedSafeBlock(45))
            .expect("progress update fits in channel");

        watcher.poll(&mut states);

        assert_eq!(states[0].last_scanned_safe_block, Some(45));
        assert_eq!(
            states[0].live_admin,
            Some(address!("2222222222222222222222222222222222222222"))
        );
        assert_eq!(states[0].entries.len(), 1);
        assert_eq!(states[0].entries[0].block_number, 42);
    }

    #[test]
    fn admin_activity_scan_end_caps_catch_up_work_per_poll() {
        assert_eq!(
            admin_activity_scan_end(10, 10 + MAX_ADMIN_ACTIVITY_BLOCKS_PER_POLL * 2),
            10 + MAX_ADMIN_ACTIVITY_BLOCKS_PER_POLL - 1
        );
        assert_eq!(admin_activity_scan_end(10, 20), 20);
    }

    #[test]
    fn decode_admin_activity_log_decodes_admin_changed_event() {
        let previous_admin = address!("1111111111111111111111111111111111111111");
        let new_admin = address!("2222222222222222222222222222222222222222");
        let caller = address!("3333333333333333333333333333333333333333");
        let tx_hash = B256::repeat_byte(0x44);
        let log = alloy_rpc_types_eth::Log {
            inner: PrimitiveLog {
                address: ActivationRegistryStorage::ADDRESS,
                data: IActivationRegistry::AdminChanged {
                    previousAdmin: previous_admin,
                    newAdmin: new_admin,
                    caller,
                }
                .encode_log_data(),
            },
            block_number: Some(42),
            block_timestamp: Some(1_725_000_000),
            transaction_hash: Some(tx_hash),
            ..Default::default()
        };

        let (entry, live_admin) =
            decode_admin_activity_log(&log, 42, 1_725_000_000).expect("log decodes");

        assert_eq!(entry.block_number, 42);
        assert_eq!(entry.timestamp, 1_725_000_000);
        assert_eq!(entry.tx_hash, tx_hash);
        assert_eq!(entry.caller, caller);
        assert_eq!(entry.action, "setAdmin");
        assert_eq!(
            entry.detail,
            format!("{} → {}", short_address(previous_admin), short_address(new_admin))
        );
        assert_eq!(live_admin, Some(new_admin));
    }

    #[test]
    fn arrow_keys_select_check_upgrade() {
        let mut view = UpgradesView::new();
        let mut resources = Resources::new(MonitoringConfig::mainnet());

        assert_eq!(view.selected_check_upgrade(100), Some("Beryl"));

        view.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut resources);
        assert_eq!(view.selected_check_upgrade(100), Some("Azul"));

        view.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &mut resources);
        assert_eq!(view.selected_check_upgrade(100), Some("Beryl"));
    }

    #[test]
    fn unscheduled_selected_checks_do_not_start() {
        let mut view = UpgradesView::new();
        view.selected_chain = 0;
        let resources = Resources::new(MonitoringConfig::mainnet());

        assert_eq!(view.selected_check_upgrade(now_unix()), Some("Beryl"));
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
}
