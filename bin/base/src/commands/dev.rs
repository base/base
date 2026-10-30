//! L1-free development chain command.

use std::{
    fs,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use alloy_rpc_types_engine::JwtSecret;
use base_common_chains::ChainConfig;
use base_consensus_engine::{EngineEndpoint, InMemoryTransport};
use base_consensus_node::{StandaloneDevChain, StandaloneImpersonationQueue};
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::ExecutionNodeConfigArgs;
use base_node_core::args::RollupArgs;
use base_node_runner::{BaseNodeExtension, BaseNodeRunner, FromExtensionConfig, NodeHooks};
use clap::Args;
use eyre::OptionExt;
use reth_cli_runner::CliRunner;
use reth_provider::HeaderProvider;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::ChainResolver;

/// Arguments for `base dev`.
#[derive(Args, Clone, Debug)]
#[command(
    mut_arg("datadir", |arg| arg
        .required_unless_present("fresh")
        // Keep the "default" placeholder, which parses as unset, so `--fresh` can omit it.
        .hide_default_value(true)
        .visible_alias("dir")),
    mut_arg("addr", |arg| arg.default_value("127.0.0.1")),
    mut_arg("port", |arg| arg.default_value("0")),
    mut_arg("ipcdisable", |arg| arg.default_value("true")),
    mut_arg("http_api", |arg| arg.default_value("all")),
    mut_arg("ws_api", |arg| arg.default_value("all"))
)]
pub(crate) struct DevCommand {
    /// Start a new chain from a fresh genesis instead of continuing a snapshot datadir.
    ///
    /// Uses a temporary datadir that is deleted on exit unless `--dir` is given; rerun with the
    /// same `--dir` to resume.
    #[arg(long)]
    pub(crate) fresh: bool,
    /// Execution node arguments.
    #[command(flatten)]
    pub(crate) execution: ExecutionNodeConfigArgs,
}

impl DevCommand {
    /// Runs the execution node and a standalone sequencer over an existing snapshot datadir, or
    /// over a fresh genesis with `--fresh`.
    pub(crate) fn run(mut self, chain_resolver: ChainResolver) -> eyre::Result<()> {
        // Without `--dir`, a fresh chain lives in a temporary datadir deleted on exit.
        let mut temporary_datadir = None;
        let (chain_spec, dev_chain) = if self.fresh {
            chain_resolver.reject_for_reth_command("base dev --fresh")?;
            let datadir = match self.execution.datadir.datadir.as_ref() {
                Some(datadir) => datadir.to_path_buf(),
                None => {
                    let temporary = tempfile::Builder::new().prefix("base-dev-").tempdir()?;
                    let datadir = temporary.path().to_path_buf();
                    self.execution.datadir.datadir = datadir.clone().into();
                    temporary_datadir = Some(temporary);
                    datadir
                }
            };
            let (dev_chain, genesis) = StandaloneDevChain::open_fresh(&datadir)?;
            (Arc::new(BaseChainSpec::try_from_genesis(genesis)?), dev_chain)
        } else {
            let datadir =
                self.execution.datadir.datadir.as_ref().ok_or_eyre("--dir is required")?;
            eyre::ensure!(
                !datadir.join(StandaloneDevChain::GENESIS_FILE).exists(),
                "{} was created by `base dev --fresh`; add --fresh to resume it",
                datadir.display()
            );
            let resolved_chain = chain_resolver.resolve()?;
            let rollup_config = ChainConfig::rollup_config_by_chain_id(resolved_chain.l2_chain_id)
                .ok_or_eyre("no built-in rollup config for this chain")?;
            (
                resolved_chain.execution_chain_spec()?,
                StandaloneDevChain::open(datadir, rollup_config)?,
            )
        };
        let mut execution =
            self.execution.into_runtime_config(chain_spec).with_unified_auth_endpoint();
        let node_config = &mut execution.node_config;
        node_config.rpc.http = true;
        node_config.rpc.ws = true;
        node_config.network.discovery.disable_discovery = true;
        node_config.network.max_peers = None;
        node_config.network.max_inbound_peers = Some(0);
        node_config.network.max_outbound_peers = Some(0);
        node_config.txpool.disable_transactions_backup = true;
        node_config.txpool.transactions_backup_path = None;
        let datadir = node_config.datadir();
        let config_path = node_config.config.clone().unwrap_or_else(|| datadir.config());
        eyre::ensure!(
            node_config.network.trusted_peers.is_empty()
                && (!config_path.exists()
                    || reth_config::Config::from_path(config_path)?.peers.trusted_nodes.is_empty()),
            "`base dev` cannot run with trusted peers; remove --trusted-peers and the config's trusted_nodes"
        );
        if self.fresh {
            info!(
                target: "base::dev",
                datadir = %datadir.data_dir().display(),
                temporary = temporary_datadir.is_some(),
                "starting fresh development chain"
            );
            println!("Prefunded accounts (mnemonic: {})", StandaloneDevChain::FRESH_MNEMONIC);
            for (index, account) in StandaloneDevChain::fresh_accounts()?.iter().enumerate() {
                println!("({index}) {} {}", account.address(), account.to_bytes());
            }
        } else {
            eyre::ensure!(
                datadir.db().join("mdbx.dat").is_file(),
                "{} has no execution database; `base dev` continues an existing snapshot datadir",
                datadir.data_dir().display()
            );
        }
        // Graceful shutdown completes the runner before the command future returns, so a
        // consensus failure that triggers it is reported here.
        let (consensus_failure_tx, consensus_failure) = mpsc::channel();
        let impersonation = StandaloneImpersonationQueue::default();

        let result = CliRunner::try_default_runtime()?.run_command_until_exit(|ctx| async move {
            let task_executor = ctx.task_executor.clone();
            info!(target: "base::dev", "starting execution node; database checks can take a while");
            let builder = execution.into_default_node_builder(ctx)?;
            let mut runner = BaseNodeRunner::new(RollupArgs::default());
            runner.install_ext::<ImpersonationRpcExtension>(impersonation.clone());
            let launched = runner.launch(builder).await?;
            let engine = EngineEndpoint::InMemory {
                transport: InMemoryTransport { module: launched.auth_module },
                engine_api: launched.engine_api,
            };
            let handle = launched.handle;
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
            let provider = execution_node.provider.clone();
            // The in-memory transport never sends the secret.
            let consensus_exit = dev_chain.run(
                engine,
                JwtSecret::random(),
                move |number| provider.header_by_number(number),
                impersonation,
                consensus_cancellation.clone(),
            );
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
        });
        if let Some(datadir) = temporary_datadir {
            // Reth's engine and persistence threads close the database shortly after the runtime
            // stops; deleting the directory first would fail their final flush. Only Linux exposes
            // the open files to check, so elsewhere the directory is deleted immediately.
            let deadline = Instant::now() + Duration::from_secs(5);
            let path = datadir.path().canonicalize()?;
            while Instant::now() < deadline
                && fs::read_dir("/proc/self/fd").is_ok_and(|fds| {
                    fds.flatten()
                        .any(|fd| fs::read_link(fd.path()).is_ok_and(|f| f.starts_with(&path)))
                })
            {
                thread::sleep(Duration::from_millis(10));
            }
            datadir.close()?;
        }
        result?;
        consensus_failure.try_recv().map_or(Ok(()), |error| Err(error.into()))
    }
}

