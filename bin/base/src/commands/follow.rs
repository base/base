//! Integrated follow-mode node command.
//!
//! Runs an execution node (reth) and a consensus follow node in a single process. The follow node
//! reads canonical payloads from `--source-l2-rpc` and inserts them into the embedded execution
//! node over its engine IPC socket, gating on Proofs `ExEx` progress when `--proofs` is set. This
//! is the unified-binary equivalent of the standalone `base-consensus follow` command.

use std::sync::Arc;

use base_consensus_cli::{
    CliMetrics, ConsensusFollowNodeArgs, ConsensusFollowNodeConfigArgs,
    EmbeddedConsensusFollowNodeConfigArgs, FollowNodeOverrides,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{ExecutionNodeArgs, chainspec::chain_value_parser};
use clap::Args;
use reth_cli_runner::CliRunner;
use tokio_util::sync::CancellationToken;

use crate::{commands::rpc::engine_ipc_url, config::ResolvedChainConfig};

/// Arguments for `base follow`.
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
pub(crate) struct FollowCommand {
    /// Execution chain spec to use instead of the root chain selection.
    #[arg(long = "execution-chain", value_parser = chain_value_parser)]
    pub(crate) execution_chain: Option<Arc<BaseChainSpec>>,

    /// Execution node arguments.
    #[command(flatten)]
    pub(crate) execution: ExecutionNodeArgs,

    /// Follow-node arguments.
    #[command(flatten)]
    pub(crate) follow: EmbeddedConsensusFollowNodeConfigArgs,
}

impl FollowCommand {
    /// Runs the `follow` flavor.
    pub(crate) fn run(
        self,
        resolved_chain: ResolvedChainConfig,
        metrics_enabled: bool,
    ) -> eyre::Result<()> {
        let Self { execution_chain, execution, follow } = self;
        let mut execution_chain = match execution_chain {
            Some(chain) => chain,
            None => resolved_chain.execution_chain_spec()?,
        };
        let consensus_chain = resolved_chain.consensus_chain_args();
        let mut execution = execution;
        let follow_config: ConsensusFollowNodeConfigArgs = follow.into();
        execution
            .standard
            .rollup_args
            .upgrade_signal_l1_rpc
            .apply_default_from(&follow_config.l1_rpc_args.l1_eth_rpc);
        let follow_args = ConsensusFollowNodeArgs::new(consensus_chain, follow_config);
        let mut rollup_config = follow_args.load_rollup_config()?;

        CliRunner::try_default_runtime()?.run_command_until_exit(|ctx| async move {
            execution
                .standard
                .rollup_args
                .upgrade_signal
                .apply_startup_to_sinks(
                    &execution.standard.rollup_args.upgrade_signal_l1_rpc,
                    "integrated follow startup",
                    execution_chain.chain().id(),
                    Arc::make_mut(&mut execution_chain),
                    &mut rollup_config,
                )
                .await?;

            if metrics_enabled {
                CliMetrics::init_rollup_config(&rollup_config);
            }
            let _upgrade_countdown_metrics = metrics_enabled
                .then(|| CliMetrics::spawn_upgrade_countdown_recorder(rollup_config.clone()));

            let execution = execution
                .into_launch_config(execution_chain)
                .with_unified_auth_endpoint()
                .with_upgrade_signal_startup_already_applied();
            let l2_engine_rpc = engine_ipc_url(execution.auth_ipc_path())?;
            let task_executor = ctx.task_executor.clone();
            let launched = execution.launch_default(ctx).await?;
            let handle = launched.handle;
            // Keep the execution node handle alive until both services have coordinated shutdown.
            let execution_node = handle.node;
            let execution_exit = handle.node_exit_future;

            let follow_cancellation = CancellationToken::new();
            let follow_exit = follow_args.start_with_overrides(
                FollowNodeOverrides::embedded_execution(l2_engine_rpc),
                follow_cancellation.clone(),
            );
            tokio::pin!(execution_exit);
            tokio::pin!(follow_exit);

            let result = tokio::select! {
                result = &mut execution_exit => {
                    follow_cancellation.cancel();
                    let follow_result = follow_exit.await;
                    result?;
                    follow_result
                }
                result = &mut follow_exit => {
                    let follow_result = result;
                    task_executor
                        .initiate_graceful_shutdown()
                        .map_err(|e| eyre::eyre!("failed to signal execution node shutdown: {e}"))?
                        .ignore_guard()
                        .await;
                    let execution_result = execution_exit.await;
                    follow_result?;
                    execution_result
                }
            };

            drop(execution_node);
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{cli::BaseCli, commands::BaseCommand};
    use clap::Parser;

    const REQUIRED_FOLLOW_ARGS: &[&str] = &[
        "--source-l2-rpc",
        "http://source-l2:8545",
        "--l1-eth-rpc",
        "http://localhost:8545",
        "--l1-beacon",
        "http://localhost:5052",
    ];

    fn follow_args(args: &'static [&'static str]) -> Vec<&'static str> {
        let mut full_args = Vec::from(args);
        full_args.extend_from_slice(REQUIRED_FOLLOW_ARGS);
        full_args
    }

    #[test]
    fn parses_follow_source_and_proofs() {
        let cli = BaseCli::parse_from(follow_args(&[
            "base",
            "follow",
            "--follow.proofs",
            "--follow.proofs.max-blocks-ahead",
            "64",
        ]));

        let BaseCommand::Follow(follow) = cli.command else {
            panic!("expected follow command");
        };

        assert_eq!(follow.follow.source_l2_rpc.as_str(), "http://source-l2:8545/");
        assert!(follow.follow.proofs);
        assert_eq!(follow.follow.proofs_max_blocks_ahead, 64);
    }

    #[test]
    fn proofs_default_to_disabled() {
        let cli = BaseCli::parse_from(follow_args(&["base", "follow"]));

        let BaseCommand::Follow(follow) = cli.command else {
            panic!("expected follow command");
        };

        assert!(!follow.follow.proofs);
    }

    #[test]
    fn rejects_engine_rpc_arg() {
        // The engine endpoint is supplied by the embedded execution node, not a flag.
        let err = BaseCli::try_parse_from(follow_args(&[
            "base",
            "follow",
            "--l2-engine-rpc",
            "http://localhost:8551",
        ]))
        .unwrap_err();

        assert!(err.to_string().contains("--l2-engine-rpc"));
    }
}
