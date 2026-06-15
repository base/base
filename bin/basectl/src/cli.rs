//! Contains the CLI arguments for the basectl binary.

use std::path::PathBuf;

use basectl_cli::ViewId;
use clap::{Args, Parser, Subcommand};
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
    /// Inspect p2p peers and advertised endpoints.
    P2p {
        #[command(subcommand)]
        command: P2pCommands,
    },
    /// Run read-only diagnostics for a single node.
    Doctor(DoctorArgs),
    /// Stream flashblocks as JSON lines.
    #[command(after_help = "Use `basectl monitor flashblocks` for the TUI.")]
    Flashblocks,
}

/// Flags for `basectl doctor`.
#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    /// Override the execution-layer RPC URL.
    ///
    /// Defaults to the chain config's `rpc` field. Pass this flag to diagnose
    /// a specific node instead of a public preset RPC.
    #[arg(long = "el-rpc", value_name = "URL")]
    pub(crate) el_rpc: Option<Url>,
    /// Override the consensus-node RPC URL.
    ///
    /// If omitted and the selected config has no `consensus_node_rpc`, CL
    /// checks are skipped with hints while EL/L1/config checks still run.
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub(crate) cl_rpc: Option<Url>,
    /// Path to the local `reth.toml` file.
    #[arg(long = "reth-config", value_name = "PATH")]
    pub(crate) reth_config: Option<PathBuf>,
    /// Connected peer count below which peer checks warn.
    #[arg(long = "peer-warn-threshold", value_name = "COUNT", default_value_t = 5)]
    pub(crate) peer_warn_threshold: u32,
    /// EL head lag above which `el_head_vs_tip` warns.
    #[arg(long = "head-lag-warn-blocks", value_name = "BLOCKS", default_value_t = 10)]
    pub(crate) head_lag_warn_blocks: u64,
    /// EL head lag above which `el_head_vs_tip` fails.
    #[arg(long = "head-lag-fail-blocks", value_name = "BLOCKS", default_value_t = 20)]
    pub(crate) head_lag_fail_blocks: u64,
    /// Safe-head lag above which `safe_head_recency` warns.
    #[arg(long = "safe-recency-warn-blocks", value_name = "BLOCKS", default_value_t = 150)]
    pub(crate) safe_recency_warn_blocks: u64,
    /// Safe-head lag above which `safe_head_recency` fails.
    #[arg(long = "safe-recency-fail-blocks", value_name = "BLOCKS", default_value_t = 300)]
    pub(crate) safe_recency_fail_blocks: u64,
    /// Emit a humanized JSON report instead of pretty text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// P2P inspection and peer-management commands.
#[derive(Debug, Subcommand)]
pub(crate) enum P2pCommands {
    /// List connected peers per layer.
    Peers(P2pArgs),
    /// Show advertised endpoints and peer-count summary per layer.
    Info(P2pArgs),
    /// Add a single execution or consensus peer.
    AddPeer(DestructivePeerArgs),
    /// Remove a single execution or consensus peer.
    RemovePeer(DestructivePeerArgs),
    /// Ban a single consensus peer.
    Ban(DestructiveClPeerArgs),
    /// Unban a single consensus peer.
    Unban(DestructiveClPeerArgs),
    /// Unban all currently banned consensus peers.
    UnbanAll(DestructiveClBulkArgs),
}

/// Shared flags for the read-only `basectl p2p` subcommands.
#[derive(Debug, Args)]
pub(crate) struct P2pArgs {
    /// Override the execution-layer RPC URL.
    ///
    /// Defaults to the chain config's `rpc` field, which on the
    /// `mainnet` and `sepolia` presets resolves to the public proxyd
    /// fleet. Pass this flag to query a single node directly.
    #[arg(long = "el-rpc", value_name = "URL")]
    pub(crate) el_rpc: Option<Url>,
    /// Override the consensus-node RPC URL.
    ///
    /// The mainnet and sepolia presets ship `consensus_node_rpc` unset,
    /// so non-devnet users must pass this flag (or set the field in
    /// their YAML config).
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub(crate) cl_rpc: Option<Url>,
    /// Emit JSON instead of the pretty table output.
    #[arg(long)]
    pub(crate) json: bool,
    /// With `--json`, emit raw RPC wire shapes instead of the humanized summary.
    #[arg(long, requires = "json")]
    pub(crate) raw: bool,
}

