//! L1-free development chain command.

use std::sync::mpsc;

use alloy_rpc_types_engine::JwtSecret;
use base_common_chains::ChainConfig;
use base_consensus_node::StandaloneDevChain;
use base_execution_cli::ExecutionNodeConfigArgs;
use base_node_core::args::RollupArgs;
use base_node_runner::BaseNodeRunner;
use clap::Args;
use eyre::OptionExt;
use reth_cli_runner::CliRunner;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{commands::rpc::engine_ipc_url, config::ResolvedChainConfig};

/// Arguments for `base dev`.
#[derive(Args, Clone, Debug)]
#[command(
    mut_arg("datadir", |arg| arg
        .required(true)
        .default_value(None::<&'static str>)
        .visible_alias("dir")),
    mut_arg("addr", |arg| arg.default_value("127.0.0.1")),
    mut_arg("port", |arg| arg.default_value("0"))
)]
pub(crate) struct DevCommand {
    /// Execution node arguments.
    #[command(flatten)]
    pub(crate) execution: ExecutionNodeConfigArgs,
}

impl DevCommand {
    /// Runs the execution node and a standalone sequencer over an existing snapshot datadir.
    pub(crate) fn run(self, resolved_chain: ResolvedChainConfig) -> eyre::Result<()> {
        let rollup_config = ChainConfig::rollup_config_by_chain_id(resolved_chain.l2_chain_id)
            .ok_or_eyre("no built-in rollup config for this chain")?;
        let mut execution = self
            .execution
            .into_runtime_config(resolved_chain.execution_chain_spec()?)
            .with_unified_auth_endpoint();
        let node_config = &mut execution.node_config;
        node_config.rpc.http = true;
        node_config.rpc.ws = true;
        node_config.network.discovery.disable_discovery = true;
        node_config.network.max_inbound_peers = Some(0);
        node_config.network.max_outbound_peers = Some(0);
        let datadir = node_config.datadir();
        eyre::ensure!(
            datadir.db().join("mdbx.dat").is_file(),
            "{} has no execution database; `base dev` continues an existing snapshot datadir",
            datadir.data_dir().display()
        );
        let dev_chain = StandaloneDevChain::open(datadir.data_dir(), rollup_config)?;
        let engine_url = engine_ipc_url(execution.auth_ipc_path())?;
        // Graceful shutdown completes the runner before the command future returns, so a
        // consensus failure that triggers it is reported here.
        let (consensus_failure_tx, consensus_failure) = mpsc::channel();

        CliRunner::try_default_runtime()?.run_command_until_exit(|ctx| async move {
            let task_executor = ctx.task_executor.clone();
            info!(target: "base::dev", "starting execution node; database checks can take a while");
            let builder = execution.into_default_node_builder(ctx)?;
            let handle = BaseNodeRunner::new(RollupArgs::default()).launch(builder).await?.handle;
            let rpc = handle.node.rpc_server_handle();
            info!(
                target: "base::dev",
                http = ?rpc.http_local_addr(),
                ws = ?rpc.ws_local_addr(),
                "RPC listening"
            );
            // Keep the execution node handle alive until both services have coordinated shutdown.
            let execution_node = handle.node;
            let execution_exit = handle.node_exit_future;

            let consensus_cancellation = CancellationToken::new();
            // Engine API IPC does not authenticate, so the secret is unused.
            let consensus_exit =
                dev_chain.run(engine_url, JwtSecret::random(), consensus_cancellation.clone());
            tokio::pin!(execution_exit);
            tokio::pin!(consensus_exit);

            let result = tokio::select! {
                result = &mut execution_exit => {
                    consensus_cancellation.cancel();
                    let consensus_result = consensus_exit.await;
                    result?;
                    consensus_result.map_err(Into::into)
                }
                result = &mut consensus_exit => {
                    if let Err(error) = result {
                        let _ = consensus_failure_tx.send(error);
                    }
                    task_executor
                        .initiate_graceful_shutdown()
                        .map_err(|e| eyre::eyre!("failed to signal execution node shutdown: {e}"))?
                        .ignore_guard()
                        .await;
                    execution_exit.await
                }
            };

            drop(execution_node);
            result
        })?;
        consensus_failure.try_recv().map_or(Ok(()), |error| Err(error.into()))
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use clap::Parser;

    use crate::{cli::BaseCli, commands::BaseCommand};

    #[test]
    fn requires_only_datadir_and_binds_p2p_to_an_unused_loopback_port() {
        let cli = BaseCli::try_parse_from(["base", "dev", "--dir", "/tmp/dev data"]).unwrap();
        let BaseCommand::Dev(dev) = cli.command else {
            panic!("expected dev command");
        };
        assert_eq!(dev.execution.datadir.datadir.to_string(), "/tmp/dev data");
        assert_eq!(dev.execution.network.addr, Ipv4Addr::LOCALHOST);
        assert_eq!(dev.execution.network.port, 0);

        let error = BaseCli::try_parse_from(["base", "dev"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}
