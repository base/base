//! Contains the CLI arguments for the basectl binary.

use basectl_cli::ViewId;
use clap::{Parser, Subcommand};
use url::Url;

/// Base infrastructure control CLI.
#[derive(Debug, Parser)]
#[command(name = "basectl")]
#[command(about = "Base infrastructure control CLI")]
pub(crate) struct Cli {
    /// Chain configuration (mainnet, sepolia, devnet, or path to config file)
    #[arg(short = 'c', long = "config", default_value = "mainnet", global = true)]
    pub(crate) config: String,
    /// Bootstrap conductor JSON-RPC URL for runtime cluster discovery.
    ///
    /// When set, basectl ignores any hardcoded conductor list in the chain
    /// config and instead asks this URL for the live raft membership, then
    /// polls all discovered peers via templated ports.
    ///
    /// Only applies to the conductor view (and views that embed it, like the
    /// command center). Ignored by `flashblocks` and other non-TUI
    /// subcommands.
    #[arg(
        long = "conductor-rpc",
        env = "BASECTL_CONDUCTOR_RPC",
        global = true,
        default_value = "http://localhost:5545"
    )]
    pub(crate) conductor_rpc: Option<Url>,
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

/// Subcommands for the basectl CLI.
#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Open the interactive TUI monitor.
    Monitor {
        #[command(subcommand)]
        command: Option<MonitorCommands>,
    },
    /// Inspect a single L2 block.
    #[command(visible_alias = "b")]
    Block {
        /// Block number (decimal or 0x-hex), tag (latest/safe/finalized/earliest), or 32-byte block hash.
        #[arg(value_name = "REF")]
        reference: String,
        /// Emit JSON (humanized — decoded numbers, ISO + local timestamps) instead of the pretty table.
        #[arg(long)]
        json: bool,
        /// With `--json`, emit the JSON-RPC wire format (camelCase, hex-string quantities) instead of the humanized JSON.
        #[arg(long, requires = "json")]
        raw: bool,
    },
    /// Report combined CL `optimism_syncStatus` + EL `eth_syncing`.
    SyncStatus {
        /// Override the execution-layer RPC URL.
        ///
        /// Defaults to the chain config's `rpc` field, which on the
        /// `mainnet` and `sepolia` presets resolves to the public proxyd
        /// fleet — `eth_syncing` against that always reports "not syncing"
        /// because proxyd routes only-healthy backends. Pass this flag to
        /// point at a single node.
        #[arg(long = "el-rpc", value_name = "URL")]
        el_rpc: Option<Url>,
        /// Override the consensus-node RPC URL.
        ///
        /// The mainnet and sepolia presets ship `consensus_node_rpc` unset, so
        /// non-devnet users must pass this flag (or set the field in their YAML
        /// config).
        #[arg(long = "cl-rpc", value_name = "URL")]
        cl_rpc: Option<Url>,
        /// Block tolerance for the tip-reference `caught_up` classification.
        ///
        /// The local node is reported as `caught_up` when within ±this many
        /// blocks of the public reference. Beyond the window, status flips
        /// to `behind` or `ahead`. Default 5 ≈ ~10s of network jitter at
        /// Base's 2s block time. Lower the value for stricter alerting,
        /// raise it to dampen noise on flaky networks.
        #[arg(long = "tip-tolerance", value_name = "BLOCKS", default_value_t = 5)]
        tip_tolerance: u64,
        /// Emit JSON (humanized — decoded numbers, ISO + local timestamps,
        /// precomputed `safeLag*`) instead of the pretty table.
        #[arg(long)]
        json: bool,
        /// With `--json`, emit the JSON-RPC wire format (the alloy-typed
        /// `optimism_syncStatus` response) instead of the humanized JSON.
        #[arg(long, requires = "json")]
        raw: bool,
    },
    /// Stream flashblocks as JSON lines.
    #[command(after_help = "Use `basectl monitor flashblocks` for the TUI.")]
    Flashblocks,
}

/// TUI monitor views.
#[derive(Debug, Subcommand)]
pub(crate) enum MonitorCommands {
    /// Chain configuration operations
    #[command(visible_alias = "c")]
    Config,
    /// Flashblocks monitor
    #[command(visible_alias = "f")]
    Flashblocks,
    /// DA (Data Availability) backlog monitor
    #[command(visible_alias = "d")]
    Da,
    /// Command center (combined view)
    #[command(visible_alias = "cc")]
    CommandCenter,
    /// HA conductor cluster monitor
    #[command(visible_alias = "co")]
    Conductor,
    /// Kubernetes pod monitor
    #[command(visible_alias = "po")]
    Pods,
    /// Network upgrade activation countdown and history
    #[command(visible_alias = "u")]
    Upgrades,
}

impl MonitorCommands {
    pub(crate) const fn view_id(&self) -> ViewId {
        match self {
            Self::Config => ViewId::Config,
            Self::Flashblocks => ViewId::Flashblocks,
            Self::Da => ViewId::DaMonitor,
            Self::CommandCenter => ViewId::CommandCenter,
            Self::Conductor => ViewId::Conductor,
            Self::Pods => ViewId::Pods,
            Self::Upgrades => ViewId::Upgrades,
        }
    }
}
