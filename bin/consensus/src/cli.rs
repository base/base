//! Contains the CLI entry point for the Base consensus binary.

use std::sync::Arc;

use base_cli_utils::{CliStyles, GlobalArgs, LogConfig, RuntimeManager};
use base_client_cli::{
    L1ClientArgs, L1ConfigFile, L2ClientArgs, L2ConfigFile, P2PArgs, RpcArgs, SequencerArgs,
};
use base_consensus_node::{EngineConfig, L1ConfigBuilder, NodeMode, RollupNodeBuilder};
use clap::Parser;
use strum::IntoEnumIterator;
use tracing::{error, info};

use crate::metrics::init_rollup_config_metrics;

/// The Base Consensus CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    styles = CliStyles::init(),
    about,
    long_about = None
)]
pub struct Cli {
    /// Global arguments for the Base Consensus CLI.
    #[command(flatten)]
    pub global: GlobalArgs,
    /// The mode to run the node in.
    #[arg(
        long = "mode",
        default_value_t = NodeMode::Validator,
        env = "BASE_NODE_MODE",
        help = format!(
            "The mode to run the node in. Supported modes are: {}",
            NodeMode::iter()
                .map(|mode| format!("\"{}\"", mode.to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    )]
    pub node_mode: NodeMode,

    /// L1 RPC CLI arguments.
    #[clap(flatten)]
    pub l1_rpc_args: L1ClientArgs,

    /// L2 engine CLI arguments.
    #[clap(flatten)]
    pub l2_client_args: L2ClientArgs,

    /// L1 configuration file.
    #[clap(flatten)]
    pub l1_config: L1ConfigFile,
    /// L2 configuration file.
    #[clap(flatten)]
    pub l2_config: L2ConfigFile,

    /// P2P CLI arguments.
    #[command(flatten)]
    pub p2p_flags: P2PArgs,
    /// RPC CLI arguments.
    #[command(flatten)]
    pub rpc_flags: RpcArgs,
    /// SEQUENCER CLI arguments.
    #[command(flatten)]
    pub sequencer_flags: SequencerArgs,
}

impl Cli {
    /// Runs the CLI.
    pub fn run(self) -> eyre::Result<()> {
        // Initialize logging from global arguments.
        LogConfig::from(self.global.logging.clone()).init_tracing_subscriber()?;

        // Initialize unified metrics
        self.global.metrics.init_with(|| {
            base_consensus_gossip::Metrics::init();
            base_consensus_disc::Metrics::init();
            base_consensus_engine::Metrics::init();
            base_consensus_node::Metrics::init();
            base_consensus_derive::Metrics::init();
            base_consensus_providers::Metrics::init();
            base_cli_utils::register_version_metrics!();
        })?;

        // Run the subcommand.
        RuntimeManager::run_until_ctrl_c(self.exec(&self.global))
    }

    /// Run the Node subcommand.
    pub async fn exec(&self, args: &GlobalArgs) -> eyre::Result<()> {
        let cfg = self.l2_config.load(&args.l2_chain_id).map_err(|e| eyre::eyre!("{e}"))?;

        info!(
            target: "rollup_node",
            chain_id = cfg.l2_chain_id.id(),
            "Starting rollup node services"
        );
        for hf in cfg.hardforks.to_string().lines() {
            info!(target: "rollup_node", hardfork = %hf, "hardfork");
        }

        let l1_chain_config =
            self.l1_config.load(cfg.l1_chain_id).map_err(|e| eyre::eyre!("{e}"))?;
        let l1_config = L1ConfigBuilder {
            chain_config: l1_chain_config,
            trust_rpc: self.l1_rpc_args.l1_trust_rpc,
            beacon: self.l1_rpc_args.l1_beacon.clone(),
            rpc_url: self.l1_rpc_args.l1_eth_rpc.clone(),
            slot_duration_override: self.l1_rpc_args.l1_slot_duration_override,
        };

        // If metrics are enabled, initialize the global cli metrics.
        args.metrics.enabled.then(|| init_rollup_config_metrics(&cfg));

        let jwt_secret = self.l2_client_args.validate_jwt().await?;

        self.p2p_flags.check_ports()?;
        let genesis_signer = args.genesis_signer().ok();
        let p2p_config = self
            .p2p_flags
            .clone()
            .config(
                &cfg,
                args.l2_chain_id.into(),
                Some(self.l1_rpc_args.l1_eth_rpc.clone()),
                genesis_signer,
            )
            .await?;
        let rpc_config = self.rpc_flags.clone().into();

        let engine_config = EngineConfig {
            config: Arc::new(cfg.clone()),
            l2_url: self.l2_client_args.l2_engine_rpc.clone(),
            l2_jwt_secret: jwt_secret,
            l1_url: self.l1_rpc_args.l1_eth_rpc.clone(),
            mode: self.node_mode,
        };

        RollupNodeBuilder::new(
            cfg,
            l1_config,
            self.l2_client_args.l2_trust_rpc,
            engine_config,
            p2p_config,
            rpc_config,
        )
        .with_sequencer_config(self.sequencer_flags.config())
        .build()
        .start()
        .await
        .map_err(|e| {
            error!(target: "rollup_node", error = %e, "Failed to start rollup node service");
            eyre::eyre!("{e}")
        })?;

        Ok(())
    }
}
