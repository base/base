//! Reusable consensus follow-node arguments and launch helpers.

use std::{num::ParseIntError, sync::Arc, time::Duration};

use alloy_provider::{Provider, RootProvider};
use base_cli_utils::{LogConfig, RuntimeManager};
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_consensus_node::{
    EngineConfig, FollowNode, FollowNodeConfig, L1Config, NodeMode, RemoteL2Client,
};
use base_consensus_providers::OnlineBeaconClient;
use base_consensus_rpc::RpcBuilder;
use clap::Args;
use tracing::{error, info, warn};
use url::Url;

use crate::{
    ConsensusChainArgs, L1ClientArgs, L1ConfigFile, L2ClientArgs, L2ConfigFile, LogArgs,
    MetricsArgs, RpcArgs, metrics::CliMetrics,
};

/// Standalone consensus follow-node command.
#[derive(Args, Clone, Debug)]
pub struct ConsensusFollowNodeCommand {
    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub metrics: MetricsArgs,

    /// Follow-node arguments.
    #[command(flatten)]
    pub args: ConsensusFollowNodeConfigArgs,
}

impl ConsensusFollowNodeCommand {
    /// Runs the standalone consensus follow-node command.
    pub fn run(self, chain: ConsensusChainArgs) -> eyre::Result<()> {
        base_cli_utils::init_tracing!(
            LogConfig::from(self.logging.clone()),
            ["libp2p_gossipsub=error"]
        )?;

        base_cli_utils::MetricsConfig::from(self.metrics.clone()).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        let args = ConsensusFollowNodeArgs::new(chain, self.args);
        let metrics_config = if self.metrics.enabled {
            let cfg = args.load_rollup_config()?;
            CliMetrics::init_rollup_config(&cfg);
            Some(cfg)
        } else {
            None
        };

        RuntimeManager::new().run_until_ctrl_c(async move {
            let _upgrade_countdown_metrics =
                metrics_config.map(CliMetrics::spawn_upgrade_countdown_recorder);
            args.start().await
        })
    }
}

/// Consensus follow-node arguments shared by the standalone and unified binaries.
#[derive(Args, Clone, Debug)]
pub struct ConsensusFollowNodeArgs {
    /// Chain selection.
    #[command(flatten)]
    pub chain: ConsensusChainArgs,

    /// Follow-node configuration.
    #[command(flatten)]
    pub config: ConsensusFollowNodeConfigArgs,
}

impl ConsensusFollowNodeArgs {
    /// Creates reusable consensus follow-node arguments from typed chain and follow config
    /// components.
    pub const fn new(chain: ConsensusChainArgs, config: ConsensusFollowNodeConfigArgs) -> Self {
        Self { chain, config }
    }
}

/// Consensus follow-node configuration arguments without chain selection.
#[derive(Args, Clone, Debug)]
pub struct ConsensusFollowNodeConfigArgs {
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

    /// Gate sync behind proofs progress via `debug_proofsSyncStatus`.
    #[arg(long = "proofs", default_value_t = false, env = "BASE_NODE_PROOFS")]
    pub proofs: bool,

    /// Maximum number of blocks the follow node may advance beyond the proofs
    /// `ExEx` head. Only effective when `--proofs` is enabled.
    #[arg(
        long = "proofs.max-blocks-ahead",
        default_value_t = 16,
        env = "BASE_NODE_PROOFS_MAX_BLOCKS_AHEAD"
    )]
    pub proofs_max_blocks_ahead: u64,

    /// Delay after each successful source payload insert, in milliseconds.
    #[arg(
        long = "follow.insert-delay-ms",
        default_value = "0",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> {
            Ok(Duration::from_millis(arg.parse()?))
        },
        env = "BASE_NODE_FOLLOW_INSERT_DELAY_MS"
    )]
    pub insert_delay: Duration,

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

impl ConsensusFollowNodeArgs {
    /// Loads the configured L2 rollup config.
    pub fn load_rollup_config(&self) -> eyre::Result<RollupConfig> {
        self.config.l2_config.load(&self.chain.l2_chain_id).map_err(|e| eyre::eyre!(e))
    }

    /// Builds a follow node with default external endpoint configuration.
    pub async fn build_follow_node(&self) -> eyre::Result<FollowNode> {
        let cfg = self.load_rollup_config()?;
        let local_l2_provider = self.local_l2_provider();
        self.follow_node(cfg, local_l2_provider).await
    }

