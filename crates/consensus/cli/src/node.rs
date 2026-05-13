//! Reusable consensus node arguments and launch helpers.

use std::{path::PathBuf, sync::Arc};

use alloy_primitives::Address;
use alloy_rpc_types_engine::JwtSecret;
use base_cli_utils::{LogConfig, RuntimeManager};
use base_common_chains::Registry;
use base_common_genesis::RollupConfig;
use base_consensus_node::{EngineConfig, L1ConfigBuilder, NodeMode, RollupNode, RollupNodeBuilder};
use clap::Args;
use eyre::Context;
use strum::IntoEnumIterator;
use tracing::{error, info};
use url::Url;

use crate::{
    ConsensusChainArgs, L1ClientArgs, L1ConfigFile, L2ClientArgs, L2ConfigFile, LogArgs,
    MetricsArgs, P2PArgs, RpcArgs, SequencerArgs, metrics::CliMetrics,
};

/// Overrides supplied by callers that embed consensus alongside another service.
#[derive(Clone, Debug, Default)]
pub struct ConsensusNodeOverrides {
    /// Override for the L2 Engine API endpoint.
    pub l2_engine_rpc: Option<Url>,
    /// Override for the L2 Engine API JWT secret.
    pub l2_engine_jwt_secret: Option<JwtSecret>,
}

/// Standalone consensus node command.
#[derive(Args, Clone, Debug)]
pub struct ConsensusNodeCommand {
    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub metrics: MetricsArgs,

    /// Consensus node arguments.
    #[command(flatten)]
    pub args: ConsensusNodeConfigArgs,
}

impl ConsensusNodeCommand {
    /// Runs the standalone consensus node command.
    pub fn run(self, chain: ConsensusChainArgs) -> eyre::Result<()> {
        base_cli_utils::init_tracing!(
            LogConfig::from(self.logging.clone()),
            ["libp2p_gossipsub=error"]
        )?;

        base_cli_utils::MetricsConfig::from(self.metrics.clone()).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        let args = ConsensusNodeArgs::new(chain, self.args);
        let cfg = args.load_rollup_config()?;
        if self.metrics.enabled {
            CliMetrics::init_rollup_config(&cfg);
            CliMetrics::init_p2p(&args.config.p2p_flags);
        }

        RuntimeManager::new().run_until_ctrl_c(args.start_with_overrides(cfg, Default::default()))
    }
}

/// Consensus node arguments shared by the standalone and unified binaries.
#[derive(Args, Clone, Debug)]
pub struct ConsensusNodeArgs {
    /// Chain selection.
    #[command(flatten)]
    pub chain: ConsensusChainArgs,

    /// Consensus node configuration.
    #[command(flatten)]
    pub config: ConsensusNodeConfigArgs,
}

impl ConsensusNodeArgs {
    /// Creates reusable consensus node arguments from typed chain and node config components.
    pub const fn new(chain: ConsensusChainArgs, config: ConsensusNodeConfigArgs) -> Self {
        Self { chain, config }
    }
}

/// Consensus node configuration arguments without chain selection.
#[derive(Args, Clone, Debug)]
pub struct ConsensusNodeConfigArgs {
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

impl ConsensusNodeArgs {
    /// Loads the configured L2 rollup config.
    pub fn load_rollup_config(&self) -> eyre::Result<RollupConfig> {
        self.config.l2_config.load(&self.chain.l2_chain_id).map_err(|e| eyre::eyre!(e))
    }

