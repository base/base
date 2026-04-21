use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use base_load_tests::{
    DisplaySnapshot, LoadRunner, MetricsSummary, OsakaTarget, PrecompileTarget, RpcClient,
    TestConfig, TxTypeConfig, WeightedTxType, devnet_funder, ensure_funder_balance, is_local_rpc,
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap},
};
use tokio::sync::{mpsc, mpsc::error::TryRecvError, watch};
use url::Url;

use crate::{
    app::{Action, LoadTestTask, Resources, View},
    commands::COLOR_BASE_BLUE,
    tui::{Keybinding, Toast},
};

// ---------------------------------------------------------------------------
// Key binding tables (two variants: idle and running)
// ---------------------------------------------------------------------------

const KEYBINDINGS_IDLE: &[Keybinding] = &[
    Keybinding { key: "←/→", description: "Select network" },
    Keybinding { key: "b", description: "Begin test" },
    Keybinding { key: "c", description: "Continuous mode" },
    Keybinding { key: "t", description: "Strategy" },
    Keybinding { key: "e", description: "Edit config" },
    Keybinding { key: "Esc", description: "Back" },
    Keybinding { key: "?", description: "Toggle help" },
];

const KEYBINDINGS_RUNNING: &[Keybinding] = &[
    Keybinding { key: "s", description: "Stop test" },
    Keybinding { key: "Esc", description: "Back" },
    Keybinding { key: "?", description: "Toggle help" },
];

// ---------------------------------------------------------------------------
// Editable config fields (subset most useful to tweak before a run)
// ---------------------------------------------------------------------------

const EDIT_FIELDS: &[&str] = &[
    "rpc",
    "duration",
    "sender_count",
    "in_flight_per_sender",
    "target_gps",
    "funding_amount",
    "funder_key",
    "block_watcher_url",
    "flashblocks_ws_url",
];

// ---------------------------------------------------------------------------
// Run state
// ---------------------------------------------------------------------------

/// Coarse phase label streamed from the background task to the status panel.
#[derive(Debug, Clone, Default)]
enum RunPhase {
    /// Transferring ETH from devnet reserve (Hardhat) accounts into the funder wallet.
    #[default]
    Bootstrap,
    /// Distributing ETH from the funder wallet to each sender account.
    Funding,
    Running,
    Draining,
}

struct RunProgress {
    run_count: u32,
    continuous: bool,
    elapsed: Duration,
    duration: Option<Duration>,
}

enum RunState {
    Idle,
    Running {
        start: Instant,
        /// When the actual test run began (after bootstrap/funding). Used for the
        /// progress bar so that bootstrap/funding time does not count against the
        /// configured duration.
        run_start: Option<Instant>,
        run_count: u32,
        stop_flag: Arc<AtomicBool>,
        /// Shared with the background task; storing `false` causes the loop to
        /// stop after the current run completes rather than starting another.
        continuous_flag: Arc<AtomicBool>,
        phase_rx: watch::Receiver<RunPhase>,
        snap_rx: watch::Receiver<DisplaySnapshot>,
        run_count_rx: watch::Receiver<u32>,
        done_rx: mpsc::Receiver<Result<MetricsSummary, String>>,
        current_snap: DisplaySnapshot,
        current_phase: RunPhase,
    },
    Complete {
        summary: MetricsSummary,
        elapsed: Duration,
        run_count: u32,
    },
    Error(String),
}

impl std::fmt::Debug for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "Idle"),
            Self::Running { run_count, run_start, .. } => {
                write!(f, "Running(run={run_count}, started={})", run_start.is_some())
            }
            Self::Complete { run_count, .. } => write!(f, "Complete(run={run_count})"),
            Self::Error(e) => write!(f, "Error({e})"),
        }
    }
}

// ---------------------------------------------------------------------------
// Strategy multiselect
// ---------------------------------------------------------------------------

/// Flat enumeration of all individually-selectable load strategies shown in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrategyOption {
    Transfer,
    Calldata,
    Ecrecover,
    Sha256,
    Ripemd160,
    Identity,
    Modexp,
    Bn254Add,
    Bn254Mul,
    Bn254Pairing,
    Blake2f,
    Kzg,
    OsakaClz,
    OsakaP256verify,
    OsakaModexp,
}

const ALL_STRATEGIES: &[StrategyOption] = &[
    StrategyOption::Transfer,
    StrategyOption::Calldata,
    StrategyOption::Ecrecover,
    StrategyOption::Sha256,
    StrategyOption::Ripemd160,
    StrategyOption::Identity,
    StrategyOption::Modexp,
    StrategyOption::Bn254Add,
    StrategyOption::Bn254Mul,
    StrategyOption::Bn254Pairing,
    StrategyOption::Blake2f,
    StrategyOption::Kzg,
    StrategyOption::OsakaClz,
    StrategyOption::OsakaP256verify,
    StrategyOption::OsakaModexp,
];

impl StrategyOption {
    const fn label(self) -> &'static str {
        match self {
            Self::Transfer => "transfer",
            Self::Calldata => "calldata",
            Self::Ecrecover => "precompile  ecrecover",
            Self::Sha256 => "precompile  sha256",
            Self::Ripemd160 => "precompile  ripemd160",
            Self::Identity => "precompile  identity",
            Self::Modexp => "precompile  modexp",
            Self::Bn254Add => "precompile  bn254_add",
            Self::Bn254Mul => "precompile  bn254_mul",
            Self::Bn254Pairing => "precompile  bn254_pairing",
            Self::Blake2f => "precompile  blake2f",
            Self::Kzg => "precompile  kzg",
            Self::OsakaClz => "osaka  clz",
            Self::OsakaP256verify => "osaka  p256verify",
            Self::OsakaModexp => "osaka  modexp",
        }
    }

    const fn to_tx_type(self) -> TxTypeConfig {
        match self {
            Self::Transfer => TxTypeConfig::Transfer,
            Self::Calldata => TxTypeConfig::Calldata { max_size: 128, repeat_count: 1 },
            Self::Ecrecover => {
                TxTypeConfig::Precompile { target: PrecompileTarget::Ecrecover, iterations: 1 }
            }
            Self::Sha256 => {
                TxTypeConfig::Precompile { target: PrecompileTarget::Sha256, iterations: 1 }
            }
            Self::Ripemd160 => {
                TxTypeConfig::Precompile { target: PrecompileTarget::Ripemd160, iterations: 1 }
            }
            Self::Identity => {
                TxTypeConfig::Precompile { target: PrecompileTarget::Identity, iterations: 1 }
            }
            Self::Modexp => {
                TxTypeConfig::Precompile { target: PrecompileTarget::Modexp, iterations: 1 }
            }
            Self::Bn254Add => {
                TxTypeConfig::Precompile { target: PrecompileTarget::Bn254Add, iterations: 1 }
            }
            Self::Bn254Mul => {
                TxTypeConfig::Precompile { target: PrecompileTarget::Bn254Mul, iterations: 1 }
            }
            Self::Bn254Pairing => {
                TxTypeConfig::Precompile { target: PrecompileTarget::Bn254Pairing, iterations: 1 }
            }
            Self::Blake2f => TxTypeConfig::Precompile {
                target: PrecompileTarget::Blake2f { rounds: None },
                iterations: 1,
            },
            Self::Kzg => TxTypeConfig::Precompile {
                target: PrecompileTarget::KzgPointEvaluation,
                iterations: 1,
            },
            Self::OsakaClz => TxTypeConfig::Osaka { target: OsakaTarget::Clz },
            Self::OsakaP256verify => TxTypeConfig::Osaka { target: OsakaTarget::P256verifyOsaka },
            Self::OsakaModexp => TxTypeConfig::Osaka { target: OsakaTarget::ModexpOsaka },
        }
    }

    const fn matches_tx_type(self, tx: &TxTypeConfig) -> bool {
        matches!(
            (self, tx),
            (Self::Transfer, TxTypeConfig::Transfer)
                | (Self::Calldata, TxTypeConfig::Calldata { .. })
                | (
                    Self::Ecrecover,
                    TxTypeConfig::Precompile { target: PrecompileTarget::Ecrecover, .. }
                )
                | (Self::Sha256, TxTypeConfig::Precompile { target: PrecompileTarget::Sha256, .. })
                | (
                    Self::Ripemd160,
                    TxTypeConfig::Precompile { target: PrecompileTarget::Ripemd160, .. }
                )
                | (
                    Self::Identity,
                    TxTypeConfig::Precompile { target: PrecompileTarget::Identity, .. }
                )
                | (Self::Modexp, TxTypeConfig::Precompile { target: PrecompileTarget::Modexp, .. })
                | (
                    Self::Bn254Add,
                    TxTypeConfig::Precompile { target: PrecompileTarget::Bn254Add, .. }
                )
                | (
                    Self::Bn254Mul,
                    TxTypeConfig::Precompile { target: PrecompileTarget::Bn254Mul, .. }
                )
                | (
                    Self::Bn254Pairing,
                    TxTypeConfig::Precompile { target: PrecompileTarget::Bn254Pairing, .. }
                )
                | (
                    Self::Blake2f,
                    TxTypeConfig::Precompile { target: PrecompileTarget::Blake2f { .. }, .. }
                )
                | (
                    Self::Kzg,
                    TxTypeConfig::Precompile { target: PrecompileTarget::KzgPointEvaluation, .. }
                )
                | (Self::OsakaClz, TxTypeConfig::Osaka { target: OsakaTarget::Clz })
                | (
                    Self::OsakaP256verify,
                    TxTypeConfig::Osaka { target: OsakaTarget::P256verifyOsaka }
                )
                | (Self::OsakaModexp, TxTypeConfig::Osaka { target: OsakaTarget::ModexpOsaka })
        )
    }
}

