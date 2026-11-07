use std::{fmt, sync::Arc};

use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseExecutorProvider;
use base_node_core::BaseNode;
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

        match self.cli.command.split_node() {
            Ok(command) => {
                // Validate RPC modules using the configured validator
                if let Some(http_api) = &command.rpc.http_api {
                    Rpc::validate_selection(http_api, "http.api").map_err(|e| eyre!("{e}"))?;
                }
                if let Some(ws_api) = &command.rpc.ws_api {
                    Rpc::validate_selection(ws_api, "ws.api").map_err(|e| eyre!("{e}"))?;
                }

                runner.run_command_until_exit(|ctx| command.execute(ctx, launcher))
            }
            Err(command) => command.run(runner),
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

impl Commands {
    /// Runs every command except `node`, which needs the launcher passed to [`CliApp::run`].
    ///
    /// Not generic over the CLI extension or RPC validator, so it compiles once.
    pub fn run(self: Box<Self>, runner: CliRunner) -> Result<()> {
        let components = |spec: Arc<BaseChainSpec>| {
            (
                BaseExecutorProvider::base(Arc::clone(&spec)),
                Arc::new(BaseBeaconConsensus::new(spec)),
            )
        };

        match *self {
            Self::Node(_) => Err(eyre!("the `node` command must be run through `CliApp::run`")),
            Self::Init(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Self::InitState(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Self::DumpGenesis(command) => runner.run_blocking_until_ctrl_c(command.execute()),
            Self::Db(command) => {
                runner.run_blocking_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            Self::Stage(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode, _>(ctx, components))
            }
            Self::P2P(command) => runner.run_until_ctrl_c(command.execute::<BaseNode>()),
            Self::Config(command) => runner.run_until_ctrl_c(command.execute()),
            Self::Prune(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            #[cfg(feature = "dev")]
            Self::TestVectors(command) => runner.run_until_ctrl_c(command.execute()),
            Self::ReExecute(command) => {
                let runtime = runner.runtime();
                runner.run_until_ctrl_c(command.execute::<BaseNode>(components, runtime))
            }
            Self::BaseProofs(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Self::SnapshotManifest(command) => {
                command.execute()?;
                Ok(())
            }
            Self::Download(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>())
            }
        }
    }
}
