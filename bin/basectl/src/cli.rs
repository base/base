//! Contains the CLI arguments for the basectl binary.

use std::path::PathBuf;

use alloy_primitives::{Address, B256};
use basectl_cli::ViewId;
use clap::{Args, Parser, Subcommand, ValueEnum};
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
    /// When no hardcoded conductor list exists in the chain config, basectl
    /// asks this URL for the live raft membership. If omitted, basectl uses
    /// `discovery.bootstrap_rpc` from config.
    ///
    /// Applies to the conductor view, views that embed it, and non-TUI
    /// `basectl conductor` / `basectl sequencer` commands. Ignored by
    /// unrelated non-TUI subcommands.
    #[arg(long = "conductor-rpc", env = "BASECTL_CONDUCTOR_RPC", global = true)]
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
    /// Inspect and clear execution-layer txpool contents.
    Txpool {
        #[command(subcommand)]
        command: TxpoolCommands,
    },
    /// Inspect and control an HA conductor cluster.
    Conductor {
        #[command(subcommand)]
        command: ConductorCommands,
    },
    /// Inspect and control sequencer activity on HA conductor nodes.
    Sequencer {
        #[command(subcommand)]
        command: SequencerCommands,
    },
    /// Run read-only diagnostics for a single node.
    Doctor(DoctorArgs),
    /// Request and inspect ZK proofs on the internal prover service.
    Proofs {
        #[command(subcommand)]
        command: ProofsCommands,
    },
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

/// Prover-service proof request and inspection commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ProofsCommands {
    /// Submit a compressed ZK proof request for a block range to speed up finality.
    Finalize(ProofsFinalizeArgs),
    /// Show status and result data for a submitted proof request.
    Status(ProofsStatusArgs),
    /// List submitted proof requests.
    List(ProofsListArgs),
}

/// Flags for `basectl proofs finalize`.
#[derive(Debug, Args)]
pub(crate) struct ProofsFinalizeArgs {
    /// First L2 block number to prove.
    #[arg(value_name = "START_BLOCK")]
    pub(crate) start_block: u64,
    /// Number of consecutive L2 blocks to prove.
    #[arg(value_name = "NUM_BLOCKS", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) num_blocks: u64,
    /// Explicit proof session ID (prover-service idempotency key).
    ///
    /// If omitted, basectl derives a deterministic session ID from the
    /// network name and block range, so re-running the same command resolves
    /// to the existing prover-service session instead of enqueueing a
    /// duplicate proof.
    #[arg(long = "session-id", value_name = "ID")]
    pub(crate) session_id: Option<String>,
    /// L1 head hash used for witness generation.
    ///
    /// If omitted, the prover service picks one.
    #[arg(long = "l1-head", value_name = "HASH")]
    pub(crate) l1_head: Option<B256>,
    /// Sequencing window passed to the prover.
    #[arg(long = "sequence-window", value_name = "N")]
    pub(crate) sequence_window: Option<u64>,
    /// Intermediate output root interval passed to the prover.
    #[arg(long = "intermediate-root-interval", value_name = "N")]
    pub(crate) intermediate_root_interval: Option<u64>,
    /// Poll the prover service until the proof succeeds or fails.
    ///
    /// Exits non-zero when the proof fails or does not complete in time.
    #[arg(long)]
    pub(crate) wait: bool,
    /// Prover-service RPC URL (also `BASECTL_PROVER_RPC` or config `prover_rpc`).
    #[arg(long = "prover-rpc", env = "BASECTL_PROVER_RPC", value_name = "URL")]
    pub(crate) prover_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,
}

/// Flags for `basectl proofs status`.
#[derive(Debug, Args)]
pub(crate) struct ProofsStatusArgs {
    /// Proof session ID returned by `basectl proofs finalize`.
    #[arg(value_name = "SESSION_ID")]
    pub(crate) session_id: String,
    /// Prover-service RPC URL (also `BASECTL_PROVER_RPC` or config `prover_rpc`).
    #[arg(long = "prover-rpc", env = "BASECTL_PROVER_RPC", value_name = "URL")]
    pub(crate) prover_rpc: Option<Url>,
    /// Emit humanized JSON instead of pretty text.
    #[arg(long)]
    pub(crate) json: bool,
    /// With `--json`, emit the prover-service wire shape instead of the humanized summary.
    #[arg(long, requires = "json")]
    pub(crate) raw: bool,
}

