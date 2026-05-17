//! Top-level command dispatch for the unified Base binary.

use clap::Subcommand;

use crate::{
    commands::{bootnode::BootnodeCommand, rpc::RpcCommand},
    config::ResolvedChainConfig,
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
}

impl BaseCommand {
    /// Runs the selected top-level command.
    pub(crate) fn run(self, resolved_chain: ResolvedChainConfig) -> eyre::Result<()> {
        match self {
            Self::Bootnode(bootnode) => (*bootnode).run(resolved_chain),
            Self::Rpc(rpc) => (*rpc).run(resolved_chain),
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
