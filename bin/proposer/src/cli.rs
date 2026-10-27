//! CLI definition for the proposer binary.

use base_cli_utils::{EnvAlias, NetworkEnv};
use clap::Parser;
use eyre::WrapErr;
use reth_node_core::args::TraceArgs;

/// Canonical network env vars accepted in addition to the `BASE_PROPOSER_*` names.
pub(crate) const ENV_ALIASES: &[EnvAlias] = &[
    EnvAlias::new(NetworkEnv::L1_ETH_RPC, "BASE_PROPOSER_L1_ETH_RPC"),
    EnvAlias::new(NetworkEnv::L2_ETH_RPC, "BASE_PROPOSER_L2_ETH_RPC"),
    EnvAlias::new(NetworkEnv::ROLLUP_RPC, "BASE_PROPOSER_ROLLUP_RPC"),
    EnvAlias::new(NetworkEnv::PROVER_RPC, "BASE_PROPOSER_PROVER_RPC"),
];

/// Base Proposer.
#[derive(Parser)]
#[command(author, version)]
#[group(skip)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: base_proposer::Cli,

    #[command(flatten)]
    traces: TraceArgs,
}

impl Cli {
    /// Run the proposer service.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let config = base_proposer::ProposerConfig::from_cli(self.args)?;
        config.log.init_with_trace_args(&self.traces, &[])?;
        config
            .metrics
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
            })
            .wrap_err("failed to install Prometheus recorder")?;
        base_proposer::ProposerService::run(config).await
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn env_aliases_target_declared_env_vars() {
        let cmd = Cli::command();
        for alias in ENV_ALIASES {
            assert!(alias.is_declared_in(&cmd), "{} is not declared", alias.legacy);
        }
    }
}