#[derive(Debug)]
struct StrategyModal {
    /// Index of the currently highlighted strategy.
    cursor: usize,
    /// Which strategies are currently enabled.
    enabled: Vec<bool>,
}

impl StrategyModal {
    fn from_config(transactions: &[WeightedTxType]) -> Self {
        let mut enabled = vec![false; ALL_STRATEGIES.len()];
        for (i, &strategy) in ALL_STRATEGIES.iter().enumerate() {
            enabled[i] = transactions.iter().any(|t| strategy.matches_tx_type(&t.tx_type));
        }
        // Default to transfer if nothing matched.
        if !enabled.iter().any(|&e| e) {
            enabled[0] = true;
        }
        Self { cursor: 0, enabled }
    }

    fn to_transactions(&self) -> Vec<WeightedTxType> {
        ALL_STRATEGIES
            .iter()
            .enumerate()
            .filter(|(i, _)| self.enabled[*i])
            .map(|(_, &strategy)| WeightedTxType { weight: 1, tx_type: strategy.to_tx_type() })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Edit modal state
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct EditModal {
    /// Index of the currently highlighted field.
    field: usize,
    /// Whether the user is actively typing into the selected field.
    typing: bool,
    /// Text buffer for the field being edited.
    buf: String,
}

// ---------------------------------------------------------------------------
// Main view struct
// ---------------------------------------------------------------------------

/// Load test runner and live metrics view.
///
/// On first tick, auto-builds a [`TestConfig`] from the active network's RPC
/// endpoint so no manual file setup is needed. Any YAML files found at
/// `~/.config/base/load-tests/<name>.yaml` overlay the synthesized defaults,
/// and all files in that directory are offered for multi-network switching with
/// `←`/`→`. The user can further edit key parameters in-memory with `e`,
/// and drive the load test with `b` (single run) or `c` (continuous loop).
/// Live [`DisplaySnapshot`] updates stream in via a watch channel pushed by
/// the runner every 500 ms. `s` stops the current run cleanly.
#[derive(Debug)]
pub struct LoadTestView {
    /// Discovered (name, config) pairs. Populated lazily on first tick.
    configs: Vec<(String, TestConfig)>,
    /// Index of the currently selected config.
    selected: usize,
    /// Per-config in-memory overrides applied before running.
    overrides: Vec<TestConfig>,
    /// Whether configs have been initialized from Resources yet.
    initialized: bool,
    /// Whether continuous-loop mode is active.
    continuous: bool,
    /// Current run state.
    state: RunState,
    /// Edit modal; `None` when the modal is closed.
    edit: Option<EditModal>,
    /// Strategy multiselect modal; `None` when the modal is closed.
    strategy_modal: Option<StrategyModal>,
    /// In-memory funder private key override (raw 0x-prefixed hex).
    /// When set, takes precedence over the `FUNDER_KEY` env var.
    funder_key_override: Option<String>,
}

impl Default for LoadTestView {
    fn default() -> Self {
        Self::new()
    }
}

impl LoadTestView {
    /// Creates a new load test view. Config discovery is deferred to the first tick.
    pub const fn new() -> Self {
        Self {
            configs: Vec::new(),
            overrides: Vec::new(),
            selected: 0,
            initialized: false,
            continuous: false,
            state: RunState::Idle,
            edit: None,
            strategy_modal: None,
            funder_key_override: None,
        }
    }

    /// Populates configs on the first tick using the active network from `Resources`.
    ///
    /// Always synthesizes entries for devnet, sepolia, and mainnet so `←`/`→`
    /// navigation works without any file setup. Any `~/.config/base/load-tests/<name>.yaml`
    /// overrides the synthesized defaults for that name. The active network
    /// (matching `resources.config.name`) is pre-selected.
    fn ensure_initialized(&mut self, resources: &Resources) {
        if self.initialized {
            return;
        }
        self.initialized = true;

        let active_name = resources.config.name.clone();
        let dir_configs = load_dir_configs();

        // Canonical list: active network first (using its actual RPC from resources),
        // then the other two known networks. Any on-disk YAML overrides the defaults.
        // Each entry carries HTTP RPC, WebSocket RPC, and flashblocks WS URLs so that
        // synthesised configs work out-of-the-box without requiring a YAML file on disk.
        let known: &[(&str, &str, &str, &str)] = &[
            ("devnet", "http://localhost:7545", "ws://localhost:8546", "ws://localhost:7111"),
            (
                "sepolia",
                "https://sepolia.base.org",
                "wss://sepolia.base.org",
                "wss://sepolia.flashblocks.base.org/ws",
            ),
            (
                "mainnet",
                "https://mainnet.base.org",
                "wss://mainnet.base.org",
                "wss://mainnet.flashblocks.base.org/ws",
            ),
        ];

        let mut configs: Vec<(String, TestConfig)> = Vec::new();

        // Add the active network first, using its live RPC from resources.
        let active_rpc = resources.config.rpc.clone();
        let active_cfg = dir_configs
            .iter()
            .find(|(n, _)| n == &active_name)
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| {
                let mut cfg = TestConfig { rpc: active_rpc, ..TestConfig::default() };
                if let Some(&(_, _, ws, fb)) = known.iter().find(|&&(n, _, _, _)| n == active_name)
                {
                    cfg.block_watcher_url = Url::parse(ws).expect("hardcoded WS URL is valid");
                    cfg.flashblocks_ws_url = Url::parse(fb).expect("hardcoded FB URL is valid");
                }
                cfg
            });
        configs.push((active_name.clone(), active_cfg));

        // Add the remaining known networks, skipping the already-added active one.
        for &(name, rpc_str, ws_str, fb_str) in known {
            if name == active_name {
                continue;
            }
            let rpc = Url::parse(rpc_str).expect("hardcoded URL is valid");
            let cfg = dir_configs
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, c)| c.clone())
                .unwrap_or_else(|| {
                    let block_watcher_url = Url::parse(ws_str).expect("hardcoded WS URL is valid");
                    let flashblocks_ws_url =
                        Url::parse(fb_str).expect("hardcoded FB URL is valid");
                    TestConfig { rpc, block_watcher_url, flashblocks_ws_url, ..TestConfig::default() }
                });
            configs.push((name.to_string(), cfg));
        }

