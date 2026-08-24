//! Top-level command dispatch for the unified Base binary.

use base_execution_cli::{chainspec::BaseChainSpecParser, commands::base_proofs};
use base_node_core::BaseNode;
use clap::Subcommand;
use reth_cli_runner::CliRunner;

use crate::{
    cli::TelemetryArgs,
    commands::{
        bootnode::BootnodeCommand, reth::RethCommand, rpc::RpcCommand, sequencer::SequencerCommand,
        snapshot::SnapshotCommand, telemetry::TelemetryCommand, update::UpdateCommand,
    },
    config::ChainResolver,
};

/// Top-level commands for `base`.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub(crate) enum BaseCommand {
    /// Run consensus and execution discovery-only bootnodes.
    #[command(name = "bootnode")]
    Bootnode(Box<BootnodeCommand>),
    /// Run the integrated node in RPC mode.
    #[command(name = "rpc")]
    Rpc(Box<RpcCommand>),
    /// Run integrated execution, builder, and consensus services in sequencer mode.
    #[command(name = "sequencer")]
    Sequencer(Box<SequencerCommand>),
    /// Update the base binary to the latest release.
    #[command(name = "update")]
    Update(Box<UpdateCommand>),
    /// Execution-layer maintenance utilities (use this group's own --chain flag).
    #[command(name = "reth")]
    Reth(Box<RethCommand>),
    /// Manage storage of historical proofs in the fault-proof window (uses its own --chain flag).
    #[command(name = "proofs")]
    Proofs(Box<base_proofs::Command<BaseChainSpecParser>>),
    /// Snapshot manifest generation and download utilities (uses its own --chain flag).
    #[command(name = "snapshot")]
    Snapshot(Box<SnapshotCommand>),
    /// Inspect what this node would report to Base telemetry.
    #[command(name = "telemetry")]
    Telemetry(Box<TelemetryCommand>),
}

impl BaseCommand {
    pub(crate) fn run(
        self,
        chain_resolver: ChainResolver,
        metrics_enabled: bool,
        telemetry: TelemetryArgs,
    ) -> eyre::Result<()> {
        match self {
            Self::Bootnode(bootnode) => (*bootnode).run(chain_resolver.resolve()?, metrics_enabled),
            Self::Rpc(rpc) => (*rpc).run(chain_resolver.resolve()?, metrics_enabled, telemetry),
            Self::Sequencer(sequencer) => {
                (*sequencer).run(chain_resolver.resolve()?, metrics_enabled, telemetry)
            }
            Self::Update(update) => (*update).run(),
            Self::Reth(reth) => {
                chain_resolver.reject_for_reth_command("base reth")?;
                (*reth).run()
            }
            Self::Proofs(command) => {
                chain_resolver.reject_for_reth_command("base proofs")?;
                let runner = CliRunner::try_default_runtime()?;
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c((*command).execute::<BaseNode>(runtime))
            }
            Self::Snapshot(snapshot) => {
                chain_resolver.reject_for_reth_command("base snapshot")?;
                (*snapshot).run()
            }
            Self::Telemetry(command) => {
                (*command).run(chain_resolver.resolve()?, metrics_enabled, telemetry)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{
        cli::{BaseCli, TelemetryArgs},
        config::ChainResolver,
    };

    #[test]
    fn rejects_legacy_node_rpc_path() {
        let err = BaseCli::try_parse_from(["base", "node", "rpc"]).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("node"));
    }

    #[test]
    fn rejects_legacy_flat_db_path() {
        let err = BaseCli::try_parse_from(["base", "db", "--help"]).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("db"));
    }

    #[test]
    fn rejects_legacy_flat_snapshot_manifest_path() {
        let err = BaseCli::try_parse_from(["base", "snapshot-manifest", "--help"]).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("snapshot-manifest"));
    }

    #[test]
    fn accepts_reth_help() {
        let err = BaseCli::try_parse_from(["base", "reth", "--help"]).unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn accepts_snapshot_help() {
        let err = BaseCli::try_parse_from(["base", "snapshot", "--help"]).unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn rejects_top_level_chain_for_reth_subcommands() {
        let cli =
            BaseCli::try_parse_from(["base", "--chain", "sepolia", "reth", "db", "stats"]).unwrap();
        let err = cli
            .command
            .run(ChainResolver::new(cli.chain), false, TelemetryArgs::default())
            .unwrap_err();

        assert!(err.to_string().contains("base reth"));
        assert!(err.to_string().contains("base --chain"));
    }
}
