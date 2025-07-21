use std::{env, sync::Arc};

use alloy_primitives::Address;
use alloy_provider::ProviderBuilder;
use alloy_transport_http::reqwest::Url;
use anyhow::Result;
use clap::Parser;
use fault_proof::{
    config::ProposerConfig, contract::DisputeGameFactory, prometheus::ProposerGauge,
    proposer::OPSuccinctProposer,
};
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher,
    metrics::{init_metrics, MetricsGauge},
    setup_logger,
};
use op_succinct_proof_utils::initialize_host;
use op_succinct_signer_utils::Signer;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = ".env.proposer")]
    env_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_logger();

    let args = Args::parse();
    dotenv::from_filename(args.env_file).ok();

    let proposer_signer = Signer::from_env()?;

    let l1_provider =
        ProviderBuilder::new().connect_http(env::var("L1_RPC").unwrap().parse::<Url>().unwrap());

    let factory = DisputeGameFactory::new(
        env::var("FACTORY_ADDRESS")
            .expect("FACTORY_ADDRESS must be set")
            .parse::<Address>()
            .unwrap(),
        l1_provider.clone(),
    );

    // Use PROVER_ADDRESS from env if available, otherwise use wallet's default signer address from
    // the private key.
    let prover_address = env::var("PROVER_ADDRESS")
        .ok()
        .and_then(|addr| addr.parse::<Address>().ok())
        .unwrap_or_else(|| proposer_signer.address());

    let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
    let host = initialize_host(Arc::new(fetcher.clone()));

    // Set a default network private key to avoid an error in mock mode.
    let network_private_key = env::var("NETWORK_PRIVATE_KEY").unwrap_or_else(|_| {
        tracing::warn!(
            "Using default NETWORK_PRIVATE_KEY of 0x01. This is only valid in mock mode."
        );
        "0x0000000000000000000000000000000000000000000000000000000000000001".to_string()
    });

    let proposer = Arc::new(
        OPSuccinctProposer::new(
            ProposerConfig::from_env()?,
            network_private_key,
            prover_address,
            proposer_signer,
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
