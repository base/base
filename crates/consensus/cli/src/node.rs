//! Reusable consensus node arguments and launch helpers.

use std::{path::PathBuf, sync::Arc};

use alloy_chains::Chain;
use alloy_primitives::Address;
use alloy_rpc_types_engine::JwtSecret;
use base_common_chains::Registry;
use base_common_genesis::RollupConfig;
use base_consensus_node::{EngineConfig, L1ConfigBuilder, NodeMode, RollupNode, RollupNodeBuilder};
use clap::Args;
use eyre::Context;
use strum::IntoEnumIterator;
use tracing::{error, info};
use url::Url;

use crate::{
    L1ClientArgs, L1ConfigFile, L2ClientArgs, L2ConfigFile, P2PArgs, RpcArgs, SequencerArgs,
};

/// Overrides supplied by callers that embed consensus alongside another service.
#[derive(Clone, Debug, Default)]
pub struct ConsensusNodeOverrides {
    /// Override for the L2 Engine API endpoint.
    pub l2_engine_rpc: Option<Url>,
    /// Override for the L2 Engine API JWT secret.
    pub l2_engine_jwt_secret: Option<JwtSecret>,
}

/// Consensus node arguments shared by the standalone and unified binaries.
#[derive(Args, Clone, Debug)]
pub struct ConsensusNodeArgs {
    /// L2 Chain ID or name (8453 = Base Mainnet, 84532 = Base Sepolia).
    #[arg(
        long = "chain",
        short = 'n',
        global = true,
        default_value = "8453",
        env = "BASE_NODE_NETWORK"
    )]
    pub l2_chain_id: Chain,

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
        self.l2_config.load(&self.l2_chain_id).map_err(|e| eyre::eyre!("{e}"))
    }

    /// Validates that a sequencer signing key is configured when running in sequencer mode.
    pub fn validate_sequencer_key(&self) -> eyre::Result<()> {
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
            self.l1_config.load(cfg.l1_chain_id).map_err(|e| eyre::eyre!("{e}"))?;
        let l1_config = L1ConfigBuilder {
            chain_config: l1_chain_config,
            trust_rpc: self.l1_rpc_args.l1_trust_rpc,
            beacon: self.l1_rpc_args.l1_beacon.clone(),
            rpc_url: self.l1_rpc_args.l1_eth_rpc.clone(),
            slot_duration_override: self.l1_rpc_args.l1_slot_duration_override,
            verifier_l1_confs: self.l1_rpc_args.l1_verifier_confs,
        };

        let l2_engine_rpc =
            overrides.l2_engine_rpc.unwrap_or_else(|| self.l2_client_args.l2_engine_rpc.clone());
        let jwt_secret = match overrides.l2_engine_jwt_secret {
            Some(secret) => secret,
            None => self.resolve_engine_jwt_secret(&l2_engine_rpc).await?,
        };

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
            l2_url: l2_engine_rpc,
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
            eyre::eyre!("{e}")
        })
    }

    /// Returns the signer [`Address`] from the rollup config for the given l2 chain id.
    fn genesis_signer(&self) -> eyre::Result<Address> {
        let id = self.l2_chain_id;
        Registry::unsafe_block_signer(id.id())
            .ok_or_else(|| eyre::eyre!("No unsafe block signer found for chain ID: {id}"))
    }

    async fn resolve_engine_jwt_secret(&self, l2_engine_rpc: &Url) -> eyre::Result<JwtSecret> {
        if l2_engine_rpc.scheme() == "file" {
            return Ok(self.l2_client_args.jwt_secret().unwrap_or_else(|_| JwtSecret::random()));
        }

        self.l2_client_args.validate_jwt().await
    }
}
