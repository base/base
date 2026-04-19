//! Contains the CLI entry point for the Base consensus binary.

use std::{path::PathBuf, sync::Arc};

use alloy_chains::Chain;
use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
use base_cli_utils::{CliStyles, LogConfig, RuntimeManager};
use base_client_cli::{
    L1ClientArgs, L1ConfigFile, L2ClientArgs, L2ConfigFile, P2PArgs, RpcArgs, SequencerArgs,
};
use base_consensus_node::{
    DelegateL2Client, EngineConfig, FollowNode, L1Config, L1ConfigBuilder, NodeMode,
    RollupNodeBuilder,
};
use base_consensus_providers::OnlineBeaconClient;
use base_consensus_registry::Registry;
use clap::{Args, Parser, Subcommand};
use eyre::Context;
use strum::IntoEnumIterator;
use tracing::{error, info, warn};
use url::Url;

use crate::metrics::{init_p2p_metrics, init_rollup_config_metrics};

base_cli_utils::define_log_args!("BASE_NODE");
base_cli_utils::define_metrics_args!("BASE_NODE", 9090);

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
    /// The command to run.
    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    /// Run the CLI.
    pub fn run(self) -> eyre::Result<()> {
        match self.command {
            Commands::Node(node) => node.run(),
            Commands::Follow(follow) => follow.run(),
        }
    }
}

/// Commands for the Base Consensus CLI.
#[derive(Subcommand, Clone, Debug)]
#[expect(clippy::large_enum_variant)]
pub enum Commands {
    /// Start the node
    #[command(name = "node")]
    Node(Node),

    /// Follows another node.
    #[command(name = "follow")]
    Follow(Follow),
}

/// Follow CLI arguments.
#[derive(Args, Clone, Debug)]
pub struct Follow {
    /// The URL of the node to follow.
    #[arg(long = "source-l2-rpc", env = "BASE_NODE_SOURCE_L2_RPC")]
    pub source_l2_rpc: Url,

    /// Local L2 execution RPC URL (non-engine, e.g. port 8545).
    #[arg(
        long = "l2-rpc-url",
        default_value = "http://localhost:8545",
        env = "BASE_NODE_L2_RPC_URL"
    )]
    pub l2_rpc_url: Url,

    /// L2 engine CLI arguments.
    #[clap(flatten)]
    pub l2_client_args: L2ClientArgs,

    /// L2 Chain ID or name (8453 = Base Mainnet, 84532 = Base Sepolia).
    #[arg(
        long = "chain",
        short = 'n',
        global = true,
        default_value = "8453",
        env = "BASE_NODE_NETWORK"
    )]
    pub l2_chain_id: Chain,

    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub metrics: MetricsArgs,

    /// Gate sync behind proofs progress via `debug_proofsSyncStatus`.
    #[arg(long = "proofs", env = "BASE_NODE_PROOFS")]
    pub proofs: bool,

    /// Maximum number of blocks the follow node may advance beyond the proofs
    /// `ExEx` head. Only effective when `--proofs` is enabled.
    #[arg(
        long = "proofs.max-blocks-ahead",
        default_value_t = 512,
        env = "BASE_NODE_PROOFS_MAX_BLOCKS_AHEAD"
    )]
    pub proofs_max_blocks_ahead: u64,

    /// RPC CLI arguments.
    #[command(flatten)]
    pub rpc_flags: RpcArgs,

    /// L2 configuration file.
    #[clap(flatten)]
    pub l2_config: L2ConfigFile,

    /// L1 configuration file.
    #[clap(flatten)]
    pub l1_config: L1ConfigFile,

    /// L1 RPC CLI arguments.
    #[clap(flatten)]
    pub l1_rpc_args: L1ClientArgs,
}

impl Follow {
    /// Runs the CLI.
    pub fn run(self) -> eyre::Result<()> {
        // Initialize logging from global arguments.
        base_cli_utils::init_tracing!(
            LogConfig::from(self.logging.clone()),
            ["libp2p_gossipsub=error"]
        )?;

        // Initialize unified metrics for the follow-node subsystems.
        base_cli_utils::MetricsConfig::from(self.metrics.clone()).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        // Run the subcommand.
        RuntimeManager::new().run_until_ctrl_c(self.exec())
    }

