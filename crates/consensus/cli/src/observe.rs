//! Standalone consensus-layer gossip observer command.
//!
//! `base-consensus observe` joins the Base CL gossip network (discv5 + gossipsub) and records
//! block-arrival latency for canonical (unsafe) blocks via the gossip layer's `LatencyRecorder`
//! (enable with `--p2p.latency.log` / `--p2p.latency.region`). Unlike `node`, it runs with **no
//! execution layer, no Engine API, and no L1 derivation** — it only listens to gossip. It is a
//! lightweight observer intended for one-off P2P latency measurements, not a full node.

use base_cli_utils::{LogConfig, RuntimeManager};
use base_common_chains::ChainConfig;
use base_consensus_node::{GossipTransport, NetworkBuilder};
use clap::Args;
use reth_node_core::args::TraceArgs;

use crate::{ConsensusChainArgs, LogArgs, MetricsArgs, P2PArgs, metrics::CliMetrics};

/// Standalone CL gossip observer command.
#[derive(Args, Clone, Debug)]
pub struct ConsensusObserveCommand {
    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub metrics: MetricsArgs,

    /// `OpenTelemetry` tracing export configuration.
    #[command(flatten)]
    pub traces: TraceArgs,

    /// P2P network configuration (including `--p2p.latency.log` / `--p2p.latency.region`).
    #[command(flatten)]
    pub p2p: P2PArgs,
}

impl ConsensusObserveCommand {
    /// Runs the standalone CL gossip observer.
    pub fn run(self, chain: ConsensusChainArgs) -> eyre::Result<()> {
        base_cli_utils::MetricsConfig::from(self.metrics.clone()).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        let chain_id = chain.l2_chain_id.id();
        let rollup_config = ChainConfig::rollup_config_by_chain_id(chain_id).ok_or_else(|| {
            eyre::eyre!(
                "no built-in rollup config for chain id {chain_id}; `observe` supports built-in chains"
            )
        })?;
        // The gossip layer validates block signatures against the unsafe block signer. Resolve it
        // from the built-in chain config (no L1 needed); custom chains must pass
        // `--p2p.unsafe.block.signer`.
        let genesis_signer = ChainConfig::by_chain_id(chain_id).and_then(|c| c.unsafe_block_signer);

        if self.metrics.enabled {
            CliMetrics::init_rollup_config(&rollup_config);
            CliMetrics::init_p2p(&self.p2p);
        }

        let rt = RuntimeManager::new().tokio_runtime()?;
        rt.block_on(async {
            LogConfig::from(self.logging.clone())
                .init_with_trace_args(&self.traces, &["libp2p_gossipsub=error"])
        })?;

        let p2p = self.p2p;
        rt.block_on(async move {
            let network_config = p2p.config(&rollup_config, chain_id, None, genesis_signer).await?;

            if network_config.latency_log.is_none() {
                tracing::warn!(
                    target: "observe",
                    "no --p2p.latency.log set; observer will join gossip but write no latency CSV"
                );
            }

            let mut handler = NetworkBuilder::from(network_config).build()?.start().await?;
            tracing::info!(
                target: "observe",
                chain_id,
                "CL gossip observer started; recording block-arrival latency"
            );

            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!(target: "observe", "Received Ctrl-C, shutting down...");
                }
                _ = async {
                    // `next_unsafe_block` drives discovery + gossip and fires the latency recorder
                    // on each first-seen block; we drop the payload since there is no engine.
                    while handler.next_unsafe_block().await.is_some() {}
                    tracing::warn!(target: "observe", "gossip transport closed unexpectedly");
                } => {}
            }

            Ok::<(), eyre::Report>(())
        })
    }
}