        // Append any directory configs for names not already covered.
        for (name, cfg) in dir_configs {
            if !configs.iter().any(|(n, _)| *n == name) {
                configs.push((name, cfg));
            }
        }

        self.overrides = configs.iter().map(|(_, c)| c.clone()).collect();
        self.configs = configs;
        self.selected = 0;
    }

    // -----------------------------------------------------------------------
    // Config helpers
    // -----------------------------------------------------------------------

    /// Returns the effective config for the selected network (override or base).
    fn effective_config(&self) -> Option<&TestConfig> {
        self.overrides.get(self.selected)
    }

    /// Returns a mutable reference to the override for the selected config.
    fn effective_config_mut(&mut self) -> Option<&mut TestConfig> {
        self.overrides.get_mut(self.selected)
    }

    /// Name of the selected network, if any.
    fn selected_name(&self) -> Option<&str> {
        self.configs.get(self.selected).map(|(n, _)| n.as_str())
    }

    // -----------------------------------------------------------------------
    // Run control
    // -----------------------------------------------------------------------

    fn start_run(&mut self, run_count: u32, resources: &mut Resources) {
        let Some(cfg) = self.effective_config().cloned() else { return };
        let funder_key_override = self.funder_key_override.clone();

        // Pre-create the stop flag so the view can stop the test immediately,
        // even before the async task has finished fetching chain_id or funding.
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_for_runner = Arc::clone(&stop_flag);

        let (phase_tx, phase_rx) = watch::channel(RunPhase::Bootstrap);
        let (snap_tx, snap_rx) = watch::channel(DisplaySnapshot::default());
        let (run_count_tx, run_count_rx) = watch::channel(run_count);
        let (done_tx, done_rx) = mpsc::channel(1);

        let continuous_flag = Arc::new(AtomicBool::new(self.continuous));
        let continuous_for_task = Arc::clone(&continuous_flag);

        let task_handle = tokio::spawn(async move {
            // Fetch chain_id from the network's RPC — required for transaction signing.
            let client = RpcClient::new(cfg.rpc.clone());
            let chain_id = client.chain_id().await.ok();

            let load_config = match cfg.to_load_config(chain_id) {
                Ok(lc) => lc,
                Err(e) => {
                    let _ = done_tx.send(Err(e.to_string())).await;
                    return;
                }
            };

            let funding_amount = match cfg.parse_funding_amount() {
                Ok(a) => a,
                Err(e) => {
                    let _ = done_tx.send(Err(e.to_string())).await;
                    return;
                }
            };

            // Capture before load_config is consumed by LoadRunner::new.
            let chain_id_val = load_config.chain_id;
            let max_gas_price = load_config.max_gas_price;
            let sender_count = cfg.sender_count;

            let mut runner = match LoadRunner::new(load_config) {
                Ok(r) => r,
                Err(e) => {
                    let _ = done_tx.send(Err(e.to_string())).await;
                    return;
                }
            };

            runner.replace_stop_flag(Arc::clone(&stop_flag_for_runner));
            runner.set_snapshot_tx(snap_tx);

            let is_local = is_local_rpc(&cfg.rpc);

            // Resolve funder: explicit override > $FUNDER_KEY env var > devnet auto-select.
            let funder =
                if let Ok(f) = TestConfig::resolve_funder_key(funder_key_override.as_deref()) {
                    Some(f)
                } else if is_local {
                    devnet_funder(&client).await
                } else {
                    None
                };

            if let Some(ref funder) = funder {
                runner.set_funder_address(funder.address().to_string());
            }

            let mut current_run = run_count;
            let mut last_result: Result<MetricsSummary, String>;

            loop {
                let _ = run_count_tx.send(current_run);

                if let Some(ref funder) = funder {
                    // On local devnets, top up the funder from Hardhat reserve accounts if
                    // needed. This runs each iteration so the funder stays topped up over long
                    // continuous sessions.
                    if is_local {
                        let _ = phase_tx.send(RunPhase::Bootstrap);
                        if let Err(e) = ensure_funder_balance(
                            &client,
                            cfg.rpc.clone(),
                            funder.address(),
                            funding_amount,
                            sender_count,
                            chain_id_val,
                            max_gas_price,
                        )
                        .await
                        {
                            // Non-fatal: fund_accounts will surface a clearer error if truly
                            // short.
                            let _ =
                                done_tx.send(Err(format!("devnet bootstrap failed: {e}"))).await;
                            return;
                        }
                    }

                    let _ = phase_tx.send(RunPhase::Funding);
                    if let Err(e) = runner.fund_accounts(funder.clone(), funding_amount).await {
                        let _ = done_tx.send(Err(format!("funding failed: {e}"))).await;
                        return;
                    }
                }

                let _ = phase_tx.send(RunPhase::Running);
                let result = runner.run().await;
                // run() always sets stop_flag=true on exit to signal the confirmer.
                // Reset it here so the next iteration's run() starts clean, and so
                // the break condition below only fires on a user-initiated stop
                // (which stores false into continuous_for_task via stop_run()).
                stop_flag_for_runner.store(false, Ordering::SeqCst);

                // Drain accounts back to funder regardless of run outcome.
                if let Some(ref funder) = funder {
                    let _ = phase_tx.send(RunPhase::Draining);
                    runner.drain_accounts(funder.clone()).await.ok();
                }

                last_result = result.map_err(|e| e.to_string());

                if last_result.is_err() || !continuous_for_task.load(Ordering::SeqCst) {
                    break;
                }

                current_run += 1;
            }

            let _ = done_tx.send(last_result).await;
        });

        resources.load_test_task =
            Some(LoadTestTask { stop_flag: Arc::clone(&stop_flag), handle: task_handle });

        self.state = RunState::Running {
            start: Instant::now(),
            run_start: None,
            run_count,
            stop_flag,
            continuous_flag,
            phase_rx,
            snap_rx,
            run_count_rx,
            done_rx,
            current_snap: DisplaySnapshot::default(),
            current_phase: RunPhase::Bootstrap,
        };

        resources.toasts.push(Toast::info(format!(
            "Load test started (run {}{})",
            run_count,
            if self.continuous { ", continuous" } else { "" },
        )));
    }

    fn stop_run(&mut self) {
        if let RunState::Running { ref stop_flag, ref continuous_flag, .. } = self.state {
            stop_flag.store(true, Ordering::SeqCst);
            continuous_flag.store(false, Ordering::SeqCst);
            self.continuous = false;
        }
    }

    // -----------------------------------------------------------------------
    // Edit modal helpers
    // -----------------------------------------------------------------------

    fn field_value(&self, field: &str) -> String {
        if field == "funder_key" {
            // Show the raw key string so the user can see/copy/edit it.
            return self.funder_key_override.clone().unwrap_or_default();
        }
        let Some(cfg) = self.effective_config() else { return String::new() };
        match field {
            "rpc" => cfg.rpc.to_string(),
            "duration" => cfg.duration.clone().unwrap_or_else(|| "∞".into()),
            "sender_count" => cfg.sender_count.to_string(),
            "in_flight_per_sender" => cfg.in_flight_per_sender.to_string(),
            "target_gps" => cfg.target_gps.map_or_else(|| "default".into(), |v| v.to_string()),
            "funding_amount" => format_wei_as_eth(&cfg.funding_amount),
            "block_watcher_url" => cfg.block_watcher_url.to_string(),
            "flashblocks_ws_url" => cfg.flashblocks_ws_url.to_string(),
            _ => String::new(),
        }
    }

    fn apply_field_edit(&mut self, field: &str, value: &str) {
        if field == "funder_key" {
            if value.is_empty() {
                self.funder_key_override = None;
            } else if TestConfig::resolve_funder_key(Some(value)).is_ok() {
                self.funder_key_override = Some(value.to_string());
            }
            // Silently ignore invalid keys; the address display won't update.
            return;
        }
        let Some(cfg) = self.effective_config_mut() else { return };
        match field {
            "rpc" => {
                if let Ok(url) = Url::parse(value) {
                    cfg.rpc = url;
                }
            }
            "duration" => {
                if value == "∞" || value.is_empty() {
                    cfg.duration = None;
                } else {
                    cfg.duration = Some(value.to_string());
                }
            }
            "sender_count" => {
                if let Ok(v) = value.parse::<u32>()
                    && v > 0
                {
                    cfg.sender_count = v;
                }
            }
            "in_flight_per_sender" => {
                if let Ok(v) = value.parse::<u32>() {
                    cfg.in_flight_per_sender = v;
                }
            }
            "target_gps" => {
                if value == "default" || value.is_empty() {
                    cfg.target_gps = None;
                } else if let Ok(v) = value.parse::<u64>() {
                    cfg.target_gps = Some(v);
                }
            }
            "funding_amount" => {
                if let Some(wei) = parse_eth_to_wei(value) {
                    cfg.funding_amount = wei.to_string();
                }
            }
            "block_watcher_url" => {
                if let Ok(url) = Url::parse(value) {
                    cfg.block_watcher_url = url;
                }
            }
            "flashblocks_ws_url" => {
                if let Ok(url) = Url::parse(value) {
                    cfg.flashblocks_ws_url = url;
                }
            }
            _ => {}
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        let Some(ref mut modal) = self.edit else { return };

        if modal.typing {
            match key.code {
                KeyCode::Enter => {
                    let field = EDIT_FIELDS[modal.field];
                    let value = modal.buf.clone();
                    self.apply_field_edit(field, &value);
                    // Borrow ends here — safe to re-borrow self.edit
                    if let Some(ref mut m) = self.edit {
                        m.typing = false;
                        m.buf.clear();
                    }
                }
                KeyCode::Esc => {
                    if let Some(ref mut m) = self.edit {
                        m.typing = false;
                        m.buf.clear();
                    }
                }
                KeyCode::Backspace => {
                    modal.buf.pop();
                }
                KeyCode::Char(c) => {
                    modal.buf.push(c);
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    modal.field = modal.field.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if modal.field + 1 < EDIT_FIELDS.len() {
                        modal.field += 1;
                    }
                }
                KeyCode::Enter => {
                    let field = EDIT_FIELDS[modal.field];
                    let current = self.field_value(field);
                    if let Some(ref mut m) = self.edit {
                        m.buf = current;
                        m.typing = true;
                    }
                }
                KeyCode::Esc => {
                    self.edit = None;
                }
                _ => {}
            }
        }
    }

    fn handle_strategy_key(&mut self, key: KeyEvent) {
        let n = ALL_STRATEGIES.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(ref mut modal) = self.strategy_modal {
                    modal.cursor = modal.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(ref mut modal) = self.strategy_modal
                    && modal.cursor + 1 < n
                {
                    modal.cursor += 1;
                }
            }
            KeyCode::Char(' ') => {
                if let Some(ref mut modal) = self.strategy_modal {
                    let i = modal.cursor;
                    modal.enabled[i] = !modal.enabled[i];
                }
            }
            KeyCode::Enter => {
                let transactions = self.strategy_modal.as_ref().map(|m| m.to_transactions());
                if let Some(txs) = transactions
                    && !txs.is_empty()
                    && let Some(cfg) = self.effective_config_mut()
                {
                    cfg.transactions = txs;
                }
                self.strategy_modal = None;
            }
            KeyCode::Esc => {
                self.strategy_modal = None;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Config discovery
// ---------------------------------------------------------------------------

/// Loads all `*.yaml` / `*.yml` files from `~/.config/base/load-tests/`, sorted by name.
/// Returns an empty list (without error) if the directory does not exist.
fn load_dir_configs() -> Vec<(String, TestConfig)> {
    let Some(dir) = config_dir() else { return Vec::new() };
    if !dir.exists() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };

    let mut configs: Vec<(String, TestConfig)> = entries
        .flatten()
        .filter(|e: &std::fs::DirEntry| {
            let ext = e.path().extension().and_then(|s| s.to_str()).map(str::to_owned);
            matches!(ext.as_deref(), Some("yaml") | Some("yml"))
        })
        .filter_map(|e: std::fs::DirEntry| {
            let path = e.path();
            let name = path.file_stem()?.to_string_lossy().into_owned();
            let cfg = TestConfig::load(&path).ok()?;
            Some((name, cfg))
        })
        .collect();

    configs.sort_by(|(a, _), (b, _)| a.cmp(b));
    configs
}

fn config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config").join("base").join("load-tests"))
}

// ---------------------------------------------------------------------------
// View trait implementation
// ---------------------------------------------------------------------------

impl View for LoadTestView {
    fn keybindings(&self) -> &'static [Keybinding] {
        match self.state {
            RunState::Running { .. } => KEYBINDINGS_RUNNING,
            _ => KEYBINDINGS_IDLE,
        }
    }

    fn consumes_esc(&self) -> bool {
        // Consume Esc ourselves when a modal is open.
        self.edit.is_some() || self.strategy_modal.is_some()
    }

    fn consumes_quit(&self) -> bool {
        self.edit.is_some() || self.strategy_modal.is_some()
    }

    fn captures_char_input(&self) -> bool {
        self.edit.is_some()
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        self.ensure_initialized(resources);

        // Drain the latest snapshot and check for completion.
        let done = if let RunState::Running {
            ref mut snap_rx,
            ref mut phase_rx,
            ref mut run_count_rx,
            ref mut done_rx,
            ref mut current_snap,
            ref mut current_phase,
            ref mut run_start,
            ref mut run_count,
            start,
            ..
        } = self.state
        {
            if phase_rx.has_changed().unwrap_or(false) {
                let new_phase = phase_rx.borrow_and_update().clone();
                // A new Bootstrap phase signals the start of the next loop iteration —
                // reset run_start so the progress bar tracks only the current run.
                if matches!(new_phase, RunPhase::Bootstrap) {
                    *run_start = None;
                }
                if matches!(new_phase, RunPhase::Running) && run_start.is_none() {
                    *run_start = Some(Instant::now());
                }
                *current_phase = new_phase;
            }
            // Update snapshot (non-blocking — take latest value).
            if snap_rx.has_changed().unwrap_or(false) {
                *current_snap = snap_rx.borrow_and_update().clone();
            }
            // Update run count from the background task loop.
            if run_count_rx.has_changed().unwrap_or(false) {
                *run_count = *run_count_rx.borrow_and_update();
            }

            // Check for completion.
            match done_rx.try_recv() {
                Ok(Ok(summary)) => Some(Ok((summary, start.elapsed(), *run_count))),
                Ok(Err(e)) => Some(Err(e)),
                Err(TryRecvError::Disconnected) => {
                    Some(Err("load test task exited unexpectedly".into()))
                }
                Err(TryRecvError::Empty) => None,
            }
        } else {
            None
        };

        if let Some(result) = done {
            match result {
                Ok((summary, elapsed, run_count)) => {
                    resources.load_test_task = None;
                    resources.toasts.push(Toast::info(format!(
                        "Load test complete in {:.1}s — {:.1} TPS / {:.0} GPS",
                        elapsed.as_secs_f64(),
                        summary.throughput.tps,
                        summary.throughput.gps,
                    )));
                    self.state = RunState::Complete { summary, elapsed, run_count };
                }
                Err(e) => {
                    resources.load_test_task = None;
                    resources.toasts.push(Toast::warning(format!("Load test error: {e}")));
                    self.state = RunState::Error(e);
                }
            }
        }

        Action::None
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        // Route keys into the edit modal when it is open.
        if self.edit.is_some() {
            self.handle_edit_key(key);
            return Action::None;
        }

        // Route keys into the strategy modal when it is open.
        if self.strategy_modal.is_some() {
            self.handle_strategy_key(key);
            return Action::None;
        }

        match key.code {
            // Network selection — only while not running.
            KeyCode::Left | KeyCode::Char('h')
                if !matches!(self.state, RunState::Running { .. })
                    && !self.configs.is_empty() =>
            {
                let n = self.configs.len();
                self.selected = (self.selected + n - 1) % n;
            }
            KeyCode::Right | KeyCode::Char('l')
                if !matches!(self.state, RunState::Running { .. })
                    && !self.configs.is_empty() =>
            {
                self.selected = (self.selected + 1) % self.configs.len();
            }

            // Begin single run.
            KeyCode::Char('b')
                if !matches!(self.state, RunState::Running { .. })
                    && !self.configs.is_empty() =>
            {
                self.continuous = false;
                self.state = RunState::Idle;
                self.start_run(1, resources);
            }

            // Begin continuous run.
            KeyCode::Char('c')
                if !matches!(self.state, RunState::Running { .. })
                    && !self.configs.is_empty() =>
            {
                self.continuous = true;
                self.state = RunState::Idle;
                self.start_run(1, resources);
            }

            // Stop.
            KeyCode::Char('s') | KeyCode::Char('x')
                if matches!(self.state, RunState::Running { .. }) =>
            {
                self.stop_run();
            }

            // Open strategy multiselect modal.
            KeyCode::Char('t')
                if !matches!(self.state, RunState::Running { .. })
                    && !self.configs.is_empty() =>
            {
                let txs =
                    self.effective_config().map(|c| c.transactions.clone()).unwrap_or_default();
                self.strategy_modal = Some(StrategyModal::from_config(&txs));
            }

            // Open edit modal.
            KeyCode::Char('e')
                if !matches!(self.state, RunState::Running { .. })
                    && !self.configs.is_empty() =>
            {
                self.edit = Some(EditModal::default());
            }

            _ => {}
        }

        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        // ensure_initialized is also called in tick(), but guard here for the first frame.
        if !self.initialized {
            self.ensure_initialized(resources);
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        let content_area = chunks[0];
        let footer_area = chunks[1];

        if self.configs.is_empty() {
            render_no_configs(frame, content_area);
        } else {
            let panels = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
                .split(content_area);

            self.render_config_panel(frame, panels[0]);
            self.render_status_panel(frame, panels[1]);
        }

        render_footer(frame, footer_area, &self.state, self.continuous);

        // Overlay the edit modal on top of everything.
        if self.edit.is_some() {
            render_edit_modal(frame, area, &self.edit, self);
        }

        // Overlay the strategy modal on top of everything.
        if let Some(ref modal) = self.strategy_modal {
            render_strategy_modal(frame, area, modal);
        }
    }
}

// ---------------------------------------------------------------------------
// Panel renderers
// ---------------------------------------------------------------------------

impl LoadTestView {
    fn render_config_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let name = self.selected_name().unwrap_or("unknown");
        let prev = if self.configs.len() > 1 {
            let i = (self.selected + self.configs.len() - 1) % self.configs.len();
            format!("← {} ", self.configs[i].0)
        } else {
            String::new()
        };
        let next = if self.configs.len() > 1 {
            let i = (self.selected + 1) % self.configs.len();
            format!(" {} →", self.configs[i].0)
        } else {
            String::new()
        };

        let title = format!(" {prev}[{name}]{next} ");

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(COLOR_BASE_BLUE));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(cfg) = self.effective_config() else { return };

        let label_style = Style::default().fg(Color::DarkGray);
        let value_style = Style::default().fg(Color::White);
        let dim_style = Style::default().fg(Color::DarkGray);

        let mut lines: Vec<Line<'_>> = Vec::new();

        let truncate = |s: String| -> String {
            if s.len() > 40 { format!("{}…", &s[..39]) } else { s }
        };

        lines.push(Line::from(vec![
            Span::styled("  RPC           ", label_style),
            Span::styled(truncate(cfg.rpc.to_string()), value_style),
        ]));

        lines.push(Line::from(vec![
            Span::styled("  Block Watch   ", label_style),
            Span::styled(truncate(cfg.block_watcher_url.to_string()), dim_style),
        ]));

        lines.push(Line::from(vec![
            Span::styled("  Flashblocks   ", label_style),
            Span::styled(truncate(cfg.flashblocks_ws_url.to_string()), dim_style),
        ]));

        lines.push(Line::from(""));

        lines.push(Line::from(vec![
            Span::styled("  Senders       ", label_style),
            Span::styled(cfg.sender_count.to_string(), value_style),
            Span::styled(
                if cfg.sender_offset > 0 {
                    format!("  (offset {})", cfg.sender_offset)
                } else {
                    String::new()
                },
                dim_style,
            ),
        ]));

        lines.push(Line::from(vec![
            Span::styled("  In-flight     ", label_style),
            Span::styled(cfg.in_flight_per_sender.to_string(), value_style),
            Span::styled(" per sender", dim_style),
        ]));

        lines.push(Line::from(vec![
            Span::styled("  Target GPS    ", label_style),
            Span::styled(
                cfg.target_gps.map(format_large_number).unwrap_or_else(|| "default".into()),
                value_style,
            ),
        ]));

        lines.push(Line::from(vec![
            Span::styled("  Duration      ", label_style),
            Span::styled(cfg.duration.as_deref().unwrap_or("∞ continuous"), value_style),
        ]));

        lines.push(Line::from(vec![
            Span::styled("  Funding       ", label_style),
            Span::styled(format_wei_as_eth(&cfg.funding_amount), value_style),
            Span::styled(" / account", dim_style),
        ]));

        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled("  Strategy", label_style)]));

        let total_weight: u32 = cfg.transactions.iter().map(|t| t.weight).sum();
        for wtx in &cfg.transactions {
            let pct = if total_weight > 0 {
                (wtx.weight as f64 / total_weight as f64 * 100.0).round() as u32
            } else {
                0
            };
            let type_str = format_tx_type(&wtx.tx_type);
            lines.push(Line::from(vec![
                Span::styled("    ", label_style),
                Span::styled(format!("{pct:>3}%  "), Style::default().fg(COLOR_BASE_BLUE)),
                Span::styled(type_str, value_style),
            ]));
        }

        lines.push(Line::from(""));

        // Funder key — shows resolved address and source (override vs env var).
        let funder_override = self.funder_key_override.as_deref();
        let funder_addr = TestConfig::funder_key_address(funder_override);
        match &funder_addr {
            Some(addr) => {
                let source =
                    if funder_override.is_some() { " (override)" } else { " ($FUNDER_KEY)" };
                lines.push(Line::from(vec![
                    Span::styled("  FUNDER_KEY     ", label_style),
                    Span::styled(format!("set ✓{source}"), Style::default().fg(Color::Green)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("                 ", label_style),
                    Span::styled(addr.as_str(), dim_style),
                ]));
            }
            None => {
                lines.push(Line::from(vec![
                    Span::styled("  FUNDER_KEY     ", label_style),
                    Span::styled(
                        "not set",
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" — accounts won't be funded", Style::default().fg(Color::Yellow)),
                ]));
            }
        }

        if matches!(self.state, RunState::Idle | RunState::Complete { .. }) {
            lines.push(Line::from(""));
            lines
                .push(Line::from(vec![Span::styled("  [t] strategy  [e] edit config", dim_style)]));
        }

        let p = Paragraph::new(lines);
        frame.render_widget(p, inner);
    }

    fn render_status_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .title(" Status ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        match &self.state {
            RunState::Idle => render_idle_status(frame, inner, self.continuous),
            RunState::Running {
                start, run_start, run_count, current_snap, current_phase, ..
            } => {
                let run_duration = self
                    .effective_config()
                    .and_then(|c: &TestConfig| c.parse_duration().ok())
                    .flatten();
                // Use the actual test start time (after bootstrap/funding) for the
                // progress bar so that funding time doesn't count against the duration.
                // Fall back to task spawn time while still in bootstrap/funding phases.
                let elapsed = run_start.map_or_else(|| start.elapsed(), |t| t.elapsed());
                render_running_status(
                    frame,
                    inner,
                    current_snap,
                    current_phase,
                    RunProgress {
                        run_count: *run_count,
                        continuous: self.continuous,
                        elapsed,
                        duration: run_duration,
                    },
                );
            }
            RunState::Complete { summary, elapsed, run_count } => {
                render_complete_status(frame, inner, summary, *elapsed, *run_count);
            }
            RunState::Error(msg) => render_error_status(frame, inner, msg),
        }
    }
}