/// Serves `dev_impersonateTransaction` on the user RPC endpoints.
#[derive(Debug)]
pub(crate) struct ImpersonationRpcExtension(StandaloneImpersonationQueue);

impl BaseNodeExtension for ImpersonationRpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(move |ctx| {
            ctx.modules.merge_configured(self.0.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for ImpersonationRpcExtension {
    type Config = StandaloneImpersonationQueue;

    fn from_config(queue: Self::Config) -> Self {
        Self(queue)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, net::Ipv4Addr};

    use clap::Parser;

    use crate::{cli::BaseCli, commands::BaseCommand, config::ChainResolver};

    #[test]
    fn rejects_trusted_peers_from_cli_and_saved_config() {
        let peer = "enode://3255458e24278e31d5940f304b16300fdff3f6efd3e2a030b5818310ac67af45e28d057e6a332d07e0c5ab09d6947fd4eed1a646edbf224e2d2fec6f49f90abc@127.0.0.1:30303";
        for saved in [false, true] {
            let datadir = tempfile::tempdir().unwrap();
            let mut args = vec!["base", "dev", "--dir", datadir.path().to_str().unwrap()];
            if saved {
                fs::write(
                    datadir.path().join("reth.toml"),
                    format!("[peers]\ntrusted_nodes = [\"{peer}\"]\n"),
                )
                .unwrap();
            } else {
                args.extend(["--trusted-peers", peer]);
            }
            let BaseCommand::Dev(dev) = BaseCli::try_parse_from(args).unwrap().command else {
                panic!("expected dev command");
            };
            let error = dev.run(ChainResolver::new(None)).unwrap_err();
            assert!(error.to_string().contains("cannot run with trusted peers"), "{error}");
            assert!(!datadir.path().join("db").exists());
        }
    }

    #[test]
    fn fresh_needs_no_datadir_and_rejects_a_chain_selector() {
        let cli =
            BaseCli::try_parse_from(["base", "--chain", "sepolia", "dev", "--fresh"]).unwrap();
        let BaseCommand::Dev(dev) = cli.command else {
            panic!("expected dev command");
        };
        assert!(dev.fresh);
        assert!(dev.execution.datadir.datadir.as_ref().is_none());
        let error = dev.run(ChainResolver::new(cli.chain)).unwrap_err();
        assert!(error.to_string().contains("manages its own chain configuration"), "{error}");
    }

    #[test]
    fn requires_only_datadir_and_binds_p2p_to_an_unused_loopback_port() {
        let cli = BaseCli::try_parse_from(["base", "dev", "--dir", "/tmp/dev data"]).unwrap();
        let BaseCommand::Dev(dev) = cli.command else {
            panic!("expected dev command");
        };
        assert_eq!(dev.execution.datadir.datadir.to_string(), "/tmp/dev data");
        assert_eq!(dev.execution.network.addr, Ipv4Addr::LOCALHOST);
        assert_eq!(dev.execution.network.port, 0);
        assert!(dev.execution.rpc.ipcdisable);
        assert_eq!(dev.execution.rpc.http_api, Some("all".parse().unwrap()));
        assert_eq!(dev.execution.rpc.ws_api, dev.execution.rpc.http_api);

        let error = BaseCli::try_parse_from(["base", "dev"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument);

        let cli = BaseCli::try_parse_from([
            "base",
            "dev",
            "--dir",
            "/tmp/dev data",
            "--http.api",
            "eth",
            "--ws.api",
            "net,web3",
        ])
        .unwrap();
        let BaseCommand::Dev(dev) = cli.command else {
            panic!("expected dev command");
        };
        assert_eq!(dev.execution.rpc.http_api, Some("eth".parse().unwrap()));
        assert_eq!(dev.execution.rpc.ws_api, Some("net,web3".parse().unwrap()));
    }
}