/// Flags for `basectl proofs list`.
#[derive(Debug, Args)]
pub(crate) struct ProofsListArgs {
    /// Only list proofs with this status.
    #[arg(long, value_enum, value_name = "STATUS")]
    pub(crate) status: Option<ProofStatusFilter>,
    /// Number of rows to skip.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub(crate) offset: u64,
    /// Maximum rows to return.
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub(crate) limit: u32,
    /// Prover-service RPC URL (also `BASECTL_PROVER_RPC` or config `prover_rpc`).
    #[arg(long = "prover-rpc", env = "BASECTL_PROVER_RPC", value_name = "URL")]
    pub(crate) prover_rpc: Option<Url>,
    /// Emit humanized JSON instead of pretty text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Proof status filter accepted by `basectl proofs list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ProofStatusFilter {
    /// Proof request is queued.
    Queued,
    /// Proof request is running.
    Running,
    /// Proof request completed successfully.
    Succeeded,
    /// Proof request failed.
    Failed,
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

/// Transaction-pool inspection and destructive clearing commands.
#[derive(Debug, Subcommand)]
pub(crate) enum TxpoolCommands {
    /// Show pending txpool transactions.
    Pending(TxpoolReadArgs),
    /// Show queued txpool transactions.
    Queued(TxpoolReadArgs),
    /// Show pending and queued txpool transactions.
    All(TxpoolReadArgs),
    /// Clear the txpool or drop every transaction for one sender.
    Clear(TxpoolClearArgs),
}

/// Shared flags for read-only `basectl txpool` subcommands.
#[derive(Debug, Args)]
pub(crate) struct TxpoolReadArgs {
    /// Optional sender address to filter at the RPC layer.
    #[arg(value_name = "SENDER")]
    pub(crate) sender: Option<Address>,
    /// Override the execution-layer RPC URL.
    ///
    /// Defaults to the chain config's `rpc` field. Pass this flag to query a
    /// single node directly.
    #[arg(long = "el-rpc", value_name = "URL")]
    pub(crate) el_rpc: Option<Url>,
    /// Emit humanized JSON instead of pretty text.
    #[arg(long)]
    pub(crate) json: bool,
    /// With `--json`, emit the txpool wire shape instead of the humanized summary.
    #[arg(long, requires = "json")]
    pub(crate) raw: bool,
}

/// Flags for destructive `basectl txpool clear`.
#[derive(Debug, Args)]
pub(crate) struct TxpoolClearArgs {
    /// Sender address whose txpool transactions should be dropped.
    #[arg(long, value_name = "ADDRESS")]
    pub(crate) sender: Option<Address>,
    /// Override the execution-layer RPC URL.
    ///
    /// Defaults to the chain config's `rpc` field. Destructive txpool calls
    /// usually require an admin-enabled node RPC.
    #[arg(long = "el-rpc", value_name = "URL")]
    pub(crate) el_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,
}

/// HA conductor inspection and control commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ConductorCommands {
    /// Show current cluster status.
    Status(ConductorStatusArgs),
    /// Transfer raft leadership away from the current leader or to a target node.
    TransferLeader(ConductorLeaderArgs),
    /// Pause op-conductor's control loop on one node.
    Pause(ConductorNodeActionArgs),
    /// Resume op-conductor's control loop on one node.
    Unpause(ConductorNodeActionArgs),
    /// Pause op-conductor's control loop on every current raft member, falling
    /// back to the configured conductor list if static membership lookup is unavailable.
    PauseAll(ConductorClusterActionArgs),
    /// Resume op-conductor's control loop on every current raft member, falling
    /// back to the configured conductor list if static membership lookup is unavailable.
    UnpauseAll(ConductorClusterActionArgs),
}