    /// Validates that a sequencer signing key is configured when running in sequencer mode.
    pub fn validate_sequencer_key(&self) -> eyre::Result<()> {
        if self.config.node_mode.is_sequencer() {
            let signer = &self.config.p2p_flags.signer;
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

    /// Builds a rollup node with default external endpoint configuration.
    pub async fn build_rollup_node(&self) -> eyre::Result<RollupNode> {
        self.build_rollup_node_with_overrides(
            self.load_rollup_config()?,
            ConsensusNodeOverrides::default(),
        )
        .await
    }

    /// Builds a rollup node with caller-supplied endpoint overrides.
    pub async fn build_rollup_node_with_overrides(
        &self,
        cfg: RollupConfig,
        overrides: ConsensusNodeOverrides,
    ) -> eyre::Result<RollupNode> {
        self.validate_sequencer_key()?;

        info!(
            target: "rollup_node",
            chain_id = cfg.l2_chain_id.id(),
            "Starting rollup node services"
        );
        for hf in cfg.hardforks.to_string().lines() {
            info!(target: "rollup_node", hardfork = %hf, "hardfork");
        }

        let l1_chain_config =
            self.config.l1_config.load(cfg.l1_chain_id).map_err(|e| eyre::eyre!(e))?;
        let l1_config = L1ConfigBuilder {
            chain_config: l1_chain_config,
            trust_rpc: self.config.l1_rpc_args.l1_trust_rpc,
            beacon: self.config.l1_rpc_args.l1_beacon.clone(),
            rpc_url: self.config.l1_rpc_args.l1_eth_rpc.clone(),
            slot_duration_override: self.config.l1_rpc_args.l1_slot_duration_override,
            verifier_l1_confs: self.config.l1_rpc_args.l1_verifier_confs,
        };

        let l2_engine_rpc = overrides
            .l2_engine_rpc
            .unwrap_or_else(|| self.config.l2_client_args.l2_engine_rpc.clone());
        let jwt_secret = match overrides.l2_engine_jwt_secret {
            Some(secret) => secret,
            None => {
                self.config.l2_client_args.resolve_jwt_secret_for_endpoint(&l2_engine_rpc).await?
            }
        };

        self.config.p2p_flags.check_ports()?;
        let genesis_signer = self.genesis_signer().ok();
        let p2p_config = self
            .config
            .p2p_flags
            .clone()
            .config(
                &cfg,
                self.chain.l2_chain_id.into(),
                Some(self.config.l1_rpc_args.l1_eth_rpc.clone()),
                genesis_signer,
            )
            .await?;
        let rpc_config = self.config.rpc_flags.clone().into();

        let engine_config = EngineConfig {
            config: Arc::new(cfg.clone()),
            l2_url: l2_engine_rpc,
            l2_jwt_secret: jwt_secret,
            l1_url: self.config.l1_rpc_args.l1_eth_rpc.clone(),
            mode: self.config.node_mode,
        };

        let mut builder = RollupNodeBuilder::new(
            cfg,
            l1_config,
            self.config.l2_client_args.l2_trust_rpc,
            engine_config,
            p2p_config,
            rpc_config,
        )
        .with_sequencer_config(self.config.sequencer_flags.config());

        if let Some(path) = self.config.safedb_path.clone() {
            builder = builder.with_safedb_path(path);
        }

        builder.build().await.wrap_err("Failed to build rollup node")
    }

    /// Starts a rollup node with default external endpoint configuration.
    pub async fn start(&self) -> eyre::Result<()> {
        self.start_with_overrides(self.load_rollup_config()?, ConsensusNodeOverrides::default())
            .await
    }

    /// Starts a rollup node with caller-supplied endpoint overrides.
    pub async fn start_with_overrides(
        &self,
        cfg: RollupConfig,
        overrides: ConsensusNodeOverrides,
    ) -> eyre::Result<()> {
        self.build_rollup_node_with_overrides(cfg, overrides).await?.start().await.map_err(|e| {
            error!(target: "rollup_node", error = %e, "Failed to start rollup node service");
            eyre::eyre!(e)
        })
    }

    /// Returns the configured genesis signer address for the selected L2 chain.
    pub fn genesis_signer(&self) -> eyre::Result<Address> {
        let id = self.chain.l2_chain_id;
        Registry::unsafe_block_signer(id.id())
            .ok_or_else(|| eyre::eyre!("No unsafe block signer found for chain ID: {id}"))
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Mutex};

    use alloy_chains::Chain;
    use alloy_primitives::B256;
    use clap::Parser;
    use rstest::rstest;

    use super::*;
    use crate::SignerArgs;

    static SIGNER_ENV_LOCK: Mutex<()> = Mutex::new(());
    const SIGNER_ENV_KEYS: &[&str] = &[
        "BASE_NODE_P2P_SEQUENCER_KEY",
        "BASE_NODE_P2P_SEQUENCER_KEY_PATH",
        "BASE_NODE_P2P_SIGNER_ENDPOINT",
        "BASE_NODE_P2P_SIGNER_ADDRESS",
    ];

    fn default_node_config_args() -> ConsensusNodeConfigArgs {
        ConsensusNodeConfigArgs {
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

    #[rstest]
    #[case::raw_key(vec![(
        "BASE_NODE_P2P_SEQUENCER_KEY",
        "bcc617ea05150ff60490d3c6058630ba94ae9f12a02a87efd291349ca0e54e0a",
    )])]
    #[case::key_path(vec![("BASE_NODE_P2P_SEQUENCER_KEY_PATH", "/tmp/key.hex")])]
    #[case::remote_endpoint(vec![
        ("BASE_NODE_P2P_SIGNER_ENDPOINT", "http://localhost:8080"),
        ("BASE_NODE_P2P_SIGNER_ADDRESS", "0xAf6E19BE0F9cE7f8afd49a1824851023A8249e8a"),
    ])]
    fn validates_sequencer_key_from_env(#[case] env_vars: Vec<(&str, &str)>) {
        let _guard = SIGNER_ENV_LOCK.lock().unwrap();

        for key in SIGNER_ENV_KEYS {
            // SAFETY: guarded by SIGNER_ENV_LOCK.
            unsafe { std::env::remove_var(key) }
        }
        for (key, value) in &env_vars {
            // SAFETY: guarded by SIGNER_ENV_LOCK.
            unsafe { std::env::set_var(key, value) }
        }
        let signer = SignerArgs::parse_from(["test"]);
        for key in SIGNER_ENV_KEYS {
            // SAFETY: guarded by SIGNER_ENV_LOCK.
            unsafe { std::env::remove_var(key) }
        }
        let args = ConsensusNodeArgs::new(
            ConsensusChainArgs { l2_chain_id: Chain::from(8453_u64) },
            ConsensusNodeConfigArgs {
                node_mode: NodeMode::Sequencer,
                p2p_flags: P2PArgs { signer, ..P2PArgs::default() },
                ..default_node_config_args()
            },
        );
        assert!(args.validate_sequencer_key().is_ok());
    }

    #[rstest]
    #[case::validator_no_key(NodeMode::Validator, SignerArgs::default(), true)]
    #[case::sequencer_no_key(NodeMode::Sequencer, SignerArgs::default(), false)]
    #[case::sequencer_raw_key(
        NodeMode::Sequencer,
        SignerArgs { sequencer_key: Some(B256::ZERO), ..Default::default() },
        true
    )]
    #[case::sequencer_key_path(
        NodeMode::Sequencer,
        SignerArgs { sequencer_key_path: Some(PathBuf::from("/tmp/key.hex")), ..Default::default() },
        true
    )]
    #[case::sequencer_remote_endpoint(
        NodeMode::Sequencer,
        SignerArgs {
            endpoint: Some(Url::parse("http://localhost:8080").unwrap()),
            ..Default::default()
        },
        true
    )]
    fn validates_sequencer_key(
        #[case] mode: NodeMode,
        #[case] signer: SignerArgs,
        #[case] expected_ok: bool,
    ) {
        let args = ConsensusNodeArgs::new(
            ConsensusChainArgs { l2_chain_id: Chain::from(8453_u64) },
            ConsensusNodeConfigArgs {
                node_mode: mode,
                p2p_flags: P2PArgs { signer, ..P2PArgs::default() },
                ..default_node_config_args()
            },
        );
        assert_eq!(args.validate_sequencer_key().is_ok(), expected_ok);
    }
}