    /// Run the Follow subcommand.
    pub async fn exec(&self) -> eyre::Result<()> {
        let cfg = self.l2_config.load(&self.l2_chain_id).map_err(|e| eyre::eyre!("{e}"))?;

        if self.metrics.enabled {
            init_rollup_config_metrics(&cfg);
        }

        if !self.proofs {
            warn!(
                target: "rollup_node",
                "Running without --proofs; this mode is mainly meant for syncing the Proofs ExEx and does not support EL sync"
            );
        }

        info!(
            target: "rollup_node",
            chain_id = cfg.l2_chain_id.id(),
            source = %self.source_l2_rpc,
            "Starting follow node"
        );

        let jwt_secret = self.l2_client_args.validate_jwt().await?;
        let rollup_config = Arc::new(cfg.clone());

        let engine_config = EngineConfig {
            config: Arc::clone(&rollup_config),
            l2_url: self.l2_client_args.l2_engine_rpc.clone(),
            l2_jwt_secret: jwt_secret,
            l1_url: self.l1_rpc_args.l1_eth_rpc.clone(),
            mode: NodeMode::Validator,
        };
        let local_l2_provider =
            RootProvider::<base_common_network::Base>::new_http(self.l2_rpc_url.clone());

        if self.proofs {
            local_l2_provider
                .raw_request::<_, serde_json::Value>("debug_proofsSyncStatus".into(), ())
                .await
                .map_err(|e| {
                    error!(target: "rollup_node", error = %e, "debug_proofsSyncStatus call failed; is the Proofs ExEx enabled on the node?");
                    eyre::eyre!("debug_proofsSyncStatus call failed: {e}")
                })?;
            info!(target: "rollup_node", "Proofs ExEx confirmed available via debug_proofsSyncStatus");
        }

        let l1_chain_config =
            self.l1_config.load(cfg.l1_chain_id).map_err(|e| eyre::eyre!("{e}"))?;

        let l2_source = DelegateL2Client::new(self.source_l2_rpc.clone());
        let rpc_builder = self.rpc_flags.clone().into();
        let l1_beacon = OnlineBeaconClient::new_http(self.l1_rpc_args.l1_beacon.to_string());

        let l1_config = L1Config {
            chain_config: Arc::new(l1_chain_config),
            trust_rpc: self.l1_rpc_args.l1_trust_rpc,
            beacon_client: l1_beacon,
            engine_provider: RootProvider::new_http(self.l1_rpc_args.l1_eth_rpc.clone()),
            finalized_poll_interval: L1Config::default_finalized_poll_interval(cfg.l1_chain_id),
            verifier_l1_confs: self.l1_rpc_args.l1_verifier_confs,
        };

        FollowNode::new(
            rollup_config,
            engine_config,
            local_l2_provider,
            l2_source,
            rpc_builder,
            l1_config,
        )
        .with_proofs(self.proofs)
        .with_proofs_max_blocks_ahead(self.proofs_max_blocks_ahead)
        .start()
        .await
        .map_err(|e| {
            error!(target: "rollup_node", error = %e, "Failed to start follow node");
            eyre::eyre!("{e}")
        })?;

        Ok(())
    }
}

/// Node CLI arguments.
#[derive(Args, Clone, Debug)]
pub struct Node {
    /// L2 Chain ID or name (8453 = Base Mainnet, 84532 = Base Sepolia).
    #[arg(
        long = "chain",
        short = 'n',
        global = true,
        default_value = "8453",
        env = "BASE_NODE_NETWORK"
    )]
    pub l2_chain_id: Chain,
    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,
    /// Metrics configuration.
    #[command(flatten)]
    pub metrics: MetricsArgs,
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

    /// Path to the `SafeDB` directory. If not set, safe head tracking is disabled.
    #[arg(long = "safedb.path", env = "BASE_NODE_SAFEDB_PATH")]
    pub safedb_path: Option<PathBuf>,
}

impl Node {
    /// Runs the CLI.
    pub fn run(self) -> eyre::Result<()> {
        // Initialize logging from global arguments.
        base_cli_utils::init_tracing!(
            LogConfig::from(self.logging.clone()),
            ["libp2p_gossipsub=error"]
        )?;

        // Initialize unified metrics
        base_cli_utils::MetricsConfig::from(self.metrics.clone()).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        // Run the subcommand.
        RuntimeManager::new().run_until_ctrl_c(self.exec())
    }

    /// Returns the signer [`Address`] from the rollup config for the given l2 chain id.
    fn genesis_signer(&self) -> eyre::Result<Address> {
        let id = self.l2_chain_id;
        Registry::unsafe_block_signer(id.id())
            .ok_or_else(|| eyre::eyre!("No unsafe block signer found for chain ID: {id}"))
    }

    /// Validates that a sequencer signing key is configured when running in sequencer mode.
    fn validate_sequencer_key(&self) -> eyre::Result<()> {
        if self.node_mode.is_sequencer() {
            let signer = &self.p2p_flags.signer;
            if signer.sequencer_key.is_none()
                && signer.sequencer_key_path.is_none()
                && signer.endpoint.is_none()
            {
                eyre::bail!(
                    "sequencer mode requires a signing key; \
                     provide --p2p.sequencer.key, --p2p.sequencer.key.path, \
                     or --p2p.signer.endpoint"
                );
            }
        }
        Ok(())
    }

