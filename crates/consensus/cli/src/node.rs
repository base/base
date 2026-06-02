//! Reusable consensus node arguments and launch helpers.

use std::{path::PathBuf, sync::Arc};

use alloy_primitives::Address;
use alloy_provider::RootProvider;
use alloy_rpc_types_engine::JwtSecret;
use base_cli_utils::{LogConfig, RuntimeManager};
use base_common_chains::ChainConfig;
use base_common_genesis::RollupConfig;
use base_consensus_node::{EngineConfig, L1ConfigBuilder, NodeMode, RollupNode, RollupNodeBuilder};
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, UpgradeSignal, UpgradeSignalArgs, UpgradeSignalConfig,
    UpgradeSignalReader,
};
use clap::Args;
use eyre::Context;
use strum::IntoEnumIterator;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use url::Url;

use crate::{
    ConsensusChainArgs, EmbeddedL2ClientArgs, EmbeddedP2PArgs, EmbeddedRpcArgs, L1ClientArgs,
    L1ConfigFile, L2ClientArgs, L2ConfigFile, LogArgs, MetricsArgs, P2PArgs, RpcArgs,
    SequencerArgs, metrics::CliMetrics,
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

    /// Path to the checkpoint database. If not set, a default path under `~/.base` is used.
    #[arg(long = "checkpoint.path", env = "BASE_NODE_CHECKPOINT_PATH")]
    pub checkpoint_path: Option<PathBuf>,

    /// L1 upgrade signal observer arguments.
    #[command(flatten)]
    pub upgrade_signal: UpgradeSignalArgs,
}

/// Consensus node configuration arguments for embedded callers.
#[derive(Args, Clone, Debug)]
pub struct EmbeddedConsensusNodeConfigArgs {
    /// L1 RPC CLI arguments.
    #[clap(flatten)]
    pub l1_rpc_args: L1ClientArgs,

    /// L2 engine CLI arguments.
    #[clap(flatten)]
    pub l2_client_args: EmbeddedL2ClientArgs,

    /// L1 configuration file.
    #[clap(flatten)]
    pub l1_config: L1ConfigFile,

    /// L2 configuration file.
    #[clap(flatten)]
    pub l2_config: L2ConfigFile,

    /// P2P CLI arguments.
    #[command(flatten)]
    pub p2p_flags: EmbeddedP2PArgs,

    /// RPC CLI arguments.
    #[command(flatten)]
    pub rpc_flags: EmbeddedRpcArgs,

    /// Path to the `SafeDB` directory. If not set, safe head tracking is disabled.
    #[arg(long = "safedb.path", env = "BASE_NODE_SAFEDB_PATH")]
    pub safedb_path: Option<PathBuf>,

    /// Path to the checkpoint database. If not set, a default path under `~/.base` is used.
    #[arg(long = "checkpoint.path", env = "BASE_NODE_CHECKPOINT_PATH")]
    pub checkpoint_path: Option<PathBuf>,

    /// L1 upgrade signal observer arguments.
    #[command(flatten)]
    pub upgrade_signal: UpgradeSignalArgs,
}

