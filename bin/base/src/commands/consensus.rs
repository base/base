//! Consensus-only node command.

use base_consensus_cli::{CliMetrics, ConsensusNodeArgs, ConsensusNodeConfigArgs};
use clap::Args;
use reth_cli_runner::CliRunner;

use crate::config::ResolvedChainConfig;

/// Arguments for `base consensus`.
#[derive(Args, Clone, Debug)]
pub(crate) struct ConsensusCommand {
    /// Consensus node arguments.
    #[command(flatten)]
    pub(crate) consensus: ConsensusNodeConfigArgs,
}

impl ConsensusCommand {
    /// Runs the `consensus` command.
    pub(crate) fn run(
        self,
        resolved_chain: ResolvedChainConfig,
        metrics_enabled: bool,
    ) -> eyre::Result<()> {
        let args = ConsensusNodeArgs::new(resolved_chain.consensus_chain_args(), self.consensus);
        let rollup_config = args.load_rollup_config()?;
        if metrics_enabled {
            CliMetrics::init_rollup_config(&rollup_config);
            CliMetrics::init_p2p(&args.config.p2p_flags);
        }

        CliRunner::try_default_runtime()?.run_until_ctrl_c(async move {
            let _upgrade_countdown_metrics = metrics_enabled
                .then(|| CliMetrics::spawn_upgrade_countdown_recorder(rollup_config.clone()));
            args.start_with_overrides(rollup_config, Default::default()).await
        })
    }
}