    /// Run the Node subcommand.
    pub async fn exec(&self) -> eyre::Result<()> {
        self.validate_sequencer_key()?;

        let cfg = self.l2_config.load(&self.l2_chain_id).map_err(|e| eyre::eyre!("{e}"))?;

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
            verifier_l1_confs: self.l1_rpc_args.l1_verifier_confs,
        };

        // If metrics are enabled, initialize the global cli metrics.
        if self.metrics.enabled {
            init_rollup_config_metrics(&cfg);
            init_p2p_metrics(&self.p2p_flags);
        }

        let jwt_secret = self.l2_client_args.validate_jwt().await?;

        self.p2p_flags.check_ports()?;
        let genesis_signer = self.genesis_signer().ok();
        let p2p_config = self
            .p2p_flags
            .clone()
            .config(
                &cfg,
                self.l2_chain_id.into(),
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

        let mut builder = RollupNodeBuilder::new(
            cfg,
            l1_config,
            self.l2_client_args.l2_trust_rpc,
            engine_config,
            p2p_config,
            rpc_config,
        )
        .with_sequencer_config(self.sequencer_flags.config());
        if let Some(path) = self.safedb_path.clone() {
            builder = builder.with_safedb_path(path);
        }
        builder.build().await.wrap_err("Failed to build rollup node")?.start().await.map_err(
            |e| {
                error!(target: "rollup_node", error = %e, "Failed to start rollup node service");
                eyre::eyre!("{e}")
            },
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use alloy_chains::Chain;
    use alloy_primitives::B256;
    use base_client_cli::{P2PArgs, SignerArgs};
    use base_consensus_node::NodeMode;
    use rstest::rstest;
    use url::Url;

    use super::*;

    fn default_node() -> Node {
        Node {
            l2_chain_id: Chain::from(8453_u64),
            logging: LogArgs::default(),
            metrics: MetricsArgs::default(),
            node_mode: NodeMode::default(),
            l1_rpc_args: L1ClientArgs::default(),
            l2_client_args: L2ClientArgs::default(),
            l1_config: L1ConfigFile::default(),
            l2_config: L2ConfigFile::default(),
            p2p_flags: P2PArgs::default(),
            rpc_flags: RpcArgs::default(),
            sequencer_flags: SequencerArgs::default(),
            safedb_path: None,
        }
    }

    /// Tests that clap correctly wires env vars into the signer fields and that the
    /// validation passes when each key source is provided via environment variable.
    #[rstest]
    #[case::raw_key(vec![("BASE_NODE_P2P_SEQUENCER_KEY", "bcc617ea05150ff60490d3c6058630ba94ae9f12a02a87efd291349ca0e54e0a")])]
    #[case::key_path(vec![("BASE_NODE_P2P_SEQUENCER_KEY_PATH", "/tmp/key.hex")])]
    #[case::remote_endpoint(vec![("BASE_NODE_P2P_SIGNER_ENDPOINT", "http://localhost:8080"), ("BASE_NODE_P2P_SIGNER_ADDRESS", "0xAf6E19BE0F9cE7f8afd49a1824851023A8249e8a")])]
    fn test_validate_sequencer_key_env_var(#[case] env_vars: Vec<(&str, &str)>) {
        for (k, v) in &env_vars {
            // SAFETY: each rstest case uses distinct env var names, so concurrent
            // test threads do not read or write the same variables simultaneously.
            unsafe { std::env::set_var(k, v) }
        }
        let signer = SignerArgs::parse_from(["test"]);
        for (k, _) in &env_vars {
            // SAFETY: see above.
            unsafe { std::env::remove_var(k) }
        }
        let node = Node {
            node_mode: NodeMode::Sequencer,
            p2p_flags: P2PArgs { signer, ..P2PArgs::default() },
            ..default_node()
        };
        assert!(node.validate_sequencer_key().is_ok());
    }

    #[rstest]
    #[case::validator_no_key(NodeMode::Validator, SignerArgs::default(), true)]
    #[case::sequencer_no_key(NodeMode::Sequencer, SignerArgs::default(), false)]
    #[case::sequencer_raw_key(NodeMode::Sequencer, SignerArgs { sequencer_key: Some(B256::ZERO), ..Default::default() }, true)]
    #[case::sequencer_key_path(NodeMode::Sequencer, SignerArgs { sequencer_key_path: Some(PathBuf::from("/tmp/key.hex")), ..Default::default() }, true)]
    #[case::sequencer_remote_endpoint(NodeMode::Sequencer, SignerArgs { endpoint: Some(Url::parse("http://localhost:8080").unwrap()), ..Default::default() }, true)]
    fn test_validate_sequencer_key(
        #[case] mode: NodeMode,
        #[case] signer: SignerArgs,
        #[case] expected_ok: bool,
    ) {
        let node = Node {
            node_mode: mode,
            p2p_flags: P2PArgs { signer, ..P2PArgs::default() },
            ..default_node()
        };
        assert_eq!(node.validate_sequencer_key().is_ok(), expected_ok);
    }
}
