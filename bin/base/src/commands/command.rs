//! Top-level command dispatch for the unified Base binary.

use clap::Subcommand;

use crate::{
    commands::{
        bootnode::BootnodeCommand, rpc::RpcCommand, sequencer::SequencerCommand,
        update::UpdateCommand,
    },
    config::ChainResolver,
};

/// Top-level commands for `base`.
#[derive(Subcommand, Clone, Debug)]
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
}

impl BaseCommand {
    /// Runs the selected top-level command.
    pub(crate) fn run(self, chain_resolver: ChainResolver) -> eyre::Result<()> {
        match self {
            Self::Bootnode(bootnode) => (*bootnode).run(chain_resolver.resolve()?),
            Self::Rpc(rpc) => (*rpc).run(chain_resolver.resolve()?),
            Self::Sequencer(sequencer) => (*sequencer).run(chain_resolver.resolve()?),
            Self::Update(update) => (*update).run(),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::BaseCli;

    #[test]
    fn rejects_legacy_node_rpc_path() {
        let err = BaseCli::try_parse_from(["base", "node", "rpc"]).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("node"));
    }
}
