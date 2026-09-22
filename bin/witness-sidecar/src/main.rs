//! Payload witness cache sidecar.

use std::{net::SocketAddr, sync::Arc};

use alloy_provider::RootProvider;
use base_cli_utils::{LogConfig, RuntimeManager};
use base_common_chains::rollup_config;
use base_common_network::Base;
use base_witness_cache::{
    DEFAULT_WITNESS_CACHE_BLOCKS, WitnessCache, WitnessFollower, WitnessServer,
};
use clap::Parser;
use eyre::eyre;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

base_cli_utils::define_log_args!("BASE_WITNESS_SIDECAR");
base_cli_utils::define_metrics_args!("BASE_WITNESS_SIDECAR", 7401);

/// Caches `debug_executePayload` witnesses for new L2 blocks.
#[derive(Parser)]
#[command(author, version)]
struct Cli {
    /// Logging arguments.
    #[command(flatten)]
    logging: LogArgs,

    /// Metrics arguments.
    #[command(flatten)]
    metrics: MetricsArgs,

    /// L2 execution layer RPC URL. Point this at a proof node.
    #[arg(long, env = "L2_ETH_URL")]
    l2_eth_url: String,

    /// L2 chain ID. Selects the rollup config used to rebuild payload attributes.
    #[arg(long, env = "L2_CHAIN_ID")]
    l2_chain_id: u64,

    /// Address the cache HTTP server binds.
    #[arg(long, env = "LISTEN_ADDR", default_value = "127.0.0.1:7400")]
    listen_addr: SocketAddr,

    /// Number of payload witnesses to retain.
    ///
    /// At a 2 second block time, 3600 blocks is about two hours. Uncompressed responses are about
    /// 15 megabytes, so the default holds on the order of 50 gigabytes.
    #[arg(long, env = "WITNESS_CACHE_BLOCKS", default_value_t = DEFAULT_WITNESS_CACHE_BLOCKS)]
    max_blocks: usize,
}

impl Cli {
    fn run(self) -> eyre::Result<()> {
        if self.max_blocks == 0 {
            return Err(eyre!("WITNESS_CACHE_BLOCKS must be at least 1"));
        }
        LogConfig::from(self.logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(self.metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        let l2_eth_url = self.l2_eth_url;
        let l2_chain_id = self.l2_chain_id;
        let listen_addr = self.listen_addr;
        let max_blocks = self.max_blocks;
        RuntimeManager::new().run_until_shutdown(move |cancel| async move {
            run(l2_eth_url, l2_chain_id, listen_addr, max_blocks, cancel).await
        })
    }
}

async fn run(
    l2_eth_url: String,
    l2_chain_id: u64,
    listen_addr: SocketAddr,
    max_blocks: usize,
    cancel: CancellationToken,
) -> eyre::Result<()> {
    let rollup_config =
        rollup_config!(l2_chain_id).ok_or_else(|| eyre!("unknown L2 chain ID: {l2_chain_id}"))?;
    let url: url::Url = l2_eth_url.parse().map_err(|error| eyre!("invalid L2_ETH_URL: {error}"))?;
    let provider = RootProvider::<Base>::new_http(url);
    let cache = Arc::new(WitnessCache::new(max_blocks));
    let listener = TcpListener::bind(listen_addr).await?;
    info!(listen_addr = %listener.local_addr()?, max_blocks, "payload witness cache listening");

    let server = WitnessServer::serve(Arc::clone(&cache), listener);
    let mut follower = tokio::spawn(WitnessFollower::new(provider, rollup_config, cache).run());
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.map_err(Into::into),
        result = &mut follower => match result {
            Ok(()) => Err(eyre!("witness follower stopped")),
            Err(error) => Err(eyre!("witness follower stopped: {error}")),
        },
        () = cancel.cancelled() => {
            follower.abort();
            info!("payload witness cache stopped");
            Ok(())
        }
    }
}

fn main() -> eyre::Result<()> {
    Cli::parse().run()
}
