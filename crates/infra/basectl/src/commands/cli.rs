//! CLI arguments and subcommands for basectl.

use anyhow::{Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use url::Url;

use super::{
    BlockCommand, CommandOutcome, ConductorCommand, DoctorCommand, P2pCommand, ProofsCommand,
    SequencerCommand, SyncStatusCommand, TxpoolCommand,
};
use crate::{MonitoringConfig, ViewId, run_app, run_flashblocks_json};

/// Base infrastructure control CLI.
#[derive(Debug, Parser)]
#[command(name = "basectl")]
#[command(about = "Base infrastructure control CLI")]
pub struct Cli {
    /// Chain configuration (mainnet, sepolia, devnet, or path to config file)
    #[arg(short = 'c', long = "config", default_value = "mainnet", global = true)]
    pub config: String,
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
    pub conductor_rpc: Option<Url>,
    /// Command to run.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Subcommands for the basectl CLI.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Open the interactive TUI monitor.
    Monitor {
        /// Monitor view to open.
        #[command(subcommand)]
        command: Option<MonitorCommands>,
    },
    /// Inspect a single L2 block.
    #[command(visible_alias = "b")]
    Block(BlockCommand),
    /// Report combined CL `optimism_syncStatus` + EL `eth_syncing`.
    SyncStatus(SyncStatusCommand),
    /// Inspect p2p peers and advertised endpoints.
    P2p(P2pCommand),
    /// Inspect and clear execution-layer txpool contents.
    Txpool(TxpoolCommand),
    /// Inspect and control an HA conductor cluster.
    Conductor(ConductorCommand),
    /// Inspect and control sequencer activity on HA conductor nodes.
    Sequencer(SequencerCommand),
    /// Run read-only diagnostics for a single node.
    Doctor(DoctorCommand),
    /// Request and inspect ZK proofs on the internal prover service.
    Proofs(ProofsCommand),
    /// Stream flashblocks as JSON lines.
    #[command(after_help = "Use `basectl monitor flashblocks` for the TUI.")]
    Flashblocks,
}

/// TUI monitor views.
#[derive(Debug, Subcommand)]
pub enum MonitorCommands {
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

impl Cli {
    /// Returns whether this invocation renders to the terminal directly (the
    /// TUI monitor, or bare `basectl` help), where a stderr tracing
    /// subscriber would corrupt or pollute the display.
    pub const fn is_tui(&self) -> bool {
        matches!(self.command, Some(Commands::Monitor { .. }) | None)
    }

    /// Runs the parsed command and returns its process outcome.
    pub async fn run(self) -> Result<CommandOutcome> {
        let conductor_rpc = self.conductor_rpc;
        let command = match self.command {
            Some(Commands::Monitor { command }) => {
                let view = command.map(|command| command.view_id()).unwrap_or(ViewId::Home);
                run_app(view, &self.config, conductor_rpc).await?;
                return Ok(CommandOutcome::Success);
            }
            None => {
                Self::command().print_help()?;
                return Ok(CommandOutcome::Success);
            }
            Some(command) => command,
        };
        let config = MonitoringConfig::load(&self.config).await?;
        match command {
            Commands::Block(command) => command.run(config).await.map(|()| CommandOutcome::Success),
            Commands::SyncStatus(command) => {
                command.run(config).await.map(|()| CommandOutcome::Success)
            }
            Commands::P2p(command) => command.run(config).await,
            Commands::Txpool(command) => {
                command.run(config).await.map(|()| CommandOutcome::Success)
            }
            Commands::Conductor(command) => command.run(config, conductor_rpc).await,
            Commands::Sequencer(command) => {
                command.run(config, conductor_rpc).await.map(|()| CommandOutcome::Success)
            }
            Commands::Proofs(command) => command.run(config).await,
            Commands::Doctor(command) => command.run(config).await,
            Commands::Flashblocks => {
                run_flashblocks_json(config).await.map(|()| CommandOutcome::Success)
            }
            // Handled by the pre-load match above; the compiler cannot narrow the type.
            Commands::Monitor { .. } => bail!("monitor reached post-load dispatch"),
        }
    }
}

impl MonitorCommands {
    /// Returns the TUI view selected by this command.
    pub const fn view_id(&self) -> ViewId {
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
    use clap::{CommandFactory, Parser};

    use super::Cli;
    use crate::{Commands, ProofsCommands, ZkBackendOption};

    fn try_parse<const N: usize>(args: [&str; N]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn block_alias_parses() {
        assert!(try_parse(["basectl", "b", "latest"]).is_ok());
    }

    #[test]
    fn monitor_aliases_parse() {
        for alias in ["c", "f", "d", "cc", "co", "po", "u"] {
            assert!(try_parse(["basectl", "monitor", alias]).is_ok(), "alias: {alias}");
        }
    }

    #[test]
    fn flashblocks_help_points_to_monitor() {
        let help = Cli::command()
            .find_subcommand_mut("flashblocks")
            .expect("flashblocks command")
            .render_long_help()
            .to_string();

        assert!(help.contains("Use `basectl monitor flashblocks` for the TUI."));
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
                "enode://example@127.0.0.1:30303",
                "--el-rpc",
                "http://127.0.0.1:8545",
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
    fn p2p_reachability_parses() {
        assert!(try_parse(["basectl", "p2p", "reachability", "enode://example", "--json"]).is_ok());
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
    fn unban_all_rejects_el_rpc() {
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
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "protocol",
                "--l1-rpc",
                "http://127.0.0.1:8545",
                "--factory",
                "0xffffffffffffffffffffffffffffffffffffffff",
            ])
            .is_ok()
        );
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "protocol",
                "--game",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--json",
            ])
            .is_ok()
        );
    }

    #[test]
    fn proofs_games_parse() {
        assert!(try_parse(["basectl", "proofs", "games"]).is_ok());
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "games",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--factory",
                "0xffffffffffffffffffffffffffffffffffffffff",
                "--l1-rpc",
                "http://127.0.0.1:8545",
                "--json",
            ])
            .is_ok()
        );
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "games",
                "--limit",
                "5",
                "--game-type",
                "3",
                "--missing-zk",
            ])
            .is_ok()
        );
    }

    #[test]
    fn proofs_propose_parse() {
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "propose",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--prover-address",
                "0xdddddddddddddddddddddddddddddddddddddddd",
            ])
            .is_ok()
        );
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "propose",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--prover-address",
                "0xdddddddddddddddddddddddddddddddddddddddd",
                "--zk-backend",
                "cluster",
                "--session-id",
                "custom-session",
                "--intermediate-root-interval",
                "100",
                "--wait",
                "--prover-rpc",
                "http://127.0.0.1:9000",
                "--factory",
                "0xffffffffffffffffffffffffffffffffffffffff",
                "--l1-rpc",
                "http://127.0.0.1:8545",
                "--yes",
                "--json",
            ])
            .is_ok()
        );
    }

    #[test]
    fn proofs_propose_rejects_bad_inputs() {
        // Game address and prover address are both required.
        assert!(try_parse(["basectl", "proofs", "propose"]).is_err());
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "propose",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ])
            .is_err()
        );
        // JSON output requires --yes so scripts do not hang on the prompt.
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "propose",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--prover-address",
                "0xdddddddddddddddddddddddddddddddddddddddd",
                "--json",
            ])
            .is_err()
        );
    }

    #[test]
    fn proofs_submit_parse() {
        // The submitter key comes from a key file or the environment at
        // runtime, never from a command-line value.
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "submit",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ])
            .is_ok()
        );
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "submit",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--private-key-file",
                "/tmp/submitter.key",
                "--session-id",
                "custom-session",
                "--zk-backend",
                "cluster",
                "--wait",
                "--prover-rpc",
                "http://127.0.0.1:9000",
                "--factory",
                "0xffffffffffffffffffffffffffffffffffffffff",
                "--l1-rpc",
                "http://127.0.0.1:8545",
                "--yes",
                "--json",
            ])
            .is_ok()
        );
    }

    #[test]
    fn proofs_submit_rejects_bad_inputs() {
        // Game address is required.
        assert!(try_parse(["basectl", "proofs", "submit"]).is_err());
        // JSON output requires --yes so scripts do not hang on the prompt.
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "submit",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--json",
            ])
            .is_err()
        );
    }

    #[test]
    fn proofs_finalize_parse() {
        // The submitter key comes from a key file or the environment at
        // runtime, never from a command-line value.
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "finalize",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ])
            .is_ok()
        );
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "finalize",
                "0x9999999999999999999999999999999999999999999999999999999999999999",
            ])
            .is_ok()
        );
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "finalize",
                "0x9999999999999999999999999999999999999999999999999999999999999999",
                "--private-key-file",
                "/tmp/submitter.key",
                "--zk-backend",
                "cluster",
                "--session-id",
                "custom-session",
                "--intermediate-root-interval",
                "100",
                "--prover-rpc",
                "http://127.0.0.1:9000",
                "--factory",
                "0xffffffffffffffffffffffffffffffffffffffff",
                "--l1-rpc",
                "http://127.0.0.1:8545",
                "--yes",
                "--json",
            ])
            .is_ok()
        );
    }

    #[test]
    fn proofs_finalize_rejects_bad_inputs() {
        // Game/transaction target is required.
        assert!(try_parse(["basectl", "proofs", "finalize"]).is_err());
        // Target must be a 20-byte game address or 32-byte transaction hash.
        assert!(try_parse(["basectl", "proofs", "finalize", "not-a-hash"]).is_err());
        // JSON output requires --yes so scripts do not hang on the prompt.
        assert!(
            try_parse([
                "basectl",
                "proofs",
                "finalize",
                "0x9999999999999999999999999999999999999999999999999999999999999999",
                "--json",
            ])
            .is_err()
        );
    }

    #[test]
    fn proofs_games_rejects_bad_inputs() {
        // Limit must be within 1..=100.
        assert!(try_parse(["basectl", "proofs", "games", "--limit", "0"]).is_err());
        assert!(try_parse(["basectl", "proofs", "games", "--limit", "101"]).is_err());
        // Game address must be a 20-byte hex address.
        assert!(try_parse(["basectl", "proofs", "games", "not-an-address"]).is_err());
        // List-only filters conflict with inspecting a single game.
        let game = "0x1111111111111111111111111111111111111111";
        assert!(try_parse(["basectl", "proofs", "games", game, "--limit", "5"]).is_err());
        assert!(try_parse(["basectl", "proofs", "games", game, "--game-type", "3"]).is_err());
        assert!(try_parse(["basectl", "proofs", "games", game, "--missing-zk"]).is_err());
    }

    #[test]
    fn proofs_finalize_defaults_to_network_backend() {
        let cli = try_parse([
            "basectl",
            "proofs",
            "finalize",
            "0x9999999999999999999999999999999999999999999999999999999999999999",
        ])
        .expect("finalize should parse");
        match cli.command {
            Some(Commands::Proofs(crate::ProofsCommand {
                command: ProofsCommands::Finalize(args),
            })) => {
                assert_eq!(args.zk_backend, ZkBackendOption::Network);
            }
            other => panic!("unexpected command: {other:?}"),
        }
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