    /// Builds a follow node from explicit runtime dependencies.
    async fn follow_node(
        &self,
        cfg: RollupConfig,
        local_l2_provider: RootProvider<Base>,
    ) -> eyre::Result<FollowNode> {
        let l2_engine_rpc = self.config.l2_client_args.l2_engine_rpc.clone();
        let jwt_secret =
            self.config.l2_client_args.resolve_jwt_secret_for_endpoint(&l2_engine_rpc).await?;
        let rollup_config = Arc::new(cfg.clone());

        let engine_config = EngineConfig {
            config: Arc::clone(&rollup_config),
            l2_url: l2_engine_rpc,
            l2_jwt_secret: jwt_secret,
            l1_url: self.config.l1_rpc_args.l1_eth_rpc.clone(),
            mode: NodeMode::Validator,
        };
        let engine_client =
            Arc::new(engine_config.build_engine_client().await.map_err(|e| eyre::eyre!(e))?);
        let l2_source = RemoteL2Client::new(self.config.source_l2_rpc.clone());
        let rpc_builder = Option::<RpcBuilder>::from(self.config.rpc_flags.clone());

        Ok(FollowNode::new(FollowNodeConfig {
            rollup_config,
            engine_client,
            local_l2_provider,
            l2_source,
            rpc_builder,
            proofs_enabled: self.config.proofs,
            proofs_max_blocks_ahead: self.config.proofs_max_blocks_ahead,
            insert_delay: self.config.insert_delay,
        }))
    }

    /// Starts a follow node.
    pub async fn start(&self) -> eyre::Result<()> {
        let cfg = self.load_rollup_config()?;
        if !self.config.proofs {
            warn!(
                target: "rollup_node",
                "Running without --proofs; this mode is mainly meant for syncing the Proofs ExEx and does not support EL sync"
            );
        }

        info!(
            target: "rollup_node",
            chain_id = cfg.l2_chain_id.id(),
            source = %self.config.source_l2_rpc,
            "Starting follow node"
        );

        let local_l2_provider = self.local_l2_provider();
        if self.config.proofs {
            self.check_proofs_rpc(&local_l2_provider).await?;
        }

        self.follow_node(cfg, local_l2_provider).await?.start().await.map_err(|e| {
            error!(target: "rollup_node", error = %e, "Failed to start follow node");
            eyre::eyre!(e)
        })?;

        Ok(())
    }

    /// Builds the local L2 RPC provider from CLI arguments.
    pub fn local_l2_provider(&self) -> RootProvider<Base> {
        RootProvider::<Base>::new_http(self.config.l2_rpc_url.clone())
    }

    /// Checks that the local execution node exposes the proofs sync RPC.
    pub async fn check_proofs_rpc(&self, provider: &RootProvider<Base>) -> eyre::Result<()> {
        provider
            .raw_request::<_, serde_json::Value>("debug_proofsSyncStatus".into(), ())
            .await
            .map_err(|e| {
                error!(target: "rollup_node", error = %e, "debug_proofsSyncStatus call failed; is the Proofs ExEx enabled on the node?");
                eyre::eyre!("debug_proofsSyncStatus call failed: {e}")
            })?;
        info!(target: "rollup_node", "Proofs ExEx confirmed available via debug_proofsSyncStatus");
        Ok(())
    }

    /// Builds the L1 configuration for the follow node.
    pub fn l1_config(&self, cfg: &RollupConfig) -> eyre::Result<L1Config> {
        let l1_chain_config =
            self.config.l1_config.load(cfg.l1_chain_id).map_err(|e| eyre::eyre!(e))?;
        let l1_beacon = OnlineBeaconClient::new_http(self.config.l1_rpc_args.l1_beacon.to_string());

        Ok(L1Config {
            chain_config: Arc::new(l1_chain_config),
            trust_rpc: self.config.l1_rpc_args.l1_trust_rpc,
            beacon_client: l1_beacon,
            engine_provider: RootProvider::new_http(self.config.l1_rpc_args.l1_eth_rpc.clone()),
            finalized_poll_interval: L1Config::default_finalized_poll_interval(cfg.l1_chain_id),
            verifier_l1_confs: self.config.l1_rpc_args.l1_verifier_confs,
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        args: ConsensusFollowNodeConfigArgs,
    }

    fn parse_config(args: &[&str]) -> ConsensusFollowNodeConfigArgs {
        let required = [
            "test",
            "--source-l2-rpc",
            "http://localhost:8545",
            "--l2-engine-rpc",
            "http://localhost:8551",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
        ];
        Command::parse_from([required.as_slice(), args].concat()).args
    }

    #[test]
    fn proofs_default_to_disabled() {
        assert!(!parse_config(&[]).proofs);
    }

    #[test]
    fn proofs_accept_bare_flag() {
        assert!(parse_config(&["--proofs"]).proofs);
    }

    #[test]
    fn rpc_disabled_stays_optional() {
        let config = parse_config(&["--rpc.disabled"]);

        assert!(Option::<RpcBuilder>::from(config.rpc_flags).is_none());
    }
}
