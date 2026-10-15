use std::{fmt, sync::Arc};

use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseExecutorProvider;
use base_node_core::{BaseLaunchPolicy, BaseNode};
use eyre::{Result, eyre};
use reth_cli_commands::launcher::Launcher;
use reth_cli_runner::CliRunner;
use reth_node_core::args::{OtlpInitStatus, OtlpLogsStatus};
use reth_node_metrics::recorder::install_prometheus_recorder;
use reth_rpc_server_types::RpcModuleValidator;
use reth_tracing::{Layers, TracingGuards};
use tracing::{info, warn};

use crate::{Cli, Commands};

/// A wrapper around a parsed CLI that handles command execution.
#[derive(Debug)]
pub struct CliApp<Ext: clap::Args + fmt::Debug, Rpc: RpcModuleValidator> {
    cli: Cli<Ext, Rpc>,
    runner: Option<CliRunner>,
    layers: Option<Layers>,
    guard: Option<TracingGuards>,
}

impl<Ext, Rpc> CliApp<Ext, Rpc>
where
    Ext: clap::Args + fmt::Debug,
    Rpc: RpcModuleValidator,
{
    pub(crate) fn new(cli: Cli<Ext, Rpc>) -> Self {
        Self { cli, runner: None, layers: Some(Layers::new()), guard: None }
    }

    /// Sets the runner for the CLI commander.
    ///
    /// This replaces any existing runner with the provided one.
    pub fn set_runner(&mut self, runner: CliRunner) {
        self.runner = Some(runner);
    }

    /// Access to tracing layers.
    ///
    /// Returns a mutable reference to the tracing layers, or error
    /// if tracing initialized and layers have detached already.
    pub fn access_tracing_layers(&mut self) -> Result<&mut Layers> {
        self.layers.as_mut().ok_or_else(|| eyre!("Tracing already initialized"))
    }

    /// Execute the configured cli command.
    ///
    /// This accepts a closure that is used to launch the node via the
    /// [`NodeCommand`](reth_cli_commands::node::NodeCommand).
    pub fn run(
        mut self,
        launcher: impl Launcher<crate::chainspec::BaseChainSpecParser, Ext>,
    ) -> Result<()> {
        let runner = match self.runner.take() {
            Some(runner) => runner,
            None => CliRunner::try_default_runtime()?,
        };

        // add network name to logs dir
        // Add network name if available to the logs dir
        if let Some(chain_spec) = self.cli.command.chain_spec() {
            self.cli.logs.log_file_directory =
                self.cli.logs.log_file_directory.join(chain_spec.chain.to_string());
        }

        self.init_tracing(&runner)?;

        // Install the prometheus recorder to be sure to record all metrics
        install_prometheus_recorder();

        let components = |spec: Arc<BaseChainSpec>| {
            (
                BaseExecutorProvider::base(Arc::clone(&spec)),
                Arc::new(BaseBeaconConsensus::new(spec)),
            )
        };

        match self.cli.command {
            Commands::Node(mut command) => {
                // The standalone node and builder binaries build their config through reth's
                // node command, so Base's launch policy has to be applied here as well.
                BaseLaunchPolicy::force_serial_import(&mut command.engine);

                // Validate RPC modules using the configured validator
                if let Some(http_api) = &command.rpc.http_api {
                    Rpc::validate_selection(http_api, "http.api").map_err(|e| eyre!("{e}"))?;
                }
                if let Some(ws_api) = &command.rpc.ws_api {
                    Rpc::validate_selection(ws_api, "ws.api").map_err(|e| eyre!("{e}"))?;
                }

                runner.run_command_until_exit(|ctx| command.execute(ctx, launcher))
            }
            Commands::Init(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Commands::InitState(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Commands::DumpGenesis(command) => runner.run_blocking_until_ctrl_c(command.execute()),
            Commands::Db(command) => {
                runner.run_blocking_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            Commands::Stage(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode, _>(ctx, components))
            }
            Commands::P2P(command) => runner.run_until_ctrl_c(command.execute::<BaseNode>()),
            Commands::Config(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::Prune(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            #[cfg(feature = "dev")]
            Commands::TestVectors(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::ReExecute(command) => {
                let runtime = runner.runtime();
                runner.run_until_ctrl_c(command.execute::<BaseNode>(components, runtime))
            }
            Commands::BaseProofs(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Commands::SnapshotManifest(command) => {
                command.execute()?;
                Ok(())
            }
            Commands::Download(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>())
            }
        }
    }

    /// Initializes tracing with the configured options.
    ///
    /// If file logging is enabled, this function stores guard to the struct.
    /// For gRPC OTLP, it requires tokio runtime context.
    pub fn init_tracing(&mut self, runner: &CliRunner) -> Result<()> {
        if self.guard.is_none() {
            let mut layers = self.layers.take().unwrap_or_default();

            let otlp_status = runner.block_on(self.cli.traces.init_otlp_tracing(&mut layers))?;
            let otlp_logs_status = runner.block_on(self.cli.traces.init_otlp_logs(&mut layers))?;

            let enable_reload = self.cli.command.debug_namespace_enabled();
            self.guard = Some(self.cli.logs.init_tracing_with_layers(layers, enable_reload)?);
            info!(target: "reth::cli", log_dir = %self.cli.logs.log_file_directory, "Initialized tracing");

            match otlp_status {
                OtlpInitStatus::Started(endpoint) => {
                    info!(target: "reth::cli", protocol = ?self.cli.traces.protocol, endpoint = %endpoint, "Started OTLP tracing export");
                }
                OtlpInitStatus::NoFeature => {
                    warn!(target: "reth::cli", "Provided OTLP tracing arguments do not have effect, compile with the `otlp` feature")
                }
                OtlpInitStatus::Disabled => {}
            }

            match otlp_logs_status {
                OtlpLogsStatus::Started(endpoint) => {
                    info!(target: "reth::cli", protocol = ?self.cli.traces.protocol, endpoint = %endpoint, "Started OTLP logs export");
                }
                OtlpLogsStatus::NoFeature => {
                    warn!(target: "reth::cli", "Provided OTLP logs arguments do not have effect, compile with the `otlp-logs` feature")
                }
                OtlpLogsStatus::Disabled => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

    use clap::Parser;
    use tempfile::TempDir;

    use crate::{Cli, Commands, StandardNodeArgs};

    /// Runs the real standalone `base-reth-node` / `base-builder` command path with a launcher
    /// that captures the tree config the node would import with, then stops before it starts.
    ///
    /// `configured` overrides `engine.bal_parallel_execution_disabled` on the parsed command, so
    /// a caller can simulate any configuration source asking for the divergent parallel path.
    fn standalone_command_disables_bal_parallel_execution(configured: Option<bool>) -> bool {
        // clap validates environment-provided values even when the flag is absent, and a
        // developer environment may set a protocol this build does not accept.
        // SAFETY: single-test context; no other thread reads this variable concurrently.
        unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_PROTOCOL", "http") };

        let datadir = TempDir::new().expect("temporary datadir");
        let datadir_arg = format!("--datadir={}", datadir.path().display());
        let mut cli = Cli::<StandardNodeArgs>::parse_from(["base-reth", "node", &datadir_arg]);
        if let Some(configured) = configured {
            match &mut cli.command {
                Commands::Node(command) => {
                    command.engine.bal_parallel_execution_disabled = configured;
                }
                other => panic!("expected the node command, got {other:?}"),
            }
        }

        let captured = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&captured);

        let result = cli.run(move |builder, _args| async move {
            observed.store(
                builder.config().tree_config().disable_bal_parallel_execution(),
                Ordering::SeqCst,
            );
            Err(eyre::eyre!("stop before the node starts"))
        });

        assert!(result.is_err(), "the launcher must stop the node before it starts");
        captured.load(Ordering::SeqCst)
    }

    #[test]
    fn standalone_node_imports_serially_by_default() {
        assert!(
            standalone_command_disables_bal_parallel_execution(None),
            "the standalone base-reth-node/base-builder launch path must import serially: \
             reth's BAL-parallel ordered commit does not apply Base's cumulative gas, Jovian DA \
             or payer-auth admission rules"
        );
    }

    #[test]
    fn standalone_node_imports_serially_when_configuration_asks_for_the_parallel_path() {
        assert!(
            standalone_command_disables_bal_parallel_execution(Some(false)),
            "configuration must not be able to re-enable the divergent BAL-parallel import path"
        );
    }
}
