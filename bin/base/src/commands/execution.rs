//! Execution-only node command.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{ExecutionNodeArgs, chainspec::chain_value_parser};
use clap::Args;
use reth_cli_runner::CliRunner;

use crate::config::ResolvedChainConfig;

/// Arguments for `base execution`.
#[derive(Args, Clone, Debug)]
#[command(
    mut_arg("builder_disallow", |arg| arg.hide(true).long("__builder-disallow-disabled")),
    mut_arg("sequencer", |arg| arg
        .hide(true)
        .long("__rollup-sequencer-disabled")
        .alias(None::<&'static str>)),
    mut_arg("sequencer_headers", |arg| arg
        .hide(true)
        .long("__rollup-sequencer-headers-disabled")
        .alias(None::<&'static str>))
)]
pub(crate) struct ExecutionCommand {
    /// Execution chain spec to use instead of the root chain selection.
    #[arg(long = "execution-chain", value_parser = chain_value_parser)]
    pub(crate) execution_chain: Option<Arc<BaseChainSpec>>,

    /// Execution node arguments.
    #[command(flatten)]
    pub(crate) execution: ExecutionNodeArgs,
}

impl ExecutionCommand {
    /// Runs the `execution` command.
    pub(crate) fn run(self, resolved_chain: ResolvedChainConfig) -> eyre::Result<()> {
        let chain = match self.execution_chain {
            Some(chain) => chain,
            None => resolved_chain.execution_chain_spec()?,
        };
        let execution = self.execution.into_launch_config(chain);

        CliRunner::try_default_runtime()?.run_command_until_exit(|ctx| async move {
            let handle = execution.launch_default(ctx).await?.handle;
            handle.node_exit_future.await
        })
    }
}