impl From<EmbeddedConsensusNodeConfigArgs> for ConsensusNodeConfigArgs {
    fn from(args: EmbeddedConsensusNodeConfigArgs) -> Self {
        Self {
            node_mode: NodeMode::Validator,
            l1_rpc_args: args.l1_rpc_args,
            l2_client_args: args.l2_client_args.into(),
            l1_config: args.l1_config,
            l2_config: args.l2_config,
            p2p_flags: args.p2p_flags.into(),
            rpc_flags: args.rpc_flags.into(),
            sequencer_flags: SequencerArgs::default(),
            safedb_path: args.safedb_path,
            checkpoint_path: args.checkpoint_path,
            upgrade_signal: args.upgrade_signal,
        }
    }
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
        mut cfg: RollupConfig,
        overrides: ConsensusNodeOverrides,
    ) -> eyre::Result<RollupNode> {
        self.validate_sequencer_key()?;
        let upgrade_signal_config = self.config.upgrade_signal.config()?;
        if let Some(signal_config) = &upgrade_signal_config {
            self.apply_initial_upgrade_signal(&mut cfg, signal_config).await?;
        }

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
        .with_sequencer_config(self.config.sequencer_flags.config())
        .with_upgrade_signal_config(upgrade_signal_config);

        if let Some(path) = self.config.checkpoint_path.clone() {
            builder = builder.with_checkpoint_path(path);
        }
        if let Some(path) = self.config.safedb_path.clone() {
            builder = builder.with_safedb_path(path);
        }

        builder.build().await.wrap_err("Failed to build rollup node")
    }

    /// Applies the configured L1 upgrade signal to the rollup config before startup.
    pub async fn apply_initial_upgrade_signal(
        &self,
        cfg: &mut RollupConfig,
        signal_config: &UpgradeSignalConfig,
    ) -> eyre::Result<()> {
        let reader = AlloyUpgradeSignalReader::new(
            RootProvider::new_http(self.config.l1_rpc_args.l1_eth_rpc.clone()),
            signal_config.contract_address,
        );
        let signal = reader.read_signal(&signal_config.hardfork_id).await?;

        Self::apply_signal_to_rollup_config(cfg, &signal);

        Ok(())
    }

    /// Applies a positive Azul activation timestamp to a rollup config.
    pub fn apply_signal_to_rollup_config(cfg: &mut RollupConfig, signal: &UpgradeSignal) -> bool {
        let Some(timestamp) = signal.azul_activation_timestamp() else {
            return false;
        };

        cfg.set_base_azul_activation_timestamp(timestamp);
        info!(
            target: "upgrade_signal",
            hardfork_id = %signal.hardfork_id,
            activation_timestamp = %timestamp,
            "applied upgrade signal to rollup config"
        );

        true
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
        self.start_with_overrides_and_cancellation(cfg, overrides, CancellationToken::new()).await
    }

    /// Starts a rollup node with caller-supplied endpoint overrides and cancellation.
    pub async fn start_with_overrides_and_cancellation(
        &self,
        cfg: RollupConfig,
        overrides: ConsensusNodeOverrides,
        cancellation: CancellationToken,
    ) -> eyre::Result<()> {
        self.build_rollup_node_with_overrides(cfg, overrides)
            .await?
            .start_with_cancellation(cancellation)
            .await
            .map_err(|e| {
                error!(target: "rollup_node", error = %e, "Failed to start rollup node service");
                eyre::eyre!(e)
            })
    }

    /// Returns the configured genesis signer address for the selected L2 chain.
    pub fn genesis_signer(&self) -> eyre::Result<Address> {
        let id = self.chain.l2_chain_id;
        ChainConfig::by_chain_id(id.id())
            .and_then(|cfg| cfg.unsafe_block_signer)
            .ok_or_else(|| eyre::eyre!("No unsafe block signer found for chain ID: {id}"))
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Command};

    use alloy_chains::Chain;
    use alloy_primitives::{B256, U256, address};
    use clap::{Args, Parser};
    use rstest::rstest;

    use super::*;
    use crate::SignerArgs;

    const SIGNER_ENV_KEYS: &[&str] = &[
        "BASE_NODE_P2P_SEQUENCER_KEY",
        "BASE_NODE_P2P_SEQUENCER_KEY_PATH",
        "BASE_NODE_P2P_SIGNER_ENDPOINT",
        "BASE_NODE_P2P_SIGNER_ADDRESS",
    ];
    const SIGNER_ENV_CHILD_TEST: &str = "node::tests::validates_sequencer_key_from_env_child";

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
            checkpoint_path: None,
            upgrade_signal: UpgradeSignalArgs::default(),
        }
    }

    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn parses_upgrade_signal_args() {
        let args = CommandParser::<ConsensusNodeConfigArgs>::parse_from([
            "base-consensus",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
            "--l2-engine-rpc",
            "http://localhost:8551",
            "--upgrade-signal.contract",
            "0x0000000000000000000000000000000000000001",
            "--upgrade-signal.hardfork-id",
            "azul",
        ])
        .args;

        assert_eq!(
            args.upgrade_signal.contract_address,
            Some(address!("0000000000000000000000000000000000000001"))
        );
        assert_eq!(args.upgrade_signal.hardfork_id.as_deref(), Some("azul"));
    }

    fn upgrade_signal(hardfork_id: &str, activation_timestamp: u64) -> UpgradeSignal {
        UpgradeSignal {
            hardfork_id: hardfork_id.to_string(),
            activation_timestamp,
            protocol_version: U256::from(7),
            l1_block_number: 1,
        }
    }

    #[test]
    fn applies_positive_azul_signal_to_rollup_config() {
        let mut cfg = RollupConfig::default();

        let applied =
            ConsensusNodeArgs::apply_signal_to_rollup_config(&mut cfg, &upgrade_signal("azul", 42));

        assert!(applied);
        assert_eq!(cfg.hardforks.base.azul, Some(42));
    }

    #[test]
    fn ignores_zero_azul_signal_for_rollup_config() {
        let mut cfg = RollupConfig::default();

        let applied =
            ConsensusNodeArgs::apply_signal_to_rollup_config(&mut cfg, &upgrade_signal("azul", 0));

        assert!(!applied);
        assert_eq!(cfg.hardforks.base.azul, None);
    }

    #[test]
    fn ignores_non_azul_signal_for_rollup_config() {
        let mut cfg = RollupConfig::default();

        let applied = ConsensusNodeArgs::apply_signal_to_rollup_config(
            &mut cfg,
            &upgrade_signal("beryl", 42),
        );

        assert!(!applied);
        assert_eq!(cfg.hardforks.base.azul, None);
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
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.arg("--exact").arg(SIGNER_ENV_CHILD_TEST).arg("--ignored");

        for key in SIGNER_ENV_KEYS {
            command.env_remove(key);
        }
        for (key, value) in env_vars {
            command.env(key, value);
        }
        let output = command.output().unwrap();

        assert!(
            output.status.success(),
            "child env parsing test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "spawned by validates_sequencer_key_from_env with isolated process env"]
    fn validates_sequencer_key_from_env_child() {
        let signer = SignerArgs::parse_from(["test"]);
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
