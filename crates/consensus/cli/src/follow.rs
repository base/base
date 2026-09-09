//! Reusable consensus follow-node arguments and launch helpers.

use std::{num::ParseIntError, sync::Arc, time::Duration};

use base_common_chain_config::RollupConfig;
use base_consensus_engine::LocalEngineClient;
use base_consensus_driver_service::{FollowNode, FollowNodeConfig, L1Config, RemoteL2Client};
use base_consensus_source_providers::{L1RpcProvider, OnlineBeaconClient};
use base_consensus_rpc::RpcBuilder;
use clap::Args;
use tracing::{error, info, warn};
use url::Url;

use crate::{
    ConsensusChainArgs, ConsensusNodeConfigArgs, L1ClientArgs, L1ConfigFile, L2ConfigFile, RpcArgs,
};

/// Follow-mode options for an integrated RPC node.
#[derive(Args, Clone, Debug)]
pub struct EmbeddedFollowArgs {
    /// Follow this node instead of deriving blocks from L1.
    #[arg(long = "source-l2-rpc", env = "BASE_NODE_SOURCE_L2_RPC")]
    pub source_l2_rpc: Option<Url>,

    /// Gate sync behind the local proofs history progress.
    #[arg(long = "follow.proofs", env = "BASE_NODE_PROOFS", requires = "source_l2_rpc")]
    pub proofs: bool,

    /// Maximum blocks to advance beyond the proofs history head.
    #[arg(
        long = "proofs.max-blocks-ahead",
        default_value_t = 16,
        env = "BASE_NODE_PROOFS_MAX_BLOCKS_AHEAD"
    )]
    pub proofs_max_blocks_ahead: u64,

    /// Delay after each successful payload insert, in milliseconds.
    #[arg(long = "follow.insert-delay-ms", default_value = "0",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> {
            Ok(Duration::from_millis(arg.parse()?))
        }, env = "BASE_NODE_FOLLOW_INSERT_DELAY_MS")]
    pub insert_delay: Duration,
}

impl EmbeddedFollowArgs {
    /// Builds follow configuration using the integrated node's local endpoints.
    pub fn into_config(
        self,
        consensus: ConsensusNodeConfigArgs,
    ) -> Option<ConsensusFollowNodeConfigArgs> {
        Some(ConsensusFollowNodeConfigArgs {
            source_l2_rpc: self.source_l2_rpc?,
            proofs: self.proofs,
            proofs_max_blocks_ahead: self.proofs_max_blocks_ahead,
            insert_delay: self.insert_delay,
            rpc_flags: consensus.rpc_flags,
            l2_config: consensus.l2_config,
            l1_config: consensus.l1_config,
            l1_rpc_args: consensus.l1_rpc_args,
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
        let mut config =
            self.config.l2_config.load(&self.chain.l2_chain_id).map_err(|e| eyre::eyre!(e))?;
        self.config.l1_rpc_args.apply_da_batch_inbox_override(&mut config);
        Ok(config)
    }

    /// Builds a follow node from explicit runtime dependencies.
    async fn follow_node(
        &self,
        cfg: RollupConfig,
        engine_client: LocalEngineClient,
    ) -> eyre::Result<FollowNode> {
        let rollup_config = Arc::new(cfg.clone());
        let local_l2_provider = engine_client.l2.clone();
        let proofs_progress = engine_client.proofs_progress.clone();
        let engine_client = Arc::new(engine_client);
        let l1_provider = L1RpcProvider::new_http_with_timeout(
            self.config.l1_rpc_args.l1_eth_rpc.clone(),
            self.config.l1_rpc_args.l1_rpc_timeout,
        );
        let l2_source = RemoteL2Client::new(self.config.source_l2_rpc.clone());
        let rpc_builder = Option::<RpcBuilder>::from(self.config.rpc_flags.clone());

        Ok(FollowNode::new(FollowNodeConfig {
            rollup_config,
            engine_client,
            l1_provider,
            local_l2_provider,
            l2_source,
            rpc_builder,
            proofs_progress,
            proofs_enabled: self.config.proofs,
            proofs_max_blocks_ahead: self.config.proofs_max_blocks_ahead,
            insert_delay: self.config.insert_delay,
        }))
    }

    /// Starts following with the integrated node's resolved upgrade schedule.
    pub async fn start_with_rollup_config(
        &self,
        cfg: RollupConfig,
        engine_client: LocalEngineClient,
    ) -> eyre::Result<()> {
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

        if self.config.proofs && engine_client.proofs_progress.is_none() {
            return Err(eyre::eyre!("follow proof gating requires the proofs-history extension"));
        }

        self.follow_node(cfg, engine_client).await?.start().await.map_err(|e| {
            error!(target: "rollup_node", error = %e, "Failed to start follow node");
            eyre::eyre!(e)
        })?;

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
            engine_provider: L1RpcProvider::new_http_with_timeout(
                self.config.l1_rpc_args.l1_eth_rpc.clone(),
                self.config.l1_rpc_args.l1_rpc_timeout,
            ),
            finalized_poll_interval: self
                .config
                .l1_rpc_args
                .l1_finalized_poll_interval
                .unwrap_or_else(|| L1Config::default_finalized_poll_interval(cfg.l1_chain_id)),
            verifier_l1_confs: self.config.l1_rpc_args.l1_verifier_confs,
            da_batcher_sender_override: self.config.l1_rpc_args.l1_da_batcher_sender_override,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_chains::Chain;
    use alloy_primitives::address;
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
    fn applies_da_batch_inbox_override() {
        let inbox = address!("3333333333333333333333333333333333333333");
        let config = parse_config(&[
            "--l1.dangerously-override-da-batch-inbox",
            "0x3333333333333333333333333333333333333333",
        ]);
        let args = ConsensusFollowNodeArgs::new(
            ConsensusChainArgs { l2_chain_id: Chain::from(8453_u64) },
            config,
        );

        let config = args.load_rollup_config().unwrap();

        assert_eq!(config.batch_inbox_address, inbox);
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