/// Shared flags for destructive `basectl p2p` subcommands.
#[derive(Debug, Args)]
pub(crate) struct DestructivePeerArgs {
    /// Peer target. `enode://...` routes to EL; CL uses ENR or multiaddr for add and peer ID for remove.
    #[arg(value_name = "TARGET")]
    pub(crate) target: String,
    /// Override the execution-layer RPC URL.
    ///
    /// Defaults to the chain config's `rpc` field, which on the
    /// `mainnet` and `sepolia` presets resolves to the public proxyd
    /// fleet. Pass this flag to query a single node directly.
    #[arg(long = "el-rpc", value_name = "URL")]
    pub(crate) el_rpc: Option<Url>,
    /// Override the consensus-node RPC URL.
    ///
    /// The mainnet and sepolia presets ship `consensus_node_rpc` unset,
    /// so non-devnet users must pass this flag (or set the field in
    /// their YAML config).
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub(crate) cl_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,
}

/// Shared flags for destructive consensus-only `basectl p2p` peer subcommands.
#[derive(Debug, Args)]
pub(crate) struct DestructiveClPeerArgs {
    /// Consensus libp2p peer ID.
    #[arg(value_name = "PEER_ID")]
    pub(crate) peer_id: String,
    /// Override the consensus-node RPC URL.
    ///
    /// The mainnet and sepolia presets ship `consensus_node_rpc` unset,
    /// so non-devnet users must pass this flag (or set the field in
    /// their YAML config).
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub(crate) cl_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,
}

/// Shared flags for destructive consensus-only `basectl p2p` bulk subcommands.
#[derive(Debug, Args)]
pub(crate) struct DestructiveClBulkArgs {
    /// Override the consensus-node RPC URL.
    ///
    /// The mainnet and sepolia presets ship `consensus_node_rpc` unset,
    /// so non-devnet users must pass this flag (or set the field in
    /// their YAML config).
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub(crate) cl_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn destructive_p2p_json_requires_yes() {
        assert!(
            Cli::try_parse_from(["basectl", "p2p", "add-peer", "enr:example", "--json"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["basectl", "p2p", "ban", "16Uiu2HAmExamplePeerId", "--json",])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["basectl", "p2p", "unban", "16Uiu2HAmExamplePeerId", "--json",])
                .is_err()
        );
        assert!(Cli::try_parse_from(["basectl", "p2p", "unban-all", "--json"]).is_err());
        assert!(
            Cli::try_parse_from([
                "basectl",
                "p2p",
                "remove-peer",
                "16Uiu2HAmExamplePeerId",
                "--json",
                "--yes",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "basectl",
                "p2p",
                "ban",
                "16Uiu2HAmExamplePeerId",
                "--cl-rpc",
                "http://127.0.0.1:9545",
                "--json",
                "--yes",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "basectl",
                "p2p",
                "unban",
                "16Uiu2HAmExamplePeerId",
                "--cl-rpc",
                "http://127.0.0.1:9545",
                "--json",
                "--yes",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "basectl",
                "p2p",
                "unban-all",
                "--cl-rpc",
                "http://127.0.0.1:9545",
                "--json",
                "--yes",
            ])
            .is_ok()
        );
    }

    #[test]
    fn destructive_cl_p2p_commands_reject_el_rpc() {
        assert!(
            Cli::try_parse_from([
                "basectl",
                "p2p",
                "ban",
                "16Uiu2HAmExamplePeerId",
                "--el-rpc",
                "http://127.0.0.1:8545",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "basectl",
                "p2p",
                "unban",
                "16Uiu2HAmExamplePeerId",
                "--el-rpc",
                "http://127.0.0.1:8545",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "basectl",
                "p2p",
                "unban-all",
                "--el-rpc",
                "http://127.0.0.1:8545",
            ])
            .is_err()
        );
    }
}
