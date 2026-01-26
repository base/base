//! Contains the CLI entry point for the Base consensus binary.

use std::{sync::Arc, time::Duration};

use alloy_rpc_types_engine::JwtSecret;
use base_cli_utils::{CliStyles, GlobalArgs, LogConfig, RuntimeManager};
use base_client_cli::{
    L1ClientArgs, L1ConfigFile, L2ClientArgs, L2ConfigFile, P2PArgs, RpcArgs, SequencerArgs,
};
use clap::Parser;
use kona_engine::RollupBoostServerArgs;
use kona_node_service::{EngineConfig, L1ConfigBuilder, NodeMode, RollupNodeBuilder};
use rollup_boost::ExecutionMode;
use strum::IntoEnumIterator;
use tracing::{error, info};
use url::Url;

use crate::{metrics::init_rollup_config_metrics, version};

/// The Base Consensus CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = version::SHORT_VERSION,
    long_version = version::LONG_VERSION,
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
            kona_gossip::Metrics::init();
            kona_disc::Metrics::init();
            kona_engine::Metrics::init();
            kona_node_service::Metrics::init();
            kona_derive::Metrics::init();
            kona_providers_alloy::Metrics::init();
            version::VersionInfo::from_build().register_version_metrics();
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
            info!(target: "rollup_node", "{hf}");
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

        // TODO: Remove hardcoded builder and rollup_boost config once we have our own
        // RollupNodeBuilder implementation. These are required by kona's EngineConfig
        // but are effectively disabled (execution_mode = Disabled, flashblocks = None).
        let engine_config = EngineConfig {
            config: Arc::new(cfg.clone()),
            builder_url: Url::parse("http://localhost:8552").expect("valid url"),
            builder_jwt_secret: JwtSecret::random(),
            builder_timeout: Duration::from_millis(30),
            l2_url: self.l2_client_args.l2_engine_rpc.clone(),
            l2_jwt_secret: jwt_secret,
            l2_timeout: Duration::from_millis(self.l2_client_args.l2_engine_timeout),
            l1_url: self.l1_rpc_args.l1_eth_rpc.clone(),
            mode: self.node_mode,
            rollup_boost: RollupBoostServerArgs {
                initial_execution_mode: ExecutionMode::Disabled,
                block_selection_policy: None,
                external_state_root: false,
                ignore_unhealthy_builders: false,
                flashblocks: None,
            },
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
            error!(target: "rollup_node", "Failed to start rollup node service: {e}");
            eyre::eyre!("{e}")
        })?;

        Ok(())
    }
}