// ---------------------------------------------------------------------------
// Status sub-renderers
// ---------------------------------------------------------------------------

fn render_idle_status(frame: &mut Frame<'_>, area: Rect, continuous: bool) {
    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled("  ● IDLE", Style::default().fg(Color::DarkGray))),
        Line::from(""),
    ];

    if continuous {
        lines.push(Line::from(vec![
            Span::styled("  ∞ ", Style::default().fg(Color::Cyan)),
            Span::styled("continuous mode on", Style::default().fg(Color::Cyan)),
        ]));
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled("  Press ", dim),
        Span::styled("[b]", key),
        Span::styled(" to begin a single run", dim),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Press ", dim),
        Span::styled("[c]", key),
        Span::styled(" to run continuously", dim),
    ]));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_running_status(
    frame: &mut Frame<'_>,
    area: Rect,
    snap: &DisplaySnapshot,
    phase: &RunPhase,
    progress: RunProgress,
) {
    let RunProgress { run_count, continuous, elapsed, duration } = progress;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header + progress bar
            Constraint::Min(0),    // metrics
        ])
        .split(area);

    // Header row — label changes with phase.
    let (phase_icon, phase_label, phase_color) = match phase {
        RunPhase::Bootstrap => ("⟳", " BOOTSTRAPPING", Color::Cyan),
        RunPhase::Funding => ("⟳", " FUNDING", Color::Yellow),
        RunPhase::Running => ("▶", " RUNNING", Color::Green),
        RunPhase::Draining => ("⟳", " DRAINING", Color::Yellow),
    };
    let continuous_badge = if continuous && matches!(phase, RunPhase::Running) {
        Span::styled("  ∞ CONTINUOUS", Style::default().fg(Color::Cyan))
    } else {
        Span::raw("")
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("  {phase_icon}{phase_label}"),
            Style::default().fg(phase_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  (run {run_count})"), Style::default().fg(Color::DarkGray)),
        continuous_badge,
    ]));
    frame.render_widget(header, chunks[0]);

    // Progress bar (duration-bounded runs only).
    let gauge_area =
        Rect { x: area.x + 2, y: area.y + 2, width: area.width.saturating_sub(4), height: 1 };
    if let Some(dur) = duration {
        let ratio = (elapsed.as_secs_f64() / dur.as_secs_f64()).min(1.0);
        let label = format!("{:.0}s / {:.0}s", elapsed.as_secs_f64(), dur.as_secs_f64());
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(COLOR_BASE_BLUE).bg(Color::DarkGray))
            .ratio(ratio)
            .label(label);
        frame.render_widget(gauge, gauge_area);
    } else {
        let label = format!("{:.0}s elapsed", elapsed.as_secs_f64());
        let p = Paragraph::new(Span::styled(label, Style::default().fg(Color::DarkGray)));
        frame.render_widget(p, gauge_area);
    }

    // Metrics grid — only meaningful once transactions are actually flying.
    let metrics_area =
        Rect { x: area.x, y: area.y + 4, width: area.width, height: area.height.saturating_sub(4) };
    if matches!(phase, RunPhase::Running) {
        render_live_metrics(frame, metrics_area, snap);
    } else {
        let msg = match phase {
            RunPhase::Bootstrap => "  Topping up funder from devnet reserves…",
            RunPhase::Funding => "  Funding sender accounts…",
            RunPhase::Draining => "  Draining sender accounts…",
            RunPhase::Running => unreachable!(),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(Color::DarkGray))),
            metrics_area,
        );
    }
}