/// Flags for `basectl conductor status`.
#[derive(Debug, Args)]
pub(crate) struct ConductorStatusArgs {
    /// Emit a structured JSON status summary instead of pretty text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Flags for `basectl conductor transfer-leader`.
#[derive(Debug, Args)]
pub(crate) struct ConductorLeaderArgs {
    /// Optional target node name. If omitted, the leader transfers to any available peer.
    #[arg(value_name = "TARGET")]
    pub(crate) target: Option<String>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,
}

/// Shared flags for single-node destructive `basectl conductor` commands.
#[derive(Debug, Args)]
pub(crate) struct ConductorNodeActionArgs {
    /// Conductor node name from the selected config or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub(crate) node: String,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,
}

/// Shared flags for cluster-wide destructive `basectl conductor` commands.
#[derive(Debug, Args)]
pub(crate) struct ConductorClusterActionArgs {
    /// Skip the typed network-name confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text. Requires `--yes`.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,
}

/// Sequencer inspection and control commands.
#[derive(Debug, Subcommand)]
pub(crate) enum SequencerCommands {
    /// Show sequencer state for every node or one selected node.
    Status(SequencerStatusArgs),
    /// Start sequencing on one node.
    Start(SequencerStartArgs),
    /// Stop sequencing on one node.
    Stop(SequencerNodeActionArgs),
}

/// Flags for `basectl sequencer status`.
#[derive(Debug, Args)]
pub(crate) struct SequencerStatusArgs {
    /// Optional node name from the selected config or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub(crate) node: Option<String>,
    /// Emit a structured JSON status summary instead of pretty text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Flags for `basectl sequencer start`.
#[derive(Debug, Args)]
pub(crate) struct SequencerStartArgs {
    /// Sequencer node name from the selected config or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub(crate) node: String,
    /// Unsafe head hash to pass to `admin_startSequencer`.
    ///
    /// If omitted, basectl uses the node's currently observed unsafe L2 hash.
    #[arg(value_name = "UNSAFE_HEAD")]
    pub(crate) unsafe_head: Option<String>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,
}

/// Flags for `basectl sequencer stop`.
#[derive(Debug, Args)]
pub(crate) struct SequencerNodeActionArgs {
    /// Sequencer node name from the selected config or discovered raft server ID.
    #[arg(value_name = "NODE")]
    pub(crate) node: String,
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

