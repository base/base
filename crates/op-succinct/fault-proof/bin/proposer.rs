use std::sync::Arc;

use alloy_provider::ProviderBuilder;
use anyhow::Result;
use clap::Parser;
use fault_proof::{
    config::ProposerConfig,
    contract::{AnchorStateRegistry, DisputeGameFactory},
    prometheus::ProposerGauge,
    proposer::OPSuccinctProposer,
};
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher,
    metrics::{init_metrics, MetricsGauge},
    setup_logger,
};
use op_succinct_proof_utils::initialize_host;
use op_succinct_signer_utils::SignerLock;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOCATOR: Jemalloc = Jemalloc;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = ".env.proposer")]
    env_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    dotenv::from_filename(args.env_file).ok();

    setup_logger();

    let proposer_config = ProposerConfig::from_env()?;
    proposer_config.log();

    let proposer_signer = SignerLock::from_env().await?;

    let l1_provider = ProviderBuilder::new().connect_http(proposer_config.l1_rpc.clone());

    let anchor_state_registry = AnchorStateRegistry::new(
        proposer_config.anchor_state_registry_address,
        l1_provider.clone(),
    );

    let factory = DisputeGameFactory::new(proposer_config.factory_address, l1_provider.clone());

    let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
    let host = initialize_host(Arc::new(fetcher.clone()));

    let proposer = Arc::new(
        OPSuccinctProposer::new(
            proposer_config,
            proposer_signer,
            anchor_state_registry,
            factory,
            Arc::new(fetcher),
            host,
        )
        .await
        .unwrap(),
    );

    // Initialize proposer gauges.
    ProposerGauge::register_all();

    // Initialize metrics exporter.
    init_metrics(&proposer.config.metrics_port);

    // Initialize the metrics gauges.
    ProposerGauge::init_all();

    proposer.run().await.expect("Runs in an infinite loop");

    Ok(())
}
