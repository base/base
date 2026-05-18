//! Combined consensus and execution bootnode command.

use base_consensus_cli::{BootnodeP2PArgs, CliMetrics, L2ConfigFile};
use base_execution_cli::commands::p2p::bootnode::Command as ExecutionBootnodeCommand;
use clap::Args;
use eyre::WrapErr;
use reth_cli_runner::CliRunner;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::ResolvedChainConfig;

/// Arguments for `base bootnode`.
#[derive(Args, Clone, Debug)]
pub(crate) struct BootnodeCommand {
    /// L2 configuration file.
    #[clap(flatten)]
    pub(crate) l2_config: L2ConfigFile,

    /// Consensus bootnode P2P discovery arguments.
    #[command(flatten)]
    pub(crate) consensus: BootnodeP2PArgs,

    /// Execution bootnode discovery arguments.
    #[command(flatten)]
    pub(crate) execution: ExecutionBootnodeCommand,
}

impl BootnodeCommand {
    /// Runs both discovery-only bootnodes.
    pub(crate) fn run(self, resolved_chain: ResolvedChainConfig) -> eyre::Result<()> {
        let consensus_chain = resolved_chain.consensus_chain_args();
        let rollup_config = self.l2_config.load(&consensus_chain.l2_chain_id)?;

        CliMetrics::init_rollup_config(&rollup_config);
        CliMetrics::init_bootnode_p2p(&self.consensus);

        CliRunner::try_default_runtime()?.run_command_until_exit(|_| async move {
            let chain_id = rollup_config.l2_chain_id.id();
            self.consensus.check_ports()?;

            let mut consensus_bootnode =
                tokio::spawn(Self::run_consensus(self.consensus, chain_id));
            let mut execution_bootnode = tokio::spawn(Self::run_execution(self.execution));

            tokio::select! {
                result = &mut consensus_bootnode => {
                    warn!(layer = "consensus", "bootnode task exited");
                    if let Err(error) = Self::stop_task("execution", execution_bootnode).await {
                        warn!(error = %error, "failed to stop execution bootnode");
                    }
                    Self::task_result("consensus", result)
                }
                result = &mut execution_bootnode => {
                    warn!(layer = "execution", "bootnode task exited");
                    if let Err(error) = Self::stop_task("consensus", consensus_bootnode).await {
                        warn!(error = %error, "failed to stop consensus bootnode");
                    }
                    Self::task_result("execution", result)
                }
            }
        })
    }

    async fn run_consensus(consensus: BootnodeP2PArgs, chain_id: u64) -> eyre::Result<()> {
        let driver = consensus.discovery_driver(chain_id)?;
        let (handler, mut discovered_enrs) = driver.start();
        let local_enr = handler.local_enr().await.wrap_err("discovery service stopped")?;
        consensus.write_enr_output(&local_enr)?;

        info!(
            target: "rollup_node::bootnode",
            chain_id = chain_id,
            enr = %local_enr,
            "Consensus bootnode started"
        );
        CliMetrics::record_bootnode_up();

        while let Some(enr) = discovered_enrs.recv().await {
            debug!(
                target: "rollup_node::bootnode",
                peer_id = %enr.node_id(),
                enr = %enr,
                "Discovered consensus peer"
            );
        }

        warn!(target: "rollup_node::bootnode", "Discovery ENR stream closed");
        Ok(())
    }

    async fn run_execution(execution: ExecutionBootnodeCommand) -> eyre::Result<()> {
        execution.execute().await
    }

    async fn stop_task(
        layer: &'static str,
        task: JoinHandle<eyre::Result<()>>,
    ) -> eyre::Result<()> {
        task.abort();
        match task.await {
            Ok(result) => {
                result.wrap_err_with(|| format!("{layer} bootnode exited while stopping"))
            }
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(eyre::eyre!("{layer} bootnode task failed while stopping: {error}")),
        }
    }

    fn task_result(
        layer: &'static str,
        result: Result<eyre::Result<()>, tokio::task::JoinError>,
    ) -> eyre::Result<()> {
        match result {
            Ok(result) => result.wrap_err_with(|| format!("{layer} bootnode exited with an error")),
            Err(error) => Err(eyre::eyre!("{layer} bootnode task failed: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{cli::BaseCli, commands::BaseCommand, config::ChainArg};

    #[test]
    fn parses_bootnode_command() {
        let cli = BaseCli::parse_from(["base", "bootnode"]);

        assert!(matches!(cli.chain, ChainArg::BuiltIn(_)));
        let BaseCommand::Bootnode(bootnode) = cli.command else {
            panic!("expected bootnode command");
        };
        assert_eq!(bootnode.consensus.listen_tcp_port, 9222);
        assert_eq!(bootnode.execution.v4_addr.to_string(), "0.0.0.0:30301");
    }
}