fn render_live_metrics(frame: &mut Frame<'_>, area: Rect, snap: &DisplaySnapshot) {
    let label = Style::default().fg(Color::DarkGray);
    let value = Style::default().fg(Color::White);
    let highlight = Style::default().fg(COLOR_BASE_BLUE);
    let warn = Style::default().fg(Color::Yellow);

    let mut lines: Vec<Line<'_>> = Vec::new();

    // Throughput.
    lines.push(Line::from(Span::styled("  THROUGHPUT", label)));
    lines.push(Line::from(vec![
        Span::styled("    TPS  ", label),
        Span::styled(format!("{:.1}", snap.rolling_tps), highlight),
        Span::styled("    GPS  ", label),
        Span::styled(format_large_number_f(snap.rolling_gps), highlight),
    ]));

    lines.push(Line::from(""));

    // Transactions.
    let success_pct = if snap.submitted > 0 {
        snap.confirmed as f64 / snap.submitted as f64 * 100.0
    } else {
        0.0
    };
    lines.push(Line::from(Span::styled("  TRANSACTIONS", label)));
    lines.push(Line::from(vec![
        Span::styled("    Submitted  ", label),
        Span::styled(format!("{}", snap.submitted), value),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    Confirmed  ", label),
        Span::styled(format!("{}", snap.confirmed), value),
        Span::styled(format!("  ({success_pct:.1}%)"), Style::default().fg(Color::Green)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    Failed     ", label),
        Span::styled(
            format!("{}", snap.failed),
            if snap.failed > 0 { Style::default().fg(Color::Red) } else { value },
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    In-flight  ", label),
        Span::styled(format!("{} / {}", snap.in_flight, snap.total_senders * 16), value),
    ]));

    lines.push(Line::from(""));

    // Latency.
    lines.push(Line::from(Span::styled("  BLOCK LATENCY (rolling 30s)", label)));
    lines.push(Line::from(vec![
        Span::styled("    p50  ", label),
        Span::styled(fmt_dur(snap.p50_latency), value),
        Span::styled("    p99  ", label),
        Span::styled(fmt_dur(snap.p99_latency), value),
    ]));
    if snap.flashblocks_p50_latency > Duration::ZERO
        || snap.flashblocks_p99_latency > Duration::ZERO
    {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  FLASHBLOCKS LATENCY (rolling 30s)", label)));
        lines.push(Line::from(vec![
            Span::styled("    p50  ", label),
            Span::styled(fmt_dur(snap.flashblocks_p50_latency), value),
            Span::styled("    p99  ", label),
            Span::styled(fmt_dur(snap.flashblocks_p99_latency), value),
        ]));
    }

    lines.push(Line::from(""));

    // Gas.
    lines.push(Line::from(Span::styled("  GAS", label)));
    lines.push(Line::from(vec![
        Span::styled("    Price  ", label),
        Span::styled(format!("{:.2} gwei", snap.gas_price_gwei), value),
    ]));

    // Balance.
    if snap.total_eth.is_some() || snap.min_eth.is_some() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  BALANCE", label)));
        if let Some(ref total) = snap.total_eth {
            lines.push(Line::from(vec![
                Span::styled("    Total     ", label),
                Span::styled(format!("{total} ETH"), value),
            ]));
        }
        if let Some(ref min) = snap.min_eth {
            let style = if snap.funds_low { warn } else { value };
            lines.push(Line::from(vec![
                Span::styled("    Min/acct  ", label),
                Span::styled(format!("{min} ETH{}", if snap.funds_low { " ⚠" } else { "" }), style),
            ]));
        }
    }

    // Accounts section — funder + first few senders.
    if snap.funder_address.is_some() || !snap.sender_addresses.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  ACCOUNTS", label)));
        if let Some(ref addr) = snap.funder_address {
            lines.push(Line::from(vec![
                Span::styled("    Funder   ", label),
                Span::styled(truncate_addr(addr), value),
            ]));
        }
        if !snap.sender_addresses.is_empty() {
            let n = snap.sender_addresses.len();
            let first = truncate_addr(&snap.sender_addresses[0]);
            let senders_label = if n == 1 { first } else { format!("{first}  +{}", n - 1) };
            lines.push(Line::from(vec![
                Span::styled("    Senders  ", label),
                Span::styled(senders_label, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn truncate_addr(addr: &str) -> String {
    // Show 0x + first 6 + … + last 4 chars.
    if addr.len() > 14 {
        format!("{}…{}", &addr[..8], &addr[addr.len() - 4..])
    } else {
        addr.to_string()
    }
}

fn render_complete_status(
    frame: &mut Frame<'_>,
    area: Rect,
    summary: &MetricsSummary,
    elapsed: Duration,
    run_count: u32,
) {
    let label = Style::default().fg(Color::DarkGray);
    let value = Style::default().fg(Color::White);
    let good = Style::default().fg(Color::Green);
    let highlight = Style::default().fg(COLOR_BASE_BLUE);
    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'_>> = Vec::new();

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  ✓ COMPLETE",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ({:.1}s, run {})", elapsed.as_secs_f64(), run_count),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled("  FINAL RESULTS", label)));
    lines.push(Line::from(vec![
        Span::styled("    TPS  ", label),
        Span::styled(format!("{:.1}", summary.throughput.tps), highlight),
        Span::styled("    GPS  ", label),
        Span::styled(format_large_number_f(summary.throughput.gps), highlight),
    ]));
    lines.push(Line::from(""));

    let sr = summary.throughput.success_rate();
    lines.push(Line::from(vec![
        Span::styled("    Submitted  ", label),
        Span::styled(format!("{}", summary.throughput.total_submitted), value),
        Span::styled("  Confirmed  ", label),
        Span::styled(format!("{}", summary.throughput.total_confirmed), good),
        Span::styled(format!("  ({sr:.1}%)"), good),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    Failed     ", label),
        Span::styled(
            format!("{}", summary.throughput.total_failed),
            if summary.throughput.total_failed > 0 {
                Style::default().fg(Color::Red)
            } else {
                value
            },
        ),
    ]));
    lines.push(Line::from(""));

    lines.push(Line::from(vec![
        Span::styled("    Latency p50  ", label),
        Span::styled(fmt_dur(summary.block_latency.p50), value),
        Span::styled("  p95  ", label),
        Span::styled(fmt_dur(summary.block_latency.p95), value),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    Latency p99  ", label),
        Span::styled(fmt_dur(summary.block_latency.p99), value),
        Span::styled("  max  ", label),
        Span::styled(fmt_dur(summary.block_latency.max), value),
    ]));
    if summary.flashblocks_latency.count > 0 {
        lines.push(Line::from(vec![
            Span::styled("    FB p50     ", label),
            Span::styled(fmt_dur(summary.flashblocks_latency.p50), value),
            Span::styled("  p90  ", label),
            Span::styled(fmt_dur(summary.flashblocks_latency.p90), value),
            Span::styled("  p99  ", label),
            Span::styled(fmt_dur(summary.flashblocks_latency.p99), value),
        ]));
    }
    lines.push(Line::from(""));

    if summary.gas.avg_gas > 0 {
        lines.push(Line::from(vec![
            Span::styled("    Avg gas    ", label),
            Span::styled(format!("{}", summary.gas.avg_gas), value),
            Span::styled("  Avg price  ", label),
            Span::styled(format!("{:.2} gwei", summary.gas.avg_gas_price as f64 / 1e9), value),
        ]));
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled("  Press ", dim),
        Span::styled("[b]", key),
        Span::styled(" to run again  ", dim),
        Span::styled("[c]", key),
        Span::styled(" for continuous", dim),
    ]));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_error_status(frame: &mut Frame<'_>, area: Rect, msg: &str) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  ✗ ERROR",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(format!("  {msg}"), Style::default().fg(Color::Red))),
        Line::from(""),
        Line::from(Span::styled("  Press [b] to try again", Style::default().fg(Color::DarkGray))),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_no_configs(frame: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .title(" Load Test ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled("Initializing…", Style::default().fg(Color::DarkGray))),
    ];

    let para = Paragraph::new(lines).alignment(Alignment::Center);

    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    frame.render_widget(para, vchunks[1]);
}

// ---------------------------------------------------------------------------
// Footer renderer
// ---------------------------------------------------------------------------

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &RunState, continuous: bool) {
    let key = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let desc = Style::default().fg(Color::DarkGray);
    let sep = Span::styled("  │  ", Style::default().fg(Color::DarkGray));
    let dim = Style::default().fg(Color::Rgb(60, 60, 60));

    let mut spans = vec![Span::styled("[Esc]", key), Span::raw(" "), Span::styled("back", desc)];

    match state {
        RunState::Running { .. } => {
            spans.push(sep.clone());
            spans.push(Span::styled("[s]", key));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("stop", desc));
            if continuous {
                spans.push(sep.clone());
                spans.push(Span::styled("∞ continuous", Style::default().fg(Color::Cyan)));
            }
        }
        _ => {
            spans.push(sep.clone());
            spans.push(Span::styled("[←/→]", key));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("select network", desc));

            spans.push(sep.clone());
            spans.push(Span::styled("[b]", key));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("begin", desc));

            spans.push(sep.clone());
            if continuous {
                spans.push(Span::styled(
                    "[c]",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw(" "));
                spans.push(Span::styled("continuous ON", Style::default().fg(Color::Cyan)));
            } else {
                spans.push(Span::styled("[c]", key));
                spans.push(Span::raw(" "));
                spans.push(Span::styled("continuous", desc));
            }

            spans.push(sep.clone());
            spans.push(Span::styled("[s]", dim));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("stop", dim));

            spans.push(sep.clone());
            spans.push(Span::styled("[t]", key));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("strategy", desc));

            spans.push(sep.clone());
            spans.push(Span::styled("[e]", key));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("edit config", desc));
        }
    }

    spans.push(sep);
    spans.push(Span::styled("[?]", key));
    spans.push(Span::raw(" "));
    spans.push(Span::styled("help", desc));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---------------------------------------------------------------------------