    fn try_parse<const N: usize>(args: [&str; N]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn destructive_p2p_json_requires_yes() {
        assert!(try_parse(["basectl", "p2p", "add-peer", "enr:example", "--json"]).is_err());
        assert!(try_parse(["basectl", "p2p", "ban", "16Uiu2HAmExamplePeerId", "--json",]).is_err());
        assert!(
            try_parse(["basectl", "p2p", "unban", "16Uiu2HAmExamplePeerId", "--json",]).is_err()
        );
        assert!(try_parse(["basectl", "p2p", "unban-all", "--json"]).is_err());
        assert!(
            try_parse([
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
            try_parse([
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
            try_parse([
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
            try_parse([
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
    fn txpool_commands_parse() {
        assert!(try_parse(["basectl", "txpool", "pending"]).is_ok());
        assert!(
            try_parse([
                "basectl",
                "txpool",
                "pending",
                "0x1111111111111111111111111111111111111111",
            ])
            .is_ok()
        );
        assert!(
            try_parse([
                "basectl",
                "txpool",
                "queued",
                "--el-rpc",
                "http://127.0.0.1:8545",
                "--json",
            ])
            .is_ok()
        );
        assert!(
            try_parse([
                "basectl",
                "txpool",
                "all",
                "0x1111111111111111111111111111111111111111",
                "--json",
                "--raw",
            ])
            .is_ok()
        );
        assert!(try_parse(["basectl", "txpool", "clear", "--yes"]).is_ok());
        assert!(
            try_parse([
                "basectl",
                "txpool",
                "clear",
                "--sender",
                "0x1111111111111111111111111111111111111111",
                "--yes",
                "--json",
            ])
            .is_ok()
        );
    }

    #[test]
    fn txpool_raw_requires_json() {
        assert!(try_parse(["basectl", "txpool", "pending", "--raw"]).is_err());
        assert!(
            try_parse([
                "basectl",
                "txpool",
                "queued",
                "0x1111111111111111111111111111111111111111",
                "--raw",
            ])
            .is_err()
        );
        assert!(try_parse(["basectl", "txpool", "all", "--json", "--raw"]).is_ok());
    }

    #[test]
    fn destructive_txpool_json_requires_yes() {
        assert!(try_parse(["basectl", "txpool", "clear", "--json"]).is_err());
        assert!(
            try_parse([
                "basectl",
                "txpool",
                "clear",
                "--sender",
                "0x1111111111111111111111111111111111111111",
                "--json",
            ])
            .is_err()
        );
        assert!(try_parse(["basectl", "txpool", "clear", "--yes", "--json"]).is_ok());
    }

    #[test]
    fn destructive_cl_p2p_commands_reject_el_rpc() {
        assert!(
            try_parse([
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
            try_parse([
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
            try_parse(["basectl", "p2p", "unban-all", "--el-rpc", "http://127.0.0.1:8545",])
                .is_err()
        );
    }

    #[test]
    fn conductor_commands_parse() {
        assert!(try_parse(["basectl", "conductor", "status", "--json"]).is_ok());
        assert!(
            try_parse([
                "basectl",
                "conductor",
                "transfer-leader",
                "op-conductor-1",
                "--yes",
                "--json",
            ])
            .is_ok()
        );
        assert!(try_parse(["basectl", "conductor", "pause", "op-conductor-0", "--yes",]).is_ok());
        assert!(try_parse(["basectl", "conductor", "unpause", "op-conductor-0", "--yes",]).is_ok());
        assert!(try_parse(["basectl", "conductor", "pause-all", "--yes", "--json",]).is_ok());
        assert!(try_parse(["basectl", "conductor", "unpause-all"]).is_ok());
    }

    #[test]
    fn sequencer_commands_parse() {
        assert!(try_parse(["basectl", "sequencer", "status", "--json"]).is_ok());
        assert!(try_parse(["basectl", "sequencer", "status", "op-conductor-0"]).is_ok());
        assert!(
            try_parse([
                "basectl",
                "sequencer",
                "start",
                "op-conductor-0",
                "0x1111111111111111111111111111111111111111111111111111111111111111",
                "--yes",
                "--json",
            ])
            .is_ok()
        );
        assert!(try_parse(["basectl", "sequencer", "stop", "op-conductor-0", "--yes",]).is_ok());
    }

    #[test]
    fn destructive_conductor_json_requires_yes() {
        assert!(try_parse(["basectl", "conductor", "pause", "op-conductor-0", "--json",]).is_err());
        assert!(try_parse(["basectl", "conductor", "transfer-leader", "--json"]).is_err());
        assert!(try_parse(["basectl", "conductor", "pause-all", "--json"]).is_err());
        assert!(try_parse(["basectl", "conductor", "unpause-all", "--json"]).is_err());
        assert!(try_parse(["basectl", "conductor", "pause-all", "--yes", "--json"]).is_ok());
        assert!(try_parse(["basectl", "conductor", "unpause-all", "--yes", "--json"]).is_ok());
    }

    #[test]
    fn proofs_commands_parse() {
        assert!(try_parse(["basectl", "proofs", "finalize", "100", "5", "--yes"]).is_ok());
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "finalize",
                "100",
                "5",
                "--session-id",
                "custom-session",
                "--l1-head",
                "0x1111111111111111111111111111111111111111111111111111111111111111",
                "--sequence-window",
                "3600",
                "--intermediate-root-interval",
                "10",
                "--wait",
                "--prover-rpc",
                "http://127.0.0.1:9000",
                "--yes",
                "--json",
            ])
            .is_ok()
        );
        assert!(try_parse(["basectl", "proofs", "status", "session-1"]).is_ok());
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "status",
                "session-1",
                "--prover-rpc",
                "http://127.0.0.1:9000",
                "--json",
                "--raw",
            ])
            .is_ok()
        );
        assert!(try_parse(["basectl", "proofs", "list"]).is_ok());
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "list",
                "--status",
                "succeeded",
                "--offset",
                "10",
                "--limit",
                "5",
                "--json",
            ])
            .is_ok()
        );
    }

    #[test]
    fn proofs_finalize_rejects_zero_blocks() {
        assert!(try_parse(["basectl", "proofs", "finalize", "100", "0", "--yes"]).is_err());
    }

    #[test]
    fn proofs_finalize_json_requires_yes() {
        assert!(try_parse(["basectl", "proofs", "finalize", "100", "5", "--json"]).is_err());
        assert!(
            try_parse(["basectl", "proofs", "finalize", "100", "5", "--yes", "--json"]).is_ok()
        );
    }

    #[test]
    fn proofs_status_raw_requires_json() {
        assert!(try_parse(["basectl", "proofs", "status", "session-1", "--raw"]).is_err());
        assert!(try_parse(["basectl", "proofs", "status", "session-1", "--json", "--raw"]).is_ok());
    }

    #[test]
    fn proofs_list_rejects_unknown_status() {
        assert!(try_parse(["basectl", "proofs", "list", "--status", "unknown"]).is_err());
    }

    #[test]
    fn destructive_sequencer_json_requires_yes() {
        assert!(try_parse(["basectl", "sequencer", "start", "op-conductor-0", "--json",]).is_err());
        assert!(try_parse(["basectl", "sequencer", "stop", "op-conductor-0", "--json",]).is_err());
        assert!(
            try_parse(["basectl", "sequencer", "start", "op-conductor-0", "--yes", "--json",])
                .is_ok()
        );
        assert!(
            try_parse(["basectl", "sequencer", "stop", "op-conductor-0", "--yes", "--json",])
                .is_ok()
        );
    }
}