// Edit modal renderer
// ---------------------------------------------------------------------------

fn render_edit_modal(
    frame: &mut Frame<'_>,
    parent: Rect,
    modal_opt: &Option<EditModal>,
    view: &LoadTestView,
) {
    let Some(modal) = modal_opt else { return };

    // Center a fixed-size popup.
    let popup_w = 52u16.min(parent.width.saturating_sub(4));
    let popup_h = (EDIT_FIELDS.len() as u16 + 6).min(parent.height.saturating_sub(4));
    let x = parent.x + parent.width.saturating_sub(popup_w) / 2;
    let y = parent.y + parent.height.saturating_sub(popup_h) / 2;
    let popup = Rect { x, y, width: popup_w, height: popup_h };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Edit Configuration ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let label_style = Style::default().fg(Color::DarkGray);
    let value_style = Style::default().fg(Color::White);
    let selected_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let cursor_style = Style::default().fg(Color::Black).bg(Color::Yellow);
    let hint_style = Style::default().fg(Color::DarkGray);

    let mut lines: Vec<Line<'_>> = vec![Line::from("")];

    for (i, &field) in EDIT_FIELDS.iter().enumerate() {
        let is_selected = i == modal.field;
        let is_typing = is_selected && modal.typing;

        let selector = if is_selected { "▸ " } else { "  " };
        let selector_style =
            if is_selected { Style::default().fg(COLOR_BASE_BLUE) } else { label_style };

        let padded = format!("{field:<20}");
        let key_span = Span::styled(padded, if is_selected { selected_style } else { label_style });

        let val_span = if is_typing {
            let buf = modal.buf.as_str();
            let cursor = Span::styled(" ", cursor_style);
            // Show buffer with a trailing cursor block.
            Line::from(vec![
                Span::styled(selector, selector_style),
                key_span,
                Span::styled(buf, value_style),
                cursor,
            ])
        } else {
            let current = view.field_value(field);
            Line::from(vec![
                Span::styled(selector, selector_style),
                key_span,
                Span::styled(current, if is_selected { selected_style } else { value_style }),
            ])
        };

        lines.push(val_span);
    }

    lines.push(Line::from(""));
    if modal.typing {
        lines.push(Line::from(vec![Span::styled("  Enter confirm  Esc cancel", hint_style)]));
    } else {
        lines.push(Line::from(vec![Span::styled(
            "  ↑/↓ select  Enter edit  Esc close",
            hint_style,
        )]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Strategy modal renderer
// ---------------------------------------------------------------------------

fn render_strategy_modal(frame: &mut Frame<'_>, parent: Rect, modal: &StrategyModal) {
    let n = ALL_STRATEGIES.len();
    let popup_w = 46u16.min(parent.width.saturating_sub(4));
    let popup_h = (n as u16 + 5).min(parent.height.saturating_sub(4));
    let x = parent.x + parent.width.saturating_sub(popup_w) / 2;
    let y = parent.y + parent.height.saturating_sub(popup_h) / 2;
    let popup = Rect { x, y, width: popup_w, height: popup_h };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Strategy ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let label_style = Style::default().fg(Color::White);
    let dim_style = Style::default().fg(Color::DarkGray);
    let selected_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let check_on_style = Style::default().fg(COLOR_BASE_BLUE);
    let hint_style = Style::default().fg(Color::DarkGray);

    // Compute scroll offset so the cursor is always visible.
    let content_overhead: usize = 3; // blank top + blank before hint + hint line
    let visible_rows = (inner.height as usize).saturating_sub(content_overhead);
    let scroll = if modal.cursor >= visible_rows { modal.cursor - visible_rows + 1 } else { 0 };

    let mut lines: Vec<Line<'_>> = vec![Line::from("")];

    let end = (scroll + visible_rows).min(n);
    for (i, &strategy) in ALL_STRATEGIES.iter().enumerate().take(end).skip(scroll) {
        let is_selected = i == modal.cursor;
        let is_enabled = modal.enabled[i];

        let selector = if is_selected { "▸ " } else { "  " };
        let selector_style = if is_selected { selected_style } else { dim_style };
        let checkbox = if is_enabled { "[x] " } else { "[ ] " };
        let check_style = if is_enabled { check_on_style } else { dim_style };
        let label_sty = if is_selected { selected_style } else { label_style };

        lines.push(Line::from(vec![
            Span::styled(selector, selector_style),
            Span::styled(checkbox, check_style),
            Span::styled(strategy.label(), label_sty),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  ↑/↓ move  Space toggle  Enter confirm  Esc cancel",
        hint_style,
    )]));

    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn fmt_dur(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 { format!("{ms}ms") } else { format!("{:.2}s", d.as_secs_f64()) }
}

fn format_large_number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_large_number_f(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.2}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}K", n / 1_000.0)
    } else {
        format!("{n:.1}")
    }
}

fn format_wei_as_eth(wei_str: &str) -> String {
    wei_str.parse::<u128>().map_or_else(
        |_| wei_str.to_string(),
        |wei| {
            let eth = wei as f64 / 1e18;
            if eth >= 1.0 { format!("{eth:.4} ETH") } else { format!("{eth:.6} ETH") }
        },
    )
}

fn parse_eth_to_wei(input: &str) -> Option<u128> {
    let s = input.trim().trim_end_matches("ETH").trim_end_matches("eth").trim();
    let eth: f64 = s.parse().ok()?;
    if eth < 0.0 {
        return None;
    }
    Some((eth * 1e18) as u128)
}

fn format_tx_type(tx_type: &TxTypeConfig) -> String {
    match tx_type {
        TxTypeConfig::Transfer => "transfer".into(),
        TxTypeConfig::Calldata { max_size, .. } => format!("calldata (max {max_size}B)"),
        TxTypeConfig::Erc20 { contract } => {
            let addr = contract.as_str();
            let short = if addr.len() > 10 {
                format!("{}…{}", &addr[..6], &addr[addr.len() - 4..])
            } else {
                addr.to_string()
            };
            format!("erc20 ({short})")
        }
        TxTypeConfig::Precompile { target, iterations } => {
            let t = format_precompile_target(target);
            if *iterations > 1 {
                format!("precompile {t} ×{iterations}")
            } else {
                format!("precompile {t}")
            }
        }
        TxTypeConfig::Osaka { target } => {
            let t = match target {
                OsakaTarget::Clz => "clz",
                OsakaTarget::P256verifyOsaka => "p256verify",
                OsakaTarget::ModexpOsaka => "modexp",
            };
            format!("osaka {t}")
        }
    }
}

const fn format_precompile_target(target: &PrecompileTarget) -> &'static str {
    match target {
        PrecompileTarget::Ecrecover => "ecrecover",
        PrecompileTarget::Sha256 => "sha256",
        PrecompileTarget::Ripemd160 => "ripemd160",
        PrecompileTarget::Identity => "identity",
        PrecompileTarget::Modexp => "modexp",
        PrecompileTarget::Bn254Add => "bn254_add",
        PrecompileTarget::Bn254Mul => "bn254_mul",
        PrecompileTarget::Bn254Pairing => "bn254_pairing",
        PrecompileTarget::Blake2f { .. } => "blake2f",
        PrecompileTarget::KzgPointEvaluation => "kzg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_wei_as_eth_converts_correctly() {
        assert_eq!(format_wei_as_eth("10000000000000000"), "0.010000 ETH");
        assert_eq!(format_wei_as_eth("1000000000000000000"), "1.0000 ETH");
        assert_eq!(format_wei_as_eth("not_a_number"), "not_a_number");
    }

    #[test]
    fn format_large_number_scales() {
        assert_eq!(format_large_number(2_100_000), "2.10M");
        assert_eq!(format_large_number(21_000), "21.0K");
        assert_eq!(format_large_number(999), "999");
    }

    #[test]
    fn fmt_dur_formats_millis_and_seconds() {
        assert_eq!(fmt_dur(Duration::from_millis(245)), "245ms");
        assert_eq!(fmt_dur(Duration::from_millis(1200)), "1.20s");
    }
}
